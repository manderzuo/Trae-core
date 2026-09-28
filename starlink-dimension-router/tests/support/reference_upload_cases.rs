use super::*;
use axum::{body::{Body,to_bytes},http::Request};
use base64::{engine::general_purpose::STANDARD,Engine as _};
use tower::ServiceExt;
use sha2::Digest;

const PNG: &[u8]=b"\x89PNG\r\n\x1a\nreference-fixture";
struct Fixture {
    app:axum::Router, state:Arc<StarlinkRouterState>, bridge:Arc<Bridge>, key:String,
    principal:aiwork_core::Principal, dir:Directory,
}
fn fixture(base:&str)->Fixture {
    let dir=Directory(std::env::temp_dir().join(format!("ref-upload-{:032x}",rand::random::<u128>())));
    let store=Arc::new(CoreStore::open(dir.path()).unwrap());store.migrate().unwrap();
    store.create_user(NewUser {id:"admin".into(),name:"Admin".into(),role:UserRole::Admin},"bootstrap").unwrap();
    let key=store.issue_api_key("admin","test",BTreeSet::from(["admin:*".into()]),"bootstrap").unwrap();
    let principal=store.authenticate_api_key(&key.plaintext).unwrap();
    store.key_quota_grant_as_admin(&principal,KeyQuotaGrant {api_key_id:key.id,resource_kind:"credits".into(),amount:500_000_000,actor_user_id:"admin".into(),reason:"test".into()}).unwrap();
    store.set_video_billing_control(aiwork_core::VideoBillingControlInput {mode:aiwork_core::VideoBillingMode::Active,reason:"fixture".into(),diagnostic_key_id:None,diagnostic_request_hash:None}).unwrap();
    let bridge=Arc::new(Bridge {claims:Mutex::new(BTreeMap::new()),sends:AtomicUsize::new(0),video_intent:true,large_downloads:std::sync::atomic::AtomicBool::new(false),active_downloads:Arc::new(AtomicUsize::new(0)),download_status:AtomicUsize::new(200)});
    let mut cfg=RouterConfig::defaults(dir.path().into());cfg.budget_billing_v2=true;cfg.public_base_url=base.into();
    let state=StarlinkRouterState::for_test(store,BridgeClient::from_transport("http://bridge","bridge-only",bridge.clone()),cfg);
    let app=starlink_dimension_router::server::build_router(state.clone());
    Fixture {app,state,bridge,key:key.plaintext,principal,dir}
}
fn input(paths:&[&str],stream:bool)->Value {
    let attached=paths.iter().map(|p|format!("<file_path>{p}</file_path>")).collect::<Vec<_>>().join("\n");
    json!({"model":"seedance","stream":stream,"messages":[{"role":"user","content":[
        {"type":"text","text":format!("<uploaded_files>Files uploaded by user:\n{attached}\n</uploaded_files>")},
        {"type":"text","text":"<user_input>这是参考图，生成5秒720P 9：16视频</user_input>"}]}],
        "tools":[{"type":"function","function":{"name":"RunCommand","description":"Execute PowerShell. NEVER use bash syntax.","parameters":{"type":"object","required":["command","blocking","requires_approval"],"properties":{"command":{"type":"string"},"blocking":{"type":"boolean"},"requires_approval":{"type":"boolean"}}}}}]})
}
async fn chat(f:&Fixture,key:&str,body:Value)->axum::response::Response {
    f.app.clone().oneshot(Request::post("/v1/chat/completions").header("authorization",format!("Bearer {key}")).header("content-type","application/json").body(Body::from(body.to_string())).unwrap()).await.unwrap()
}
async fn json_response(r:axum::response::Response)->Value {
    serde_json::from_slice(&to_bytes(r.into_body(),256*1024).await.unwrap()).unwrap()
}
async fn pending(f:&Fixture,body:Value)->(Value,Value) {
    let r=chat(f,&f.key,body).await;assert_eq!(r.status(),StatusCode::OK);
    let value=json_response(r).await;
    let call=&value["choices"][0]["message"]["tool_calls"][0];
    let args:Value=serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
    let cmd=args["command"].as_str().unwrap();
    let encoded=cmd.split("FromBase64String('").nth(1).unwrap().split('\'').next().unwrap();
    let cfg:Value=serde_json::from_slice(&STANDARD.decode(encoded).unwrap()).unwrap();
    (value,cfg)
}
async fn upload(f:&Fixture,cfg:&Value,index:usize,bytes:&[u8])->axum::response::Response {
    f.app.clone().oneshot(Request::post(format!("/v1/reference-uploads/{}/{index}",cfg["id"].as_str().unwrap()))
        .header("x-seedance-upload",cfg["authorization"].as_str().unwrap()).body(Body::from(bytes.to_vec())).unwrap()).await.unwrap()
}
fn follow(value:&Value)->Value {
    json!({"model":"seedance","messages":[value["choices"][0]["message"],{"role":"tool",
        "tool_call_id":value["choices"][0]["message"]["tool_calls"][0]["id"],"content":"uploaded"}]})
}

