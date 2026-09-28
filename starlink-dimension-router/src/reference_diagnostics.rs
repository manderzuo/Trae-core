//! Bounded, allowlisted input shapes only. Never log client values or credentials.
use serde_json::{json, Value};

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null", Value::Bool(_) => "bool", Value::Number(_) => "number",
        Value::String(_) => "string", Value::Array(_) => "array", Value::Object(_) => "object",
    }
}

fn fields(value: &Value) -> Value {
    let mut result = serde_json::Map::new();
    for field in ["image_asset_ids", "video_asset_ids", "image_urls", "video_urls", "image_url",
        "input_image", "image", "images", "attachments", "files", "file", "file_id", "source", "input"] {
        if let Some(v) = value.get(field) {
            result.insert(field.into(), json!({"kind":kind(v), "items":v.as_array().map(Vec::len)}));
        }
    }
    Value::Object(result)
}

fn count(map: &mut std::collections::BTreeMap<&'static str, usize>, key: &'static str) {
    *map.entry(key).or_default() += 1;
}

pub(crate) fn summarize(body: &Value) -> Value {
    let messages = body["messages"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let start = messages.len().saturating_sub(8);
    let details: Vec<_> = messages.iter().enumerate().skip(start).map(|(index, message)| {
        let role = match message["role"].as_str() {
            Some("user") => "user", Some("assistant") => "assistant", Some("system") => "system",
            Some("developer") => "developer", Some("tool") => "tool", _ => "other",
        };
        let mut types = std::collections::BTreeMap::new();
        let mut urls = std::collections::BTreeMap::new();
        let mut media = std::collections::BTreeMap::new();
        let content = &message["content"];
        let parts = content.as_array().map(Vec::as_slice).unwrap_or(&[]);
        let mut marker = false;
        let mut data_image_text = false;
        let mut image_tag_text = false;
        let mut http_text = false;
        let mut inspect_text = |text: &str| {
            marker |= text.contains("[AIWORK_REFERENCE:");
            data_image_text |= text.contains("data:image/");
            image_tag_text |= text.contains("<image") || text.contains("![");
            http_text |= text.contains("https://") || text.contains("http://");
        };
        if let Some(text) = content.as_str() { inspect_text(text); }
        for part in parts.iter().take(64) {
            let ty = match part["type"].as_str() {
                Some("text") => "text", Some("image_url") => "image_url", Some("image") => "image",
                Some("input_image") => "input_image", Some("input_text") => "input_text",
                Some("file") => "file", Some("video_url") => "video_url", _ => "other",
            };
            count(&mut types, ty);
            if let Some(text) = part["text"].as_str() { inspect_text(text); }
            for field in ["image_url", "input_image", "image", "source", "file", "file_id"] {
                if part.get(field).is_some() { count(&mut media, field); }
            }
            if let Some(value) = part.get("image_url") {
                let url = value.as_str().or_else(||value["url"].as_str());
                let category = match url {
                    Some(s) if s.starts_with("data:image/") => "inline_image",
                    Some(s) if s.starts_with("https://") => "https",
                    Some(s) if s.starts_with("http://") => "http",
                    Some(s) if s.starts_with("file:") => "local_file",
                    Some(_) => "other_string", None => "invalid_shape",
                };
                count(&mut urls, category);
            }
        }
        json!({"index":index, "role":role, "content_kind":kind(content),
            "parts_total":parts.len(), "parts_truncated":parts.len()>64,
            "part_types":types, "image_url_kinds":urls, "media_fields":media,
            "attachment_fields":fields(message), "content_fields":fields(content),
            "text_hints":{"reference_marker":marker,"data_image":data_image_text,
                "image_markup":image_tag_text,"http_link":http_text}})
    }).collect();
    json!({"messages_total":messages.len(), "messages_truncated":start>0,
        "top_level_fields":fields(body), "messages":details})
}

pub(crate) fn rejection(code: &'static str, summary: Value) -> Value {
    let id = format!("refdiag-{:032x}", rand::random::<u128>());
    // All strings in summary are fixed enums/field names; no raw prompt, role,
    // content type, URL, token, body, account identity or headers are emitted.
    eprintln!("{}", json!({"event":"reference_input_rejected", "diagnostic_id":id,
        "code":code, "input_summary":summary}));
    let message = if code == "reference_image_missing" {
        "本次请求未识别到可用参考图片或素材标记；未提交视频、未扣费"
    } else {
        "参考素材上下文无效或已过期；未提交视频、未扣费"
    };
    json!({"error":{"code":code, "message":format!("{message}。诊断编号：{id}"),
        "diagnostic_id":id, "input_summary":summary}})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shape_distinguishes_inline_images_from_nonstandard_attachments_without_values() {
        let body = json!({"authorization":"PRIVATE", "unknown_PRIVATE":{"url":"PRIVATE"},
            "attachments":[{"name":"PRIVATE"}], "messages":[
                {"role":"user","content":[{"type":"text","text":"PRIVATE"},
                    {"type":"image_url","image_url":{"url":"data:image/png;base64,PRIVATE"}},
                    {"type":"image","source":{"data":"PRIVATE"}},
                    {"type":"PRIVATE","image_url":"https://private.invalid/PRIVATE?ticket=PRIVATE"}]},
                {"role":"PRIVATE","content":"PRIVATE [AIWORK_REFERENCE:PRIVATE]", "files":["PRIVATE"]}]});
        let s = summarize(&body);
        assert_eq!(s["messages"][0]["part_types"]["image_url"],1);
        assert_eq!(s["messages"][0]["part_types"]["image"],1);
        assert_eq!(s["messages"][0]["image_url_kinds"]["inline_image"],1);
        assert_eq!(s["messages"][0]["image_url_kinds"]["https"],1);
        assert_eq!(s["messages"][1]["role"],"other");
        assert_eq!(s["messages"][1]["text_hints"]["reference_marker"],true);
        assert_eq!(s["top_level_fields"]["attachments"]["items"],1);
        assert!(!s.to_string().contains("PRIVATE"));
        assert!(!s.to_string().contains("private.invalid"));
    }
    #[test]
    fn diagnostic_work_and_output_are_bounded_and_explicit_about_truncation() {
        let parts=vec![json!({"type":"PRIVATE","image_url":{"url":"PRIVATE"}});1000];
        let body=json!({"messages":vec![json!({"role":"user","content":parts});20]});
        let s=summarize(&body);
        assert_eq!(s["messages_total"],20);
        assert_eq!(s["messages_truncated"],true);
        assert_eq!(s["messages"].as_array().unwrap().len(),8);
        assert_eq!(s["messages"][0]["index"],12);
        assert_eq!(s["messages"][0]["parts_truncated"],true);
        assert_eq!(s["messages"][0]["part_types"]["other"],64);
        assert!(s.to_string().len()<8192);
        assert!(!s.to_string().contains("PRIVATE"));
    }
}
