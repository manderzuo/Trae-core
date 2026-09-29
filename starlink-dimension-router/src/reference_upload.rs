//! Client-local attachments require a tool round trip before any paid work.
use std::sync::Arc;
use aiwork_core::{Principal, ReferenceUpload};
use axum::{extract::{Path, Request, State}, http::{HeaderMap, StatusCode}, response::{IntoResponse, Response}, Json};
use base64::{engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD}, Engine as _};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use crate::state::StarlinkRouterState;

const PREFIX: &str = "call_ref_upload_";
const RECEIPT_PREFIX: &str = "SEEDANCE_REFERENCE_UPLOAD=";
const TTL: i64 = 30*60*1000;
fn context(id: &str) -> String { format!("reference-upload-v1:{id}") }
fn valid_id(id: &str) -> bool { id.len()==32 && id.bytes().all(|b|b.is_ascii_hexdigit()) }
fn error(status: StatusCode, code: &str) -> Response {
    let message = match code {
        "reference_upload_tool_unavailable" => "素材仅提供了本机路径，但客户端本轮没有可用的联网终端工具。请允许终端工具或直接上传素材；未提交视频、未扣费。",
        "reference_upload_incomplete" => "参考素材尚未完整上传，未提交视频、未扣费。请重新附加素材并允许客户端上传。",
        "reference_upload_failed" => "客户端素材上传工具执行失败，未提交视频、未扣费。",
        "reference_upload_invalid_path" => "附件不是支持的本机图片或 MP4/WebM 视频路径；未提交视频、未扣费。",
        "reference_upload_type_mismatch" => "素材内容与附件格式不一致；未提交视频、未扣费。",
        _ => "参考素材上传会话无效或已过期，请重新附加素材；未提交视频、未扣费。",
    };
    (status,Json(json!({"error":{"type":"invalid_request_error","code":code,"message":message}}))).into_response()
}
fn bad(code: &str) -> Response { error(StatusCode::BAD_REQUEST,code) }
fn user_text(message: &Value) -> Vec<&str> {
    match &message["content"] {
        Value::String(s)=>vec![s.as_str()],
        Value::Array(parts)=>parts.iter().filter(|p|p["type"]=="text").filter_map(|p|p["text"].as_str()).collect(),
        _=>Vec::new(),
    }
}
fn valid_path(path: &str) -> bool {
    let windows=path.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
        && path.as_bytes().get(1)==Some(&b':') && path.as_bytes().get(2).is_some_and(|b|*b==b'\\'||*b==b'/');
    let unix=path.starts_with('/') && !path.starts_with("//");
    let ext=path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    path.len()<=4096 && (windows||unix) && !path.chars().any(char::is_control)
        && !path.split(['/', '\\']).any(|part|part=="..")
        && !path[if windows {2} else {0}..].contains(':')
        && matches!(ext.as_str(),"png"|"jpg"|"jpeg"|"gif"|"webp"|"mp4"|"webm")
}
fn path_mime(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "png"=>"image/png", "jpg"|"jpeg"=>"image/jpeg", "gif"=>"image/gif",
        "webp"=>"image/webp", "mp4"=>"video/mp4", "webm"=>"video/webm", _=>"",
    }
}
fn paths(body: &Value) -> Result<Vec<String>, &'static str> {
    let Some(last)=body["messages"].as_array().and_then(|m|m.last()).filter(|m|m["role"]=="user") else {return Ok(vec![])};
    let mut found=Vec::new();
    for text in user_text(last) {
        let mut rest=text;
        while let Some((_,block))=rest.split_once("<uploaded_files>") {
            let (block,after)=block.split_once("</uploaded_files>").ok_or("reference_upload_invalid_path")?;
            rest=after;
            let mut entries=block;
            while let Some((_,entry))=entries.split_once("<file_path>") {
                let (path,after)=entry.split_once("</file_path>").ok_or("reference_upload_invalid_path")?;
                entries=after;
                let path=path.trim();
                if !valid_path(path) {return Err("reference_upload_invalid_path");}
                if !found.iter().any(|p|p==path) {found.push(path.to_owned());}
                if found.len()>10 {return Err("reference_upload_invalid_path");}
            }
        }
    }
    Ok(found)
}
/// Planning hint only. Actual uploaded asset ownership/content is still checked
/// before any helper or video dispatch; an ID alone is not authorization.
pub(crate) fn has_source_video(body: &Value) -> bool {
    body["video_asset_ids"].as_array().is_some_and(|a| !a.is_empty())
        || paths(body).is_ok_and(|a| a.iter().any(|p| path_mime(p).starts_with("video/")))
}
fn has_media(body: &Value) -> bool {
    ["image_asset_ids","video_asset_ids","image_urls","video_urls"].iter().any(|k|body[*k].as_array().is_some_and(|a|!a.is_empty()))
        || body["messages"].as_array().and_then(|a|a.iter().rev().find(|m|m["role"]=="user"))
            .is_some_and(|m|m["content"].as_array().is_some_and(|a|a.iter().any(|p|p["type"]=="image_url")))
}
fn remove_attachment_markup(body: &mut Value) {
    fn strip(text: &str) -> String {
        let mut out=String::new();let mut rest=text;
        while let Some((before,block))=rest.split_once("<uploaded_files>") {
            out.push_str(before);
            let Some((_,after))=block.split_once("</uploaded_files>") else {break};rest=after;
        }
        out.push_str(rest);out
    }
    if let Some(last)=body["messages"].as_array_mut().and_then(|a|a.last_mut()) {
        match &mut last["content"] {
            Value::String(s)=>*s=strip(s),
            Value::Array(parts)=>for p in parts {if p["type"]=="text" {if let Some(t)=p["text"].as_str() {p["text"]=json!(strip(t));}}},
            _=>{},
        }
    }
}
fn response(value: Value, stream: bool) -> Response {
    if !stream {return Json(value).into_response();}
    Response::builder().header("content-type","text/event-stream; charset=utf-8").header("cache-control","no-cache, no-transform").header("x-accel-buffering","no")
        .body(axum::body::Body::from(crate::video_delivery::sse_completion(&value))).unwrap()
}
fn active(state: &StarlinkRouterState, record: &ReferenceUpload) -> Option<Principal> {
    let p=state.store.active_principal_for_key(&record.key_id).ok()??;
    aiwork_core::require_scope(&p,"assets:write").ok()?;
    Some(p)
}
fn capability(state: &StarlinkRouterState, id: &str, key: &str) -> Result<String,Response> {
    let sealed=state.key_vault.encrypt(&format!("{}:upload-only",context(id)),key).map_err(|_|bad("reference_upload_unavailable"))?;
    Ok(format!("{}.{}",sealed.key_version,URL_SAFE_NO_PAD.encode(sealed.ciphertext)))
}
fn verify(state: &StarlinkRouterState,id: &str,token: &str,record: &ReferenceUpload) -> bool {
    if token.len()>1024 {return false;}
    let Some((version,cipher))=token.split_once('.') else {return false};
    let (Ok(version),Ok(cipher))=(version.parse(),URL_SAFE_NO_PAD.decode(cipher)) else {return false};
    state.key_vault.decrypt(&format!("{}:upload-only",context(id)),version,&cipher).is_ok_and(|key|key==record.key_id)
}
pub(crate) fn command(config: &Value, bash: bool) -> String {
    let encoded=STANDARD.encode(config.to_string());
    let ps=include_str!("reference_upload.ps1").replace("__CONFIG_BASE64__",&encoded);
    if !bash {return ps;}
    let encoded_ps=STANDARD.encode(ps.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>());
    let unix=include_str!("reference_upload.sh").replace("__CONFIG_BASE64__",&encoded);
    format!("case \"$(uname -s)\" in\nMINGW*|MSYS*|CYGWIN*) powershell.exe -NoProfile -NonInteractive -EncodedCommand {encoded_ps} ;;\n*)\n{unix}\n;;\nesac")
}

