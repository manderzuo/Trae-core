use std::{collections::{BTreeMap, HashMap}, io, pin::Pin, sync::Arc, task::{Context, Poll}, time::{Duration, Instant}};

use aiwork_core::{
    BillingQuote, BillingReceiptResult, BillingReservationResult, BeginRequest, BeginRequestInput,
    CoreError, CoreStore, CreditAmount, Principal, RequestResult, RequestState, Settlement,
    UpstreamCreditSnapshot,
};
use axum::{body::{Body, Bytes}, extract::{Extension, Path, Query, State}, http::{HeaderMap, StatusCode}, response::{IntoResponse, Response}, Json};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::Utc;
use futures_core::Stream;
use serde_json::{json, Value};

use crate::state::{StarlinkRouterState, UserVideoJob};
use crate::bridge_client::{BridgeBillingResult, BridgeQuoteResult, BridgeResponse, BridgeStreamingResponse};
use crate::video_billing::{admit_video_request, VideoAdmission};

fn request_id() -> String { format!("core-{}-{:016x}", Utc::now().timestamp_millis(), rand::random::<u64>()) }

fn header_map(headers: &HeaderMap) -> BTreeMap<String, String> {
    headers.iter().filter_map(|(key, value)| value.to_str().ok().map(|value| (key.as_str().to_ascii_lowercase(), value.to_string()))).collect()
}

fn video_forward_headers(headers: &HeaderMap, request_id: &str) -> BTreeMap<String, String> {
    let mut forwarded = header_map(headers);
    if forwarded.get("idempotency-key").is_none_or(|value| value.trim().is_empty()) {
        // The Core request ID is durable and unique even when the client cannot
        // send this header. AI Work requires it before creating a video task.
        forwarded.insert("idempotency-key".into(), request_id.into());
    }
    forwarded
}

fn idempotency(headers: &HeaderMap) -> String {
    headers.get("idempotency-key").and_then(|value| value.to_str().ok()).filter(|value| !value.trim().is_empty()).map(ToString::to_string).unwrap_or_else(request_id)
}

fn authorize_scope(principal: &Principal, scope: &str) -> Result<(), Response> {
    if principal.scopes.contains(scope) || principal.scopes.contains("admin:*") { Ok(()) } else {
        Err((StatusCode::FORBIDDEN, Json(json!({"error": {"type": "permission_error", "code": "insufficient_scope", "message": format!("需要作用域 {scope}")}}))).into_response())
    }
}

const MAX_VISION_DATA_URL_BYTES: usize = 6 * 1024 * 1024;

fn is_seedance_model(model: &str) -> bool { model.trim().eq_ignore_ascii_case("seedance") }

fn request_error(status: StatusCode, error_type: &str, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": {"type": error_type, "message": message.into()}}))).into_response()
}

fn validate_vision_data_urls(body: &Value) -> Result<(), Response> {
    let mut total = 0_usize;
    let Some(messages) = body.get("messages").and_then(Value::as_array) else { return Ok(()); };
    for message in messages {
        let Some(parts) = message.get("content").and_then(Value::as_array) else { continue; };
        for part in parts {
            let Some(url) = part.get("image_url").and_then(Value::as_object).and_then(|image| image.get("url")).and_then(Value::as_str) else { continue; };
            if url.starts_with("data:") {
                total = total.saturating_add(url.len());
                if total > MAX_VISION_DATA_URL_BYTES {
                    return Err(request_error(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large", "图片 data URL 总大小超过限制"));
                }
            }
        }
    }
    Ok(())
}

const MAX_SEEDANCE_ASSIST_PROMPT_BYTES: usize = 12 * 1024;

fn extract_seedance_prompt(body: &Value) -> Result<String, Response> {
    let user_message = body
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| messages.iter().rev().find(|message| message.get("role").and_then(Value::as_str) == Some("user")))
        .ok_or_else(|| request_error(StatusCode::BAD_REQUEST, "seedance_prompt_missing", "Seedance 请求缺少用户提示词"))?;
    let mut chunks = Vec::new();
    match user_message.get("content") {
        Some(Value::String(text)) => chunks.push(text.as_str()),
        Some(Value::Array(parts)) => {
            for part in parts {
                if part.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        chunks.push(text);
                    }
                }
            }
        }
        _ => {}
    }
    let prompt = chunks.join("\n").trim().to_string();
    if prompt.is_empty() {
        return Err(request_error(StatusCode::BAD_REQUEST, "seedance_prompt_missing", "Seedance 请求缺少用户文字提示词"));
    }
    if prompt.len() > MAX_SEEDANCE_ASSIST_PROMPT_BYTES {
        return Err(request_error(StatusCode::PAYLOAD_TOO_LARGE, "seedance_prompt_too_large", "Seedance 提示词超过辅助模型处理限制"));
    }
    Ok(prompt)
}

fn infer_video_parameters_from_prompt(body: &mut Value) {
    let Ok(prompt) = extract_seedance_prompt(body) else { return; };
    let normalized = prompt.to_ascii_lowercase();
    let chars: Vec<char> = normalized.chars().collect();
    let duration = chars.iter().enumerate().find_map(|(start, ch)| {
        if !ch.is_ascii_digit() || (start > 0 && chars[start - 1].is_ascii_digit()) {
            return None;
        }
        let mut end = start;
        while end < chars.len() && chars[end].is_ascii_digit() { end += 1; }
        let value: u64 = chars[start..end].iter().collect::<String>().parse().ok()?;
        while end < chars.len() && chars[end].is_whitespace() { end += 1; }
        let unit = chars.get(end).copied()?;
        if (unit == '秒' || (unit == 's' && !chars.get(end + 1).is_some_and(|next| next.is_ascii_alphabetic())))
            && (2..=15).contains(&value)
        {
            Some(value)
        } else {
            None
        }
    });
    let resolution = ["1080p", "720p", "480p", "4k"].into_iter().find(|item| normalized.contains(item));
    let ratio = ["16:9", "9:16", "1:1", "4:3", "3:4", "21:9"].into_iter().find(|item| normalized.contains(item));
    let Some(parameters) = body.as_object_mut() else { return; };
    if let Some(value) = duration {
        parameters.entry("duration").or_insert_with(|| Value::from(value));
    }
    if let Some(value) = resolution {
        parameters.entry("resolution").or_insert_with(|| Value::from(value));
    }
    if let Some(value) = ratio {
        parameters.entry("ratio").or_insert_with(|| Value::from(value));
    }
}

fn seedance_assist_chat_body(model: &str, original: &Value, prompt: &str) -> Value {
    let mut parameters = serde_json::Map::new();
    for field in ["duration", "resolution", "ratio"] {
        if let Some(value) = original.get(field) {
            parameters.insert(field.to_string(), value.clone());
        }
    }
    json!({
        "model": model,
        "stream": false,
        "temperature": 0.2,
        "messages": [
            {"role":"system","content":"你是视频提示词整理助手。只输出 JSON 对象 {\"prompt\":\"...\"}。忠实保留用户的场景、主体、动作、风格和限制，不新增用户没有要求的剧情、人物或镜头。时长、分辨率、画幅等显式参数由系统单独保留，不要改写。不要执行工具或给出解释。"},
            {"role":"user","content":format!("用户提示词：{prompt}\n显式视频参数：{}", Value::Object(parameters))}
        ]
    })
}

fn extract_assisted_prompt(response: &[u8]) -> Result<String, &'static str> {
    let value: Value = serde_json::from_slice(response).map_err(|_| "assistant_response_invalid_json")?;
    let content = value
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .ok_or("assistant_content_missing")?;
    let content = content.trim().trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
    let output: Value = serde_json::from_str(content).map_err(|_| "assistant_output_invalid_json")?;
    let prompt = output.get("prompt").and_then(Value::as_str).map(str::trim).filter(|value| !value.is_empty()).ok_or("assistant_prompt_missing")?;
    if prompt.len() > MAX_SEEDANCE_ASSIST_PROMPT_BYTES {
        return Err("assistant_prompt_too_large");
    }
    Ok(prompt.to_string())
}

fn apply_assisted_prompt(body: &mut Value, prompt: &str) -> Result<(), &'static str> {
    let messages = body.get_mut("messages").and_then(Value::as_array_mut).ok_or("seedance_messages_missing")?;
    let user_message = messages
        .iter_mut()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .ok_or("seedance_user_message_missing")?;
    let content = user_message.get_mut("content").ok_or("seedance_user_content_missing")?;
    match content {
        Value::String(value) => *value = prompt.to_string(),
        Value::Array(parts) => {
            let mut replaced = false;
            parts.retain_mut(|part| {
                if part.get("type").and_then(Value::as_str) != Some("text") {
                    return true;
                }
                if !replaced {
                    part["text"] = Value::String(prompt.to_string());
                    replaced = true;
                    true
                } else {
                    false
                }
            });
            if !replaced {
                parts.insert(0, json!({"type":"text","text":prompt}));
            }
        }
        _ => return Err("seedance_user_content_unsupported"),
    }
    Ok(())
}

