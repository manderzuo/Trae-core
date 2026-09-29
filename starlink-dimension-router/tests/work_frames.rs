use std::{collections::BTreeMap,sync::Arc};
use aiwork_core::{BudgetStepView,BudgetStepKind,BudgetExecutionState,BudgetFinancialState,CreditAmount};
use starlink_dimension_router::bridge_client::{BridgeClient,BridgeTransport,BridgeResponse};
use sha2::{Digest,Sha256};
struct Reply {body:Vec<u8>,headers:BTreeMap<String,String>}
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
