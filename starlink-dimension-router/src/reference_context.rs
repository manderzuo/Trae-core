//! Explicit, authenticated attachment continuity; never a Key-wide last-image cache.
use aiwork_core::Principal;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json,Value};
use crate::{assets::ParsedAssetUpload,state::StarlinkRouterState};

const PREFIX:&str="[AIWORK_REFERENCE:";
fn text(message:&Value)->String {
    match &message["content"] {
        Value::String(s)=>s.clone(),
        Value::Array(parts)=>parts.iter().filter_map(|p|p["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _=>String::new(),
    }
}
fn wants_reference(s:&str)->bool {
    ["这是人物参考图","这是参考图","以图片为参考","使用参考图","根据参考图","附带参考图","参考图如下","参考图说明"]
        .iter().any(|p|s.contains(p))
}
fn has_references(body:&Value)->bool {
    ["image_asset_ids","video_asset_ids","image_urls","video_urls"].iter().any(|f|body[*f].as_array().is_some_and(|v|!v.is_empty()))
        || body["messages"].as_array().and_then(|m|m.iter().rev().find(|m|m["role"]=="user"))
        .and_then(|m|m["content"].as_array()).is_some_and(|p|p.iter().any(|p|p["type"]=="image_url"))
}
fn context(principal:&Principal)->String {format!("seedance-reference-v1:{}",principal.key_id)}
fn validate_image(state:&StarlinkRouterState,principal:&Principal,id:&str)->Result<i64,&'static str> {
    let asset=crate::assets::read_owned(&state.store,&state.config.data_dir,principal,id).map_err(|_|"reference_asset_unavailable")?;
    if asset.record.state!=aiwork_core::AssetState::Active || asset.record.expires_at_ms<=chrono::Utc::now().timestamp_millis() {
        return Err("reference_asset_unavailable");
    }
    if !asset.record.mime_type.starts_with("image/") {return Err("reference_asset_type_mismatch");}
    Ok(asset.record.expires_at_ms)
}

pub(crate) fn recover(state:&StarlinkRouterState,principal:&Principal,body:&mut Value)->Result<(),&'static str> {
    // Validate explicit references before merging an opaque marker. Never turn
    // a malformed client field into an empty list or pay the helper first.
    for field in ["image_asset_ids","video_asset_ids"] {
        if let Some(value)=body.get(field) {
            let ids=value.as_array().ok_or("invalid_reference_context")?;
            if ids.len()>10 {return Err("reference_image_limit");}
            if ids.iter().any(|id|id.as_str().is_none_or(|s|s.trim().is_empty() || s.len()>128 || s.chars().any(char::is_control))) {
                return Err("invalid_reference_context");
            }
        }
    }
    let messages=body["messages"].as_array().ok_or("invalid_reference_context")?;
    let index=messages.iter().rposition(|m|m["role"]=="user").ok_or("invalid_reference_context")?;
    let prompt=text(&messages[index]);
    let mut ids=Vec::<Value>::new();
    let mut remaining=prompt.as_str();let mut markers=0;
    while let Some((_,tail))=remaining.split_once(PREFIX) {
        markers+=1;if markers>10 {return Err("reference_image_limit");}
        let (token,rest)=tail.split_once(']').ok_or("invalid_reference_context")?;
        if token.len()>4096 {return Err("invalid_reference_context");}
        let (version,cipher)=token.split_once('.').ok_or("invalid_reference_context")?;
        let version=version.parse::<u32>().map_err(|_|"invalid_reference_context")?;
        let cipher=URL_SAFE_NO_PAD.decode(cipher).map_err(|_|"invalid_reference_context")?;
        let plaintext=zeroize::Zeroizing::new(state.key_vault.decrypt(&context(principal),version,&cipher).map_err(|_|"invalid_reference_context")?);
        let decoded:Value=serde_json::from_str(&plaintext).map_err(|_|"invalid_reference_context")?;
        if decoded["expires_at_ms"].as_i64().is_none_or(|t|t<=chrono::Utc::now().timestamp_millis()) {return Err("reference_context_expired");}
        let refs=decoded["asset_ids"].as_array().filter(|v|!v.is_empty() && v.len()<=10).ok_or("invalid_reference_context")?;
        for id in refs {
            let id=id.as_str().filter(|s|s.len()<=128).ok_or("invalid_reference_context")?;
            validate_image(state,principal,id)?;
            let value=json!(id);if !ids.contains(&value) {ids.push(value);}
        }
        remaining=rest;
    }
    if !ids.is_empty() {
        let existing=body["image_asset_ids"].as_array().cloned().unwrap_or_default();
        for id in existing {if !ids.contains(&id) {ids.push(id);}}
        if ids.len()>10 {return Err("reference_image_limit");}
        body["image_asset_ids"]=json!(ids);
        // The token is a transport capability, not prompt content for the helper.
        let content=&mut body["messages"][index]["content"];
        match content {
            Value::String(s)=>*s=strip_markers(s),
            Value::Array(parts)=>for part in parts {if let Some(s)=part["text"].as_str() {part["text"]=json!(strip_markers(s));}},
            _=>{},
        }
    }
    if wants_reference(&prompt) && !has_references(body) {
        // Carry only user-supplied images within this submitted conversation.
        let previous=body["messages"].as_array().unwrap()[..index].iter().rev().filter(|m|m["role"]=="user")
            .filter_map(|m|m["content"].as_array()).find_map(|parts| {
                let images:Vec<_>=parts.iter().filter(|p|p["type"]=="image_url").cloned().collect();
                (!images.is_empty()).then_some(images)
            });
        if let Some(images)=previous {
            let content=&mut body["messages"][index]["content"];
            if let Value::String(s)=content {*content=json!([{"type":"text","text":s.clone()}]);}
            content.as_array_mut().ok_or("invalid_reference_context")?.extend(images);
        }
        if !has_references(body) {return Err("reference_image_missing");}
    }
    Ok(())
}
fn strip_markers(s:&str)->String {
    let mut result=String::new();let mut tail=s;
    while let Some((head,rest))=tail.split_once(PREFIX) {
        result.push_str(head);
        if let Some((_,next))=rest.split_once(']') {tail=next;} else {tail=rest;break;}
    }
    result.push_str(tail);result
}
pub(crate) fn caption_reference(state:&StarlinkRouterState,principal:&Principal,images:&[ParsedAssetUpload],body:&Value)->Result<Option<String>,String> {
    let mut ids=body["image_asset_ids"].as_array().cloned().unwrap_or_default();
    if ids.len()+images.len()>10 {return Err("reference_image_limit".into());}
    let mut expires=chrono::Utc::now().timestamp_millis()+30*60*1000;
    for id in &ids {
        expires=expires.min(validate_image(state,principal,id.as_str().ok_or("invalid_reference_context")?)?);
    }
    for image in images {
        let _permit=state.asset_limiter.acquire(&principal.key_id,image.bytes.len()).map_err(|_|"reference_upload_limited")?;
        let stored=crate::assets::write_asset(&state.config.data_dir,principal,ParsedAssetUpload {filename:image.filename.clone(),declared_mime:image.declared_mime.clone(),bytes:image.bytes.clone()}).map_err(|_|"reference_materialization_failed")?;
        let asset=crate::assets::persist_asset(&state.store,principal,&stored).map_err(|_|"reference_materialization_failed")?;
        expires=expires.min(asset.expires_at_ms);ids.push(json!(asset.id));
    }
    if ids.is_empty() {return Ok(None);}
    if ids.len()>10 {return Err("reference_image_limit".into());}
    let sealed=state.key_vault.encrypt(&context(principal),&json!({"asset_ids":ids,"expires_at_ms":expires}).to_string()).map_err(|_|"reference_materialization_failed")?;
    Ok(Some(format!("{PREFIX}{}.{}]",sealed.key_version,URL_SAFE_NO_PAD.encode(sealed.ciphertext))))
}