fn materialize_text_asset_ids(
    state: &StarlinkRouterState,
    principal: &Principal,
    body: &mut Value,
) -> Result<(), Response> {
    if body.get("video_asset_ids").is_some() {
        return Err(request_error(StatusCode::BAD_REQUEST, "invalid_request_error", "文字模型不支持 video_asset_ids"));
    }
    let Some(value) = body.get("image_asset_ids") else { return Ok(()); };
    let Some(ids) = value.as_array() else {
        return Err(request_error(StatusCode::BAD_REQUEST, "invalid_request_error", "image_asset_ids 必须是字符串数组"));
    };
    let mut image_parts = Vec::with_capacity(ids.len());
    let mut total = 0_usize;
    for value in ids {
        let Some(id) = value.as_str().map(str::trim).filter(|value| !value.is_empty()) else {
            return Err(request_error(StatusCode::BAD_REQUEST, "invalid_request_error", "image_asset_ids 只能包含非空字符串"));
        };
        let asset = crate::assets::read_owned(&state.store, &state.config.data_dir, principal, id)
            .map_err(crate::assets::response)?;
        let encoded = STANDARD.encode(asset.bytes);
        let url = format!("data:{};base64,{}", asset.record.mime_type, encoded);
        total = total.saturating_add(url.len());
        if total > MAX_VISION_DATA_URL_BYTES {
            return Err(request_error(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large", "图片 data URL 总大小超过限制"));
        }
        image_parts.push(json!({"type":"image_url","image_url":{"url":url}}));
    }
    if image_parts.is_empty() {
        body.as_object_mut().map(|object| { object.remove("image_asset_ids"); });
        return Ok(());
    }
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return Err(request_error(StatusCode::BAD_REQUEST, "invalid_request_error", "使用 image_asset_ids 时必须提供 messages"));
    };
    let Some(message) = messages.iter_mut().rev().find(|message| message.get("role").and_then(Value::as_str).map(|role| role.eq_ignore_ascii_case("user")).unwrap_or(false)) else {
        return Err(request_error(StatusCode::BAD_REQUEST, "invalid_request_error", "使用 image_asset_ids 时必须有 user 消息"));
    };
    let Some(content) = message.get_mut("content") else {
        return Err(request_error(StatusCode::BAD_REQUEST, "invalid_request_error", "user 消息缺少 content"));
    };
    match content {
        Value::String(text) => {
            let mut parts = vec![json!({"type":"text","text":text.clone()})];
            parts.extend(image_parts);
            *content = Value::Array(parts);
        }
        Value::Array(parts) => parts.extend(image_parts),
        _ => return Err(request_error(StatusCode::BAD_REQUEST, "invalid_request_error", "user content 必须是字符串或数组")),
    }
    if let Some(object) = body.as_object_mut() { object.remove("image_asset_ids"); }
    Ok(())
}

#[derive(Default)]
struct BridgeAssetMap {
    image: Vec<String>,
    video: Vec<String>,
}

struct PendingAsset {
    core_id: String,
    filename: String,
    mime_type: String,
    bytes: Vec<u8>,
}

async fn materialize_bridge_assets(
    state: &StarlinkRouterState,
    principal: &Principal,
    body: &mut Value,
    request_id: &str,
) -> Result<BridgeAssetMap, Response> {
    let mut requested = Vec::<(String, String)>::new();
    for field in ["image_asset_ids", "video_asset_ids"] {
        let Some(value) = body.get(field) else { continue; };
        let Some(ids) = value.as_array() else {
            return Err((StatusCode::BAD_REQUEST, Json(json!({"error": {"type": "invalid_request_error", "message": format!("{field} 必须是字符串数组")}}))).into_response());
        };
        for id in ids {
            let Some(id) = id.as_str().map(str::trim).filter(|value| !value.is_empty()) else {
                return Err((StatusCode::BAD_REQUEST, Json(json!({"error": {"type": "invalid_request_error", "message": format!("{field} 只能包含非空字符串")}}))).into_response());
            };
            requested.push((field.to_string(), id.to_string()));
        }
    }
    if requested.is_empty() { return Ok(BridgeAssetMap::default()); }

    let mut pending = Vec::<PendingAsset>::new();
    let mut seen = HashMap::<String, usize>::new();
    for (_, id) in &requested {
        if seen.contains_key(id) { continue; }
        let asset = crate::assets::read_owned(&state.store, &state.config.data_dir, principal, id)
            .map_err(crate::assets::response)?;
        seen.insert(id.clone(), pending.len());
        pending.push(PendingAsset { core_id: id.clone(), filename: asset.record.filename, mime_type: asset.record.mime_type, bytes: asset.bytes });
    }

    let mut bridge_ids = HashMap::<String, String>::new();
    for asset in pending {
        let bridge_id = state.bridge.lock().unwrap().upload_asset(&asset.filename, &asset.mime_type, &asset.bytes, request_id)
            .map_err(|error| (StatusCode::BAD_GATEWAY, Json(json!({"error": {"type": "bridge_error", "message": error, "request_id": request_id}}))).into_response())?;
        bridge_ids.insert(asset.core_id, bridge_id);
    }

    let mut result = BridgeAssetMap::default();
    for (field, id) in requested {
        let bridge_id = bridge_ids.get(&id).expect("bridge asset map must contain every validated asset").clone();
        if field == "image_asset_ids" { result.image.push(bridge_id.clone()); } else { result.video.push(bridge_id.clone()); }
        let values = body.get_mut(&field).and_then(Value::as_array_mut).expect("asset id field was validated as array");
        for value in values { if value.as_str() == Some(id.as_str()) { *value = Value::String(bridge_id.clone()); } }
    }
    Ok(result)
}

pub async fn assets_upload(
    State(state): State<Arc<StarlinkRouterState>>,
    Extension(principal): Extension<Principal>,
    body: Bytes,
) -> Response {
    if let Err(response) = authorize_scope(&principal, "assets:write") { return response; }
    let parsed = match crate::assets::parse_upload(&body) {
        Ok(parsed) => parsed,
        Err(error) => return crate::assets::response(error),
    };
    let _permit = match state.asset_limiter.acquire(&principal.key_id, body.len()) {
        Ok(permit) => permit,
        Err(error) => return crate::assets::response(error),
    };
    let stored = match crate::assets::write_asset(&state.config.data_dir, &principal, parsed) {
        Ok(stored) => stored,
        Err(error) => return crate::assets::response(error),
    };
    let record = match crate::assets::persist_asset(&state.store, &principal, &stored) {
        Ok(record) => record,
        Err(error) => return crate::assets::response(error),
    };
    Json(json!({
        "object": "asset",
        "id": record.id,
        "filename": record.filename,
        "mime_type": record.mime_type,
        "bytes": record.size,
        "sha256": record.sha256,
        "created_at": record.created_at_ms / 1000,
        "expires_at": record.expires_at_ms / 1000,
        "content_url": crate::assets::content_url(&state.config, &record.id, &stored.public_token),
    })).into_response()
}

#[derive(serde::Deserialize)]
pub struct AssetContentQuery {
    token: Option<String>,
}

pub async fn assets_content(
    State(state): State<Arc<StarlinkRouterState>>,
    Path(asset_id): Path<String>,
    Query(query): Query<AssetContentQuery>,
) -> Response {
    let Some(token) = query.token else { return crate::assets::response(crate::assets::AssetError::NotFound); };
    let asset = match crate::assets::read_public(&state.store, &state.config.data_dir, &asset_id, &token) {
        Ok(asset) => asset,
        Err(error) => return crate::assets::response(error),
    };
    let mut response = Response::new(Body::from(asset.bytes));
    response.headers_mut().insert("content-type", asset.record.mime_type.parse().unwrap_or_else(|_| "application/octet-stream".parse().unwrap()));
    response.headers_mut().insert("cache-control", "no-store".parse().unwrap());
    response.headers_mut().insert("x-content-type-options", "nosniff".parse().unwrap());
    response.headers_mut().insert("referrer-policy", "no-referrer".parse().unwrap());
    response
}

fn begin_billed_request(
    store: &CoreStore,
    principal: &Principal,
    endpoint: &str,
    model: &str,
    idempotency_key: String,
    body: &Value,
) -> Result<BeginRequest, Response> {
    store
        .begin_billed_request(BeginRequestInput {
            user_id: principal.user_id.clone(),
            api_key_id: principal.key_id.clone(),
            protocol: "openai".into(),
            endpoint: endpoint.into(),
            model: model.into(),
            idempotency_key,
            body: body.clone(),
        })
        .map_err(|error| request_error(StatusCode::INTERNAL_SERVER_ERROR, "core_error", error.to_string()))
}

fn begin_implicit_billed_video_request(
    store: &CoreStore,
    principal: &Principal,
    model: &str,
    body: &Value,
) -> Result<BeginRequest, Response> {
    store
        .begin_implicit_billed_video_request(BeginRequestInput {
            user_id: principal.user_id.clone(),
            api_key_id: principal.key_id.clone(),
            protocol: "openai".into(),
            endpoint: "videos".into(),
            model: model.into(),
            idempotency_key: String::new(),
            body: body.clone(),
        })
        .map_err(|error| request_error(StatusCode::INTERNAL_SERVER_ERROR, "core_error", error.to_string()))
}

fn quote_unavailable(request_id: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": {
            "type":"billing_error",
            "code":"quote_unavailable",
            "message":"AI Work 未提供这次请求的可信积分上限，已阻止付费请求",
            "request_id":request_id
        }})),
    )
        .into_response()
}

fn upstream_credits_unavailable(request_id: &str, code: &str, message: impl Into<String>) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": {
            "type":"billing_error",
            "code":code,
            "message":message.into(),
            "request_id":request_id
        }})),
    )
        .into_response()
}

fn reconcile_required(request_id: &str) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        Json(json!({"error": {
            "type":"billing_error",
            "code":"reconcile_required",
            "message":"请求结果或积分回执尚未确认；额度仍被保留，请勿重复提交生成请求",
            "request_id":request_id,
            "reconcile_required":true
        }})),
    )
        .into_response()
}

fn format_microcredits(value: i64) -> String {
    format!("{}.{:06}", value / 1_000_000, value.abs() % 1_000_000)
}

fn quote_and_reserve(
    state: &StarlinkRouterState,
    request_id: &str,
    endpoint: &str,
    model: &str,
) -> Result<(String, String), Response> {
    let upstream_snapshot = state
        .bridge
        .lock()
        .unwrap()
        .upstream_credit_snapshot()
        .map_err(|_| {
            upstream_credits_unavailable(
                request_id,
                "upstream_credits_unavailable",
                "AI Work 统一积分余额不可用或已过期，已阻止付费请求",
            )
        })?;
    let fingerprint = state
        .store
        .request_fingerprint_for_billing(request_id)
        .map_err(|error| request_error(StatusCode::INTERNAL_SERVER_ERROR, "core_error", error.to_string()))?;
    let quote = state
        .bridge
        .lock()
        .unwrap()
        .quote(request_id, endpoint, model, &fingerprint);
    let quote = match quote {
        Ok(BridgeQuoteResult::Quoted(quote)) => quote,
        Ok(BridgeQuoteResult::Unavailable { .. }) | Err(_) => return Err(quote_unavailable(request_id)),
    };
    let quote_id = quote.quote_id.clone();
    match state
        .store
        .reserve_credit_quote_with_upstream_snapshot(quote, upstream_snapshot)
    {
        Ok(BillingReservationResult::Created { reservation, .. }) => {
            Ok((quote_id, reservation.id))
        }
        Ok(BillingReservationResult::Existing { .. }) => Err((
            StatusCode::CONFLICT,
            Json(json!({"error": {
                "type":"billing_error",
                "code":"request_already_reserved",
                "message":"该请求已存在积分预占，已阻止重复提交",
                "request_id":request_id
            }})),
        )
            .into_response()),
        Ok(BillingReservationResult::Insufficient { available, required }) => Err((
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error": {
                "type":"insufficient_quota",
                "code":"insufficient_quota",
                "message":format!("积分不足：可用 {}，本次上限 {}", format_microcredits(available), format_microcredits(required)),
                "request_id":request_id
            }})),
        )
            .into_response()),
        Err(error) => {
            let (status, code, message) = match &error {
                CoreError::KeyConcurrencyExceeded { .. } => (
                    StatusCode::TOO_MANY_REQUESTS,
                    "concurrency_limit",
                    error.to_string(),
                ),
                CoreError::KeyQuotaNotConfigured { .. } => (
                    StatusCode::TOO_MANY_REQUESTS,
                    "insufficient_quota",
                    "该 API Key 尚未分配积分额度，请联系管理员配置后再试".into(),
                ),
                CoreError::QuotaInsufficient { available, required } => (
                    StatusCode::TOO_MANY_REQUESTS,
                    "insufficient_quota",
                    format!("积分不足：可用 {}，本次上限 {}", format_microcredits(*available), format_microcredits(*required)),
                ),
                CoreError::BillingQuoteExpired { .. }
                | CoreError::BillingQuoteMismatch { .. }
                | CoreError::BillingQuoteConflict { .. } => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "quote_unavailable",
                    "AI Work 报价已过期或与请求不匹配".into(),
                ),
                CoreError::UpstreamCreditsUnavailable { .. } => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "upstream_credits_unavailable",
                    "AI Work 统一积分余额不可用或已过期，已阻止付费请求".into(),
                ),
                CoreError::UpstreamCommitmentsExceedBalance { upstream_total, committed } => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "upstream_commitments_exceed_balance",
                    format!(
                        "AI Work 统一积分余额 {} 小于所有 Key 尚未使用的承诺额度 {}，已暂停付费请求",
                        format_microcredits(*upstream_total),
                        format_microcredits(*committed)
                    ),
                ),
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "core_error",
                    error.to_string(),
                ),
            };
            Err((status, Json(json!({"error": {"type":code,"code":code,"message":message,"request_id":request_id}}))).into_response())
        }
    }
}

