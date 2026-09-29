//! Bounded video intent planning; the model cannot select ownership or budgets.
use aiwork_core::{VideoWorkSnapshot, WorkAction};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkIntent {
    Create,
    Revise,
    Continue,
    Status,
    Download,
    Clarify,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkDecision {
    pub action: WorkIntent,
    pub effective_prompt: Option<String>,
    pub spec_patch: Value,
    pub reference_policy: String,
    pub clarification: Option<String>,
}
impl WorkDecision {
    pub fn paid_action(&self) -> Option<WorkAction> {
        match self.action {
            WorkIntent::Create => Some(WorkAction::Create),
            WorkIntent::Revise => Some(WorkAction::Revise),
            WorkIntent::Continue => Some(WorkAction::Continue),
            _ => None,
        }
    }
}
fn invalid() -> String {
    "work_decision_invalid".into()
}
fn message_text(m: &Value) -> String {
    match &m["content"] {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter(|p| p["type"] == "text")
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}
pub fn current_text(body: &Value) -> String {
    body["messages"]
        .as_array()
        .and_then(|m| m.iter().rev().find(|m| m["role"] == "user"))
        .map(message_text)
        .or_else(|| body["prompt"].as_str().map(str::to_owned))
        .unwrap_or_default()
}
pub fn clear_reference(text: &str) -> bool {
    [
        "不用参考图",
        "取消参考图",
        "去掉参考图",
        "不使用参考图",
        "不要参考图",
    ]
    .iter()
    .any(|s| text.contains(s))
}
fn merge_reference(text: &str) -> bool {
    ["合并", "一起参考", "保留原参考"]
        .iter()
        .any(|s| text.contains(s))
}
fn cut(s: &str, max: usize) -> String {
    let mut n = s.len().min(max);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    s[..n].to_string()
}
fn sanitized(text: &str) -> String {
    let mut s = crate::work_context::strip_markers(text);
    for (start, end) in [
        ("<uploaded_files>", "</uploaded_files>"),
        ("[AIWORK_REFERENCE:", "]"),
    ] {
        while let Some(i) = s.find(start) {
            let n = s[i + start.len()..]
                .find(end)
                .map(|n| i + start.len() + n + end.len())
                .unwrap_or(s.len());
            s.replace_range(i..n, "");
        }
    }
    s = s.replace("<user_input>", "").replace("</user_input>", "");
    for prefix in [
        "aw_live_",
        "sk-",
        "Bearer ",
        "data:image/",
        "data:video/",
        "http://",
        "https://",
    ] {
        let mut offset = 0;
        while let Some(n) = s[offset..].find(prefix) {
            let i = offset + n;
            let search = i + prefix.len();
            let end = s[search..]
                .char_indices()
                .find(|(_, c)| c.is_whitespace() || matches!(*c, '"' | '\'' | '<' | '>'))
                .map(|(n, _)| search + n)
                .unwrap_or(s.len());
            let end = if end <= i { i + prefix.len() } else { end };
            s.replace_range(i..end, "[已隐藏]");
            offset = i + "[已隐藏]".len();
        }
    }
    crate::user_routes::normalize_video_spec_text(&s)
        .trim()
        .to_string()
}
pub fn read_only_decision(body: &Value, has_parent: bool) -> Option<WorkDecision> {
    let t = current_text(body);
    let t = t.trim();
    let explicit = body["action"].as_str();
    let action = match explicit {
        Some("status") => WorkIntent::Status,
        Some("download") => WorkIntent::Download,
        Some("clarify") => WorkIntent::Clarify,
        Some("create" | "revise" | "continue") => return None,
        Some(_) => WorkIntent::Clarify,
        None if ["不满意", "不好", "不行", "重新做"].contains(&t)
            || ["这个视频不满意", "视频不满意"].contains(&t) =>
        {
            WorkIntent::Clarify
        }
        None if t == "查看任务状态" || t == "生成好了没" || t == "现在进度怎么样" => {
            WorkIntent::Status
        }
        None if t == "重新下载刚才的视频" || t == "重新下载" => WorkIntent::Download,
        _ => return None,
    };
    let action = if !has_parent && matches!(action, WorkIntent::Status | WorkIntent::Download) {
        WorkIntent::Clarify
    } else {
        action
    };
    Some(WorkDecision {
        action,
        effective_prompt: None,
        spec_patch: json!({}),
        reference_policy: "inherit".into(),
        clarification: Some(
            if has_parent {
                "请说明要修改哪些内容；本次未提交视频。"
            } else {
                "请先指定已有视频版本；本次未提交视频。"
            }
            .into(),
        ),
    })
}
pub fn build_helper_input(
    snapshot: Option<&VideoWorkSnapshot>,
    body: &Value,
    model: &str,
) -> Result<Value, String> {
    let current = sanitized(&current_text(body));
    if current.is_empty() || current.len() > 12 * 1024 {
        return Err("work_context_input_too_large".into());
    }
    let mut history = Vec::new();
    let mut remaining = 8 * 1024;
    if snapshot.is_some() {
        let users: Vec<_> = body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["role"] == "user")
            .collect();
        for m in users.iter().rev().skip(1).take(4) {
            let s = sanitized(&message_text(m));
            if s.is_empty() {
                continue;
            }
            let s = cut(&s, remaining.min(2048));
            remaining -= s.len();
            history.push(s);
            if remaining == 0 {
                break;
            }
        }
        history.reverse();
    }
    let parent=snapshot.map(|s|json!({"effective_prompt":cut(&sanitized(&s.effective_prompt),12*1024),"duration":s.duration,"resolution":s.resolution,"ratio":s.ratio,"watermark":s.watermark,
        "user_reference_count":s.user_media_ids.len(),"has_tail_frame":s.tail_frame_media_id.is_some(),"summary":cut(&sanitized(&s.summary),1024)}));
    let mut spec = Map::new();
    for f in ["duration", "resolution", "ratio", "watermark"] {
        if let Some(v) = body.get(f) {
            spec.insert(f.into(), v.clone());
        }
    }
    let payload = json!({"parent":parent,"history":history,"current":current,"explicit_spec":spec,"requested_action":body["action"].as_str()});
    let value = json!({"model":model,"stream":false,"max_tokens":1024,"temperature":0.2,"messages":[
        {"role":"system","content":"你是视频作业规划助手。仅输出严格JSON对象，字段只能是action、effective_prompt、spec_patch、reference_policy、clarification。action只能是create/revise/continue/status/download/clarify。用户明确独立生成才create；在已提供parent上明确修改才revise；明确接着上一段生成才continue；查看进度用status、重新下载用download，普通问候/测试/不满意但无修改方向用clarify。无parent不得猜父版本。effective_prompt忠实合并parent和当前修改，保留人物、动作、场景与未被修改的约束，不新增剧情；只读操作为null。spec_patch只能包含当前用户明确指定的duration/resolution/ratio/watermark；时长4至15秒，480p或720p，画幅16:9/9:16/1:1/4:3/3:4/21:9。全角字符规范化为半角。reference_policy为inherit/replace/merge/clear，新参考默认replace，只有用户明确合并才merge，明确取消才clear。clarification用于简短追问或只读回复，否则null。不要输出任何ID、链接、账户、工具、扣费字段；不要虚构已提交或完成。"},
        {"role":"user","content":payload.to_string()}]});
    if serde_json::to_vec(&value).map_err(|_| invalid())?.len() > 32 * 1024 {
        return Err("work_context_input_too_large".into());
    }
    Ok(value)
}
pub fn parse_decision(raw: &str) -> Result<WorkDecision, String> {
    if raw.len() > 16 * 1024 {
        return Err(invalid());
    }
    let mut d: WorkDecision = serde_json::from_str(raw.trim()).map_err(|_| invalid())?;
    // GLM may emit null when this turn changes no specification. Null is
    // exactly an empty patch, never permission to invent defaults or fields.
    if d.spec_patch.is_null() {d.spec_patch=json!({});}
    if !matches!(
        d.reference_policy.as_str(),
        "inherit" | "replace" | "merge" | "clear"
    ) || !d.spec_patch.is_object()
    {
        return Err(invalid());
    }
    validate_spec(&d.spec_patch)?;
    if d.paid_action().is_some()
        && d.effective_prompt
            .as_ref()
            .map_or(true, |s| s.trim().is_empty() || s.len() > 12 * 1024)
    {
        return Err(invalid());
    }
    if d.clarification.as_ref().is_some_and(|s| s.len() > 2048) {
        return Err(invalid());
    }
    Ok(d)
}

/// An explicitly uploaded source video is usable without a stored parent.
/// "Continue" in the prompt can mean extending that file, not Core's
/// tail-frame workflow. Keep the full video and prompt; never claim native
/// extension support from this routing decision alone.
pub fn resolve_uploaded_video_action(decision: &mut WorkDecision, has_parent: bool, body: &Value) {
    if !has_parent && decision.action == WorkIntent::Continue
        && matches!(body["action"].as_str(), None | Some("create"))
        && body["video_asset_ids"].as_array().is_some_and(|a| !a.is_empty())
    {
        decision.action = WorkIntent::Create;
    }
}
fn validate_spec(v: &Value) -> Result<(), String> {
    let fields = v.as_object().ok_or_else(invalid)?;
    for (field, val) in fields {
        match field.as_str() {
            "duration" if val.as_u64().is_some_and(|n| (4..=15).contains(&n)) => {}
            "resolution"
                if val.as_str().is_some_and(|s| {
                    matches!(s.to_ascii_lowercase().as_str(), "480p" | "720p")
                }) => {}
            "ratio"
                if val.as_str().is_some_and(|s| {
                    matches!(
                        crate::user_routes::normalize_video_spec_text(s)
                            .replace(' ', "")
                            .as_str(),
                        "16:9" | "9:16" | "1:1" | "4:3" | "3:4" | "21:9"
                    )
                }) => {}
            "watermark" if val.is_boolean() => {}
            _ => return Err("work_spec_unsupported".into()),
        }
    }
    Ok(())
}
fn text_spec(text: &str) -> Result<Value, String> {
    let text = crate::user_routes::normalize_video_spec_text(text).to_ascii_lowercase();
    let chars: Vec<char> = text.chars().collect();
    let mut patch = Map::new();
    for (i, c) in chars.iter().enumerate() {
        if !c.is_ascii_digit() || (i > 0 && chars[i - 1].is_ascii_digit()) {
            continue;
        }
        let mut end = i;
        while end < chars.len() && chars[end].is_ascii_digit() {
            end += 1;
        }
        let num: u64 = chars[i..end]
            .iter()
            .collect::<String>()
            .parse()
            .map_err(|_| invalid())?;
        let mut u = end;
        while u < chars.len() && chars[u].is_whitespace() {
            u += 1;
        }
        if matches!(chars.get(u), Some('秒' | 's'))
            && !chars.get(u + 1).is_some_and(|c| c.is_ascii_alphabetic())
            && !patch.contains_key("duration")
        {
            patch.insert("duration".into(), json!(num));
        }
        if chars.get(u) == Some(&'p') {
            patch
                .entry("resolution")
                .or_insert(json!(format!("{num}p")));
        }
        if chars.get(u) == Some(&'k') {
            patch
                .entry("resolution")
                .or_insert(json!(format!("{num}k")));
        }
        if chars.get(u) == Some(&':') {
            let mut j = u + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            let start = j;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if start < j {
                patch.entry("ratio").or_insert(json!(format!(
                    "{num}:{}",
                    chars[start..j].iter().collect::<String>()
                )));
            }
        }
    }
    let patch = Value::Object(patch);
    validate_spec(&patch)?;
    Ok(patch)
}
/// `work_user_media_ids` is populated only by the owned-media executor, never
/// copied from client input. This pure merger does not authorize media IDs.
pub fn merge_snapshot(
    base: Option<&VideoWorkSnapshot>,
    d: &WorkDecision,
    body: &Value,
) -> Result<VideoWorkSnapshot, String> {
    if d.paid_action().is_none() {
        return Err(invalid());
    }
    if matches!(d.action, WorkIntent::Revise | WorkIntent::Continue) && base.is_none() {
        return Err("work_parent_required".into());
    }
    validate_spec(&d.spec_patch)?;
    let current = current_text(body);
    let text = text_spec(&current)?;
    let mut explicit = Map::new();
    for field in ["duration", "resolution", "ratio", "watermark"] {
        if let Some(v) = body.get(field) {
            explicit.insert(field.into(), v.clone());
        }
    }
    validate_spec(&Value::Object(explicit.clone()))?;
    // A helper patch must not invent a specification absent from this turn.
    for (k, v) in d.spec_patch.as_object().ok_or_else(invalid)? {
        if text.get(k).or_else(|| explicit.get(k)).is_none() {
            return Err(invalid());
        }
        validate_spec(&json!({k:v}))?;
    }
    let base = if d.action == WorkIntent::Create {
        None
    } else {
        base
    };
    let mut s = base.cloned().unwrap_or(VideoWorkSnapshot {
        effective_prompt: String::new(),
        duration: 5,
        resolution: "720p".into(),
        ratio: "16:9".into(),
        watermark: false,
        user_media_ids: vec![],
        tail_frame_media_id: None,
        parent_version_id: None,
        source_request_id: None,
        reference_mode: "none".into(),
        summary: String::new(),
        dispatch_body:None,
    });
    s.effective_prompt = crate::user_routes::normalize_video_spec_text(
        d.effective_prompt
            .as_deref()
            .filter(|s| !s.trim().is_empty() && s.len() <= 12 * 1024)
            .ok_or_else(invalid)?,
    );
    for spec in [&text, &Value::Object(explicit)] {
        if let Some(v) = spec["duration"].as_u64() {
            s.duration = v as i64;
        }
        if let Some(v) = spec["resolution"].as_str() {
            s.resolution = v.to_ascii_lowercase();
        }
        if let Some(v) = spec["ratio"].as_str() {
            s.ratio = crate::user_routes::normalize_video_spec_text(v).replace(' ', "");
        }
        if let Some(v) = spec["watermark"].as_bool() {
            s.watermark = v;
        }
    }
    validate_spec(
        &json!({"duration":s.duration,"resolution":s.resolution,"ratio":s.ratio,"watermark":s.watermark}),
    )?;
    let fresh = match body.get("work_user_media_ids") {
        None => vec![],
        Some(v) => v
            .as_array()
            .filter(|a| a.len() <= 10)
            .ok_or_else(invalid)?
            .iter()
            .map(|v| {
                v.as_str()
                    .filter(|s| !s.is_empty() && s.len() <= 128)
                    .map(str::to_owned)
                    .ok_or_else(invalid)
            })
            .collect::<Result<Vec<_>, _>>()?,
    };
    if d.reference_policy == "clear" && !clear_reference(&current) {
        return Err(invalid());
    }
    if d.reference_policy == "merge" && !merge_reference(&current) {
        return Err(invalid());
    }
    if clear_reference(&current) {
        s.user_media_ids.clear();
        s.tail_frame_media_id = None;
    } else if !fresh.is_empty() {
        if merge_reference(&current) && d.reference_policy == "merge" {
            for id in fresh {
                if !s.user_media_ids.contains(&id) {
                    s.user_media_ids.push(id);
                }
            }
        } else {
            s.user_media_ids = fresh;
        }
        s.tail_frame_media_id = None;
    }
    if s.user_media_ids.len() > 10 {
        return Err("reference_image_limit".into());
    }
    // A revision regenerates from original user reference, not a stale derived
    // tail frame. Continuation's new parent frame is added by its executor.
    if d.action != WorkIntent::Continue {
        s.tail_frame_media_id = None;
    }
    s.reference_mode = if s.user_media_ids.is_empty() {
        "none"
    } else {
        "user_reference"
    }
    .into();
    s.parent_version_id = None;
    s.source_request_id = None;
    s.dispatch_body = None;
    Ok(s)
}
