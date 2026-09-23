use serde_json::{json, Value};

pub(crate) enum VideoStreamEvent {
    Progress(String),
    Completed {
        task_id: String,
        content_url: String,
        request_id: String,
    },
    Failed {
        code: String,
        request_id: String,
    },
}

pub(crate) fn keep_alive() -> &'static [u8] {
    b": keep-alive\n\n"
}

fn frame(value: Value) -> Vec<u8> {
    let mut encoded = b"data: ".to_vec();
    encoded.extend_from_slice(value.to_string().as_bytes());
    encoded.extend_from_slice(b"\n\n");
    encoded
}

pub(crate) fn encode_event(id: &str, event: VideoStreamEvent) -> Vec<u8> {
    let (content, finish_reason, extension, terminal) = match event {
        VideoStreamEvent::Progress(message) => (message, Value::Null, Value::Null, false),
        VideoStreamEvent::Completed {
            task_id,
            content_url,
            request_id,
        } => (
            format!("视频生成完成，可通过该地址下载：{content_url}"),
            json!("stop"),
            json!({
                "video_task": {
                    "id": task_id,
                    "status": "completed",
                    "content_url": content_url,
                    "request_id": request_id,
                }
            }),
            true,
        ),
        VideoStreamEvent::Failed { code, request_id } => (
            format!("视频任务未能完成，request_id={request_id}"),
            json!("stop"),
            json!({"error": {"code": code, "request_id": request_id}}),
            true,
        ),
    };

    let mut chunk = json!({
        "id": format!("chatcmpl-{id}"),
        "object": "chat.completion.chunk",
        "created": chrono::Utc::now().timestamp(),
        "model": "seedance",
        "choices": [{
            "index": 0,
            "delta": {"content": content},
            "finish_reason": finish_reason,
        }],
    });
    if let (Some(target), Some(values)) = (chunk.as_object_mut(), extension.as_object()) {
        target.extend(values.clone());
    }

    let mut encoded = frame(chunk);
    if terminal {
        encoded.extend_from_slice(b"data: [DONE]\n\n");
    }
    encoded
}
