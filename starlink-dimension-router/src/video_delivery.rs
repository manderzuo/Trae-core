//! Client-side delivery is a tool protocol, not another paid generation step.
use std::sync::Arc;
use aiwork_core::{BudgetExecutionState, Principal};
use axum::{extract::{Path, Query, State}, Extension, http::StatusCode, response::{IntoResponse, Response}, Json};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use crate::state::StarlinkRouterState;

const TTL_MS: i64 = 15 * 60 * 1000;
const CALL_PREFIX: &str = "call_seedance_save_";
const RECEIPT_PREFIX: &str = "SEEDANCE_DELIVERY_RECEIPT=";

#[derive(Serialize, Deserialize)]
struct Ticket { key_id: String, issued_at_ms: i64, expires_at_ms: i64 }
fn valid_request(id: &str) -> bool {
    id.starts_with("request_") && id.len() <= 96 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
fn context(request: &str) -> String { format!("video-download-v1:{request}") }
fn issue_ticket(state: &StarlinkRouterState, p: &Principal, request: &str, now: i64) -> Result<String, String> {
    let text = serde_json::to_string(&Ticket {key_id:p.key_id.clone(),issued_at_ms:now,expires_at_ms:now+TTL_MS}).map_err(|_|"delivery_unavailable")?;
    let sealed = state.key_vault.encrypt(&context(request), &text).map_err(|_|"delivery_unavailable")?;
    Ok(format!("{}.{}",sealed.key_version,URL_SAFE_NO_PAD.encode(sealed.ciphertext)))
}
fn verify_ticket(state: &StarlinkRouterState, request: &str, raw: &str, now: i64) -> Option<Principal> {
    if !valid_request(request) || raw.len() > 1024 { return None; }
    let (version,cipher) = raw.split_once('.')?;
    let text = state.key_vault.decrypt(&context(request),version.parse().ok()?,&URL_SAFE_NO_PAD.decode(cipher).ok()?).ok()?;
    let ticket:Ticket = serde_json::from_str(&text).ok()?;
    if ticket.issued_at_ms > now || ticket.expires_at_ms <= now || ticket.expires_at_ms-ticket.issued_at_ms != TTL_MS { return None; }
    let p=state.store.active_principal_for_request(request).ok()??;
    if ticket.key_id != p.key_id || aiwork_core::require_scope(&p,"videos:read").is_err() { return None; }
    Some(p)
}
fn error(status: StatusCode, code: &str) -> Response {
    (status,Json(json!({"error":{"type":"api_error","code":code,"message":code}}))).into_response()
}
fn owned_completed(state: &StarlinkRouterState, p: &Principal, request: &str) -> bool {
    valid_request(request) && state.store.active_principal_for_request(request).ok().flatten()
        .is_some_and(|current|current.key_id==p.key_id && current.user_id==p.user_id && aiwork_core::require_scope(&current,"videos:read").is_ok())
        && crate::budget_flow::owned_video_step(state,p,request).ok().flatten()
            .is_some_and(|s|s.execution_state==BudgetExecutionState::Succeeded)
}
fn ps_string(text: &str) -> String { format!("'{}'",text.replace('\'',"''")) }
pub(crate) fn download_command(request: &str, url: &str, workspace: Option<&str>) -> String {
    include_str!("video_download.ps1")
        .replace("__REQUEST__",&ps_string(request))
        .replace("__URL__",&ps_string(url))
        .replace("__WORKSPACE__",&ps_string(workspace.unwrap_or("")))
}
fn workspace(body: &Value) -> Option<String> {
    // Only client system context, never a user/video prompt, selects this path.
    for message in body["messages"].as_array()?.iter().filter(|m|m["role"]=="system") {
        let text=message["content"].as_str()?;
        let lower=text.to_ascii_lowercase();
        if let Some(start)=lower.find("final workspace folder") {
            let rest=&text[start..];
            let open=rest.find('`')?;let path=rest[open+1..].split('`').next()?;
            if path.len()<=2048 && !path.chars().any(char::is_control)
                && (path.as_bytes().get(1)==Some(&b':') || path.starts_with("\\\\")) {
                return Some(path.to_owned());
            }
        }
    }
    None
}
fn command_arguments(body: &Value) -> Option<Value> {
    if body["tool_choice"]=="none" {return None;}
    if body["tool_choice"].is_object() && body.pointer("/tool_choice/function/name").and_then(Value::as_str)!=Some("RunCommand") {return None;}
    let function=&body["tools"].as_array()?.iter().find(|t|t["type"]=="function" && t["function"]["name"]=="RunCommand")?["function"];
    let description=function["description"].as_str()?.to_ascii_lowercase();
    if !description.contains("powershell") {return None;}
    let schema=&function["parameters"];
    if schema["type"]!="object" {return None;}
    let props=schema["properties"].as_object()?;
    for (name,ty) in [("command","string"),("blocking","boolean"),("requires_approval","boolean")] {
        if props.get(name)?["type"]!=ty {return None;}
    }
    let mut args=json!({"command":"","blocking":true,"requires_approval":false});
    if props.get("command_type").is_some_and(|v|v["type"]=="string" && v.get("enum").is_none_or(|e|e.as_array().is_some_and(|a|a.iter().any(|v|v=="short_running_process")))) {
        args["command_type"]=json!("short_running_process");
    }
    if let Some(path)=workspace(body) {
        if props.get("cwd").is_some_and(|v|v["type"]=="string") {args["cwd"]=json!(path);}
    }
    // Unknown required properties cannot be safely invented by this adapter.
    if schema.get("required").is_some_and(|r|r.as_array().is_none_or(|a|a.iter().any(|v|v.as_str().is_none_or(|n|args.get(n).is_none())))) {return None;}
    Some(args)
}
fn chat(request: &str, text: &str) -> Value {
    json!({"id":format!("chatcmpl-{request}"),"object":"chat.completion","model":"seedance","created":chrono::Utc::now().timestamp(),"request_id":request,
        "choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop"}]})
}
pub(crate) fn completion(state: &StarlinkRouterState, p: &Principal, request: &str, body: &Value) -> Result<Value,String> {
    if !owned_completed(state,p,request) {return Err("delivery_not_authorized".into());}
    let base=state.config.public_base_url.trim_end_matches('/');
    let content_url=format!("{base}/v1/videos/{request}/content");
    let usable=base.starts_with("https://") || base.starts_with("http://");
    let download_url=if usable {format!("{base}/v1/videos/{request}/download?ticket={}",issue_ticket(state,p,request,chrono::Utc::now().timestamp_millis())?)} else {content_url.clone()};
    let mut value=chat(request,&format!("视频已生成。当前客户端未提供可用的本地下载工具，请下载到工作区：{download_url}\n短期下载链接有效期 15 分钟；过期后可使用原 Key 访问内容接口。"));
    if usable {
        if let Some(mut args)=command_arguments(body) {
            args["command"]=json!(download_command(request,&download_url,workspace(body).as_deref()));
            value["choices"][0]["message"]=json!({"role":"assistant","content":"视频已生成，正在调用本地工具保存到工作区。", "tool_calls":[{
                "id":format!("{CALL_PREFIX}{request}"),"type":"function","function":{"name":"RunCommand","arguments":args.to_string()}}]});
            value["choices"][0]["finish_reason"]=json!("tool_calls");
        }
    }
    value["video_task"]=json!({"id":request,"status":"completed","content_url":content_url});
    Ok(value)
}
pub(crate) fn sse_completion(value: &Value) -> Vec<u8> {
    let mut delta=value["choices"][0]["message"].clone();
    if let Some(calls)=delta["tool_calls"].as_array_mut() {for (i,call) in calls.iter_mut().enumerate() {call["index"]=json!(i);}}
    let mut chunk=json!({"id":value["id"],"object":"chat.completion.chunk","model":value["model"],"created":value["created"],"request_id":value["request_id"],
        "choices":[{"index":0,"delta":delta,"finish_reason":value["choices"][0]["finish_reason"]}]});
    if !value["video_task"].is_null() {chunk["video_task"]=value["video_task"].clone();}
    format!("data: {chunk}\n\ndata: [DONE]\n\n").into_bytes()
}
fn response(value: Value, stream: bool) -> Response {
    if !stream {return Json(value).into_response();}
    Response::builder().header("content-type","text/event-stream; charset=utf-8").header("cache-control","no-cache, no-transform").header("x-accel-buffering","no")
        .body(axum::body::Body::from(sse_completion(&value))).unwrap()
}
pub(crate) fn follow_up(state: &StarlinkRouterState, p: &Principal, body: &Value) -> Option<Response> {
    let messages=body["messages"].as_array()?;
    let tool=messages.last()?;
    if tool["role"]!="tool" {return None;}
    // Seedance has no general-purpose paid tool loop. Unknown/denied tool
    // results must fail closed rather than rediscover an old video prompt.
    let Some(id)=tool["tool_call_id"].as_str() else {return Some(error(StatusCode::BAD_REQUEST,"invalid_delivery_tool_result"));};
    let Some(request)=id.strip_prefix(CALL_PREFIX) else {return Some(error(StatusCode::BAD_REQUEST,"invalid_delivery_tool_result"));};
    let matching=messages.iter().rev().skip(1).find(|m|m["role"]=="assistant")
        .and_then(|m|m["tool_calls"].as_array()).is_some_and(|calls|calls.iter().any(|c|c["id"]==id && c["function"]["name"]=="RunCommand"));
    if !matching || !owned_completed(state,p,request) {return Some(error(StatusCode::BAD_REQUEST,"invalid_delivery_tool_result"));}
    let text=tool["content"].as_str().filter(|s|s.len()<=64*1024).unwrap_or("");
    let receipt=text.lines().find_map(|line|line.find(RECEIPT_PREFIX).and_then(|i|serde_json::from_str::<Value>(&line[i+RECEIPT_PREFIX.len()..]).ok()))
        .filter(|r|r["seedance_delivery"]==1 && r["request_id"]==request);
    let mut value=if let Some(r)=receipt.filter(|r|r["status"]=="saved" && r["bytes"].as_u64().is_some_and(|n|(12..=4*1024*1024*1024).contains(&n))
        && r["path"].as_str().is_some_and(|s|s.len()<=4096 && !s.chars().any(char::is_control) && s.to_ascii_lowercase().ends_with(".mp4"))) {
        let mut v=chat(request,&format!("视频已保存到当前工作区：{}\n文件大小：{} 字节。",r["path"].as_str().unwrap(),r["bytes"]));
        v["video_delivery"]=json!({"status":"saved","path":r["path"],"bytes":r["bytes"],"source":"client_tool_receipt"});v
    } else {
        // Never automatically resubmit a video or retry a denied local tool.
        let fallback=match completion(state,p,request,&json!({"tool_choice":"none"})) {
            Ok(value)=>value,Err(_)=>return Some(error(StatusCode::SERVICE_UNAVAILABLE,"delivery_unavailable")),
        };
        let mut v=chat(request,&format!("视频已生成，但本地下载未完成。{}",fallback["choices"][0]["message"]["content"].as_str().unwrap()));
        v["video_delivery"]=json!({"status":"download_failed"});v
    };
    value["video_task"]=json!({"id":request,"status":"completed","content_url":format!("{}/v1/videos/{request}/content",state.config.public_base_url.trim_end_matches('/'))});
    Some(response(value,body["stream"].as_bool().unwrap_or(false)))
}