#[derive(Default)]
struct UploadReply {
    id: Option<String>,
    files: Option<usize>,
    recognized: bool,
    failed: bool,
    invalid: bool,
}
impl UploadReply {
    fn identify(&mut self, id: &str) {
        self.recognized = true;
        if !valid_id(id) || self.id.as_deref().is_some_and(|old| old != id) {
            self.invalid = true;
        } else {
            self.id = Some(id.to_owned());
        }
    }
    fn receipt(&mut self, value: &Value) {
        self.recognized = true;
        let Some(id) = value["id"].as_str() else { self.invalid = true; return; };
        self.identify(id);
        let Some(files) = value["files"].as_u64().filter(|n| (1..=10).contains(n)) else {
            self.invalid = true; return;
        };
        if self.files.is_some_and(|old| old != files as usize) { self.invalid = true; }
        self.files = Some(files as usize);
        match value["status"].as_str() {
            Some("uploaded") => {},
            Some("failed") => self.failed = true,
            _ => self.invalid = true,
        }
    }
}

// Native agents may rewrite the tool ID and wrap stdout in JSON or text
// blocks. Inspect only bounded tool output, never user/assistant history.
fn scan_reply(value: &Value, depth: usize, remaining: &mut usize, reply: &mut UploadReply) {
    if depth > 6 || *remaining == 0 { reply.invalid = true; return; }
    *remaining -= 1;
    if let Some(text) = value.as_str() {
        if text.len() > 64*1024 { reply.invalid = true; return; }
        // Unwrap JSON first: escaped stdout is not itself a receipt line.
        if let Ok(parsed) = serde_json::from_str::<Value>(text) {
            scan_reply(&parsed, depth+1, remaining, reply);
            return;
        }
        for line in text.lines() {
            if line.contains("SEEDANCE_REFERENCE_UPLOAD_FAILED:") {
                reply.recognized = true;
                reply.failed = true;
            }
            let mut rest = line;
            while let Some(index) = rest.find(RECEIPT_PREFIX) {
                if *remaining == 0 { reply.invalid = true; return; }
                *remaining -= 1;
                rest = &rest[index+RECEIPT_PREFIX.len()..];
                reply.recognized = true;
                match serde_json::Deserializer::from_str(rest).into_iter::<Value>().next() {
                    Some(Ok(receipt)) => reply.receipt(&receipt),
                    _ => reply.invalid = true,
                }
            }
        }
    } else if let Some(values) = value.as_array() {
        if values.len() > 32 { reply.invalid = true; }
        for value in values.iter().take(32) { scan_reply(value, depth+1, remaining, reply); }
    } else {
        for field in ["text", "stdout", "output", "content", "result", "data", "message"] {
            if let Some(value) = value.get(field) { scan_reply(value, depth+1, remaining, reply); }
        }
    }
}

