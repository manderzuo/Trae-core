use std::{collections::BTreeMap,sync::Arc};
use aiwork_core::{BudgetStepView,BudgetStepKind,BudgetExecutionState,BudgetFinancialState,CreditAmount};
use starlink_dimension_router::bridge_client::{BridgeClient,BridgeTransport,BridgeResponse};
use sha2::{Digest,Sha256};
struct Reply {body:Vec<u8>,headers:BTreeMap<String,String>}
struct SourceReply(Reply);
impl BridgeTransport for SourceReply {
    fn send(&self,method:&str,url:&str,headers:&BTreeMap<String,String>,_:&[u8])->Result<BridgeResponse,String> {
        assert_eq!(method,"GET");assert!(url.ends_with("/requests/request-frame/content?budget_id=budget-frame"));assert_eq!(headers["authorization"],"Bearer bridge-only");
        Ok(BridgeResponse{status:200,headers:self.0.headers.clone(),body:self.0.body.clone()})
    }
}
fn source_reply()->Reply {
    let mut r=reply();r.body=b"\x00\x00\x00\x18ftypisom\x00\x00\x00\x00isomiso2".to_vec();
    r.headers.insert("content-type".into(),"video/mp4".into());r.headers.insert("content-length".into(),"24".into());
    r.headers.insert("x-aiwork-task-ref".into(),"video-frame".into());r
}
#[test]
fn source_video_rejects_wrong_binding_truncated_or_oversized_artifact() {
    let get=|r:Reply|BridgeClient::from_transport("http://bridge","bridge-only",Arc::new(SourceReply(r))).source_video(&step());
    assert_eq!(get(source_reply()).unwrap().len(),24);
    for (field,value) in [("x-aiwork-request-id","other"),("x-aiwork-budget-id","other"),("x-aiwork-core-key-id","other"),("x-aiwork-account-ref","other"),("x-aiwork-bridge-instance-id","other"),("x-aiwork-task-ref","other"),("content-type","text/html"),("content-length","25"),("content-length","33554433")] {
        let mut r=source_reply();r.headers.insert(field.into(),value.into());assert!(get(r).is_err(),"{field}");
    }
    let mut r=source_reply();r.body[4]=b'x';assert!(get(r).is_err());
    let mut pending=step();pending.execution_state=BudgetExecutionState::Running;
    assert!(BridgeClient::from_transport("http://bridge","bridge-only",Arc::new(SourceReply(source_reply()))).source_video(&pending).is_err());
}
#[test]
fn real_http_source_video_retains_task_identity_headers() {
    use std::{io::{Read,Write},net::TcpListener,time::Duration};
    let listener=TcpListener::bind("127.0.0.1:0").unwrap();let base=format!("http://{}",listener.local_addr().unwrap());let expected=source_reply();
    let server=std::thread::spawn(move||{
        let (mut stream,_)=listener.accept().unwrap();stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut bytes=Vec::new();let mut buffer=[0;1024];
        while !bytes.windows(4).any(|w|w==b"\r\n\r\n") {let n=stream.read(&mut buffer).unwrap();assert!(n>0);bytes.extend_from_slice(&buffer[..n]);assert!(bytes.len()<8192);}
        assert!(String::from_utf8(bytes).unwrap().starts_with("GET /internal/bridge/v2/requests/request-frame/content?budget_id=budget-frame "));
        let mut head=String::from("HTTP/1.1 200 OK\r\nConnection: close\r\n");for(k,v)in expected.headers{head.push_str(&format!("{k}: {v}\r\n"));}head.push_str("\r\n");stream.write_all(head.as_bytes()).unwrap();stream.write_all(&expected.body).unwrap();
    });
    let result=BridgeClient::new(base,"bridge-only").source_video(&step());server.join().unwrap();assert_eq!(result.unwrap().len(),24);
}
impl BridgeTransport for Reply {
    fn send(&self,method:&str,url:&str,headers:&BTreeMap<String,String>,_:&[u8])->Result<BridgeResponse,String> {
        assert_eq!(method,"POST");assert!(url.ends_with("/requests/request-frame/last-frame?budget_id=budget-frame"));assert_eq!(headers["authorization"],"Bearer bridge-only");
        Ok(BridgeResponse{status:200,headers:self.headers.clone(),body:self.body.clone()})
    }
}
fn step()->BudgetStepView {BudgetStepView {
    operation_id:"op-frame".into(),parent_request_id:"request-frame".into(),request_id:"request-frame".into(),kind:BudgetStepKind::Video,budget_id:"budget-frame".into(),core_key_id:"key-frame".into(),request_fingerprint:"fingerprint".into(),endpoint:"videos".into(),model:"seedance".into(),account_ref:"account-frame".into(),bridge_instance_id:"bridge-frame".into(),profile_fingerprint:"profile".into(),policy_version:"policy".into(),hold_credits:CreditAmount::parse("40","credits").unwrap(),expires_at_ms:1000,reservation_id:"reservation".into(),dispatch_attempted:true,execution_state:BudgetExecutionState::Succeeded,financial_state:BudgetFinancialState::Held,actual_credits:None,task_ref:Some("video-frame".into())
}}
fn reply()->Reply {
    use base64::Engine;
    let body=base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aVl8AAAAASUVORK5CYII=").unwrap();
    let headers=BTreeMap::from([
        ("content-type".into(),"image/png".into()),("content-length".into(),body.len().to_string()),
        ("x-aiwork-frame-width".into(),"1".into()),("x-aiwork-frame-height".into(),"1".into()),("x-aiwork-frame-timestamp-ms".into(),"875".into()),
        ("x-aiwork-source-sha256".into(),"ab".repeat(32)),("x-aiwork-frame-sha256".into(),format!("{:x}",Sha256::digest(&body))),
        ("x-aiwork-request-id".into(),"request-frame".into()),("x-aiwork-budget-id".into(),"budget-frame".into()),("x-aiwork-core-key-id".into(),"key-frame".into()),("x-aiwork-account-ref".into(),"account-frame".into()),("x-aiwork-bridge-instance-id".into(),"bridge-frame".into())]);Reply{body,headers}
}