fn reserve_quote_with_snapshot(
    state: &StarlinkRouterState,
    request_id: &str,
    quote: BillingQuote,
    upstream_snapshot: UpstreamCreditSnapshot,
) -> Result<(String, String), Response> {
    let quote_id = quote.quote_id.clone();
    match state.store.reserve_credit_quote_with_upstream_snapshot(quote, upstream_snapshot) {
        Ok(BillingReservationResult::Created { reservation, .. }) => Ok((quote_id, reservation.id)),
        Ok(BillingReservationResult::Existing { .. }) => Err(request_error(
            StatusCode::CONFLICT,
            "request_already_reserved",
            format!("该请求已存在积分预占，已阻止重复提交。request_id={request_id}"),
        )),
        Ok(BillingReservationResult::Insufficient { available, required }) => Err(request_error(
            StatusCode::TOO_MANY_REQUESTS,
            "insufficient_quota",
            format!("积分不足：可用 {}，本次上限 {}。request_id={request_id}", format_microcredits(available), format_microcredits(required)),
        )),
        Err(error) => {
            let (status, code) = match error {
                CoreError::KeyConcurrencyExceeded { .. } => (StatusCode::TOO_MANY_REQUESTS, "concurrency_limit"),
                CoreError::KeyQuotaNotConfigured { .. } | CoreError::QuotaInsufficient { .. } => (StatusCode::TOO_MANY_REQUESTS, "insufficient_quota"),
                error @ CoreError::UpstreamCreditsUnavailable { .. } => {
                    return Err(upstream_credits_unavailable(request_id, "upstream_credits_unavailable", error.to_string()));
                }
                error @ CoreError::UpstreamCommitmentsExceedBalance { .. } => {
                    return Err(upstream_credits_unavailable(request_id, "upstream_commitments_exceed_balance", error.to_string()));
                }
                _ => (StatusCode::INTERNAL_SERVER_ERROR, "one_shot_reservation_failed"),
            };
            Err(request_error(status, code, format!("一次性积分预占失败，未发送视频请求。request_id={request_id}")))
        }
    }
}

fn quote_and_reserve_with_one_shot_fallback(
    state: &StarlinkRouterState,
    principal: &Principal,
    request_id: &str,
    endpoint: &str,
    model: &str,
    allow_unquoted_one_shot: bool,
) -> Result<(String, String, bool), Response> {
    let upstream_snapshot = state.bridge.lock().unwrap().upstream_credit_snapshot().map_err(|_| {
        upstream_credits_unavailable(
            request_id,
            "upstream_credits_unavailable",
            "AI Work 统一积分余额不可用或已过期，已阻止付费请求",
        )
    })?;
    let fingerprint = state.store.request_fingerprint_for_billing(request_id).map_err(|error| {
        request_error(StatusCode::INTERNAL_SERVER_ERROR, "core_error", error.to_string())
    })?;
    let quote_result = state.bridge.lock().unwrap().quote(request_id, endpoint, model, &fingerprint);
    let (quote, one_shot_test) = match quote_result {
        Ok(BridgeQuoteResult::Quoted(quote)) => (quote, false),
        Ok(BridgeQuoteResult::Unavailable { error_code })
            if allow_unquoted_one_shot && error_code == "quote_unavailable" =>
        {
            let balance = state.store.key_quota_balance_for_principal(principal, "credits").map_err(|error| {
                request_error(StatusCode::TOO_MANY_REQUESTS, "insufficient_quota", error.to_string())
            })?;
            let Some(max_credits) = CreditAmount::try_from_microcredits(balance.available) else {
                return Err(request_error(StatusCode::TOO_MANY_REQUESTS, "insufficient_quota", "该 API Key 没有可用积分，未发送付费请求"));
            };
            if max_credits.as_microcredits() == 0 {
                return Err(request_error(StatusCode::TOO_MANY_REQUESTS, "insufficient_quota", "该 API Key 没有可用积分，未发送付费请求"));
            }
            let expires_at_ms = Utc::now().timestamp_millis().saturating_add(30 * 24 * 60 * 60 * 1_000);
            (
                BillingQuote {
                    request_id: request_id.into(),
                    quote_id: format!("authorized-one-shot-test-{request_id}"),
                    request_fingerprint: fingerprint,
                    endpoint: endpoint.into(),
                    model: model.into(),
                    max_credits,
                    unit: "credits".into(),
                    expires_at_ms,
                    source_ref: "core-approved-one-shot-unquoted-test".into(),
                },
                true,
            )
        }
        Ok(BridgeQuoteResult::Unavailable { .. }) | Err(_) => return Err(quote_unavailable(request_id)),
    };
    reserve_quote_with_snapshot(state, request_id, quote, upstream_snapshot)
        .map(|(quote_id, reservation_id)| (quote_id, reservation_id, one_shot_test))
}

fn quote_and_reserve_video(
    state: &StarlinkRouterState,
    principal: &Principal,
    request_id: &str,
    model: &str,
    allow_unquoted_one_shot: bool,
) -> Result<(String, String, bool), Response> {
    quote_and_reserve_with_one_shot_fallback(
        state,
        principal,
        request_id,
        "videos",
        model,
        allow_unquoted_one_shot,
    )
}

fn transition_request_to(
    state: &StarlinkRouterState,
    request_id: &str,
    next: RequestState,
    error_code: Option<&str>,
) -> Result<(), String> {
    let current = state.store.request_state(request_id).map_err(|error| error.to_string())?;
    if current == next {
        return Ok(());
    }
    state
        .store
        .transition_request(
            request_id,
            current,
            next,
            error_code.map(|error_code| RequestResult {
                status: None,
                error_code: Some(error_code.into()),
            }),
        )
        .map_err(|error| error.to_string())
}

fn fail_unreserved_pre_dispatch_request(
    state: &StarlinkRouterState,
    request_id: &str,
    status: u16,
    error_code: &str,
) -> Result<(), String> {
    if state
        .store
        .reservation_for_request(request_id)
        .map_err(|error| error.to_string())?
        .is_some()
    {
        return Err(format!("request {request_id} already has a reservation; refusing to release it as an unreserved failure"));
    }

    let mut current = state.store.request_state(request_id).map_err(|error| error.to_string())?;
    if current == RequestState::Received {
        state
            .store
            .transition_request(request_id, RequestState::Received, RequestState::Validating, None)
            .map_err(|error| error.to_string())?;
        current = RequestState::Validating;
    }
    if current == RequestState::Failed {
        return Ok(());
    }
    if current != RequestState::Validating {
        return Err(format!("request {request_id} reached {current:?} before dispatch failure cleanup"));
    }
    state
        .store
        .transition_request(
            request_id,
            RequestState::Validating,
            RequestState::Failed,
            Some(RequestResult {
                status: Some(i64::from(status)),
                error_code: Some(error_code.into()),
            }),
        )
        .map_err(|error| error.to_string())
}

fn mark_request_unknown(state: &StarlinkRouterState, request_id: &str, error_code: &str) {
    if let Ok(current) = state.store.request_state(request_id) {
        if current != RequestState::Unknown {
            let _ = state.store.transition_request(
                request_id,
                current,
                RequestState::Unknown,
                Some(RequestResult {
                    status: None,
                    error_code: Some(error_code.into()),
                }),
            );
        }
    }
}

fn prepare_for_final_receipt(state: &StarlinkRouterState, request_id: &str) -> Result<(), String> {
    let mut current = state.store.request_state(request_id).map_err(|error| error.to_string())?;
    if current == RequestState::Reserved {
        state.store.transition_request(request_id, current, RequestState::Queued, None)
            .map_err(|error| error.to_string())?;
        current = RequestState::Queued;
    }
    if current == RequestState::Queued {
        state.store.transition_request(request_id, current, RequestState::Dispatched, None)
            .map_err(|error| error.to_string())?;
        current = RequestState::Dispatched;
    }
    if current == RequestState::Dispatched {
        state.store.transition_request(request_id, current, RequestState::Completing, None)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn mark_request_dispatched(state: &StarlinkRouterState, request_id: &str) -> Result<(), String> {
    let mut current = state.store.request_state(request_id).map_err(|error| error.to_string())?;
    if current == RequestState::Reserved {
        state.store.transition_request(request_id, current, RequestState::Queued, None)
            .map_err(|error| error.to_string())?;
        current = RequestState::Queued;
    }
    if current == RequestState::Queued {
        state.store.transition_request(request_id, current, RequestState::Dispatched, None)
            .map_err(|error| error.to_string())?;
        current = RequestState::Dispatched;
    }
    if current == RequestState::Dispatched { Ok(()) } else { Err(format!("请求状态不允许上游发送: {current:?}")) }
}

pub(crate) fn reconcile_billing_request_once(
    state: &StarlinkRouterState,
    request_id: &str,
) -> Result<BillingReceiptResult, String> {
    let result = state.bridge.lock().unwrap().billing(request_id);
    let result = match result {
        Ok(BridgeBillingResult::Unresolved)
            if state.store.is_seedance_assist_child(request_id).map_err(|error| error.to_string())? =>
        {
            state.bridge.lock().unwrap().finalize_chat_billing(request_id)
        }
        other => other,
    };
    match result {
        Ok(BridgeBillingResult::Final(receipt)) => {
            prepare_for_final_receipt(state, request_id)?;
            state.store.apply_credit_receipt(receipt).map_err(|error| error.to_string())
        }
        Ok(BridgeBillingResult::Unresolved) => {
            mark_request_unknown(state, request_id, "billing_receipt_unresolved");
            Ok(BillingReceiptResult::Pending)
        }
        Err(error) => {
            mark_request_unknown(state, request_id, "billing_query_failed");
            Err(error)
        }
    }
}

/// Restart recovery is query-only: it discovers durable held billing records
/// and asks AI Work for their receipts. Requests with a live video job are
/// reconciled by the video status worker instead, avoiding duplicate polling.
pub fn reconcile_pending_billing_requests_once(state: &StarlinkRouterState) -> usize {
    let pending = match state.store.recoverable_billing_requests(100) {
        Ok(requests) => requests,
        Err(_) => return 0,
    };
    let video_job_requests = state
        .jobs
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .values()
        .filter(|job| job.reservation_id.is_some() && job.billing_state != "settled" && job.billing_state != "released")
        .map(|job| job.request_id.clone())
        .collect::<std::collections::HashSet<_>>();
    let mut queried = 0;
    for request in pending {
        if video_job_requests.contains(&request.request_id) {
            continue;
        }
        queried += 1;
        let _ = reconcile_billing_request_once(state, &request.request_id);
    }
    queried
}

fn receipt_is_settled(result: &BillingReceiptResult) -> bool {
    matches!(result, BillingReceiptResult::Settled { .. } | BillingReceiptResult::Duplicate)
}

fn existing_request_error(state: &StarlinkRouterState, request_id: &str) -> Response {
    if state.store.reservation_for_request(request_id).ok().flatten().is_some() {
        match reconcile_billing_request_once(state, request_id) {
            Ok(result) if receipt_is_settled(&result) => {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({"error": {
                        "type":"billing_error",
                        "code":"request_already_processed",
                        "message":"该请求已处理，无法重放原结果；未重复提交生成",
                        "request_id":request_id
                    }})),
                )
                    .into_response();
            }
            _ => return reconcile_required(request_id),
        }
    }
    quote_unavailable(request_id)
}