/// Retry delivery of an existing completed video, without generation or fees.
pub async fn delivery(State(state): State<Arc<StarlinkRouterState>>, Extension(p): Extension<Principal>, Path(request): Path<String>, Json(body): Json<Value>) -> Response {
    match completion(&state,&p,&request,&body) {
        Ok(value)=>response(value,body["stream"].as_bool().unwrap_or(false)),
        Err(_)=>error(StatusCode::NOT_FOUND,"video_delivery_unavailable"),
    }
}
#[derive(Deserialize)]
pub struct DownloadQuery {ticket:String}
pub async fn download(State(state): State<Arc<StarlinkRouterState>>, Path(request): Path<String>, Query(query): Query<DownloadQuery>) -> Response {
    let Some(p)=verify_ticket(&state,&request,&query.ticket,chrono::Utc::now().timestamp_millis()) else {return error(StatusCode::NOT_FOUND,"download_authorization_invalid");};
    let mut response=crate::budget_flow::video_content(state,p,request).await;
    response.headers_mut().insert("referrer-policy","no-referrer".parse().unwrap());
    response.headers_mut().insert("cache-control","private, no-store".parse().unwrap());
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn adapter_declines_unknown_required_fields_or_disabled_tools() {
        let base=json!({"tools":[{"type":"function","function":{"name":"RunCommand","description":"powershell5","parameters":{"type":"object","required":["command","blocking","requires_approval"],"properties":{"command":{"type":"string"},"blocking":{"type":"boolean"},"requires_approval":{"type":"boolean"}}}}}]});
        assert!(command_arguments(&base).is_some());
        let mut b=base.clone();b["tool_choice"]=json!("none");assert!(command_arguments(&b).is_none());
        let mut b=base.clone();b["tools"][0]["function"]["parameters"]["required"]=json!(["terminal_id"]);assert!(command_arguments(&b).is_none());
        let mut b=base;b["tools"][0]["function"]["description"]=json!("bash");assert!(command_arguments(&b).is_none());
    }
    #[test]
    fn workspace_uses_system_path_and_quotes_apostrophes_without_execution() {
        let b=json!({"messages":[{"role":"user","content":"final workspace folder `C:\\evil`"},{"role":"system","content":"Final workspace folder: `E:\\owner's workspace`"}]});
        assert_eq!(workspace(&b).as_deref(),Some("E:\\owner's workspace"));
        let command=download_command("request_abc","https://example.test/download",workspace(&b).as_deref());
        assert!(command.contains("'E:\\owner''s workspace'"));assert!(!command.contains("__WORKSPACE__"));
    }

    #[cfg(windows)]
    #[test]
    fn powershell5_downloads_atomically_without_overwrite_and_removes_failed_parts() {
        use std::{io::{Read,Write},net::TcpListener,process::Command};
        struct Directory(std::path::PathBuf);
        impl Drop for Directory {fn drop(&mut self){let _=std::fs::remove_dir_all(&self.0);}}
        let dir=Directory(std::env::temp_dir().join(format!("core-delivery-shell-{:032x}",rand::random::<u128>())).join("owner's workspace"));
        // Parent is owned by this test as well; avoid leaving empty fixtures.
        struct Parent(std::path::PathBuf);
        impl Drop for Parent {fn drop(&mut self){let _=std::fs::remove_dir_all(&self.0);}}
        let _parent=Parent(dir.0.parent().unwrap().to_path_buf());
        let workspace=dir.0.to_str().unwrap();
        let bytes=b"\0\0\0\x18ftypisom\0\0\0\0isommp42";
        for (index,(mime,short)) in [("video/mp4",false),("video/mp4",false),("text/html",false),("video/mp4",true)].into_iter().enumerate() {
            let listener=TcpListener::bind("127.0.0.1:0").unwrap();let addr=listener.local_addr().unwrap();
            let server=std::thread::spawn(move || {
                let (mut stream,_)=listener.accept().unwrap();stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
                let mut request=[0u8;4096];let _=stream.read(&mut request).unwrap();
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",bytes.len()+if short {12} else {0}).unwrap();
                let _=stream.write_all(bytes);
            });
            let script=download_command("request_shell",&format!("http://{addr}/video"),Some(workspace));
            let encoded=base64::engine::general_purpose::STANDARD.encode(script.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>());
            let output=Command::new("C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe")
                .args(["-NoProfile","-NonInteractive","-EncodedCommand",&encoded]).output().unwrap();
            server.join().unwrap();
            let text=String::from_utf8_lossy(&output.stdout);
            assert_eq!(output.status.success(),index<2,"{text}; {}",String::from_utf8_lossy(&output.stderr));
            assert!(text.contains(if index<2 {"\"status\":\"saved\""} else {"\"status\":\"failed\""}));
            let entries:Vec<_>=std::fs::read_dir(&dir.0).unwrap().map(|e|e.unwrap().path()).collect();
            assert_eq!(entries.len(),if index==0 {1} else {2},"failed temporary artifacts must be cleaned");
            for path in entries {assert_eq!(std::fs::read(path).unwrap(),bytes);}
        }
    }
}