fn upload_reply(body: &Value) -> Result<Option<(&Value, String, Option<usize>)>, &'static str> {
    let Some(messages) = body["messages"].as_array() else { return Ok(None); };
    let Some(last) = messages.last() else { return Ok(None); };
    let tools: Vec<&Value> = if last["role"] == "tool" {
        messages.iter().rev().take_while(|m| m["role"] == "tool").take(33).collect()
    } else if last["role"] == "user" {
        last["content"].as_array().map(|parts| parts.iter().filter(|p| p["type"] == "tool_result").take(33).collect()).unwrap_or_default()
    } else { return Ok(None); };
    let mut reply = UploadReply::default();
    let mut remaining = 256;
    let mut other_handoff = false;
    for tool in &tools {
        for field in ["tool_call_id", "tool_use_id"] {
            if let Some(call) = tool[field].as_str() {
                if let Some(id) = call.strip_prefix(PREFIX) { reply.identify(id); }
                else if call.starts_with("call_seedance_") { other_handoff = true; }
            }
        }
        scan_reply(&tool["content"], 0, &mut remaining, &mut reply);
    }
    if !reply.recognized { return Ok(None); }
    // Exactly one issued upload command belongs to this continuation. Never
    // discard another result or choose one of several conflicting receipts.
    if tools.len() != 1 || reply.invalid || other_handoff { return Err("reference_upload_invalid"); }
    if reply.failed { return Err("reference_upload_failed"); }
    let Some(id) = reply.id else { return Err("reference_upload_invalid"); };
    Ok(Some((tools[0], id, reply.files)))
}

