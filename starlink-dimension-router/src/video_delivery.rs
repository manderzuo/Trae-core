//! Delivery uses an idempotent billed text planner, never another video generation.
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
pub(crate) fn workspace(body: &Value) -> Option<String> {
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
#[cfg(test)]
fn command_arguments(body: &Value) -> Option<Value> {crate::delivery_assist::tools(body).into_iter().find(|t|t.name=="RunCommand").map(|t|t.arguments)}
fn bash_command(request: &str, url: &str, directory:Option<&str>) -> String {
    use std::io::Write;
    let ps=download_command(request,url,directory);
    let mut gzip=flate2::write::GzEncoder::new(Vec::new(),flate2::Compression::default());
    gzip.write_all(ps.as_bytes()).expect("memory compression");
    let packed=base64::engine::general_purpose::STANDARD.encode(gzip.finish().expect("memory compression"));
    let bootstrap=format!("$b=[Convert]::FromBase64String(\"{packed}\");$m=New-Object IO.MemoryStream(,$b);$g=New-Object IO.Compression.GZipStream($m,[IO.Compression.CompressionMode]::Decompress);$r=New-Object IO.StreamReader($g,[Text.Encoding]::UTF8);& ([scriptblock]::Create($r.ReadToEnd()))");
    let unix=include_str!("video_download.sh").replace("__REQUEST__",&sh_string(request)).replace("__URL__",&sh_string(url)).replace("__DIRECTORY__",&sh_string(directory.unwrap_or("")));
    format!("case \"$(uname -s)\" in\n  MINGW*|MSYS*|CYGWIN*) powershell.exe -NoProfile -NonInteractive -Command {} ;;\n  *)\n{unix}\n;;\nesac",sh_string(&bootstrap))
}
fn sh_string(text:&str)->String {format!("'{}'",text.replace('\'',"'\"'\"'"))}
fn chat(request: &str, text: &str) -> Value {
    json!({"id":format!("chatcmpl-{request}"),"object":"chat.completion","model":"seedance","created":chrono::Utc::now().timestamp(),"request_id":request,
        "choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop"}]})
}
fn fallback(state:&StarlinkRouterState,p:&Principal,request:&str)->Result<Value,String> {
    if !owned_completed(state,p,request) {return Err("delivery_not_authorized".into());}
    let base=state.config.public_base_url.trim_end_matches('/');
    let content_url=format!("{base}/v1/videos/{request}/content");
    let usable=base.starts_with("https://") || base.starts_with("http://");
    let download_url=if usable {format!("{base}/v1/videos/{request}/download?ticket={}",issue_ticket(state,p,request,chrono::Utc::now().timestamp_millis())?)} else {content_url.clone()};
    let mut value=chat(request,&format!("视频已生成，但当前无法自动保存到本机 Downloads。下载地址：{download_url}\n链接有效期 15 分钟；过期后可使用原 Key 访问内容接口。"));
    value["video_task"]=json!({"id":request,"status":"completed","content_url":content_url,"download_url":download_url});
    crate::work_context::decorate_owned_request(state,p,request,&mut value)?;
    Ok(value)
}
pub(crate) async fn completion(state:&Arc<StarlinkRouterState>,p:&Principal,request:&str,body:&Value)->Result<Value,String> {
    let mut value=fallback(state,p,request)?;
    let download_url=value["video_task"]["download_url"].as_str().unwrap().to_owned();
    let usable=state.config.public_base_url.starts_with("https://") || state.config.public_base_url.starts_with("http://");
    if usable {
        let tools=crate::delivery_assist::tools(body);
        let selected=crate::delivery_assist::select(state,p,request,&tools).await;
        if let Some(tool)=selected.as_deref().and_then(|name|tools.iter().find(|t|t.name==name)).or_else(||tools.first()) {
            let mut args=tool.arguments.clone();
            args[tool.command_key]=json!(if tool.bash {bash_command(request,&download_url,None)} else {download_command(request,&download_url,None)});
            if let Some(path)=workspace(body) {
                if body["tools"].as_array().into_iter().flatten().any(|t|t["function"]["name"]==tool.name && t.pointer("/function/parameters/properties/cwd/type").is_some_and(|v|v=="string")) {args["cwd"]=json!(path);}
            }
            value["choices"][0]["message"]=json!({"role":"assistant","content":"视频已生成，正在调用本机工具保存到系统 Downloads。", "tool_calls":[{
                "id":format!("{CALL_PREFIX}{request}"),"type":"function","function":{"name":tool.name,"arguments":args.to_string()}}]});
            value["choices"][0]["finish_reason"]=json!("tool_calls");
            value["video_delivery"]=json!({"status":"download_requested","tool":tool.name});
            value["video_task"]["delivery_model"]=json!(state.config.seedance_assistant_model);
            value["video_task"]["delivery_planner"]=json!(if selected.is_some(){"assistant"}else{"validated_adapter_fallback"});
        } else {
            if let Some(call)=crate::delivery_discovery::first_call(body,request) {
                set_discovery_call(&mut value,call,"视频已生成，正在查找客户端可用的本地下载工具。","discovering_tools");
                crate::work_context::decorate_owned_request(state,p,request,&mut value)?;
                return Ok(value);
            }
            let reason=crate::delivery_assist::unavailable_reason(body);
            value["video_delivery"]=json!({"status":"download_unavailable","reason":reason});
            let explanation=match reason {
                "client_network_restricted"=>"客户端声明终端工具不能联网，因此没有执行下载命令。",
                "client_tools_disabled"=>"客户端本轮关闭了工具调用。",
                "client_tools_not_provided"=>"客户端本轮没有提供本地工具。",
                "client_terminal_not_supported"=>"客户端没有提供受支持的本地终端工具。",
                _=>"客户端终端工具的参数格式暂不受支持。",
            };
            let text=value["choices"][0]["message"]["content"].as_str().unwrap_or("").to_owned();
            value["choices"][0]["message"]["content"]=json!(format!("{explanation}\n{text}"));
        }
    } else {
        value["video_delivery"]=json!({"status":"download_unavailable","reason":"public_base_url_unconfigured"});
    }
    crate::work_context::decorate_owned_request(state,p,request,&mut value)?;
    Ok(value)
}
pub fn sse_completion(value: &Value) -> Vec<u8> {
    let mut delta=value["choices"][0]["message"].clone();
    if let Some(calls)=delta["tool_calls"].as_array_mut() {for (i,call) in calls.iter_mut().enumerate() {call["index"]=json!(i);}}
    let mut chunk=json!({"id":value["id"],"object":"chat.completion.chunk","model":value["model"],"created":value["created"],"request_id":value["request_id"],
        "choices":[{"index":0,"delta":delta,"finish_reason":value["choices"][0]["finish_reason"]}]});
    if !value["video_task"].is_null() {chunk["video_task"]=value["video_task"].clone();}
    if !value["video_delivery"].is_null() {chunk["video_delivery"]=value["video_delivery"].clone();}
    if !value["work_context"].is_null() {chunk["work_context"]=value["work_context"].clone();}
    format!("data: {chunk}\n\ndata: [DONE]\n\n").into_bytes()
}
fn response(value: Value, stream: bool) -> Response {
    if !stream {return Json(value).into_response();}
    Response::builder().header("content-type","text/event-stream; charset=utf-8").header("cache-control","no-cache, no-transform").header("x-accel-buffering","no")
        .body(axum::body::Body::from(sse_completion(&value))).unwrap()
}
fn set_discovery_call(value:&mut Value,call:Value,text:&str,status:&str) {
    value["choices"][0]["message"]=json!({"role":"assistant","content":text,"tool_calls":[call]});
    value["choices"][0]["finish_reason"]=json!("tool_calls");
    value["video_delivery"]=json!({"status":status});
}
pub(crate) async fn follow_up(state: &Arc<StarlinkRouterState>, p: &Principal, body: &Value) -> Option<Response> {
    let messages=body["messages"].as_array()?;
    let last=messages.last()?;
    let tool=if last["role"]=="tool" {last} else if last["role"]=="user" {
        let mut results=last["content"].as_array()?.iter().filter(|v|v["type"]=="tool_result");
        let tool=results.next()?;
        if results.next().is_some() {return Some(error(StatusCode::BAD_REQUEST,"invalid_delivery_tool_result"));}
        tool
    } else {return None;};
    // Seedance has no general-purpose paid tool loop. Unknown/denied tool
    // results must fail closed rather than rediscover an old video prompt.
    let call_id=tool["tool_call_id"].as_str().or_else(||tool["tool_use_id"].as_str());
    if let Some((stage,request))=call_id.and_then(crate::delivery_discovery::request) {
        if !owned_completed(state,p,request) {return Some(error(StatusCode::BAD_REQUEST,"invalid_delivery_tool_result"));}
        if stage==crate::delivery_discovery::Stage::Find && !tool_failed(tool,0) && crate::delivery_assist::tools(body).is_empty() {
            if let Some(call)=crate::delivery_discovery::unlock_call(body,request,&tool["content"]) {
                let mut value=match fallback(state,p,request) {Ok(v)=>v,Err(_)=>return Some(error(StatusCode::SERVICE_UNAVAILABLE,"delivery_unavailable"))};
                set_discovery_call(&mut value,call,"正在启用客户端发现的本地终端工具。","unlocking_tools");
                if crate::work_context::decorate_owned_request(state,p,request,&mut value).is_err() {return Some(error(StatusCode::SERVICE_UNAVAILABLE,"work_context_unavailable"));}
                return Some(response(value,body["stream"].as_bool().unwrap_or(false)));
            }
        }
        return Some(match completion(state,p,request,body).await {
            Ok(value)=>response(value,body["stream"].as_bool().unwrap_or(false)),
            Err(_)=>error(StatusCode::SERVICE_UNAVAILABLE,"delivery_unavailable"),
        });
    }
    let receipt=receipt(&tool["content"],0);
    let call_request=call_id.and_then(|id|id.strip_prefix(CALL_PREFIX));
    let receipt_request=receipt.as_ref().and_then(|r|r["request_id"].as_str());
    if call_request.is_some() && receipt_request.is_some() && call_request!=receipt_request {return Some(error(StatusCode::BAD_REQUEST,"invalid_delivery_tool_result"));}
    let Some(request)=receipt_request.or(call_request) else {return Some(error(StatusCode::BAD_REQUEST,"invalid_delivery_tool_result"));};
    if !owned_completed(state,p,request) {return Some(error(StatusCode::BAD_REQUEST,"invalid_delivery_tool_result"));}
    let mut value=if let Some(r)=receipt.as_ref().filter(|r|!tool_failed(tool,0) && r["status"]=="saved" && r["bytes"].as_u64().is_some_and(|n|(12..=4*1024*1024*1024).contains(&n))
        && r["path"].as_str().is_some_and(|s|s.len()<=4096 && !s.chars().any(char::is_control) && s.to_ascii_lowercase().ends_with(".mp4"))) {
        let mut v=chat(request,&format!("视频已保存到本机 Downloads：{}\n文件大小：{} 字节。",r["path"].as_str().unwrap(),r["bytes"]));
        v["video_delivery"]=json!({"status":"saved","path":r["path"],"bytes":r["bytes"],"source":"client_tool_receipt"});v
    } else {
        // Never automatically resubmit a video or retry a denied local tool.
        let fallback=match fallback(state,p,request) {
            Ok(value)=>value,Err(_)=>return Some(error(StatusCode::SERVICE_UNAVAILABLE,"delivery_unavailable")),
        };
        let mut v=chat(request,&format!("视频已生成，但本地下载未完成。{}",fallback["choices"][0]["message"]["content"].as_str().unwrap()));
        v["video_delivery"]=json!({"status":"download_failed"});v
    };
    value["video_task"]=json!({"id":request,"status":"completed","content_url":format!("{}/v1/videos/{request}/content",state.config.public_base_url.trim_end_matches('/'))});
    if crate::work_context::decorate_owned_request(state,p,request,&mut value).is_err() {return Some(error(StatusCode::SERVICE_UNAVAILABLE,"work_context_unavailable"));}
    Some(response(value,body["stream"].as_bool().unwrap_or(false)))
}
pub(crate) fn tool_failed(value:&Value,depth:usize)->bool {
    if depth>6 {return true;}
    if value["isError"]==true || value["is_error"]==true || value["timedOut"]==true || value["aborted"]==true
        || value.pointer("/sandbox/denied")==Some(&json!(true)) || value.pointer("/sandbox/runnerFailed")==Some(&json!(true))
        || value.get("signal").is_some_and(|v|!v.is_null())
        || ["exitCode","exit_code"].iter().any(|key|value.get(*key).is_some_and(|code|code.as_i64()!=Some(0))) {return true;}
    if let Some(text)=value.as_str().filter(|s|s.len()<=64*1024) {
        if text.lines().any(|line| {
            let line=line.trim();
            line.strip_prefix("[exit code: ").and_then(|s|s.strip_suffix(']')).is_some_and(|code|code.parse::<i64>()!=Ok(0))
                || line.starts_with("[timed out after ") || line.starts_with("[killed by signal: ") || line.starts_with("[sandbox:")
        }) {return true;}
        if let Ok(parsed)=serde_json::from_str::<Value>(text) {return tool_failed(&parsed,depth+1);}
    }
    if let Some(values)=value.as_array() {return values.iter().take(32).any(|v|tool_failed(v,depth+1));}
    ["content","output","result","data","message","stdout","stderr","text"].iter().any(|key|value.get(*key).is_some_and(|v|tool_failed(v,depth+1)))
}
fn receipt(value:&Value,depth:usize)->Option<Value> {
    if depth>6 {return None;}
    if value["seedance_delivery"]==1 {return Some(value.clone());}
    if let Some(text)=value.as_str().filter(|s|s.len()<=64*1024) {
        for line in text.lines() {
            if let Some(i)=line.find(RECEIPT_PREFIX) {
                if let Some(Ok(v))=serde_json::Deserializer::from_str(&line[i+RECEIPT_PREFIX.len()..]).into_iter::<Value>().next() {if v["seedance_delivery"]==1 {return Some(v);}}
            }
        }
        if let Ok(v)=serde_json::from_str::<Value>(text) {return receipt(&v,depth+1);}
    }
    if let Some(values)=value.as_array() {return values.iter().take(32).find_map(|v|receipt(v,depth+1));}
    for field in ["text","stdout","output","content","result","data","message"] {if let Some(v)=value.get(field) {if let Some(r)=receipt(v,depth+1) {return Some(r);}}}
    None
}