fn replay_existing_video_request(
    state: Arc<StarlinkRouterState>,
    principal: Principal,
    request_id: &str,
    stream: bool,
) -> Response {
    let job = state.jobs.lock().unwrap_or_else(|error| error.into_inner()).values()
        .find(|job| job.request_id == request_id && job.user_id == principal.user_id && job.api_key_id == principal.key_id)
        .cloned();
    if stream {
        if let Some(job) = job.as_ref() {
            return stream_seedance_video_response(state, principal, job.id.clone());
        }
    }
    if let Some(job) = job.filter(|job| !job.reconcile_required && job.upstream_id.is_some()) {
        return Json(json!({"task": {"id": job.id, "status": job.status}, "core_replay": true})).into_response();
    }
    existing_request_error(&state, request_id)
}

fn require_video_admission(
    state: &StarlinkRouterState,
    principal: &Principal,
    model: &str,
    body: &Value,
) -> Result<VideoAdmission, Response> {
    match admit_video_request(&state.store, principal, model, body) {
        Ok(admission @ (VideoAdmission::Active | VideoAdmission::DiagnosticClaimed)) => Ok(admission),
        Ok(VideoAdmission::Paused) => Err(request_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "video_billing_paused",
            "视频计费闸门当前处于暂停状态，尚未取得可核验的单任务积分回执",
        )),
        Err(error) => Err(request_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "core_error",
            error.to_string(),
        )),
    }
}

pub async fn models(State(state): State<Arc<StarlinkRouterState>>, headers: HeaderMap, Extension(_principal): Extension<Principal>) -> Response {
    let request_id = request_id();
    match state.bridge.lock().unwrap().forward("GET", "/v1/models", &[], &header_map(&headers), &request_id) {
        Ok(response) => proxy(response.status, response.headers, response.body),
        Err(error) => (StatusCode::BAD_GATEWAY, Json(json!({"error": {"type": "bridge_error", "message": error}}))).into_response(),
    }
}

fn release_before_dispatch(
    state: &StarlinkRouterState,
    principal: &Principal,
    reservation_id: &str,
    status: u16,
    error_code: &str,
) {
    let _ = state.store.settle_request(
        principal,
        reservation_id,
        Settlement::Release,
        RequestState::Failed,
        Some(RequestResult {
            status: Some(i64::from(status)),
            error_code: Some(error_code.into()),
        }),
    );
}

fn settle_chat_response(state: &StarlinkRouterState, request_id: &str) -> bool {
    if prepare_for_final_receipt(state, request_id).is_err() {
        mark_request_unknown(state, request_id, "request_state_transition_failed");
        return false;
    }
    matches!(
        reconcile_billing_request_once(state, request_id),
        Ok(result) if receipt_is_settled(&result)
    )
}

fn run_seedance_assist(
    state: &StarlinkRouterState,
    principal: &Principal,
    parent_request_id: &str,
    original: &Value,
    allow_unquoted_one_shot: bool,
) -> Result<String, Response> {
    let prompt = extract_seedance_prompt(original)?;
    let model = if state.config.default_model.trim().is_empty() || is_seedance_model(&state.config.default_model) {
        "deepseek-v4-flash"
    } else {
        state.config.default_model.trim()
    };
    let assist_body = seedance_assist_chat_body(model, original, &prompt);
    let assist_idempotency = format!("{parent_request_id}:seedance-assist-v1");
    let request = match begin_billed_request(
        &state.store,
        principal,
        "chat",
        model,
        assist_idempotency,
        &assist_body,
    ) {
        Ok(BeginRequest::Created(request)) => request,
        Ok(BeginRequest::Existing(_)) => return Err(request_error(
            StatusCode::CONFLICT,
            "seedance_assist_already_processed",
            "该请求的提示词辅助步骤已处理，已阻止重复调用",
        )),
        Ok(BeginRequest::Conflict) => return Err(request_error(
            StatusCode::CONFLICT,
            "seedance_assist_idempotency_conflict",
            "提示词辅助请求与已有请求冲突",
        )),
        Err(response) => return Err(response),
    };
    let child_request_id = request.id.clone();
    if let Err(error) = state.store.link_seedance_assist_request(parent_request_id, &child_request_id) {
        if let Err(cleanup_error) = fail_unreserved_pre_dispatch_request(
            state,
            &child_request_id,
            StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
            "request_relation_failed",
        ) {
            return Err(request_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "request_cleanup_failed",
                format!("关联 DeepSeek 辅助请求失败，且子请求状态清理失败：{cleanup_error}"),
            ));
        }
        return Err(request_error(StatusCode::INTERNAL_SERVER_ERROR, "request_relation_failed", error.to_string()));
    }
    let quote_result = if allow_unquoted_one_shot {
        quote_and_reserve_with_one_shot_fallback(
            state,
            principal,
            &child_request_id,
            "chat",
            model,
            true,
        )
    } else {
        quote_and_reserve(state, &child_request_id, "chat", model)
            .map(|(quote_id, reservation_id)| (quote_id, reservation_id, false))
    };
    let (quote_id, reservation_id, _one_shot_test) = match quote_result {
        Ok(reservation) => reservation,
        Err(response) => {
            let status = response.status().as_u16();
            if let Err(error) = fail_unreserved_pre_dispatch_request(
                state,
                &child_request_id,
                status,
                "seedance_assist_quote_failed",
            ) {
                return Err(request_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "request_cleanup_failed",
                    format!("DeepSeek 辅助报价失败，且子请求状态清理失败：{error}"),
                ));
            }
            return Err(response);
        }
    };
    if let Err(error) = mark_request_dispatched(state, &child_request_id) {
        release_before_dispatch(state, principal, &reservation_id, 500, "request_state_transition_failed");
        return Err(request_error(StatusCode::INTERNAL_SERVER_ERROR, "core_error", error));
    }
    let body = serde_json::to_vec(&assist_body).unwrap_or_default();
    let headers = BTreeMap::from([("content-type".into(), "application/json".into())]);
    let upstream = state.bridge.lock().unwrap_or_else(|error| error.into_inner()).forward_billed_for_key(
        "POST",
        "/v1/chat/completions",
        &body,
        &headers,
        &child_request_id,
        &quote_id,
        &principal.key_id,
    );
    let response = match upstream {
        Ok(response) => response,
        Err(error) => {
            mark_request_unknown(state, &child_request_id, "seedance_assist_result_unknown");
            let settled = matches!(
                reconcile_billing_request_once(state, &child_request_id),
                Ok(result) if receipt_is_settled(&result)
            );
            return if settled {
                Err(request_error(StatusCode::BAD_GATEWAY, "seedance_assist_result_unavailable", format!("DeepSeek 辅助请求结果不可用；request_id={child_request_id}。{error}")))
            } else {
                Err(reconcile_required(&child_request_id))
            };
        }
    };
    if !(200..300).contains(&response.status) {
        if settle_chat_response(state, &child_request_id) {
            return Err(request_error(StatusCode::BAD_GATEWAY, "seedance_assist_upstream_error", format!("DeepSeek 辅助请求返回 HTTP {}；request_id={child_request_id}", response.status)));
        }
        return Err(reconcile_required(&child_request_id));
    }
    if !settle_chat_response(state, &child_request_id) {
        return Err(reconcile_required(&child_request_id));
    }
    extract_assisted_prompt(&response.body).map_err(|code| {
        request_error(
            StatusCode::BAD_GATEWAY,
            "seedance_assist_invalid_response",
            format!("DeepSeek 辅助结果无法解析（{code}）；request_id={child_request_id}"),
        )
    })
}

struct BridgeBodyStream(tokio::sync::mpsc::Receiver<Result<Bytes, io::Error>>);

impl Stream for BridgeBodyStream {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.0.poll_recv(context)
    }
}

fn stream_chat_response(
    state: Arc<StarlinkRouterState>,
    request_id: String,
    upstream: BridgeStreamingResponse,
) -> Response {
    const MAX_STREAM_BYTES: u64 = 64 * 1024 * 1024;
    const DONE_MARKER: &[u8] = b"data: [DONE]";

    let (sender, receiver) = tokio::sync::mpsc::channel(8);
    let body = Body::from_stream(BridgeBodyStream(receiver));
    let mut response = Response::new(body);
    *response.status_mut() = StatusCode::from_u16(upstream.status).unwrap_or(StatusCode::BAD_GATEWAY);
    if let Some(content_type) = upstream.headers.get("content-type") {
        if let Ok(value) = content_type.parse() {
            response.headers_mut().insert("content-type", value);
        }
    }
    response.headers_mut().insert("cache-control", "no-cache".parse().unwrap());
    response.headers_mut().insert("x-accel-buffering", "no".parse().unwrap());

    tokio::task::spawn_blocking(move || {
        let mut reader = upstream.body;
        let mut buffer = [0_u8; 8192];
        let mut pending = Vec::new();
        let mut terminal_seen = false;
        let mut total_bytes = 0_u64;
        let mut over_limit = false;
        let mut downstream_connected = true;
        let mut read_error = None;

        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    total_bytes = total_bytes.saturating_add(count as u64);
                    if total_bytes > MAX_STREAM_BYTES {
                        over_limit = true;
                        continue;
                    }
                    pending.extend_from_slice(&buffer[..count]);
                    if !terminal_seen {
                        if let Some(index) = pending.windows(DONE_MARKER.len()).position(|window| window == DONE_MARKER) {
                            send_stream_bytes(&sender, &mut downstream_connected, &pending[..index]);
                            pending.drain(..index);
                            terminal_seen = true;
                        } else if pending.len() > DONE_MARKER.len() - 1 {
                            let send_len = pending.len() - (DONE_MARKER.len() - 1);
                            send_stream_bytes(&sender, &mut downstream_connected, &pending[..send_len]);
                            pending.drain(..send_len);
                        }
                    }
                }
                Err(error) => {
                    read_error = Some(error.to_string());
                    break;
                }
            }
        }

        let receipt_confirmed = settle_chat_response(&state, &request_id);
        if downstream_connected {
            if receipt_confirmed && !over_limit && read_error.is_none() {
                send_stream_bytes(&sender, &mut downstream_connected, &pending);
            } else {
                if !terminal_seen && receipt_confirmed {
                    send_stream_bytes(&sender, &mut downstream_connected, &pending);
                }
                let message = if over_limit {
                    "上游流式响应超过 64 MiB 限制，结果可能不完整".to_string()
                } else if let Some(error) = read_error {
                    format!("读取上游流式响应中断：{error}")
                } else {
                    "本次生成的最终积分回执尚未确认，额度仍被保留，请勿重复提交".to_string()
                };
                let event = format!("data: {}\n\n", json!({
                    "error": {
                        "type": "billing_error",
                        "code": "reconcile_required",
                        "message": message,
                        "request_id": request_id
                    }
                }));
                send_stream_bytes(&sender, &mut downstream_connected, event.as_bytes());
            }
        }
    });
    response
}