#[tokio::test]
async fn reference_upload_roundtrip_is_immutable_idempotent_and_restores_original_spec() {
    let f=fixture("https://api.example.test");
    let (call,cfg)=pending(&f,input(&["C:\\Test\\参考'&$.png"],false)).await;
    assert_eq!(f.bridge.sends.load(Ordering::SeqCst),0);
    let failed=chat(&f,&f.key,follow(&call)).await;
    assert_eq!(failed.status(),StatusCode::BAD_REQUEST,"claimed tool success without server bytes is not evidence");
    let r=upload(&f,&cfg,0,PNG).await;assert_eq!(r.status(),StatusCode::OK);
    let asset=json_response(r).await;
    assert_eq!(asset["sha256"],hex::encode(sha2::Sha256::digest(PNG)));
    let replay=json_response(upload(&f,&cfg,0,PNG).await).await;assert_eq!(replay["id"],asset["id"]);
    let changed=[PNG,b"different"].concat();assert_eq!(upload(&f,&cfg,0,&changed).await.status(),StatusCode::CONFLICT);
    let mut body=follow(&call);body["messages"].as_array_mut().unwrap().insert(0,json!({"role":"user","content":"malicious replacement 15秒1080p"}));
    let result=chat(&f,&f.key,body.clone()).await;assert_eq!(result.status(),StatusCode::OK);
    let result=json_response(result).await;
    let sends=f.bridge.sends.load(Ordering::SeqCst);
    let repeated=chat(&f,&f.key,body).await;assert_eq!(repeated.status(),StatusCode::OK);
    let repeated=json_response(repeated).await;
    assert_eq!(repeated["request_id"],result["request_id"]);
    assert_eq!(f.bridge.sends.load(Ordering::SeqCst),sends,"tool replay cannot generate or charge again");
    let claims=f.bridge.claims.lock().unwrap();
    let videos:Vec<_>=claims.values().filter(|c|c["step_kind"]=="video").collect();assert_eq!(videos.len(),1);
    assert_eq!(videos[0]["body"]["image_asset_ids"],json!(["bridge-image"]));
    assert_eq!(videos[0]["body"]["duration"],5);assert_eq!(videos[0]["body"]["resolution"],"720p");assert_eq!(videos[0]["body"]["ratio"],"9:16");
    assert!(!claims.values().any(|v|v.to_string().contains("malicious replacement")));
    assert!(!claims.values().any(|v|v.to_string().contains("<uploaded_files>")));
}