struct ToolFailure {status:u16,body:Vec<u8>}
impl BridgeTransport for ToolFailure {
    fn send(&self,_:&str,_:&str,headers:&BTreeMap<String,String>,_:&[u8])->Result<BridgeResponse,String> {
        assert_eq!(headers["authorization"],"Bearer bridge-only");
        Ok(BridgeResponse {status:self.status,headers:BTreeMap::new(),body:self.body.clone()})
    }
}
#[test]
fn frame_tool_errors_preserve_only_bounded_known_codes() {
    for code in ["frame_extractor_unconfigured","frame_extractor_unavailable","frame_extractor_digest_mismatch"] {
        let client=BridgeClient::from_transport("http://bridge","bridge-only",Arc::new(ToolFailure {status:503,body:serde_json::to_vec(&serde_json::json!({"error":{"code":code}})).unwrap()}));
        assert_eq!(client.last_frame(&step()).unwrap_err(),code);
    }
    for body in [b"not json".to_vec(),serde_json::to_vec(&serde_json::json!({"error":{"code":"private-key-and-path"}})).unwrap(),vec![b' ';8193]] {
        let client=BridgeClient::from_transport("http://bridge","bridge-only",Arc::new(ToolFailure {status:503,body}));
        assert_eq!(client.last_frame(&step()).unwrap_err(),"frame_extraction_unavailable");
    }
}
#[test]
fn real_http_tool_failure_keeps_the_exact_safe_reason() {
    use std::{io::{Read,Write},net::TcpListener,time::Duration};
    let listener=TcpListener::bind("127.0.0.1:0").unwrap();let base=format!("http://{}",listener.local_addr().unwrap());
    let server=std::thread::spawn(move|| {
        let (mut stream,_)=listener.accept().unwrap();stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut bytes=Vec::new();let mut buffer=[0;1024];
        while !bytes.windows(4).any(|w|w==b"\r\n\r\n") {let n=stream.read(&mut buffer).unwrap();assert!(n>0);bytes.extend_from_slice(&buffer[..n]);assert!(bytes.len()<8192);}
        assert!(String::from_utf8(bytes).unwrap().to_ascii_lowercase().contains("authorization: bearer bridge-only"));
        let body=br#"{"error":{"code":"frame_extractor_digest_mismatch","type":"bridge_error"}}"#;
        stream.write_all(format!("HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).unwrap();stream.write_all(body).unwrap();
    });
    let result=BridgeClient::new(base,"bridge-only").last_frame(&step());server.join().unwrap();
    assert_eq!(result.unwrap_err(),"frame_extractor_digest_mismatch");
}
#[test]
fn last_frame_checks_exact_identity_digest_dimensions_and_size() {
    let client=BridgeClient::from_transport("http://bridge","bridge-only",Arc::new(reply()));let frame=client.last_frame(&step()).unwrap();assert_eq!((frame.width,frame.height,frame.timestamp_ms),(1,1,875));
    for (field,value) in [("x-aiwork-core-key-id","other"),("x-aiwork-budget-id","other"),("x-aiwork-request-id","other"),("x-aiwork-account-ref","other"),("x-aiwork-frame-width","2"),("x-aiwork-frame-sha256","ab"),("x-aiwork-source-sha256","bad")] {let mut r=reply();r.headers.insert(field.into(),value.into());assert!(BridgeClient::from_transport("http://bridge","bridge-only",Arc::new(r)).last_frame(&step()).is_err());}
    let mut r=reply();r.body=vec![0;8*1024*1024+1];r.headers.insert("content-length".into(),r.body.len().to_string());assert!(BridgeClient::from_transport("http://bridge","bridge-only",Arc::new(r)).last_frame(&step()).is_err());
    let mut s=step();s.execution_state=BudgetExecutionState::Running;assert!(client.last_frame(&s).is_err());
}

#[test]
fn real_http_stream_preserves_authenticated_frame_provenance_headers() {
    use std::{io::{Read,Write},net::TcpListener,time::Duration};
    let listener=TcpListener::bind("127.0.0.1:0").unwrap();
    let base=format!("http://{}",listener.local_addr().unwrap());
    let expected=reply();
    let server=std::thread::spawn(move || {
        let (mut stream,_)=listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut bytes=Vec::new();let mut buffer=[0;1024];
        while !bytes.windows(4).any(|w|w==b"\r\n\r\n") {
            let n=stream.read(&mut buffer).unwrap();assert!(n>0);bytes.extend_from_slice(&buffer[..n]);assert!(bytes.len()<8192);
        }
        let request=String::from_utf8(bytes).unwrap();
        assert!(request.starts_with("POST /internal/bridge/v2/requests/request-frame/last-frame?budget_id=budget-frame "));
        assert!(request.to_ascii_lowercase().contains("authorization: bearer bridge-only"));
        let mut header=String::from("HTTP/1.1 200 OK\r\nConnection: close\r\n");
        for (k,v) in expected.headers {header.push_str(&format!("{k}: {v}\r\n"));}
        header.push_str("\r\n");stream.write_all(header.as_bytes()).unwrap();stream.write_all(&expected.body).unwrap();
    });
    let result=BridgeClient::new(base,"bridge-only").last_frame(&step());
    server.join().unwrap();
    let frame=result.expect("real HTTP transport must preserve frame length and owned provenance, not only content-type");
    assert_eq!((frame.width,frame.height,frame.timestamp_ms),(1,1,875));
}