fn send_stream_bytes(
    sender: &tokio::sync::mpsc::Sender<Result<Bytes, io::Error>>,
    downstream_connected: &mut bool,
    bytes: &[u8],
) {
    if *downstream_connected && !bytes.is_empty()
        && sender.blocking_send(Ok(Bytes::copy_from_slice(bytes))).is_err()
    {
        *downstream_connected = false;
    }
}

fn stream_seedance_video_response(
    state: Arc<StarlinkRouterState>,
    principal: Principal,
    job_id: String,
) -> Response {
    const MAX_OBSERVERS: usize = 128;
    const MAX_WAIT: Duration = Duration::from_secs(14 * 60);
    const POLL_INTERVAL: Duration = Duration::from_secs(2);
    const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

    let job = match state.jobs.lock().unwrap_or_else(|error| error.into_inner()).get(&job_id).cloned() {
        Some(job) if job.user_id == principal.user_id && job.api_key_id == principal.key_id => job,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    {
        let mut observers = state.video_stream_observers.lock().unwrap_or_else(|error| error.into_inner());
        if observers.contains(&job_id) {
            return request_error(StatusCode::CONFLICT, "stream_already_observed", "该视频任务已有一个流式观察连接；可用同一 Key 查询任务状态");
        }
        if observers.len() >= MAX_OBSERVERS {
            return request_error(StatusCode::TOO_MANY_REQUESTS, "stream_observer_limit", "当前流式观察连接已达上限");
        }
        observers.insert(job_id.clone());
    }

    let (sender, receiver) = tokio::sync::mpsc::channel(8);
    let mut response = Response::new(Body::from_stream(BridgeBodyStream(receiver)));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert("content-type", "text/event-stream; charset=utf-8".parse().unwrap());
    response.headers_mut().insert("cache-control", "no-cache, no-transform".parse().unwrap());
    response.headers_mut().insert("x-accel-buffering", "no".parse().unwrap());
    response.headers_mut().insert("x-content-type-options", "nosniff".parse().unwrap());

    let stream_state = state.clone();
    tokio::spawn(async move {
        let started = Instant::now();
        let mut last_heartbeat = Instant::now();
        let mut last_status = String::new();
        let initial = crate::seedance_sse::encode_event(
            &job.request_id,
            crate::seedance_sse::VideoStreamEvent::Progress("视频任务已提交，正在生成".into()),
        );
        if sender.send(Ok(Bytes::from(initial))).await.is_err() {
            stream_state.video_stream_observers.lock().unwrap_or_else(|error| error.into_inner()).remove(&job_id);
            return;
        }

        loop {
            if started.elapsed() >= MAX_WAIT {
                let terminal = crate::seedance_sse::encode_event(
                    &job.request_id,
                    crate::seedance_sse::VideoStreamEvent::Failed {
                        code: "stream_wait_timeout".into(),
                        request_id: job.request_id.clone(),
                    },
                );
                let _ = sender.send(Ok(Bytes::from(terminal))).await;
                break;
            }

            let poll_state = stream_state.clone();
            let poll_job_id = job_id.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let _ = reconcile_video_job_once(&poll_state, &poll_job_id);
            }).await;

            let current = stream_state.jobs.lock().unwrap_or_else(|error| error.into_inner()).get(&job_id).cloned();
            let Some(current) = current else {
                let terminal = crate::seedance_sse::encode_event(
                    &job.request_id,
                    crate::seedance_sse::VideoStreamEvent::Failed {
                        code: "video_job_missing".into(),
                        request_id: job.request_id.clone(),
                    },
                );
                let _ = sender.send(Ok(Bytes::from(terminal))).await;
                break;
            };

            if current.billing_state == "settled" || current.billing_state == "released" {
                let completed = matches!(current.status.as_str(), "completed" | "succeeded" | "success");
                if completed && current.billing_state == "settled" {
                    let content_path = format!("/v1/videos/{}/content", current.upstream_id.as_deref().unwrap_or(&current.id));
                    let content_state = stream_state.clone();
                    let request_id = current.request_id.clone();
                    let probe = tokio::task::spawn_blocking(move || {
                        content_state.bridge.lock().unwrap_or_else(|error| error.into_inner()).forward(
                            "HEAD", &content_path, &[], &BTreeMap::new(), &request_id,
                        )
                    }).await;
                    match probe {
                        Ok(Ok(content)) if (200..300).contains(&content.status) => {
                            let public_base = stream_state.config.public_base_url.trim().trim_end_matches('/');
                            let content_url = if public_base.is_empty() {
                                format!("/v1/videos/{}/content", current.id)
                            } else {
                                format!("{public_base}/v1/videos/{}/content", current.id)
                            };
                            let terminal = crate::seedance_sse::encode_event(
                                &current.request_id,
                                crate::seedance_sse::VideoStreamEvent::Completed {
                                    task_id: current.id.clone(),
                                    content_url,
                                    request_id: current.request_id.clone(),
                                },
                            );
                            let _ = sender.send(Ok(Bytes::from(terminal))).await;
                        }
                        _ => {
                            let terminal = crate::seedance_sse::encode_event(
                                &current.request_id,
                                crate::seedance_sse::VideoStreamEvent::Failed {
                                    code: "video_content_unavailable".into(),
                                    request_id: current.request_id.clone(),
                                },
                            );
                            let _ = sender.send(Ok(Bytes::from(terminal))).await;
                        }
                    }
                } else {
                    let terminal = crate::seedance_sse::encode_event(
                        &current.request_id,
                        crate::seedance_sse::VideoStreamEvent::Failed {
                            code: current.error_code.clone().unwrap_or_else(|| "video_generation_failed".into()),
                            request_id: current.request_id.clone(),
                        },
                    );
                    let _ = sender.send(Ok(Bytes::from(terminal))).await;
                }
                break;
            }

            if (current.reconcile_required || current.billing_state == "reconcile_required")
                && current.error_code.as_deref() != Some("billing_receipt_unresolved")
            {
                let terminal = crate::seedance_sse::encode_event(
                    &current.request_id,
                    crate::seedance_sse::VideoStreamEvent::Failed {
                        code: current.error_code.clone().unwrap_or_else(|| "billing_reconcile_required".into()),
                        request_id: current.request_id.clone(),
                    },
                );
                let _ = sender.send(Ok(Bytes::from(terminal))).await;
                break;
            }

            if current.status != last_status {
                last_status = current.status.clone();
                let message = match current.status.as_str() {
                    "queued" => "视频任务排队中".to_string(),
                    "running" | "processing" => "视频正在生成".to_string(),
                    "reconcile_required" if current.error_code.as_deref() == Some("billing_receipt_unresolved") => "视频已生成，正在核对积分".to_string(),
                    _ => "视频任务处理中".to_string(),
                };
                let progress = crate::seedance_sse::encode_event(
                    &current.request_id,
                    crate::seedance_sse::VideoStreamEvent::Progress(message),
                );
                if sender.send(Ok(Bytes::from(progress))).await.is_err() {
                    break;
                }
            }

            if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
                if sender.send(Ok(Bytes::from_static(crate::seedance_sse::keep_alive()))).await.is_err() {
                    break;
                }
                last_heartbeat = Instant::now();
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }

        stream_state.video_stream_observers.lock().unwrap_or_else(|error| error.into_inner()).remove(&job_id);
    });
    response
}