#[tokio::test]
async fn reference_upload_parallel_handoffs_do_not_share_images_or_trust_foreign_keys() {
    let f=fixture("https://api.example.test");
    let ((a,ca),(b,cb))=tokio::join!(pending(&f,input(&["C:\\a.png"],false)),pending(&f,input(&["C:\\b.png","C:\\c.png"],false)));
    assert_ne!(ca["id"],cb["id"]);
    assert_eq!(upload(&f,&ca,0,PNG).await.status(),StatusCode::OK);
    assert_eq!(chat(&f,&f.key,follow(&b)).await.status(),StatusCode::BAD_REQUEST);
    assert_eq!(upload(&f,&cb,0,PNG).await.status(),StatusCode::OK);
    assert_eq!(chat(&f,&f.key,follow(&b)).await.status(),StatusCode::BAD_REQUEST,"partial upload cannot generate");
    let foreign=f.state.store.issue_api_key("admin","foreign",BTreeSet::from(["admin:*".into()]),"bootstrap").unwrap();
    assert_eq!(chat(&f,&foreign.plaintext,follow(&a)).await.status(),StatusCode::BAD_REQUEST);
    let mut tamper=ca.clone();tamper["id"]=cb["id"].clone();
    assert_eq!(upload(&f,&tamper,1,PNG).await.status(),StatusCode::UNAUTHORIZED);
    assert_eq!(upload(&f,&ca,1,PNG).await.status(),StatusCode::UNAUTHORIZED);
    assert_eq!(upload(&f,&cb,1,b"not an image").await.status(),StatusCode::BAD_REQUEST);
    let mut denied=follow(&a);denied["messages"][1]["content"]=json!("{\"exitCode\":1,\"stdout\":\"uploaded\"}");
    assert_eq!(chat(&f,&f.key,denied).await.status(),StatusCode::BAD_REQUEST);
    assert_eq!(f.bridge.sends.load(Ordering::SeqCst),0);
    f.state.store.revoke_api_key(&f.principal.key_id,"admin").unwrap();
    assert_eq!(upload(&f,&ca,0,PNG).await.status(),StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn reference_upload_streams_tool_call_and_rejects_expiry_unsafe_paths_or_disabled_tools() {
    let f=fixture("https://api.example.test");
    let r=chat(&f,&f.key,input(&["C:\\a.png"],true)).await;
    assert_eq!(r.status(),StatusCode::OK);assert!(r.headers()["content-type"].to_str().unwrap().starts_with("text/event-stream"));
    let raw=to_bytes(r.into_body(),256*1024).await.unwrap();let wire=std::str::from_utf8(&raw).unwrap();
    let frame:Value=serde_json::from_str(wire.lines().next().unwrap().strip_prefix("data: ").unwrap()).unwrap();
    assert_eq!(frame["choices"][0]["finish_reason"],"tool_calls");assert_eq!(frame["choices"][0]["delta"]["tool_calls"][0]["index"],0);
    assert!(wire.ends_with("data: [DONE]\n\n"));
    for path in ["\\\\server\\private.png","C:\\a.png:private","C:\\a\n.png","relative.png","C:\\secret.pem","/tmp/../secret.png"] {
        assert_eq!(chat(&f,&f.key,input(&[path],false)).await.status(),StatusCode::BAD_REQUEST);
    }
    let mut disabled=input(&["C:\\a.png"],false);disabled["tool_choice"]=json!("none");
    assert_eq!(chat(&f,&f.key,disabled).await.status(),StatusCode::BAD_REQUEST);
    let (call,cfg)=pending(&f,input(&["C:\\a.png"],false)).await;
    let db=rusqlite::Connection::open(f.dir.path().join("data").join(aiwork_core::CORE_DB_FILE)).unwrap();
    db.execute("UPDATE reference_uploads SET expires_at_ms=0 WHERE id=?1",[cfg["id"].as_str().unwrap()]).unwrap();
    assert_eq!(upload(&f,&cfg,0,PNG).await.status(),StatusCode::UNAUTHORIZED);
    assert_eq!(chat(&f,&f.key,follow(&call)).await.status(),StatusCode::BAD_REQUEST);
    assert_eq!(f.bridge.sends.load(Ordering::SeqCst),0);
    starlink_dimension_router::assets::cleanup_expired_assets_once(&f.state.store,f.dir.path(),chrono::Utc::now().timestamp_millis()).unwrap();
    let remaining:i64=db.query_row("SELECT count(*) FROM reference_uploads WHERE id=?1",[cfg["id"].as_str().unwrap()],|r|r.get(0)).unwrap();
    assert_eq!(remaining,0,"expired encrypted client paths must be purged even without new uploads");
}

#[tokio::test]
async fn reference_upload_initial_retry_keeps_one_handoff_and_rejects_idempotency_conflicts() {
    let f=fixture("https://api.example.test");
    let body=input(&["C:\\a.png"],false);
    let submit=|body:Value|f.app.clone().oneshot(Request::post("/v1/chat/completions")
        .header("authorization",format!("Bearer {}",f.key)).header("content-type","application/json")
        .header("idempotency-key","one-client-action").body(Body::from(body.to_string())).unwrap());
    let first=json_response(submit(body.clone()).await.unwrap()).await;
    let second=json_response(submit(body).await.unwrap()).await;
    assert_eq!(first["choices"][0]["message"]["tool_calls"][0]["id"],second["choices"][0]["message"]["tool_calls"][0]["id"],"retrying the first response must not permit a second paid generation");
    assert_eq!(submit(input(&["C:\\b.png"],false)).await.unwrap().status(),StatusCode::CONFLICT);
    assert_eq!(f.bridge.sends.load(Ordering::SeqCst),0);
}

#[tokio::test]
async fn reference_upload_cannot_replace_malformed_reference_fields() {
    let f=fixture("https://api.example.test");
    let mut body=input(&["C:\\a.png"],false);body["image_asset_ids"]=json!("bad-reference-array");
    let r=chat(&f,&f.key,body).await;
    assert_eq!(r.status(),StatusCode::BAD_REQUEST,"upload must not erase malformed existing reference fields");
}

#[tokio::test(flavor="multi_thread",worker_threads=2)]
async fn reference_upload_generated_powershell_command_sends_exact_unicode_path_bytes() {
    if !cfg!(windows) {return;}
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let f=fixture(&format!("http://{}",listener.local_addr().unwrap()));
    let image=f.dir.path().join("参考'&$.png");std::fs::write(&image,PNG).unwrap();
    let app=f.app.clone();let server=tokio::spawn(async move {axum::serve(listener,app).await.unwrap()});
    let (call,cfg)=pending(&f,input(&[image.to_str().unwrap()],false)).await;
    let args:Value=serde_json::from_str(call["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"].as_str().unwrap()).unwrap();
    let cmd=STANDARD.encode(args["command"].as_str().unwrap().encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>());
    for shell in ["C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe","pwsh.exe"] {
        let result=tokio::process::Command::new(shell)
            .args(["-NoProfile","-NonInteractive","-EncodedCommand",&cmd]).env_remove("PSModulePath").output().await.unwrap();
        assert!(result.status.success(),"{shell}: {}",String::from_utf8_lossy(&result.stdout));
        assert!(String::from_utf8_lossy(&result.stdout).contains("SEEDANCE_REFERENCE_UPLOAD="));
    }
    let record=f.state.store.reference_upload(cfg["id"].as_str().unwrap()).unwrap().unwrap();
    let asset=starlink_dimension_router::assets::read_owned(&f.state.store,&f.state.config.data_dir,&f.principal,record.asset_ids[0].as_deref().unwrap()).unwrap();
    assert_eq!(asset.bytes,PNG);assert_eq!(f.bridge.sends.load(Ordering::SeqCst),0);
    server.abort();let _=server.await;
}

#[tokio::test(flavor="multi_thread",worker_threads=2)]
async fn reference_upload_generated_bash_command_sends_exact_literal_path_bytes() {
    if !cfg!(unix) {return;}
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let f=fixture(&format!("http://{}",listener.local_addr().unwrap()));
    let image=f.dir.path().join("参考'&$(echo never).png");std::fs::write(&image,PNG).unwrap();
    let app=f.app.clone();let server=tokio::spawn(async move {axum::serve(listener,app).await.unwrap()});
    let mut body=input(&[image.to_str().unwrap()],false);
    body["tools"][0]["function"]["name"]=json!("bash");body["tools"][0]["function"]["description"]=json!("Execute Bash command");
    let call=json_response(chat(&f,&f.key,body).await).await;
    let args:Value=serde_json::from_str(call["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"].as_str().unwrap()).unwrap();
    let result=tokio::process::Command::new("bash").args(["-c",args["command"].as_str().unwrap()]).output().await.unwrap();
    assert!(result.status.success(),"{}",String::from_utf8_lossy(&result.stdout));
    let id=call["choices"][0]["message"]["tool_calls"][0]["id"].as_str().unwrap().strip_prefix("call_ref_upload_").unwrap();
    let record=f.state.store.reference_upload(id).unwrap().unwrap();
    let asset=starlink_dimension_router::assets::read_owned(&f.state.store,&f.state.config.data_dir,&f.principal,record.asset_ids[0].as_deref().unwrap()).unwrap();
    assert_eq!(asset.bytes,PNG);assert_eq!(f.bridge.sends.load(Ordering::SeqCst),0);
    server.abort();let _=server.await;
}

#[tokio::test]
async fn reference_upload_survives_router_restart_without_reading_tool_claimed_asset_ids() {
    let f=fixture("https://api.example.test");
    let (call,cfg)=pending(&f,input(&["C:\\a.png"],false)).await;
    assert_eq!(upload(&f,&cfg,0,PNG).await.status(),StatusCode::OK);
    let reopened=Arc::new(CoreStore::open(f.dir.path()).unwrap());reopened.migrate().unwrap();
    let state=StarlinkRouterState::for_test(reopened,BridgeClient::from_transport("http://bridge","bridge-only",f.bridge.clone()),f.state.config.clone());
    let mut body=follow(&call);body["image_asset_ids"]=json!(["asset-foreign"]);body["messages"][1]["content"]=json!("{\"asset_ids\":[\"asset-foreign\"],\"status\":\"uploaded\"}");
    let r=user_routes::chat_completions(State(state),HeaderMap::new(),Extension(f.principal.clone()),Bytes::from(body.to_string())).await;
    assert_eq!(r.status(),StatusCode::OK);
    let claims=f.bridge.claims.lock().unwrap();
    assert!(claims.values().any(|c|c["step_kind"]=="video" && c["body"]["image_asset_ids"]==json!(["bridge-image"])));
    assert!(!claims.values().any(|c|c.to_string().contains("asset-foreign")));
}