/// Retry delivery of an existing video; the identical text plan is reused.
pub async fn delivery(State(state): State<Arc<StarlinkRouterState>>, Extension(p): Extension<Principal>, Path(request): Path<String>, Json(body): Json<Value>) -> Response {
    match completion(&state,&p,&request,&body).await {
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
    fn receipt_survives_native_terminal_data_wrapper() {
        let value=json!({"data":{"stdout":"SEEDANCE_DELIVERY_RECEIPT={\"seedance_delivery\":1,\"request_id\":\"request_nested\",\"status\":\"saved\",\"bytes\":24,\"path\":\"D:\\\\Downloads\\\\video.mp4\"}","exitCode":0}});
        let parsed=receipt(&value,0).expect("the native tool's data wrapper must not lose the receipt");
        assert_eq!(parsed["request_id"],"request_nested");
        assert_eq!(parsed["status"],"saved");
    }
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
    #[cfg(windows)]
    #[test]
    fn redirected_downloads_and_git_bash_save_without_installed_skill() {
        use std::{io::{Read,Write},net::TcpListener,process::Command};
        struct Directory(std::path::PathBuf);
        impl Drop for Directory {fn drop(&mut self){let _=std::fs::remove_dir_all(&self.0);}}
        let root=Directory(std::env::temp_dir().join(format!("core-downloads-{:032x}",rand::random::<u128>())));
        let destination=root.0.join("redirected owner's 中文 Downloads");
        let bytes=b"\0\0\0\x18ftypisom\0\0\0\0isommp42";
        for shell in ["redirected","git-bash"] {
            let listener=TcpListener::bind("127.0.0.1:0").unwrap();let addr=listener.local_addr().unwrap();
            listener.set_nonblocking(true).unwrap();
            let server=std::thread::spawn(move || {
                let start=std::time::Instant::now();
                let mut stream=loop {match listener.accept() {Ok((s,_))=>break s,Err(e) if e.kind()==std::io::ErrorKind::WouldBlock && start.elapsed().as_secs()<10=>std::thread::sleep(std::time::Duration::from_millis(20)),_=>return}};
                stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
                let mut request=[0u8;4096];let _=stream.read(&mut request).unwrap();
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",bytes.len()).unwrap();stream.write_all(bytes).unwrap();
            });
            let id=format!("request_{shell}");let url=format!("http://{addr}/video");
            let output=if shell=="redirected" {
                let override_registry=format!("function Get-ItemProperty {{ [pscustomobject]@{{ '{{374DE290-123F-4565-9164-39C4925E467B}}' = {} }} }}\n",ps_string(destination.to_str().unwrap()));
                let script=override_registry+&download_command(&id,&url,None);
                let encoded=base64::engine::general_purpose::STANDARD.encode(script.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>());
                Command::new("C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe").args(["-NoProfile","-NonInteractive","-EncodedCommand",&encoded]).output().unwrap()
            } else {
                let path=destination.to_str().unwrap();
                let script=bash_command(&id,&url,Some(path));
                assert!(script.len()<8192,"Windows shell command remains below conservative argument limit");
                Command::new("C:/Program Files/Git/bin/bash.exe").args(["-c",&script]).output().unwrap()
            };
            server.join().unwrap();
            assert!(output.status.success(),"{shell}: {}; {}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));
            assert_eq!(std::fs::read(destination.join(format!("seedance-{id}.mp4"))).unwrap(),bytes);
            let parsed=receipt(&json!(String::from_utf8_lossy(&output.stdout)),0).unwrap();assert_eq!(parsed["status"],"saved");assert_eq!(parsed["bytes"],bytes.len());
            assert_eq!(parsed["path"].as_str(),destination.join(format!("seedance-{id}.mp4")).to_str(),"UTF-8 receipt must preserve the real local path");
        }
    }
}