pub async fn chat_completions(State(state): State<Arc<StarlinkRouterState>>, headers: HeaderMap, Extension(principal): Extension<Principal>, body: Bytes) -> Response {
    let value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"error": {"type": "invalid_request_error", "message": "请求体必须是 JSON"}}))).into_response(),
    };
    let model = value.get("model").and_then(Value::as_str).unwrap_or(&state.config.default_model).to_string();
    let seedance = is_seedance_model(&model);
    let seedance_stream = seedance && value.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if let Err(response) = authorize_scope(&principal, if seedance { "videos:submit" } else { "chat:invoke" }) { return response; }
    let supplied_idempotency = headers.get("idempotency-key").and_then(|value| value.to_str().ok()).filter(|value| !value.trim().is_empty());
    let headerless_seedance_stream = seedance_stream && supplied_idempotency.is_none();
    if seedance {
        let previous = if headerless_seedance_stream {
            state.store.lookup_implicit_billed_video_request(&principal.user_id, &principal.key_id, &model, &value)
        } else if let Some(key) = supplied_idempotency {
            state.store.lookup_idempotent_request(&principal.user_id, &principal.key_id, "videos", &model, &value, key)
        } else {
            Ok(None)
        };
        match previous {
            Ok(Some(BeginRequest::Existing(request))) => {
                return replay_existing_video_request(state, principal, &request.id, seedance_stream);
            }
            Ok(Some(BeginRequest::Conflict)) => {
                return request_error(StatusCode::CONFLICT, "idempotency_conflict", "Idempotency-Key 与历史请求内容不一致");
            }
            Ok(Some(BeginRequest::Created(_))) | Ok(None) => {}
            Err(error) => return request_error(StatusCode::INTERNAL_SERVER_ERROR, "core_error", error.to_string()),
        }
    }
    let video_admission = if seedance {
        match require_video_admission(&state, &principal, &model, &value) {
            Ok(admission) => admission,
            Err(response) => return response,
        }
    } else {
        VideoAdmission::Active
    };
    let endpoint = if seedance { "videos" } else { "chat" };
    let begun = if headerless_seedance_stream {
        begin_implicit_billed_video_request(&state.store, &principal, &model, &value)
    } else {
        begin_billed_request(&state.store, &principal, endpoint, &model, idempotency(&headers), &value)
    };
    let request = match begun {
        Ok(BeginRequest::Created(request)) => request,
        Ok(BeginRequest::Conflict) => return (StatusCode::CONFLICT, Json(json!({"error": {"type": "idempotency_conflict", "message": "Idempotency-Key 与历史请求内容不一致"}}))).into_response(),
        Ok(BeginRequest::Existing(request)) => {
            if seedance {
                return replay_existing_video_request(state, principal, &request.id, seedance_stream);
            }
            return existing_request_error(&state, &request.id);
        }
        Err(response) => return response,
    };
    let request_id = request.id.clone();
    let mut forward_value = value;
    if seedance {
        infer_video_parameters_from_prompt(&mut forward_value);
    }
    let mut assisted_before_reservation = false;
    if seedance && video_admission == VideoAdmission::DiagnosticClaimed {
        let has_asset_ids = forward_value.get("image_asset_ids").is_some() || forward_value.get("video_asset_ids").is_some();
        if has_asset_ids {
            if let Err(response) = materialize_bridge_assets(&state, &principal, &mut forward_value, &request_id).await {
                let _ = transition_request_to(&state, &request_id, RequestState::Validating, None);
                let _ = transition_request_to(&state, &request_id, RequestState::Failed, Some("asset_materialization_failed"));
                return response;
            }
        }
        if let Err(response) = validate_vision_data_urls(&forward_value) {
            let _ = transition_request_to(&state, &request_id, RequestState::Validating, None);
            let _ = transition_request_to(&state, &request_id, RequestState::Failed, Some("vision_input_invalid"));
            return response;
        }
        let assisted_prompt = match run_seedance_assist(&state, &principal, &request_id, &forward_value, true) {
            Ok(prompt) => prompt,
            Err(response) => {
                let _ = transition_request_to(&state, &request_id, RequestState::Validating, None);
                let _ = transition_request_to(&state, &request_id, RequestState::Failed, Some("seedance_assist_failed"));
                return response;
            }
        };
        if let Err(error) = apply_assisted_prompt(&mut forward_value, &assisted_prompt) {
            let _ = transition_request_to(&state, &request_id, RequestState::Validating, None);
            let _ = transition_request_to(&state, &request_id, RequestState::Failed, Some("seedance_assist_invalid_response"));
            return request_error(StatusCode::BAD_GATEWAY, "seedance_assist_invalid_response", error);
        }
        assisted_before_reservation = true;
    }
    let (quote_id, reservation_id, one_shot_test) = if seedance {
        match quote_and_reserve_video(
            &state,
            &principal,
            &request_id,
            &model,
            video_admission == VideoAdmission::DiagnosticClaimed,
        ) {
            Ok(reservation) => reservation,
            Err(response) => return response,
        }
    } else {
        match quote_and_reserve(&state, &request_id, endpoint, &model) {
            Ok((quote_id, reservation_id)) => (quote_id, reservation_id, false),
            Err(response) => return response,
        }
    };
    if seedance && !assisted_before_reservation {
        let has_asset_ids = forward_value.get("image_asset_ids").is_some() || forward_value.get("video_asset_ids").is_some();
        if has_asset_ids {
            if let Err(response) = materialize_bridge_assets(&state, &principal, &mut forward_value, &request_id).await {
                release_before_dispatch(&state, &principal, &reservation_id, response.status().as_u16(), "asset_materialization_failed");
                return response;
            }
        }
        if let Err(response) = validate_vision_data_urls(&forward_value) {
            release_before_dispatch(&state, &principal, &reservation_id, response.status().as_u16(), "vision_input_invalid");
            return response;
        }
        let assisted_prompt = match run_seedance_assist(&state, &principal, &request_id, &forward_value, false) {
            Ok(prompt) => prompt,
            Err(response) => {
                release_before_dispatch(&state, &principal, &reservation_id, response.status().as_u16(), "seedance_assist_failed");
                return response;
            }
        };
        if let Err(error) = apply_assisted_prompt(&mut forward_value, &assisted_prompt) {
            release_before_dispatch(&state, &principal, &reservation_id, 502, "seedance_assist_invalid_response");
            return request_error(StatusCode::BAD_GATEWAY, "seedance_assist_invalid_response", error);
        }
    }
    if seedance_stream {
        forward_value["stream"] = Value::Bool(false);
    }
    if !seedance {
        if let Err(response) = materialize_text_asset_ids(&state, &principal, &mut forward_value) {
            release_before_dispatch(&state, &principal, &reservation_id, response.status().as_u16(), "vision_input_invalid");
            return response;
        }
        if let Err(response) = validate_vision_data_urls(&forward_value) {
            release_before_dispatch(&state, &principal, &reservation_id, response.status().as_u16(), "vision_input_invalid");
            return response;
        }
    }
    let forward_body = if forward_value != serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null) {
        serde_json::to_vec(&forward_value).unwrap_or_else(|_| body.to_vec())
    } else {
        body.to_vec()
    };
    if let Err(error) = mark_request_dispatched(&state, &request_id) {
        release_before_dispatch(&state, &principal, &reservation_id, 500, "request_state_transition_failed");
        return request_error(StatusCode::INTERNAL_SERVER_ERROR, "core_error", error);
    }
    let streaming = !seedance && forward_value.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if streaming {
        let bridge = state.bridge.lock().unwrap().clone();
        let request_headers = header_map(&headers);
        let request_id_for_send = request_id.clone();
        let quote_id_for_send = quote_id.clone();
        let core_key_id_for_send = principal.key_id.clone();
        let send_result = tokio::task::spawn_blocking(move || {
            bridge.forward_billed_stream_for_key(
                "POST",
                "/v1/chat/completions",
                &forward_body,
                &request_headers,
                &request_id_for_send,
                &quote_id_for_send,
                &core_key_id_for_send,
            )
        }).await;
        match send_result {
            Ok(Ok(upstream))
                if (200..300).contains(&upstream.status)
                    && upstream.headers.get("content-type").is_some_and(|value| value.to_ascii_lowercase().starts_with("text/event-stream")) =>
            {
                return stream_chat_response(state, request_id, upstream);
            }
            Ok(Ok(upstream)) => {
                let buffered = tokio::task::spawn_blocking(move || upstream.into_buffered()).await;
                match buffered {
                    Ok(Ok(response)) if settle_chat_response(&state, &request_id) => {
                        return proxy(response.status, response.headers, response.body);
                    }
                    Ok(Ok(_)) => return reconcile_required(&request_id),
                    Ok(Err(error)) => {
                        mark_request_unknown(&state, &request_id, "bridge_stream_read_failed");
                        let _ = error;
                        return reconcile_required(&request_id);
                    }
                    Err(_) => {
                        mark_request_unknown(&state, &request_id, "bridge_stream_worker_failed");
                        return reconcile_required(&request_id);
                    }
                }
            }
            Ok(Err(error)) => {
                mark_request_unknown(&state, &request_id, "bridge_result_unknown");
                return match reconcile_billing_request_once(&state, &request_id) {
                    Ok(result) if receipt_is_settled(&result) => request_error(
                        StatusCode::BAD_GATEWAY,
                        "upstream_result_unavailable",
                        format!("上游连接中断，计费已按回执入账，但生成结果未能返回。request_id={request_id}"),
                    ),
                    _ => {
                        let _ = error;
                        reconcile_required(&request_id)
                    }
                };
            }
            Err(_) => {
                mark_request_unknown(&state, &request_id, "bridge_stream_worker_failed");
                let _ = reconcile_billing_request_once(&state, &request_id);
                return reconcile_required(&request_id);
            }
        }
    }
    let forwarded_headers = if seedance {
        video_forward_headers(&headers, &request_id)
    } else {
        header_map(&headers)
    };
    let upstream = state.bridge.lock().unwrap().forward_billed_for_key(
        "POST",
        "/v1/chat/completions",
        &forward_body,
        &forwarded_headers,
        &request_id,
        &quote_id,
        &principal.key_id,
    );
    if seedance {
        return match upstream {
            Ok(response) if (200..300).contains(&response.status) => {
                let value = serde_json::from_slice::<Value>(&response.body).unwrap_or(Value::Null);
                let Some(task_id) = extract_upstream_task_id(&value) else {
                    let task_id = format!("task-{request_id}");
                    mark_request_unknown(&state, &request_id, "bridge_task_id_missing");
                    let _ = reconcile_billing_request_once(&state, &request_id);
                    state.jobs.lock().unwrap().insert(task_id.clone(), unknown_video_job(&principal, &request_id, task_id.clone(), Some(reservation_id.clone()), "bridge_task_id_missing", one_shot_test));
                    state.persist_jobs();
                    return (StatusCode::BAD_GATEWAY, Json(json!({"error": {"type": "reconcile_required", "message": "上游已接受请求但没有返回可查询的视频任务 ID，请勿重复提交", "request_id": request_id}}))).into_response();
                };
                state.jobs.lock().unwrap().insert(task_id.clone(), accepted_video_job(&principal, &request_id, task_id, Some(reservation_id.clone()), extract_upstream_task_status(&value).as_deref().unwrap_or("queued"), None, one_shot_test));
                state.persist_jobs();
                if seedance_stream {
                    return stream_seedance_video_response(state, principal, extract_upstream_task_id(&value).unwrap_or_default());
                }
                proxy(response.status, response.headers, response.body)
            }
            Ok(response) => {
                if let Some(code) = confirmed_video_pre_dispatch_error_code(&response) {
                    if release_confirmed_video_pre_dispatch(&state, &principal, &reservation_id, response.status, code).is_ok() {
                        return proxy(response.status, response.headers, response.body);
                    }
                    mark_request_unknown(&state, &request_id, "video_pre_dispatch_release_failed");
                    return reconcile_required(&request_id);
                }
                if !settle_chat_response(&state, &request_id) {
                    return reconcile_required(&request_id);
                }
                proxy(response.status, response.headers, response.body)
            }
            Err(error) => {
                mark_request_unknown(&state, &request_id, "bridge_result_unknown");
                let _ = reconcile_billing_request_once(&state, &request_id);
                let task_id = format!("task-{request_id}");
                state.jobs.lock().unwrap().insert(task_id.clone(), unknown_video_job(&principal, &request_id, task_id, Some(reservation_id.clone()), "bridge_result_unknown", one_shot_test));
                state.persist_jobs();
                (StatusCode::BAD_GATEWAY, Json(json!({"error": {"type": "bridge_error", "message": error, "request_id": request_id, "reconcile_required": true}}))).into_response()
            }
        };
    }
    match upstream {
        Ok(response) => {
            if settle_chat_response(&state, &request_id) {
                proxy(response.status, response.headers, response.body)
            } else {
                reconcile_required(&request_id)
            }
        }
        Err(error) => {
            mark_request_unknown(&state, &request_id, "bridge_result_unknown");
            match reconcile_billing_request_once(&state, &request_id) {
                Ok(result) if receipt_is_settled(&result) => request_error(
                    StatusCode::BAD_GATEWAY,
                    "upstream_result_unavailable",
                    format!("上游连接中断，计费已按回执入账，但生成结果未能返回。request_id={request_id}"),
                ),
                _ => {
                    let _ = error;
                    reconcile_required(&request_id)
                }
            }
        }
    }
}