/// Returns early for an upload tool call/error; successful follow-up restores
/// the encrypted original request and pins its generation idempotency key.
pub(crate) async fn before_chat(state:&Arc<StarlinkRouterState>,p:&Principal,headers:&mut HeaderMap,body:&mut Value)->Option<Response> {
    let follow_up=match upload_reply(body) {Ok(v)=>v,Err(code)=>return Some(bad(code))};
    if let Some((tool,id,files))=follow_up {
        let record=match state.store.reference_upload(&id) {Ok(Some(r))=>r,_=>return Some(bad("reference_upload_invalid"))};
        if record.key_id!=p.key_id || active(state,&record).is_none() {return Some(bad("reference_upload_invalid"));}
        if crate::video_delivery::tool_failed(tool,0) {return Some(bad("reference_upload_failed"));}
        if files.is_some_and(|n|n!=record.asset_ids.len()) {return Some(bad("reference_upload_invalid"));}
        if record.asset_ids.iter().any(Option::is_none) {return Some(bad("reference_upload_incomplete"));}
        let text=match state.key_vault.decrypt(&context(&id),record.key_version,&record.ciphertext) {Ok(t)=>t,_=>return Some(bad("reference_upload_invalid"))};
        let mut original:Value=match serde_json::from_str(&text) {Ok(v)=>v,Err(_)=>return Some(bad("reference_upload_invalid"))};
        let declared=match paths(&original) {Ok(v) if v.len()==record.asset_ids.len()=>v,_=>return Some(bad("reference_upload_invalid"))};
        let mut images=Vec::new();let mut videos=Vec::new();
        for (path,asset) in declared.iter().zip(&record.asset_ids) {
            if path_mime(path).starts_with("video/") {videos.push(asset.clone());} else {images.push(asset.clone());}
        }
        remove_attachment_markup(&mut original);
        original["image_asset_ids"]=json!(images);
        original["video_asset_ids"]=json!(videos);
        if state.config.work_context_for_key(&p.key_id) {
            match state.store.owned_work_for_conversation(p,&format!("upload:{id}")) {
                Ok(Some(w))=>original["work_context"]=json!({"work_id":w.work_id}),
                Ok(None)=>{},Err(_)=>return Some(bad("work_context_unavailable")),
            }
        }
        // Tool output and changed prompts/tools are not the paid request.
        original["stream"]=json!(body["stream"].as_bool().unwrap_or(false));
        headers.insert("idempotency-key",format!("reference-upload:{id}").parse().unwrap());
        *body=original;
        return None;
    }
    if has_media(body) {return None;}
    let paths=match paths(body) {Ok(v) if v.is_empty()=>return None,Ok(v)=>v,Err(code)=>return Some(bad(code))};
    for field in ["image_asset_ids","video_asset_ids"] {
        if body.get(field).is_some_and(|v|v.as_array().is_none_or(|ids|ids.len()>10 || ids.iter().any(|id|id.as_str().is_none_or(|s|s.trim().is_empty() || s.len()>128 || s.chars().any(char::is_control))))) {
            return Some(bad("invalid_reference_context"));
        }
    }
    if aiwork_core::require_scope(p,"assets:write").is_err() {return Some(error(StatusCode::FORBIDDEN,"insufficient_scope"));}
    let Some(tool)=crate::delivery_assist::tools(body).into_iter().next() else {return Some(bad("reference_upload_tool_unavailable"))};
    let base=state.config.public_base_url.trim_end_matches('/');
    // Configuration, never client text, chooses the upload destination.
    if !(base.starts_with("https://") || base.starts_with("http://127.0.0.1:") || base.starts_with("http://localhost:")) {return Some(bad("reference_upload_unavailable"));}
    let id=format!("{:032x}",rand::random::<u128>());
    let sealed=match state.key_vault.encrypt(&context(&id),&body.to_string()) {Ok(v)=>v,Err(_)=>return Some(bad("reference_upload_unavailable"))};
    let explicit=headers.get("idempotency-key").and_then(|h|h.to_str().ok()).filter(|s|!s.trim().is_empty());
    let fingerprint=aiwork_core::canonical_json_hash(body);
    let dedupe=aiwork_core::canonical_json_hash(&match explicit {Some(key)=>json!({"explicit":key}),None=>json!({"implicit":hex::encode(fingerprint)})});
    let id=match state.store.save_reference_upload(p,&id,paths.len(),chrono::Utc::now().timestamp_millis()+TTL,sealed.key_version,&sealed.ciphertext,&dedupe,&fingerprint,explicit.is_some()) {
        Ok(Some(id))=>id,Ok(None)=>return Some(error(StatusCode::CONFLICT,"idempotency_conflict")),
        Err(_)=>return Some(error(StatusCode::TOO_MANY_REQUESTS,"reference_upload_unavailable")),
    };
    let ticket=match capability(state,&id,&p.key_id) {Ok(t)=>t,Err(r)=>return Some(r)};
    let config=json!({"id":id,"base":format!("{base}/v1/reference-uploads/{id}"),"authorization":ticket,"paths":paths});
    let mut args=tool.arguments;
    args[tool.command_key]=json!(command(&config,tool.bash));
    if args.get("description").is_some() {args["description"]=json!("Upload the user-attached reference media before generating video");}
    let mut value=json!({"id":format!("chatcmpl-ref-{id}"),"object":"chat.completion","model":"seedance","created":chrono::Utc::now().timestamp(),
        "choices":[{"index":0,"message":{"role":"assistant","content":"正在通过本机工具上传本次参考素材，上传完成后开始处理生成请求。此步骤尚未提交视频、未扣视频积分。","tool_calls":[{"id":format!("{PREFIX}{id}"),"type":"function","function":{"name":tool.name,"arguments":args.to_string()}}]},"finish_reason":"tool_calls"}]});
    if state.config.work_context_for_key(&p.key_id) {
        let resolved=match crate::work_context::resolve(state,p,headers,body) {Ok(r)=>r,Err(_)=>return Some(bad("work_context_unavailable"))};
        let (work,version)=match resolved {
            crate::work_context::WorkResolution::Existing{work,base_version}=>(work,base_version),
            crate::work_context::WorkResolution::New=>match state.store.create_video_work(p,&format!("upload:{id}")) {Ok(w)=>(w,None),Err(_)=>return Some(bad("work_context_unavailable"))},
            crate::work_context::WorkResolution::Clarify{text}=>{value["choices"][0]["message"]=json!({"role":"assistant","content":text});value["choices"][0]["finish_reason"]=json!("stop");return Some(response(value,body["stream"].as_bool().unwrap_or(false)));},
        };
        let h=match crate::work_context::issue_handle(state,p,&work.work_id,version.as_ref().map(|v|v.version_id.as_str())) {Ok(h)=>h,Err(_)=>return Some(bad("work_context_unavailable"))};
        crate::work_context::decorate_reply(&mut value,&h,&json!({"work_id":work.work_id,"base_version_id":version.map(|v|v.version_id)}));
    }
    Some(response(value,body["stream"].as_bool().unwrap_or(false)))
}

