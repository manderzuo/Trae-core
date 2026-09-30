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
    let messages=body["messages"].as_array();
    let latest=messages.and_then(|m|m.iter().rposition(|m|m["role"]=="user"));
    let current=current_user_input(&messages.zip(latest).map(|(m,i)|message_text(&m[i]))
        .or_else(||body["prompt"].as_str().map(str::to_owned)).unwrap_or_default());
    if !current.is_empty() || body["action"].as_str().is_some()
        || !crate::reference_upload::has_source_video(body) {return current;}
    let Some((messages,latest))=messages.zip(latest) else {return current};
    let Some(previous)=messages[..latest].iter().rposition(|m|m["role"]=="user") else {return current};
    let awaiting=messages[previous+1..latest].iter().rev().find(|m|m["role"]=="assistant")
        .is_some_and(|m|{let text=message_text(m);text.contains("请") && text.contains("未提交视频")});
    if !awaiting {return current;}
    // A supplemental file may fulfil the nearest pending user request. Never
    // skip a cancellation/new user turn, or replay an already completed video.
    if messages[previous+1..latest].iter().any(|m|m["role"]=="assistant" && (
        !m.pointer("/work_context/base_version_id").unwrap_or(&Value::Null).is_null()
        || m.pointer("/video_task/status").is_some_and(|s|s=="completed")
        || ["视频已生成","视频已完成"].iter().any(|s|message_text(m).contains(s)))) {return current;}
    let prior=current_user_input(&message_text(&messages[previous]));
    let lower=prior.to_lowercase();
    let video=["视频","片段","video","clip"].iter().any(|s|lower.contains(s));
    let requested=["生成","制作","做一个","续写","继续","接着","generate","create","extend","continue"].iter().any(|s|lower.contains(s));
    let cancelled=["取消","请取消","停止生成","暂停生成","先别生成","先不要生成","不要生成视频","别生成视频",
        "cancel the ","cancel this ","do not generate a video","don't generate a video","stop generating"]
        .iter().any(|s|lower.starts_with(s))
        || ["不要生成","别生成","cancel","do not generate"].contains(&lower.as_str());
    if video && requested && !cancelled {prior} else {current}
}
fn current_user_input(raw: &str) -> String {
    let mut text=raw.to_owned();
    while let Some(start)=text.find("<system-reminder>") {
        let end=text[start..].find("</system-reminder>").map(|n|start+n+"</system-reminder>".len()).unwrap_or(text.len());
        text.replace_range(start..end,"");
    }
    // Agent clients may bundle earlier turns into one user message. Only an
    // explicit current-input wrapper is a boundary; never split on prompt words.
    if let Some((_,tail))=text.rsplit_once("<user_input>") {
        if let Some((current,_))=tail.split_once("</user_input>") {return current.trim().to_owned();}
    }
    while let Some(start)=text.find("<uploaded_files>") {
        let end=text[start..].find("</uploaded_files>").map(|n|start+n+"</uploaded_files>".len()).unwrap_or(text.len());
        text.replace_range(start..end,"");
    }
    // Strip only the known TRAE client boilerplate, not arbitrary instructions
    // or unknown REQUIREMENT blocks supplied by the user.
    let boilerplate=[
        "- Detect the language XX( such english ,chinese ) of the user's query.All outputs throughout the entire workflow must use the language XX.",
        "- You MUST NOT spawn more than 3 Explore subagents at the same time",
    ];
    let has_boilerplate=text.lines().any(|line|boilerplate.contains(&line.trim()));
    text.lines().filter(|line|!boilerplate.contains(&line.trim())
        && !(has_boilerplate && line.trim()=="REQUIREMENT:"))
        .collect::<Vec<_>>().join("\n").trim().to_owned()
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
    // Sanitizing credentials is not permission to rewrite dialogue punctuation.
    // Specification inference normalizes its own copy separately.
    s.trim().to_string()
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
        None if t.is_empty() => WorkIntent::Clarify,
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
            if t.is_empty() {
                "素材已收到，请说明要生成或修改的视频内容；本次未提交视频。"
            } else if has_parent {
                "请说明要修改哪些内容；本次未提交视频。"
            } else {
                "请先指定已有视频版本；本次未提交视频。"
            }
            .into(),
        ),
    })
}
/// Embedded contract, not a claim of native upstream response_format support.
pub fn helper_output_schema() -> Value {
    json!({"type":"object","additionalProperties":false,
        "required":["action","effective_prompt","spec_patch","reference_policy","clarification"],
        "properties":{
            "action":{"type":"string","enum":["create","revise","continue","status","download","clarify"]},
            "effective_prompt":{"type":["string","null"],"maxLength":12288},
            "spec_patch":{"type":["object","null"],"additionalProperties":false,"properties":{
                "duration":{"type":"integer","minimum":4,"maximum":15},
                "resolution":{"type":"string","enum":["480p","720p"]},
                "ratio":{"type":"string","enum":["16:9","9:16","1:1","4:3","3:4","21:9"]},
                "watermark":{"type":"boolean"}}},
            "reference_policy":{"type":["string","null"],"enum":["inherit","replace","merge","clear",null]},
            "clarification":{"type":["string","null"],"maxLength":2048}},
        "allOf":[{"if":{"properties":{"action":{"enum":["create","revise","continue"]}}},
            "then":{"properties":{"effective_prompt":{"type":"string","minLength":1},
                "reference_policy":{"type":"string"}}}}]})
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
    let (text_spec,explicit)=current_specs(body)?;
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
    let mut normalized=text_spec.as_object().ok_or_else(invalid)?.clone();
    normalized.extend(explicit.clone());
    let current_references=json!({"video_count":body["video_asset_ids"].as_array().map_or(0,Vec::len),
        "image_count":body["image_asset_ids"].as_array().map_or(0,Vec::len)});
    let payload = json!({"parent":parent,"history":history,"current":current,"explicit_spec":explicit,"current_references":current_references,
        "normalized_spec":normalized,"create_defaults":create_defaults(),"requested_action":body["action"].as_str()});
    let mut value = json!({"model":model,"stream":false,"max_tokens":4096,"temperature":0.2,"messages":[
        {"role":"system","content":"你是视频作业规划助手，负责规范自然语言提示词和视频规格。仅输出严格JSON对象，字段只能是action、effective_prompt、spec_patch、reference_policy、clarification。action只能是create/revise/continue/status/download/clarify。用户明确独立生成才create；在已提供parent上明确修改才revise；明确接着上一段生成才continue；查看进度用status、重新下载用download，普通问候/测试/不满意但无修改方向用clarify。无parent不得猜父版本。effective_prompt忠实合并parent和当前修改，保留人物、动作、场景与未被修改的约束，不新增剧情；只读操作为null。spec_patch只使用normalized_spec中已确认的duration/resolution/ratio/watermark：竖屏/竖构图为9:16，横屏/横构图为16:9，正方形为1:1，高清/高分辨率在当前能力下为720p，低分辨率为480p；explicit_spec优先。分镜的0-2秒等是区间，不能当总时长。未指定字段在revise/continue时沿用parent，create时使用create_defaults；可以省略这些字段或原值回显，不得猜测新值。时长4至15秒，480p或720p，画幅16:9/9:16/1:1/4:3/3:4/21:9。全角字符规范化为半角。reference_policy为inherit/replace/merge/clear，新参考默认replace，只有用户明确合并才merge，明确取消才clear。clarification用于简短追问或只读回复，否则null。不要输出任何ID、链接、账户、工具、扣费字段；不要虚构已提交或完成。"},
        {"role":"user","content":payload.to_string()}]});
    let instruction=value["messages"][0]["content"].as_str().ok_or_else(invalid)?;
    let instruction=instruction.replace("全角字符规范化为半角。", "仅将视频规格中的全角数字、字母和冒号规范为半角；保留正文和台词原文。");
    let example=json!({"action":"create","effective_prompt":"猫说：\"你好。\"\n路径文字 C:\\clips\\cat.mp4。","spec_patch":{},"reference_policy":"inherit","clarification":null});
    value["messages"][0]["content"]=json!(format!("{instruction} current_references是服务器已验证的素材数量，不是用户自称。仅根据current判断本轮意图，不要把分镜里角色继续行动、上一段剧情等叙述误判为对已生成视频的续写；用户明确请求独立生成新视频时action=create，即使脚本很长。无parent且用户明确要求修改或续写已有视频时action=clarify，clarification请其指定原视频；不要猜测父版本。无parent但video_count大于0时，用户明确要求从上传视频继续生成或续写新片段，应使用该素材规划create、reference_policy=replace，不得要求提供Core已有父版本；仍不得虚构原生延长、严格首帧锁定或已完成。\n{}\n必须遵守的JSON Schema：{}\n仅作编码示例，不得复制其剧情：{example}\n长度限制按UTF-8字节计算：effective_prompt不超过12288，clarification不超过2048，整个对象不超过16384。",crate::assistant_json::ENCODING_RULES,helper_output_schema()));
    if serde_json::to_vec(&value).map_err(|_| invalid())?.len() > 32 * 1024 {
        return Err("work_context_input_too_large".into());
    }
    Ok(value)
}
pub fn parse_decision(raw: &str) -> Result<WorkDecision, String> {
    let value=crate::assistant_json::object_checked(raw,16*1024).map_err(|e|e.code.to_string())?;
    parse_decision_value(value)
}
pub(crate) fn parse_helper_result(result:&Value)->Result<WorkDecision,crate::assistant_json::Diagnostic> {
    let value=crate::assistant_json::result_object(result)?;
    parse_decision_value(value).map_err(|code|crate::assistant_json::Diagnostic::new(
        crate::budget_errors::public_code(&code).unwrap_or("assistant_schema_invalid")))
}
fn parse_decision_value(mut value:Value)->Result<WorkDecision,String> {
    let schema_error=||"assistant_schema_invalid".to_string();
    if value.as_object().is_none_or(|o|o.len()!=5 || ["action","effective_prompt","spec_patch","reference_policy","clarification"].iter().any(|k|!o.contains_key(*k))) {
        return Err(schema_error());
    }
    // Read-only helper replies cannot dispatch or change references. GLM's
    // null policy there means no reference change; paid actions remain strict.
    if matches!(value["action"].as_str(),Some("clarify"|"status"|"download"))
        && value["reference_policy"].is_null() {
        value["reference_policy"]=json!("inherit");
    }
    let mut d: WorkDecision = serde_json::from_value(value).map_err(|_| schema_error())?;
    // GLM may emit null when this turn changes no specification. Null is
    // exactly an empty patch, never permission to invent defaults or fields.
    if d.spec_patch.is_null() {d.spec_patch=json!({});}
    if !matches!(
        d.reference_policy.as_str(),
        "inherit" | "replace" | "merge" | "clear"
    ) || !d.spec_patch.is_object()
    {
        return Err(schema_error());
    }
    validate_spec(&d.spec_patch)?;
    if d.paid_action().is_some()
        && d.effective_prompt
            .as_ref()
            .map_or(true, |s| s.trim().is_empty() || s.len() > 12 * 1024)
    {
        return Err(schema_error());
    }
    if d.clarification.as_ref().is_some_and(|s| s.len() > 2048) {
        return Err(schema_error());
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
fn create_defaults()->Value {
    json!({"duration":5,"resolution":"720p","ratio":"16:9","watermark":false})
}
fn natural_spec(text:&str,aliases:&[(&str,&str)])->Result<Option<String>,String> {
    let mut selected=None;
    for (phrase,value) in aliases {
        for (i,_) in text.match_indices(phrase) {
            // English descriptors must be words, not substrings of paths or
            // unrelated words. Negated descriptors do not authorize a change.
            let before=&text[..i];let after=&text[i+phrase.len()..];
            if phrase.is_ascii() && (before.chars().next_back().is_some_and(|c|c.is_ascii_alphanumeric())
                || after.chars().next().is_some_and(|c|c.is_ascii_alphanumeric())) {continue;}
            let prefix=before.trim_end();
            if ["不要","不需要","不用","不使用","不采用","不是","取消","去掉","非","not","no","without"]
                .iter().any(|negation|prefix.strip_suffix(negation).is_some_and(|leading|
                    !negation.is_ascii() || !leading.chars().next_back().is_some_and(|c|c.is_ascii_alphanumeric()))) {continue;}
            if selected.as_deref().is_some_and(|v|v!=*value) {return Err("work_spec_unsupported".into());}
            selected=Some((*value).to_owned());
        }
    }
    Ok(selected)
}
fn chinese_digit(c:char)->Option<u64> {
    Some(match c {'零'|'〇'=>0,'一'=>1,'二'|'两'=>2,'三'=>3,'四'=>4,'五'=>5,'六'=>6,'七'=>7,'八'=>8,'九'=>9,_=>return None})
}
fn normalize_chinese_seconds(text:&str)->Result<String,String> {
    let chars:Vec<char>=text.chars().collect();let mut out=String::new();let mut i=0;
    while i<chars.len() {
        if chinese_digit(chars[i]).is_none() && !matches!(chars[i],'十'|'百'|'千') {
            out.push(chars[i]);i+=1;continue;
        }
        let start=i;
        while i<chars.len() && (chinese_digit(chars[i]).is_some() || matches!(chars[i],'十'|'百'|'千')) {i+=1;}
        let mut unit=i;while unit<chars.len() && chars[unit].is_whitespace() {unit+=1;}
        // Only duration units/range boundaries authorize numeric conversion.
        // Counts such as "三个镜头" remain narrative content.
        if matches!(chars.get(unit),Some('秒'|'-')) {
            let digits=&chars[start..i];
            let value=match digits {
                ['十']=>Some(10),
                [n]=>chinese_digit(*n),
                ['十',n]=>chinese_digit(*n).map(|n|10+n),
                [n,'十']=>chinese_digit(*n).map(|n|n*10),
                [n,'十',m]=>chinese_digit(*n).zip(chinese_digit(*m)).map(|(n,m)|n*10+m),
                _=>None,
            }.ok_or("work_spec_unsupported")?;
            out.push_str(&value.to_string());
        } else {out.extend(chars[start..i].iter());}
    }
    Ok(out)
}
fn text_spec(text: &str, explicit: &Map<String,Value>) -> Result<Value, String> {
    let text = crate::user_routes::normalize_video_spec_text(text).to_ascii_lowercase().replace(['－','–','—','~','～','至'],"-");
    let text=if explicit.contains_key("duration") {text} else {normalize_chinese_seconds(&text)?};
    let chars: Vec<char> = text.chars().collect();
    let mut patch = Map::new();
    let mut ranges=Vec::new();
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
        {
            let mut k=i;
            while k>0 && chars[k-1].is_whitespace() {k-=1;}
            let range_start=if k>0 && chars[k-1]=='-' {
                k-=1;while k>0 && chars[k-1].is_whitespace() {k-=1;}
                let end=k;while k>0 && chars[k-1].is_ascii_digit() {k-=1;}
                chars[k..end].iter().collect::<String>().parse::<u64>().ok()
            } else {None};
            if let Some(start)=range_start {ranges.push((start,num));}
            else {patch.entry("duration").or_insert(json!(num));}
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
    if !explicit.contains_key("duration") && !patch.contains_key("duration") && !ranges.is_empty() {
        let mut end=0;
        for (start,next) in ranges {
            if start!=end || next<=start {return Err("work_spec_unsupported".into());}
            end=next;
        }
        patch.insert("duration".into(),json!(end));
    }
    for (field,aliases) in [
        ("ratio",&[("竖屏","9:16"),("竖构图","9:16"),("纵向构图","9:16"),("横屏","16:9"),("横构图","16:9"),("横向构图","16:9"),("正方形","1:1"),("方形视频","1:1"),("portrait orientation","9:16"),("landscape orientation","16:9"),("square video","1:1")][..]),
        ("resolution",&[("高分辨率","720p"),("高清","720p"),("高画质","720p"),("低分辨率","480p"),("标清","480p"),("high resolution","720p"),("hd","720p"),("low resolution","480p")][..]),
    ] {
        if !patch.contains_key(field) && !explicit.contains_key(field) {
            if let Some(value)=natural_spec(&text,aliases)? {patch.insert(field.into(),json!(value));}
        }
    }
    Ok(Value::Object(patch))
}
fn current_specs(body:&Value)->Result<(Value,Map<String,Value>),String> {
    let mut explicit=Map::new();
    for field in ["duration","resolution","ratio","watermark"] {
        if let Some(v)=body.get(field) {explicit.insert(field.into(),v.clone());}
    }
    validate_spec(&Value::Object(explicit.clone()))?;
    let text=text_spec(&sanitized(&current_text(body)),&explicit)?;
    let mut effective=text.as_object().ok_or_else(invalid)?.clone();
    effective.extend(explicit.clone());
    validate_spec(&Value::Object(effective))?;
    Ok((text,explicit))
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
    let (text,explicit)=current_specs(body)?;
    // Natural hints and numeric fields are normalized by Core. An unspecified
    // helper field may only echo the inherited/default value, never change it.
    let effective=if d.action==WorkIntent::Create {create_defaults()} else {
        let base=base.ok_or("work_parent_required")?;
        json!({"duration":base.duration,"resolution":base.resolution,"ratio":base.ratio,"watermark":base.watermark})
    };
    for (k, v) in d.spec_patch.as_object().ok_or_else(invalid)? {
        if text.get(k).or_else(|| explicit.get(k)).is_none() {
            let echo=match k.as_str() {
                "resolution"=>v.as_str().map(|s|json!(s.to_ascii_lowercase())).unwrap_or_else(||v.clone()),
                "ratio"=>v.as_str().map(|s|json!(crate::user_routes::normalize_video_spec_text(s).replace(' ',""))).unwrap_or_else(||v.clone()),
                _=>v.clone(),
            };
            if effective.get(k)!=Some(&echo) {return Err(invalid());}
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
        continuation_video_media_id: None,
        parent_version_id: None,
        source_request_id: None,
        reference_mode: "none".into(),
        summary: String::new(),
        dispatch_body:None,
    });
    s.effective_prompt = d.effective_prompt
            .as_deref()
            .filter(|s| !s.trim().is_empty() && s.len() <= 12 * 1024)
            .ok_or_else(invalid)?.to_string();
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
        s.continuation_video_media_id = None;
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
        s.continuation_video_media_id = None;
    }
    if s.user_media_ids.len() > 10 {
        return Err("reference_image_limit".into());
    }
    // A revision regenerates from original user reference, not a stale derived
    // tail frame. Continuation's new parent frame is added by its executor.
    if d.action != WorkIntent::Continue {
        s.tail_frame_media_id = None;
        s.continuation_video_media_id = None;
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