pub async fn video_generations(State(state): State<Arc<StarlinkRouterState>>, headers: HeaderMap, Extension(principal): Extension<Principal>, body: Bytes) -> Response {
    let value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"error": {"type": "invalid_request_error", "message": "请求体必须是 JSON"}}))).into_response(),
    };
    if let Err(response) = authorize_scope(&principal, "videos:submit") { return response; }
    let model = value.get("model").and_then(Value::as_str).unwrap_or("seedance").to_string();
    let video_admission = match require_video_admission(&state, &principal, &model, &value) {
        Ok(admission) => admission,
        Err(response) => return response,
    };
    let request = match begin_billed_request(
        &state.store,
        &principal,
        "videos",
        &model,
        idempotency(&headers),
        &value,
    ) {
        Ok(BeginRequest::Created(request)) => request,
        Ok(BeginRequest::Conflict) => return (StatusCode::CONFLICT, Json(json!({"error": {"type": "idempotency_conflict", "message": "Idempotency-Key 与历史请求内容不一致"}}))).into_response(),
        Ok(BeginRequest::Existing(request)) => {
            let job = state.jobs.lock().unwrap().values().find(|job| job.request_id == request.id && job.user_id == principal.user_id).cloned();
            if let Some(job) = job.filter(|job| !job.reconcile_required && job.upstream_id.is_some()) {
                return Json(json!({"task": {"id": job.id, "status": job.status}, "core_replay": true})).into_response();
            }
            return existing_request_error(&state, &request.id);
        }
        Err(response) => return response,
    };
    let request_id = request.id.clone();
    let (quote_id, reservation_id, one_shot_test) = match quote_and_reserve_video(
        &state,
        &principal,
        &request_id,
        &model,
        video_admission == VideoAdmission::DiagnosticClaimed,
    ) {
        Ok(reservation) => reservation,
        Err(response) => return response,
    };
    let mut forward_value = value;
    let has_asset_ids = forward_value.get("image_asset_ids").is_some() || forward_value.get("video_asset_ids").is_some();
    if has_asset_ids {
        if let Err(response) = materialize_bridge_assets(&state, &principal, &mut forward_value, &request_id).await {
            release_before_dispatch(&state, &principal, &reservation_id, response.status().as_u16(), "asset_materialization_failed");
            return response;
        }
    }
    let forward_body = if has_asset_ids {
        match serde_json::to_vec(&forward_value) {
            Ok(body) => body,
            Err(error) => {
                release_before_dispatch(&state, &principal, &reservation_id, 500, "asset_request_encoding_failed");
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"type": "internal_error", "message": error.to_string()}}))).into_response();
            }
        }
    } else { body.to_vec() };
    if let Err(error) = transition_request_to(&state, &request_id, RequestState::Queued, None) {
        release_before_dispatch(&state, &principal, &reservation_id, 500, "request_state_transition_failed");
        return request_error(StatusCode::INTERNAL_SERVER_ERROR, "core_error", error);
    }
    let response = match state.bridge.lock().unwrap().forward_billed_for_key("POST", "/v1/videos/generations", &forward_body, &video_forward_headers(&headers, &request_id), &request_id, &quote_id, &principal.key_id) {
        Ok(response) => response,
        Err(error) => {
            mark_request_unknown(&state, &request_id, "bridge_result_unknown");
            let _ = reconcile_billing_request_once(&state, &request_id);
            let task_id = format!("task-{request_id}");
            state.jobs.lock().unwrap().insert(task_id.clone(), unknown_video_job(&principal, &request_id, task_id, Some(reservation_id.clone()), "bridge_result_unknown", one_shot_test));
            state.persist_jobs();
            return (StatusCode::BAD_GATEWAY, Json(json!({"error": {"type": "bridge_error", "message": error, "request_id": request_id, "reconcile_required": true}}))).into_response();
        }
    };
    if !(200..300).contains(&response.status) {
        if let Some(code) = confirmed_video_pre_dispatch_error_code(&response) {
            if release_confirmed_video_pre_dispatch(&state, &principal, &reservation_id, response.status, code).is_ok() {
                return proxy(response.status, response.headers, response.body);
            }
            mark_request_unknown(&state, &request_id, "video_pre_dispatch_release_failed");
            return reconcile_required(&request_id);
        }
        if settle_chat_response(&state, &request_id) {
            return proxy(response.status, response.headers, response.body);
        }
        return reconcile_required(&request_id);
    }
    let upstream_id = serde_json::from_slice::<Value>(&response.body).ok().and_then(|value| {
        extract_upstream_task_id(&value)
    });
    let Some(task_id) = upstream_id else {
        let task_id = format!("task-{request_id}");
        mark_request_unknown(&state, &request_id, "bridge_task_id_missing");
        let _ = reconcile_billing_request_once(&state, &request_id);
        state.jobs.lock().unwrap().insert(task_id.clone(), unknown_video_job(&principal, &request_id, task_id.clone(), Some(reservation_id.clone()), "bridge_task_id_missing", one_shot_test));
        state.persist_jobs();
        return (StatusCode::BAD_GATEWAY, Json(json!({"error": {"type": "reconcile_required", "message": "上游已接受请求但没有返回可查询的视频任务 ID，请勿重复提交", "request_id": request_id}}))).into_response();
    };
    let upstream_value = serde_json::from_slice::<Value>(&response.body).unwrap_or(Value::Null);
    state.jobs.lock().unwrap().insert(task_id.clone(), accepted_video_job(&principal, &request_id, task_id, Some(reservation_id.clone()), extract_upstream_task_status(&upstream_value).as_deref().unwrap_or("queued"), None, one_shot_test));
    state.persist_jobs();
    proxy(response.status, response.headers, response.body)
}

pub async fn video_task(State(state): State<Arc<StarlinkRouterState>>, Path(task_id): Path<String>, headers: HeaderMap, Extension(principal): Extension<Principal>) -> Response {
    let job = match state.jobs.lock().unwrap().get(&task_id).cloned() { Some(job) if job.user_id == principal.user_id && job.api_key_id == principal.key_id => job, Some(_) => return StatusCode::NOT_FOUND.into_response(), None => return StatusCode::NOT_FOUND.into_response() };
    let upstream_id = job.upstream_id.as_deref().unwrap_or(&task_id);
    let path = format!("/v1/videos/{upstream_id}");
    let result = state.bridge.lock().unwrap().forward("GET", &path, &[], &header_map(&headers), &job.request_id);
    match result {
        Ok(response) => {
            if (200..300).contains(&response.status) {
                match serde_json::from_slice::<Value>(&response.body) {
                    Ok(value) => { let _ = settle_video_job(&state, &task_id, &value); }
                    Err(_) => mark_video_reconcile(&state, &task_id, "video_status_invalid_json", None),
                }
            } else {
                mark_video_reconcile(&state, &task_id, "video_status_bridge_http_error", None);
            }
            proxy(response.status, response.headers, response.body)
        }
        Err(error) => {
            mark_video_reconcile(&state, &task_id, "video_status_bridge_error", None);
            (StatusCode::BAD_GATEWAY, Json(json!({"error": {"type": "bridge_error", "message": error}}))).into_response()
        }
    }
}