/// This capability permits only one immutable media file per predeclared slot. It
/// cannot generate videos, inspect balances, download outputs or act as a Key.
pub(crate) async fn upload(State(state):State<Arc<StarlinkRouterState>>,Path((id,index)):Path<(String,usize)>,request:Request)->Response {
    if !valid_id(&id) {return error(StatusCode::UNAUTHORIZED,"reference_upload_invalid");}
    let record=match state.store.reference_upload(&id) {Ok(Some(r))=>r,_=>return error(StatusCode::UNAUTHORIZED,"reference_upload_invalid")};
    let token=request.headers().get("x-seedance-upload").and_then(|h|h.to_str().ok()).unwrap_or("");
    if !verify(&state,&id,token,&record) || index>=record.asset_ids.len() {return error(StatusCode::UNAUTHORIZED,"reference_upload_invalid");}
    let Some(p)=active(&state,&record) else {return error(StatusCode::UNAUTHORIZED,"reference_upload_invalid")};
    let bytes=match axum::body::to_bytes(request.into_body(),crate::assets::MAX_ASSET_BYTES).await {
        Ok(v)=>v,Err(_)=>return error(StatusCode::PAYLOAD_TOO_LARGE,"reference_upload_invalid_image"),
    };
    let result=tokio::task::spawn_blocking(move ||->Response {
        let declared=state.key_vault.decrypt(&context(&id),record.key_version,&record.ciphertext)
            .ok().and_then(|s|serde_json::from_str::<Value>(&s).ok()).and_then(|v|paths(&v).ok());
        let Some(path)=declared.as_ref().and_then(|v|v.get(index)) else {return bad("reference_upload_invalid")};
        let Some((mime,ext))=crate::assets::detect_format(&bytes) else {return bad("reference_upload_type_mismatch")};
        let expected=path_mime(path);
        // Preserve image content sniffing: some clients rename JPEGs image.png.
        // A video can never occupy an image slot, or change its container type.
        if !(mime==expected || (mime.starts_with("image/")&&expected.starts_with("image/"))) {return bad("reference_upload_type_mismatch");}
        let parsed=if mime.starts_with("image/") {
            match crate::assets::parse_reference_image(bytes.to_vec()) {Ok(v)=>v,Err(e)=>return crate::assets::response(e)}
        } else {crate::assets::ParsedAssetUpload {filename:format!("reference.{ext}"),declared_mime:Some(mime.into()),bytes:bytes.to_vec()}};
        let _permit=match state.asset_limiter.acquire(&p.key_id,parsed.bytes.len()) {Ok(p)=>p,Err(e)=>return crate::assets::response(e)};
        let sha=hex::encode(Sha256::digest(&parsed.bytes));
        if let Some(existing)=&record.asset_ids[index] {
            return match crate::assets::read_owned(&state.store,&state.config.data_dir,&p,existing) {
                Ok(a) if a.record.sha256==sha=>Json(json!({"id":existing,"sha256":sha,"bytes":a.record.size})).into_response(),
                _=>error(StatusCode::CONFLICT,"reference_upload_slot_conflict"),
            };
        }
        let stored=match crate::assets::write_asset(&state.config.data_dir,&p,parsed) {Ok(v)=>v,Err(e)=>return crate::assets::response(e)};
        let asset=match crate::assets::persist_asset(&state.store,&p,&stored) {Ok(v)=>v,Err(e)=>return crate::assets::response(e)};
        match state.store.bind_reference_upload_asset(&p,&id,index,&asset.id) {
            Ok(winner)=>Json(json!({"id":winner,"sha256":sha,"bytes":asset.size})).into_response(),
            Err(_)=>error(StatusCode::CONFLICT,"reference_upload_slot_conflict"),
        }
    }).await;
    result.unwrap_or_else(|_|error(StatusCode::SERVICE_UNAVAILABLE,"reference_upload_unavailable"))
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "0123456789abcdef0123456789abcdef";
    fn receipt() -> String {
        format!("SEEDANCE_REFERENCE_UPLOAD={{\"id\":\"{ID}\",\"status\":\"uploaded\",\"files\":1}}")
    }
    fn body(content: Value) -> Value {
        json!({"messages":[{"role":"tool","tool_call_id":"rewritten","content":content}]})
    }
    #[test]
    fn receipt_in_user_or_old_assistant_text_cannot_resume_upload() {
        for role in ["user", "assistant", "system"] {
            let b=json!({"messages":[{"role":role,"content":receipt()}]});
            assert!(upload_reply(&b).unwrap().is_none());
        }
        let b=json!({"messages":[{"role":"assistant","content":receipt()},
            {"role":"tool","tool_call_id":"unrelated","content":"done"}]});
        assert!(upload_reply(&b).unwrap().is_none());
    }
    #[test]
    fn receipt_scan_rejects_unbounded_output_instead_of_ignoring_ambiguity() {
        let mut deep=json!(receipt());
        for _ in 0..7 {deep=json!({"content":deep});}
        let large=json!("x".repeat(64*1024+1));
        let many=json!(vec![json!(receipt());33]);
        for content in [json!({"stdout":receipt(),"data":deep}),
            json!({"stdout":receipt(),"output":large}),many] {
            assert_eq!(upload_reply(&body(content)).err(),Some("reference_upload_invalid"));
        }
    }
    #[test]
    fn repeated_identical_receipt_is_not_a_different_upload_session() {
        let b=body(json!({"stdout":format!("{}\n{}",receipt(),receipt())}));
        let (_,id,files)=upload_reply(&b).unwrap().unwrap();
        assert_eq!(id,ID);assert_eq!(files,Some(1));
    }
    #[test]
    fn receipts_on_one_terminal_line_cannot_hide_a_conflicting_session() {
        let other="SEEDANCE_REFERENCE_UPLOAD={\"id\":\"ffffffffffffffffffffffffffffffff\",\"status\":\"uploaded\",\"files\":1}";
        let b=body(json!(format!("{} {other}",receipt())));
        assert_eq!(upload_reply(&b).err(),Some("reference_upload_invalid"));
    }
    #[test]
    fn upload_cannot_discard_a_second_tool_result_or_conflicting_id_field() {
        let mut b=body(json!(receipt()));
        b["messages"].as_array_mut().unwrap().push(json!({"role":"tool","tool_call_id":"unrelated","content":"done"}));
        assert_eq!(upload_reply(&b).err(),Some("reference_upload_invalid"));
        let mut b=body(json!(receipt()));
        b["messages"][0]["tool_use_id"]=json!("call_ref_upload_ffffffffffffffffffffffffffffffff");
        assert_eq!(upload_reply(&b).err(),Some("reference_upload_invalid"));
    }
}