pub async fn video_content(State(state): State<Arc<StarlinkRouterState>>, Path(task_id): Path<String>, headers: HeaderMap, Extension(principal): Extension<Principal>) -> Response {
    let job = match state.jobs.lock().unwrap().get(&task_id).cloned() { Some(job) if job.user_id == principal.user_id && job.api_key_id == principal.key_id => job, Some(_) => return StatusCode::NOT_FOUND.into_response(), None => return StatusCode::NOT_FOUND.into_response() };
    let upstream_id = job.upstream_id.as_deref().unwrap_or(&task_id);
    let path = format!("/v1/videos/{upstream_id}/content");
    match state.bridge.lock().unwrap().forward("GET", &path, &[], &header_map(&headers), &job.request_id) { Ok(response) => proxy(response.status, response.headers, response.body), Err(error) => (StatusCode::BAD_GATEWAY, Json(json!({"error": {"type": "bridge_error", "message": error}}))).into_response() }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoBillingDecision {
    Held,
    Settled,
    Released,
    ReconcileRequired,
}

fn extract_upstream_task_id(value: &Value) -> Option<String> {
    value
        .pointer("/video_task/id")
        .or_else(|| value.pointer("/task/id"))
        .or_else(|| value.pointer("/data/task/id"))
        .or_else(|| value.get("id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToString::to_string)
}

fn extract_upstream_task_status(value: &Value) -> Option<String> {
    value
        .pointer("/video_task/status")
        .or_else(|| value.pointer("/task/status"))
        .or_else(|| value.pointer("/data/task/status"))
        .or_else(|| value.get("status"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|status| !status.is_empty())
        .map(ToString::to_string)
}

fn update_video_job(state: &StarlinkRouterState, task_id: &str, update: impl FnOnce(&mut UserVideoJob)) {
    let changed = {
        let mut jobs = state.jobs.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(job) = jobs.get_mut(task_id) {
            update(job);
            true
        } else {
            false
        }
    };
    if changed {
        state.persist_jobs();
    }
}

fn mark_video_reconcile(
    state: &StarlinkRouterState,
    task_id: &str,
    error_code: &str,
    actual_credits: Option<String>,
) {
    let now = Utc::now().timestamp_millis();
    update_video_job(state, task_id, |job| {
        if job.billing_state == "settled" || job.billing_state == "released" {
            return;
        }
        job.status = "reconcile_required".into();
        job.billing_state = "reconcile_required".into();
        job.reconcile_required = true;
        job.error_code = Some(error_code.into());
        if actual_credits.is_some() {
            job.actual_credits = actual_credits;
        }
        job.last_reconciled_at_ms = Some(now);
    });
}

fn accepted_video_job(
    principal: &Principal,
    request_id: &str,
    task_id: String,
    reservation_id: Option<String>,
    status: &str,
    error_code: Option<String>,
    one_shot_test: bool,
) -> UserVideoJob {
    UserVideoJob {
        id: task_id.clone(),
        user_id: principal.user_id.clone(),
        api_key_id: principal.key_id.clone(),
        request_id: request_id.into(),
        upstream_id: Some(task_id),
        status: status.into(),
        output_ref: None,
        error_code,
        reconcile_required: false,
        reservation_id,
        billing_state: "held".into(),
        actual_credits: None,
        last_reconciled_at_ms: None,
        one_shot_test,
    }
}

fn confirmed_video_pre_dispatch_error_code(response: &BridgeResponse) -> Option<String> {
    if response.status != StatusCode::BAD_REQUEST.as_u16() {
        return None;
    }
    let body: Value = serde_json::from_slice(&response.body).ok()?;
    let code = body.pointer("/error/code")?.as_str()?;
    // These AI Work errors occur before video task creation. Other HTTP
    // failures do not prove zero upstream cost and must remain held.
    matches!(code, "idempotency_key_required" | "seedance_stream_unsupported")
        .then(|| code.to_string())
}

fn release_confirmed_video_pre_dispatch(
    state: &StarlinkRouterState,
    principal: &Principal,
    reservation_id: &str,
    status: u16,
    code: String,
) -> Result<(), String> {
    state.store.settle_request(
        principal,
        reservation_id,
        Settlement::Release,
        RequestState::Failed,
        Some(RequestResult { status: Some(i64::from(status)), error_code: Some(code) }),
    ).map(|_| ()).map_err(|error| error.to_string())
}

fn unknown_video_job(
    principal: &Principal,
    request_id: &str,
    task_id: String,
    reservation_id: Option<String>,
    error_code: &str,
    one_shot_test: bool,
) -> UserVideoJob {
    UserVideoJob {
        id: task_id,
        user_id: principal.user_id.clone(),
        api_key_id: principal.key_id.clone(),
        request_id: request_id.into(),
        upstream_id: None,
        status: "reconcile_required".into(),
        output_ref: None,
        error_code: Some(error_code.into()),
        reconcile_required: true,
        reservation_id,
        billing_state: "reconcile_required".into(),
        actual_credits: None,
        last_reconciled_at_ms: None,
        one_shot_test,
    }
}

pub(crate) fn settle_video_job(
    state: &StarlinkRouterState,
    job_id: &str,
    upstream: &Value,
) -> Result<VideoBillingDecision, CoreError> {
    let job = state
        .jobs
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(job_id)
        .cloned()
        .ok_or_else(|| CoreError::InvalidConfiguration {
            key: "video_jobs".into(),
            value: job_id.into(),
        })?;
    if job.billing_state == "settled" {
        return Ok(VideoBillingDecision::Settled);
    }
    if job.billing_state == "released" {
        return Ok(VideoBillingDecision::Released);
    }

    let Some(upstream_id) = job.upstream_id.as_deref() else {
        mark_video_reconcile(state, job_id, "upstream_task_id_missing", None);
        return Ok(VideoBillingDecision::ReconcileRequired);
    };
    let Some(observed_task_id) = extract_upstream_task_id(upstream) else {
        mark_video_reconcile(state, job_id, "upstream_task_id_missing", None);
        return Ok(VideoBillingDecision::ReconcileRequired);
    };
    if observed_task_id != upstream_id {
        mark_video_reconcile(state, job_id, "upstream_task_id_mismatch", None);
        return Ok(VideoBillingDecision::ReconcileRequired);
    }
    let Some(status) = extract_upstream_task_status(upstream) else {
        mark_video_reconcile(state, job_id, "upstream_status_missing", None);
        return Ok(VideoBillingDecision::ReconcileRequired);
    };
    let terminal_success = matches!(status.as_str(), "completed" | "succeeded" | "success");
    let terminal_failure = matches!(status.as_str(), "failed" | "error" | "canceled" | "cancelled");
    if !terminal_success && !terminal_failure {
        update_video_job(state, job_id, |stored| {
            stored.status = status.clone();
            stored.last_reconciled_at_ms = Some(Utc::now().timestamp_millis());
        });
        return Ok(VideoBillingDecision::Held);
    }
    let Some(reservation_id) = job.reservation_id.as_deref() else {
        mark_video_reconcile(state, job_id, "reservation_id_missing", None);
        return Ok(VideoBillingDecision::ReconcileRequired);
    };
    let Some(reservation) = state.store.reservation_for_request(&job.request_id)? else {
        mark_video_reconcile(state, job_id, "reservation_missing", None);
        return Ok(VideoBillingDecision::ReconcileRequired);
    };
    if reservation.id != reservation_id {
        mark_video_reconcile(state, job_id, "reservation_identity_mismatch", None);
        return Ok(VideoBillingDecision::ReconcileRequired);
    }
    let receipt_result = if job.one_shot_test {
        state.bridge.lock().unwrap().finalize_video_billing(&job.request_id, upstream_id)
    } else {
        state.bridge.lock().unwrap().billing(&job.request_id)
    };
    match receipt_result {
        Ok(BridgeBillingResult::Final(receipt)) => {
            if receipt.task_ref.as_deref() != Some(upstream_id) {
                mark_request_unknown(state, &job.request_id, "billing_task_ref_mismatch");
                mark_video_reconcile(state, job_id, "billing_task_ref_mismatch", None);
                return Ok(VideoBillingDecision::ReconcileRequired);
            }
            prepare_for_final_receipt(state, &job.request_id)
                .map_err(|error| CoreError::InvalidConfiguration {
                    key: "billing_receipt_request_state".into(),
                    value: error,
                })?;
            let actual_credits = receipt.actual_credits.map(|amount| format_microcredits(amount.as_microcredits()));
            let no_charge = receipt.status == aiwork_core::BillingReceiptStatus::FailedNoCharge;
            match state.store.apply_credit_receipt(receipt)? {
                BillingReceiptResult::Settled { .. } | BillingReceiptResult::Duplicate => {
                    update_video_job(state, job_id, |stored| {
                        stored.status = status.clone();
                        stored.reconcile_required = false;
                        stored.billing_state = if no_charge { "released" } else { "settled" }.into();
                        stored.actual_credits = actual_credits.clone().or_else(|| no_charge.then(|| "0.000000".into()));
                        stored.error_code = None;
                        stored.last_reconciled_at_ms = Some(Utc::now().timestamp_millis());
                    });
                    Ok(if no_charge { VideoBillingDecision::Released } else { VideoBillingDecision::Settled })
                }
                BillingReceiptResult::Pending | BillingReceiptResult::Conflict => {
                    mark_request_unknown(state, &job.request_id, "billing_receipt_unresolved");
                    mark_video_reconcile(state, job_id, "billing_receipt_unresolved", actual_credits);
                    Ok(VideoBillingDecision::ReconcileRequired)
                }
            }
        }
        Ok(BridgeBillingResult::Unresolved) => {
            mark_request_unknown(state, &job.request_id, "billing_receipt_unresolved");
            mark_video_reconcile(state, job_id, "billing_receipt_unresolved", None);
            Ok(VideoBillingDecision::ReconcileRequired)
        }
        Err(_) => {
            mark_request_unknown(state, &job.request_id, "billing_query_failed");
            mark_video_reconcile(state, job_id, "billing_query_failed", None);
            Ok(VideoBillingDecision::ReconcileRequired)
        }
    }
}

pub(crate) fn reconcile_video_job_once(
    state: &StarlinkRouterState,
    job_id: &str,
) -> Result<VideoBillingDecision, CoreError> {
    let job = state
        .jobs
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(job_id)
        .cloned()
        .ok_or_else(|| CoreError::InvalidConfiguration {
            key: "video_jobs".into(),
            value: job_id.into(),
        })?;
    let Some(upstream_id) = job.upstream_id.as_deref() else {
        if job.one_shot_test {
            mark_request_unknown(state, &job.request_id, "upstream_task_id_missing");
            mark_video_reconcile(state, job_id, "upstream_task_id_missing", None);
            return Ok(VideoBillingDecision::ReconcileRequired);
        }
        return match reconcile_billing_request_once(state, &job.request_id) {
            Ok(result) if receipt_is_settled(&result) => {
                update_video_job(state, job_id, |stored| {
                    stored.billing_state = "settled".into();
                    stored.error_code = Some("upstream_task_id_missing".into());
                    stored.reconcile_required = true;
                    stored.status = "reconcile_required".into();
                    stored.last_reconciled_at_ms = Some(Utc::now().timestamp_millis());
                });
                Ok(VideoBillingDecision::ReconcileRequired)
            }
            _ => {
                mark_video_reconcile(state, job_id, "upstream_task_id_missing", None);
                Ok(VideoBillingDecision::ReconcileRequired)
            }
        };
    };
    let path = format!("/v1/videos/{upstream_id}");
    let response = match state.bridge.lock().unwrap().forward(
        "GET",
        &path,
        &[],
        &BTreeMap::new(),
        &job.request_id,
    ) {
        Ok(response) => response,
        Err(_) => {
            mark_video_reconcile(state, job_id, "reconcile_bridge_error", None);
            return Ok(VideoBillingDecision::ReconcileRequired);
        }
    };
    if !(200..300).contains(&response.status) {
        mark_video_reconcile(state, job_id, "reconcile_bridge_http_error", None);
        return Ok(VideoBillingDecision::ReconcileRequired);
    }
    let value = match serde_json::from_slice::<Value>(&response.body) {
        Ok(value) => value,
        Err(_) => {
            mark_video_reconcile(state, job_id, "reconcile_invalid_json", None);
            return Ok(VideoBillingDecision::ReconcileRequired);
        }
    };
    settle_video_job(state, job_id, &value)
}

fn proxy(status: u16, headers: BTreeMap<String, String>, body: Vec<u8>) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    if let Some(content_type) = headers.get("content-type") { response.headers_mut().insert("content-type", content_type.parse().unwrap_or_else(|_| "application/json".parse().unwrap())); }
    response
}

#[cfg(test)]
mod tests {
    use super::{apply_assisted_prompt, authorize_scope, extract_assisted_prompt, extract_seedance_prompt, seedance_assist_chat_body};
    use aiwork_core::Principal;
    use std::collections::BTreeSet;
    use serde_json::json;

    #[test]
    fn ordinary_user_scope_is_checked_before_forwarding() {
        let principal = Principal { user_id: "u".into(), key_id: "k".into(), scopes: BTreeSet::from(["chat:invoke".into()]) };
        assert!(authorize_scope(&principal, "videos:submit").is_err());
        assert!(authorize_scope(&principal, "chat:invoke").is_ok());
    }

    #[test]
    fn seedance_assist_receives_text_only_and_preserves_reference_image_and_explicit_parameters() {
        let mut video = json!({
            "model":"seedance",
            "duration":5,
            "resolution":"720p",
            "ratio":"16:9",
            "messages":[{"role":"user","content":[
                {"type":"text","text":"生成一只橘猫"},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,private-image-bytes"}}
            ]}]
        });
        let prompt = extract_seedance_prompt(&video).unwrap();
        assert_eq!(prompt, "生成一只橘猫");
        let helper = seedance_assist_chat_body("deepseek-v4-flash", &video, &prompt);
        let helper_text = helper.to_string();
        assert!(helper_text.contains("deepseek-v4-flash"));
        assert!(helper_text.contains("720p"));
        assert!(!helper_text.contains("private-image-bytes"));

        let response = r#"{"choices":[{"message":{"content":"{\"prompt\":\"一只橘猫在窗边自然伸懒腰\"}"}}]}"#;
        let assisted = extract_assisted_prompt(response.as_bytes()).unwrap();
        apply_assisted_prompt(&mut video, &assisted).unwrap();
        assert_eq!(video["duration"], 5);
        assert_eq!(video["resolution"], "720p");
        assert_eq!(video["ratio"], "16:9");
        assert_eq!(video["messages"][0]["content"][0]["text"], assisted);
        assert_eq!(video["messages"][0]["content"][1]["image_url"]["url"], "data:image/png;base64,private-image-bytes");
    }

    #[test]
    fn malformed_seedance_assist_json_fails_closed() {
        assert!(extract_assisted_prompt(br#"{"choices":[{"message":{"content":"not-json"}}]}"#).is_err());
    }
}
