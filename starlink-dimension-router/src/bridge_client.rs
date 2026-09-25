use std::{collections::BTreeMap, io::{Cursor, Read}, sync::Arc, time::Duration};

use aiwork_core::{
    BillingQuote, BillingReceipt, BillingReceiptStatus, CreditAmount, UpstreamCreditSnapshot,
    UPSTREAM_CREDIT_SNAPSHOT_MAX_AGE_MS,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Debug)]
pub struct BridgeResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

pub struct BridgeStreamingResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Box<dyn Read + Send>,
}

impl BridgeStreamingResponse {
    pub fn into_buffered(self) -> Result<BridgeResponse, String> {
        let body = read_bounded_response(self.body, 64 * 1024 * 1024)?;
        Ok(BridgeResponse { status: self.status, headers: self.headers, body })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BridgeQuoteResult {
    Quoted(BillingQuote),
    Unavailable { error_code: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BridgeBillingResult {
    Final(BillingReceipt),
    Unresolved,
}

#[derive(Debug, Deserialize)]
struct QuoteResponse {
    request_id: String,
    status: String,
    quote_id: Option<String>,
    request_fingerprint: Option<String>,
    endpoint: Option<String>,
    model: Option<String>,
    max_credits: Option<CreditAmount>,
    unit: Option<String>,
    expires_at_ms: Option<i64>,
    source_ref: Option<String>,
    error_code: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BillingResponse {
    request_id: String,
    status: String,
    actual_credits: Option<CreditAmount>,
    unit: Option<String>,
    source_ref: Option<String>,
    task_ref: Option<String>,
    observed_at_ms: i64,
}

#[derive(Debug, Deserialize)]
struct BridgeSummaryResponse {
    upstream_credits: BridgeCreditAggregate,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum UpstreamCreditAmount {
    Decimal(String),
    Number(serde_json::Number),
}

#[derive(Debug, Deserialize)]
struct BridgeCreditAggregate {
    #[serde(default, deserialize_with = "deserialize_optional_credit_amount")]
    video_available: Option<CreditAmount>,
    #[serde(default, deserialize_with = "deserialize_optional_credit_amount")]
    value: Option<CreditAmount>,
    source: String,
    fresh: bool,
    updated_at: i64,
    error_code: Option<String>,
}

fn deserialize_optional_credit_amount<'de, D>(
    deserializer: D,
) -> Result<Option<CreditAmount>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<UpstreamCreditAmount>::deserialize(deserializer)?;
    value
        .map(|value| match value {
            UpstreamCreditAmount::Decimal(value) => CreditAmount::parse(&value, "credits"),
            UpstreamCreditAmount::Number(value) => parse_numeric_credit_amount(value),
        })
        .transpose()
        .map_err(|error| <D::Error as serde::de::Error>::custom(error))
}

fn parse_numeric_credit_amount(value: serde_json::Number) -> Result<CreditAmount, String> {
    if let Some(value) = value.as_u64() {
        return CreditAmount::parse(&value.to_string(), "credits");
    }
    if let Some(value) = value.as_i64() {
        return CreditAmount::parse(&value.to_string(), "credits");
    }

    let value = value
        .as_f64()
        .ok_or_else(|| "numeric credit amount is not representable".to_string())?;
    if !value.is_finite() || value.is_sign_negative() {
        return Err("numeric credit amount must be finite and non-negative".into());
    }

    const MICROCREDITS_PER_CREDIT: f64 = 1_000_000.0;
    const MAX_SAFE_ROUNDING_ERROR: f64 = 0.000_000_5;
    let tolerance = value.abs().max(1.0) * f64::EPSILON * 8.0;
    if tolerance >= MAX_SAFE_ROUNDING_ERROR {
        return Err("numeric credit amount exceeds safe six-decimal precision".into());
    }

    let rounded = (value * MICROCREDITS_PER_CREDIT).round() / MICROCREDITS_PER_CREDIT;
    if !rounded.is_finite() || (value - rounded).abs() > tolerance {
        return Err("numeric credit amount has more than six meaningful decimals".into());
    }

    CreditAmount::parse(&format!("{rounded:.6}"), "credits")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeCreditStatus {
    pub total: Option<CreditAmount>,
    pub source: String,
    pub fresh: bool,
    pub updated_at_ms: Option<i64>,
    pub error_code: Option<String>,
}

pub trait BridgeTransport: Send + Sync {
    fn send(
        &self,
        method: &str,
        url: &str,
        headers: &BTreeMap<String, String>,
        body: &[u8],
    ) -> Result<BridgeResponse, String>;

    fn send_stream(
        &self,
        method: &str,
        url: &str,
        headers: &BTreeMap<String, String>,
        body: &[u8],
    ) -> Result<BridgeStreamingResponse, String> {
        let response = self.send(method, url, headers, body)?;
        Ok(BridgeStreamingResponse {
            status: response.status,
            headers: response.headers,
            body: Box::new(Cursor::new(response.body)),
        })
    }
}

#[derive(Clone)]
pub struct BridgeClient {
    base_url: String,
    bridge_secret: String,
    transport: Arc<dyn BridgeTransport>,
    background_registry_sync: bool,
}

#[derive(Serialize)]
struct CoreKeyRegistryKey {
    id: String,
    display_name: String,
    active: bool,
}

#[derive(Serialize)]
struct CoreKeyRegistrySnapshot {
    version: i64,
    keys: Vec<CoreKeyRegistryKey>,
}

impl BridgeClient {
    pub fn new(base_url: impl Into<String>, bridge_secret: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            bridge_secret: bridge_secret.into(),
            transport: Arc::new(HttpBridgeTransport),
            background_registry_sync: true,
        }
    }

    pub fn from_transport(
        base_url: impl Into<String>,
        bridge_secret: impl Into<String>,
        transport: Arc<dyn BridgeTransport>,
    ) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            bridge_secret: bridge_secret.into(),
            transport,
            background_registry_sync: false,
        }
    }

    pub fn base_url(&self) -> &str { &self.base_url }

    pub fn background_registry_sync_enabled(&self) -> bool {
        self.background_registry_sync
            && !self.base_url.trim().is_empty()
            && !self.bridge_secret.trim().is_empty()
    }

    pub fn test(&self) -> Result<Value, String> {
        self.json_request("GET", "/internal/bridge/status", &[], None)
    }

    pub fn models(&self) -> Result<Value, String> {
        self.json_request("GET", "/internal/bridge/models", &[], None)
    }

    pub fn summary(&self) -> Result<Value, String> {
        self.json_request("GET", "/internal/bridge/summary", &[], None)
    }

    pub fn sync_core_key_registry(
        &self,
        version: i64,
        keys: Vec<(String, String, bool)>,
    ) -> Result<Value, String> {
        if version <= 0 || keys.len() > 10_000 {
            return Err("Core Key registry snapshot is outside the allowed bounds".into());
        }
        let snapshot = CoreKeyRegistrySnapshot {
            version,
            keys: keys.into_iter().map(|(id, display_name, active)| CoreKeyRegistryKey {
                id,
                display_name,
                active,
            }).collect(),
        };
        let body = serde_json::to_vec(&snapshot)
            .map_err(|error| format!("Core Key registry encoding failed: {error}"))?;
        let mut headers = BTreeMap::new();
        headers.insert("content-type".into(), "application/json".into());
        let response = self.forward(
            "PUT",
            "/internal/bridge/key-registry",
            &body,
            &headers,
            "core-key-registry-sync",
        )?;
        if !(200..300).contains(&response.status) {
            return Err(format!("AI Work Key registry sync returned HTTP {}", response.status));
        }
        serde_json::from_slice(&response.body)
            .map_err(|error| format!("AI Work Key registry response is invalid: {error}"))
    }

    pub fn upstream_credit_snapshot(&self) -> Result<UpstreamCreditSnapshot, String> {
        let status = self.upstream_credit_status()?;
        if !status.fresh {
            return Err(status.error_code.unwrap_or_else(|| "AI Work 积分汇总缺失或已过期".into()));
        }
        let total = status.total.ok_or_else(|| "AI Work 未返回统一积分余额".to_string())?;
        let updated_at_ms = status.updated_at_ms.ok_or_else(|| "AI Work 积分汇总缺少有效更新时间".to_string())?;
        Ok(UpstreamCreditSnapshot {
            total,
            updated_at_ms,
        })
    }

    pub fn upstream_credit_status(&self) -> Result<BridgeCreditStatus, String> {
        let summary = self.summary()?;
        let summary: BridgeSummaryResponse = serde_json::from_value(summary)
            .map_err(|error| format!("AI Work 积分汇总格式无效: {error}"))?;
        let credits = summary.upstream_credits;
        let now_ms = chrono::Utc::now().timestamp_millis();
        let fields_match = credits.video_available.is_none()
            || credits.value.is_none()
            || credits.video_available == credits.value;
        let total = if fields_match {
            credits.video_available.or(credits.value)
        } else {
            None
        };
        let timestamp_valid = credits.updated_at > 0 && credits.updated_at <= now_ms;
        let age_ms = now_ms.saturating_sub(credits.updated_at);
        let source_valid = credits.source == "aiwork-upstream-aggregate";
        let fresh = fields_match
            && source_valid
            && total.is_some()
            && credits.fresh
            && timestamp_valid
            && age_ms <= UPSTREAM_CREDIT_SNAPSHOT_MAX_AGE_MS;
        let error_code = if fresh {
            None
        } else {
            credits.error_code.or_else(|| {
                Some(if !fields_match {
                    "upstream_credit_fields_mismatch".into()
                } else if !source_valid {
                    "upstream_credit_source_unverified".into()
                } else if total.is_none() {
                    "upstream_credit_total_missing".into()
                } else {
                    "upstream_balance_stale".into()
                })
            })
        };
        Ok(BridgeCreditStatus {
            total,
            source: credits.source,
            fresh,
            updated_at_ms: (credits.updated_at > 0).then_some(credits.updated_at),
            error_code,
        })
    }

    pub fn quote(
        &self,
        request_id: &str,
        endpoint: &str,
        model: &str,
        request_fingerprint: &str,
    ) -> Result<BridgeQuoteResult, String> {
        let body = serde_json::to_vec(&json!({
            "request_id": request_id,
            "endpoint": endpoint,
            "model": model,
            "request_fingerprint": request_fingerprint,
        }))
        .map_err(|error| format!("编码 AI Work 报价请求失败: {error}"))?;
        let mut headers = BTreeMap::new();
        headers.insert("accept".into(), "application/json".into());
        headers.insert("content-type".into(), "application/json".into());
        let response = self.forward(
            "POST",
            "/internal/bridge/quotes",
            &body,
            &headers,
            request_id,
        )?;
        let envelope: QuoteResponse = serde_json::from_slice(&response.body)
            .map_err(|error| format!("AI Work 报价响应格式无效: {error}"))?;
        if envelope.request_id != request_id {
            return Err("AI Work 报价响应 request_id 不匹配".into());
        }
        if envelope.status == "unavailable" {
            return Ok(BridgeQuoteResult::Unavailable {
                error_code: envelope
                    .error_code
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| "quote_unavailable".into()),
            });
        }
        if !(200..300).contains(&response.status) || envelope.status != "quoted" {
            return Err("AI Work 未返回可信报价".into());
        }

        let quote = BillingQuote {
            request_id: envelope.request_id,
            quote_id: required_nonempty(envelope.quote_id, "quote_id")?,
            request_fingerprint: required_nonempty(
                envelope.request_fingerprint,
                "request_fingerprint",
            )?,
            endpoint: required_nonempty(envelope.endpoint, "endpoint")?,
            model: required_nonempty(envelope.model, "model")?,
            max_credits: envelope
                .max_credits
                .ok_or_else(|| "AI Work 报价缺少 max_credits".to_string())?,
            unit: required_nonempty(envelope.unit, "unit")?,
            expires_at_ms: envelope
                .expires_at_ms
                .ok_or_else(|| "AI Work 报价缺少 expires_at_ms".to_string())?,
            source_ref: required_nonempty(envelope.source_ref, "source_ref")?,
        };
        if quote.request_fingerprint != request_fingerprint
            || quote.endpoint != endpoint
            || quote.model != model
            || quote.unit != "credits"
            || quote.max_credits.as_microcredits() <= 0
            || quote.expires_at_ms <= chrono::Utc::now().timestamp_millis()
        {
            return Err("AI Work 报价与 Core 请求不匹配或已过期".into());
        }
        Ok(BridgeQuoteResult::Quoted(quote))
    }

    pub fn billing(&self, request_id: &str) -> Result<BridgeBillingResult, String> {
        let path = format!("/internal/bridge/requests/{request_id}/billing");
        let response = self.forward("GET", &path, &[], &BTreeMap::new(), request_id)?;
        Self::decode_billing_response(response, request_id, None)
    }

    pub fn finalize_chat_billing(&self, request_id: &str) -> Result<BridgeBillingResult, String> {
        let path = format!("/internal/bridge/requests/{request_id}/billing/finalize-chat");
        let response = self.forward("POST", &path, &[], &BTreeMap::new(), request_id)?;
        Self::decode_billing_response(response, request_id, None)
    }

    pub fn finalize_video_billing(
        &self,
        request_id: &str,
        task_ref: &str,
    ) -> Result<BridgeBillingResult, String> {
        let path = format!("/internal/bridge/requests/{request_id}/billing/finalize");
        let body = serde_json::to_vec(&json!({"task_ref": task_ref}))
            .map_err(|error| format!("编码视频计费最终确认请求失败: {error}"))?;
        let mut headers = BTreeMap::new();
        headers.insert("content-type".into(), "application/json".into());
        let response = self.forward("POST", &path, &body, &headers, request_id)?;
        Self::decode_billing_response(response, request_id, Some(task_ref))
    }

    fn decode_billing_response(
        response: BridgeResponse,
        request_id: &str,
        expected_task_ref: Option<&str>,
    ) -> Result<BridgeBillingResult, String> {
        if !(200..300).contains(&response.status) {
            return Err(format!("AI Work 计费查询返回 HTTP {}", response.status));
        }
        let receipt: BillingResponse = serde_json::from_slice(&response.body)
            .map_err(|error| format!("AI Work 计费回执格式无效: {error}"))?;
        if receipt.request_id != request_id {
            return Err("AI Work 计费回执 request_id 不匹配".into());
        }

        let unit_is_credits = receipt.unit.as_deref() == Some("credits");
        let source_ref = receipt
            .source_ref
            .filter(|value| !value.trim().is_empty());
        let source_is_verified = source_ref.is_some() && receipt.observed_at_ms > 0;
        if let Some(expected_task_ref) = expected_task_ref {
            if receipt.task_ref.as_deref() != Some(expected_task_ref)
                || !source_ref.as_deref().is_some_and(|value| value.starts_with("trae-usage-session:"))
            {
                return Ok(BridgeBillingResult::Unresolved);
            }
        }
        let expected_pre_dispatch_source = format!("aiwork-pre-dispatch-no-charge:{request_id}");
        if source_ref.as_deref().is_some_and(|value| value.starts_with("aiwork-pre-dispatch-no-charge:"))
            && (source_ref.as_deref() != Some(expected_pre_dispatch_source.as_str())
                || receipt.task_ref.is_some()
                || receipt.actual_credits.is_none_or(|amount| amount.as_microcredits() != 0))
        {
            return Ok(BridgeBillingResult::Unresolved);
        }
        let status = match receipt.status.as_str() {
            "final" if unit_is_credits && source_is_verified
                && source_ref.as_deref() == Some(expected_pre_dispatch_source.as_str())
                && receipt.task_ref.is_none()
                && receipt.actual_credits.is_some_and(|amount| amount.as_microcredits() == 0) =>
            {
                BillingReceiptStatus::FailedNoCharge
            }
            "final" if unit_is_credits && source_is_verified && receipt.actual_credits.is_some() => {
                BillingReceiptStatus::Final
            }
            "failed_no_charge"
                if unit_is_credits
                    && source_is_verified
                    && receipt.actual_credits.map_or(true, |amount| amount.as_microcredits() == 0) =>
            {
                BillingReceiptStatus::FailedNoCharge
            }
            _ => return Ok(BridgeBillingResult::Unresolved),
        };
        Ok(BridgeBillingResult::Final(BillingReceipt {
            request_id: receipt.request_id,
            status,
            actual_credits: receipt.actual_credits,
            unit: "credits".into(),
            source_ref: source_ref.expect("verified source checked above"),
            task_ref: receipt.task_ref,
            observed_at_ms: receipt.observed_at_ms,
        }))
    }

    pub fn forward(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        incoming_headers: &BTreeMap<String, String>,
        request_id: &str,
    ) -> Result<BridgeResponse, String> {
        self.forward_with_quote(
            method,
            path,
            body,
            incoming_headers,
            request_id,
            None,
            None,
        )
    }

    pub fn forward_billed(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        incoming_headers: &BTreeMap<String, String>,
        request_id: &str,
        quote_id: &str,
    ) -> Result<BridgeResponse, String> {
        if quote_id.trim().is_empty() {
            return Err("Core 报价编号缺失；拒绝转发付费请求".into());
        }
        self.forward_with_quote(
            method,
            path,
            body,
            incoming_headers,
            request_id,
            Some(quote_id),
            None,
        )
    }

    pub fn forward_billed_for_key(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        incoming_headers: &BTreeMap<String, String>,
        request_id: &str,
        quote_id: &str,
        core_key_id: &str,
    ) -> Result<BridgeResponse, String> {
        if quote_id.trim().is_empty() || core_key_id.trim().is_empty() {
            return Err("Core billing quote or API Key identity is missing".into());
        }
        self.forward_with_quote(
            method,
            path,
            body,
            incoming_headers,
            request_id,
            Some(quote_id),
            Some(core_key_id),
        )
    }

    pub fn forward_controlled_for_key(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        incoming_headers: &BTreeMap<String, String>,
        request_id: &str,
        operation_id: &str,
        core_key_id: &str,
    ) -> Result<BridgeResponse, String> {
        let headers = self.controlled_headers(incoming_headers, request_id, operation_id, core_key_id)?;
        let url = format!("{}{}", self.base_url, normalize_path(path));
        self.transport.send(method, &url, &headers, body)
    }

    /// Only observes an already created AI Work task. Never submits video.
    pub fn controlled_video_task(&self, request_id: &str) -> Result<Option<(String, String)>, String> {
        let path = format!("/internal/bridge/requests/{request_id}/video-task");
        let response = self.forward("GET", &path, &[], &BTreeMap::new(), request_id)?;
        if !(200..300).contains(&response.status) {
            return Err(format!("AI Work 受控视频任务查询返回 HTTP {}", response.status));
        }
        let value: Value = serde_json::from_slice(&response.body)
            .map_err(|error| format!("AI Work 受控视频任务响应无效: {error}"))?;
        if value.get("request_id").and_then(Value::as_str) != Some(request_id) {
            return Err("AI Work 受控视频任务 request_id 不匹配".into());
        }
        if value.get("task").is_some_and(Value::is_null) {
            return Ok(None);
        }
        let task = value.get("task").ok_or("AI Work 受控视频任务字段缺失")?;
        let id = task.get("id").and_then(Value::as_str).filter(|id| !id.trim().is_empty())
            .ok_or("AI Work 受控视频任务 ID 缺失")?;
        let status = task.get("status").and_then(Value::as_str).filter(|status| !status.trim().is_empty())
            .ok_or("AI Work 受控视频任务状态缺失")?;
        Ok(Some((id.into(), status.into())))
    }

    pub fn forward_controlled_stream_for_key(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        incoming_headers: &BTreeMap<String, String>,
        request_id: &str,
        operation_id: &str,
        core_key_id: &str,
    ) -> Result<BridgeStreamingResponse, String> {
        let headers = self.controlled_headers(incoming_headers, request_id, operation_id, core_key_id)?;
        let url = format!("{}{}", self.base_url, normalize_path(path));
        self.transport.send_stream(method, &url, &headers, body)
    }

    fn controlled_headers(
        &self,
        incoming_headers: &BTreeMap<String, String>,
        request_id: &str,
        operation_id: &str,
        core_key_id: &str,
    ) -> Result<BTreeMap<String, String>, String> {
        let valid = |value: &str| {
            !value.is_empty() && value.len() <= 128
                && value.bytes().all(|byte| byte.is_ascii_alphanumeric()
                    || matches!(byte, b'-' | b'_' | b'.' | b':'))
        };
        if !valid(request_id) || !valid(operation_id) || !valid(core_key_id) {
            return Err("Core 受控操作关联编号无效；拒绝转发付费请求".into());
        }
        let mut headers = safe_headers(incoming_headers);
        headers.insert("authorization".into(), format!("Bearer {}", self.bridge_secret));
        headers.insert("x-core-request-id".into(), request_id.into());
        headers.insert("x-core-controlled-operation-id".into(), operation_id.into());
        headers.insert("x-core-key-id".into(), core_key_id.into());
        Ok(headers)
    }

    pub fn forward_billed_stream(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        incoming_headers: &BTreeMap<String, String>,
        request_id: &str,
        quote_id: &str,
    ) -> Result<BridgeStreamingResponse, String> {
        self.forward_billed_stream_inner(method, path, body, incoming_headers, request_id, quote_id, None)
    }

    pub fn forward_billed_stream_for_key(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        incoming_headers: &BTreeMap<String, String>,
        request_id: &str,
        quote_id: &str,
        core_key_id: &str,
    ) -> Result<BridgeStreamingResponse, String> {
        self.forward_billed_stream_inner(
            method,
            path,
            body,
            incoming_headers,
            request_id,
            quote_id,
            Some(core_key_id),
        )
    }

    fn forward_billed_stream_inner(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        incoming_headers: &BTreeMap<String, String>,
        request_id: &str,
        quote_id: &str,
        core_key_id: Option<&str>,
    ) -> Result<BridgeStreamingResponse, String> {
        if quote_id.trim().is_empty() {
            return Err("Core 报价编号缺失；拒绝转发付费请求".into());
        }
        let mut headers = safe_headers(incoming_headers);
        headers.insert("authorization".into(), format!("Bearer {}", self.bridge_secret));
        headers.insert("x-core-request-id".into(), request_id.to_string());
        headers.insert("x-core-quote-id".into(), quote_id.to_string());
        if let Some(core_key_id) = core_key_id.filter(|value| !value.trim().is_empty()) {
            headers.insert("x-core-key-id".into(), core_key_id.to_string());
        }
        headers.remove("x-user-id");
        headers.remove("x-api-key");
        let url = format!("{}{}", self.base_url, normalize_path(path));
        self.transport.send_stream(method, &url, &headers, body)
    }

    fn forward_with_quote(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        incoming_headers: &BTreeMap<String, String>,
        request_id: &str,
        quote_id: Option<&str>,
        core_key_id: Option<&str>,
    ) -> Result<BridgeResponse, String> {
        let mut headers = safe_headers(incoming_headers);
        // Core 用户 Key 永远不会透传给 AI Work；桥接密钥在这一层覆盖。
        headers.insert("authorization".into(), format!("Bearer {}", self.bridge_secret));
        headers.insert("x-core-request-id".into(), request_id.to_string());
        if let Some(quote_id) = quote_id {
            headers.insert("x-core-quote-id".into(), quote_id.to_string());
        }
        if let Some(core_key_id) = core_key_id.filter(|value| !value.trim().is_empty()) {
            headers.insert("x-core-key-id".into(), core_key_id.to_string());
        }
        headers.remove("x-user-id");
        headers.remove("x-api-key");
        let url = format!("{}{}", self.base_url, normalize_path(path));
        self.transport.send(method, &url, &headers, body)
    }

    pub fn upload_asset(
        &self,
        filename: &str,
        mime_type: &str,
        bytes: &[u8],
        request_id: &str,
    ) -> Result<String, String> {
        if filename.trim().is_empty() || filename.len() > 128 || filename.contains('/') || filename.contains('\\') {
            return Err("桥接素材 filename 无效".into());
        }
        if mime_type.trim().is_empty() || mime_type.chars().any(char::is_whitespace) {
            return Err("桥接素材 mime_type 无效".into());
        }
        let body = serde_json::to_vec(&serde_json::json!({
            "filename": filename,
            "mime_type": mime_type,
            "data_base64": STANDARD.encode(bytes),
        })).map_err(|error| format!("编码桥接素材失败: {error}"))?;
        let mut headers = BTreeMap::new();
        headers.insert("content-type".into(), "application/json".into());
        let response = self.forward("POST", "/v1/assets", &body, &headers, request_id)?;
        if !(200..300).contains(&response.status) {
            return Err(format!("AI Work 素材桥接返回 HTTP {}", response.status));
        }
        let value: Value = serde_json::from_slice(&response.body)
            .map_err(|error| format!("AI Work 素材桥接响应不是有效 JSON: {error}"))?;
        value
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .ok_or_else(|| "AI Work 素材桥接响应缺少素材 ID".into())
    }

    fn json_request(&self, method: &str, path: &str, body: &[u8], request_id: Option<&str>) -> Result<Value, String> {
        let mut incoming = BTreeMap::new();
        incoming.insert("accept".into(), "application/json".into());
        let response = self.forward(
            method,
            path,
            body,
            &incoming,
            request_id.unwrap_or("core-control-check"),
        )?;
        if !(200..300).contains(&response.status) {
            return Err(format!("AI Work bridge 返回 HTTP {}", response.status));
        }
        serde_json::from_slice(&response.body).map_err(|e| format!("桥接响应不是有效 JSON: {e}"))
    }
}

fn required_nonempty(value: Option<String>, field: &str) -> Result<String, String> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("AI Work 报价缺少 {field}"))
}

fn normalize_path(path: &str) -> String {
    if path.starts_with('/') { path.to_string() } else { format!("/{path}") }
}

fn safe_headers(input: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    input
        .iter()
        .filter(|(key, _)| matches!(key.as_str(), "accept" | "content-type" | "idempotency-key"))
        .map(|(key, value)| (key.to_ascii_lowercase(), value.clone()))
        .collect()
}

fn read_bounded_response(reader: impl Read, max_bytes: u64) -> Result<Vec<u8>, String> {
    let mut data = Vec::new();
    reader.take(max_bytes + 1).read_to_end(&mut data)
        .map_err(|error| format!("读取桥接响应失败: {error}"))?;
    if data.len() as u64 > max_bytes {
        return Err(format!("桥接响应超过 {max_bytes} 字节上限；已拒绝截断的视频文件"));
    }
    Ok(data)
}

struct HttpBridgeTransport;

impl BridgeTransport for HttpBridgeTransport {
    fn send(
        &self,
        method: &str,
        url: &str,
        headers: &BTreeMap<String, String>,
        body: &[u8],
    ) -> Result<BridgeResponse, String> {
        let response = Self::send_request(method, url, headers, body)?;
        let status = response.status() as u16;
        let content_type = response.header("content-type").map(ToString::to_string);
        let data = read_bounded_response(response.into_reader(), 64 * 1024 * 1024)?;
        let mut response_headers = BTreeMap::new();
        if let Some(value) = content_type {
            response_headers.insert("content-type".into(), value);
        }
        Ok(BridgeResponse { status, headers: response_headers, body: data })
    }

    fn send_stream(
        &self,
        method: &str,
        url: &str,
        headers: &BTreeMap<String, String>,
        body: &[u8],
    ) -> Result<BridgeStreamingResponse, String> {
        let response = Self::send_request(method, url, headers, body)?;
        let status = response.status() as u16;
        let mut response_headers = BTreeMap::new();
        if let Some(value) = response.header("content-type") {
            response_headers.insert("content-type".into(), value.to_string());
        }
        Ok(BridgeStreamingResponse {
            status,
            headers: response_headers,
            body: Box::new(response.into_reader()),
        })
    }
}

impl HttpBridgeTransport {
    fn send_request(
        method: &str,
        url: &str,
        headers: &BTreeMap<String, String>,
        body: &[u8],
    ) -> Result<ureq::Response, String> {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(120))
            .timeout_write(Duration::from_secs(30))
            .redirects(0)
            .build();
        let mut request = agent.request(method, url);
        for (key, value) in headers {
            request = request.set(key, value);
        }
        match request.send_bytes(body) {
            Ok(response) => Ok(response),
            Err(ureq::Error::Status(_, response)) => Ok(response),
            Err(error) => return Err(format!("桥接网络请求失败: {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{read_bounded_response, BridgeBillingResult, BridgeClient, BridgeResponse, BridgeTransport};
    use aiwork_core::BillingReceiptStatus;
    use std::{collections::BTreeMap, io::{Read, Write}, net::TcpListener, sync::{Arc, Mutex}, thread, time::Duration};

    struct RecordingBridge {
        last_headers: Mutex<BTreeMap<String, String>>,
        last_request_id: Mutex<Option<String>>,
        last_url: Mutex<String>,
        last_body: Mutex<Vec<u8>>,
        response_status: Mutex<u16>,
        response_body: Mutex<Vec<u8>>,
    }

    impl Default for RecordingBridge {
        fn default() -> Self {
            Self::responding_with_body(200, br#"{}"#)
        }
    }

    impl RecordingBridge {
        fn responding_with_asset(id: &str) -> Self {
            Self::responding_with_body(200, format!(r#"{{"object":"asset","id":"{id}"}}"#).as_bytes())
        }

        fn responding_with_body(status: u16, body: &[u8]) -> Self {
            Self {
                last_headers: Mutex::new(BTreeMap::new()),
                last_request_id: Mutex::new(None),
                last_url: Mutex::new(String::new()),
                last_body: Mutex::new(Vec::new()),
                response_status: Mutex::new(status),
                response_body: Mutex::new(body.to_vec()),
            }
        }

        fn last_path(&self) -> String {
            self.last_url.lock().unwrap().trim_start_matches("http://bridge").to_string()
        }

        fn last_headers(&self) -> BTreeMap<String, String> {
            self.last_headers.lock().unwrap().clone()
        }

        fn last_body_string(&self) -> String {
            String::from_utf8_lossy(&self.last_body.lock().unwrap()).to_string()
        }
    }

    impl BridgeTransport for RecordingBridge {
        fn send(&self, _method: &str, url: &str, headers: &BTreeMap<String, String>, body: &[u8]) -> Result<BridgeResponse, String> {
            *self.last_headers.lock().unwrap() = headers.clone();
            *self.last_request_id.lock().unwrap() = headers.get("x-core-request-id").cloned();
            *self.last_url.lock().unwrap() = url.to_owned();
            *self.last_body.lock().unwrap() = body.to_vec();
            Ok(BridgeResponse { status: *self.response_status.lock().unwrap(), headers: BTreeMap::new(), body: self.response_body.lock().unwrap().clone() })
        }
    }

    #[test]
    fn upstream_credit_snapshot_requires_fresh_exact_decimal_totals() {
        let now = chrono::Utc::now().timestamp_millis();
        let body = serde_json::to_vec(&serde_json::json!({
            "upstream_credits": {
                "video_available": "10.250000",
                "value": "10.250000",
                "source": "aiwork-upstream-aggregate",
                "fresh": true,
                "updated_at": now
            }
        }))
        .unwrap();
        let recording = Arc::new(RecordingBridge::responding_with_body(200, &body));
        let client = BridgeClient::from_transport("http://bridge", "bridge-secret", recording);
        let snapshot = client.upstream_credit_snapshot().unwrap();
        assert_eq!(snapshot.total.to_string(), "10.250000");
        assert_eq!(snapshot.updated_at_ms, now);

        let stale_body = serde_json::to_vec(&serde_json::json!({
            "upstream_credits": {
                "video_available": "10.250000",
                "value": "10.250000",
                "source": "aiwork-upstream-aggregate",
                "fresh": false,
                "error_code": "upstream_balance_stale",
                "updated_at": now - 300_001
            }
        }))
        .unwrap();
        let stale = BridgeClient::from_transport(
            "http://bridge",
            "bridge-secret",
            Arc::new(RecordingBridge::responding_with_body(200, &stale_body)),
        );
        assert!(stale.upstream_credit_snapshot().is_err());

        let imprecise_body = serde_json::to_vec(&serde_json::json!({
            "upstream_credits": {
                "video_available": "10.1234567",
                "value": "10.1234567",
                "source": "aiwork-upstream-aggregate",
                "fresh": true,
                "updated_at": now
            }
        }))
        .unwrap();
        let imprecise = BridgeClient::from_transport(
            "http://bridge",
            "bridge-secret",
            Arc::new(RecordingBridge::responding_with_body(200, &imprecise_body)),
        );
        assert!(imprecise.upstream_credit_snapshot().is_err());
    }

    #[test]
    fn numeric_upstream_credit_normalizes_only_float_noise() {
        let live_value = serde_json::Number::from_f64(24763.920000000002).unwrap();
        let parsed = super::parse_numeric_credit_amount(live_value).unwrap();
        assert_eq!(parsed.to_string(), "24763.920000");

        let meaningful_extra_precision = serde_json::Number::from_f64(10.1234564).unwrap();
        assert!(super::parse_numeric_credit_amount(meaningful_extra_precision).is_err());

        let negative = serde_json::Number::from_f64(-0.1).unwrap();
        assert!(super::parse_numeric_credit_amount(negative).is_err());

        let imprecise_large_balance = serde_json::Number::from_f64(1_000_000_000.0).unwrap();
        assert!(super::parse_numeric_credit_amount(imprecise_large_balance).is_err());
    }

    #[test]
    fn forwarding_overwrites_user_authorization_and_request_id() {
        let recording = Arc::new(RecordingBridge::default());
        let client = BridgeClient::from_transport("http://bridge", "bridge-secret", recording.clone());
        let mut headers = BTreeMap::new();
        headers.insert("authorization".into(), "Bearer user-key".into());
        headers.insert("x-api-key".into(), "user-key".into());
        headers.insert("x-core-request-id".into(), "client-forged".into());
        client.forward("POST", "/v1/chat/completions", b"{}", &headers, "server-request").unwrap();
        let captured = recording.last_headers.lock().unwrap().clone();
        assert_eq!(captured.get("authorization"), Some(&"Bearer bridge-secret".to_string()));
        assert!(!captured.contains_key("x-api-key"));
        assert_eq!(captured.get("x-core-request-id"), Some(&"server-request".to_string()));
    }

    #[test]
    fn billed_forward_overwrites_untrusted_key_id_and_keeps_user_secret_out() {
        let recording = Arc::new(RecordingBridge::default());
        let client = BridgeClient::from_transport("http://bridge", "bridge-secret", recording.clone());
        let mut headers = BTreeMap::new();
        headers.insert("x-core-key-id".into(), "client-forged-key".into());
        headers.insert("authorization".into(), "Bearer aw_live_never_forward".into());
        client.forward_billed_for_key(
            "POST", "/v1/chat/completions", b"{}", &headers,
            "server-request", "server-quote", "key_server_owned",
        ).unwrap();
        let captured = recording.last_headers();
        assert_eq!(captured.get("x-core-request-id").map(String::as_str), Some("server-request"));
        assert_eq!(captured.get("x-core-key-id").map(String::as_str), Some("key_server_owned"));
        assert_eq!(captured.get("authorization").map(String::as_str), Some("Bearer bridge-secret"));
        assert!(!captured.values().any(|value| value.contains("aw_live_never_forward")));
    }

    #[test]
    fn controlled_video_task_lookup_requires_matching_request_identity() {
        let body = br#"{"request_id":"req-one","task":{"id":"video-one","status":"processing"}}"#;
        let client = BridgeClient::from_transport("http://bridge", "secret",
            Arc::new(RecordingBridge::responding_with_body(200, body)));
        assert_eq!(client.controlled_video_task("req-one").unwrap(), Some(("video-one".into(), "processing".into())));
        assert!(client.controlled_video_task("req-other").is_err());
    }

    #[test]
    fn pre_dispatch_zero_receipt_is_failed_no_charge_not_successful_video() {
        let body = br#"{"request_id":"req-zero","status":"final","actual_credits":"0.000000","unit":"credits","source_ref":"aiwork-pre-dispatch-no-charge:req-zero","task_ref":null,"observed_at_ms":1790000000000}"#;
        let client = BridgeClient::from_transport("http://bridge", "secret",
            Arc::new(RecordingBridge::responding_with_body(200, body)));
        let BridgeBillingResult::Final(receipt) = client.billing("req-zero").unwrap() else {
            panic!("verified zero receipt must be final");
        };
        assert_eq!(receipt.status, BillingReceiptStatus::FailedNoCharge);
        assert_eq!(receipt.actual_credits.unwrap().as_microcredits(), 0);
    }

    #[test]
    fn controlled_bridge_headers_use_server_identity_only() {
        let recording = Arc::new(RecordingBridge::default());
        let client = BridgeClient::from_transport("http://bridge", "bridge-secret", recording.clone());
        let headers = BTreeMap::from([
            ("x-core-controlled-operation-id".into(), "client-forged".into()),
            ("x-core-quote-id".into(), "client-fake-quote".into()),
            ("x-core-key-id".into(), "client-key".into()),
            ("authorization".into(), "Bearer user-key".into()),
        ]);
        client.forward_controlled_for_key("POST", "/v1/chat/completions", b"{}", &headers,
            "request-assist", "operation-parent", "key-server").unwrap();
        let seen = recording.last_headers();
        assert_eq!(seen.get("x-core-controlled-operation-id").map(String::as_str), Some("operation-parent"));
        assert_eq!(seen.get("x-core-request-id").map(String::as_str), Some("request-assist"));
        assert_eq!(seen.get("x-core-key-id").map(String::as_str), Some("key-server"));
        assert_eq!(seen.get("authorization").map(String::as_str), Some("Bearer bridge-secret"));
        assert!(!seen.contains_key("x-core-quote-id"));

        client.forward_controlled_stream_for_key("POST", "/v1/chat/completions", b"{}", &headers,
            "request-video", "operation-parent", "key-server").unwrap();
        let stream_seen = recording.last_headers();
        assert_eq!(stream_seen.get("x-core-request-id").map(String::as_str), Some("request-video"));
        assert_eq!(stream_seen.get("x-core-controlled-operation-id").map(String::as_str), Some("operation-parent"));
        assert!(!stream_seen.contains_key("x-core-quote-id"));
        assert!(client.forward_controlled_for_key("POST", "/v1/chat/completions", b"{}", &headers,
            "request-assist", "bad\noperation", "key-server").is_err());
    }

    #[test]
    fn key_registry_sync_sends_only_safe_metadata_to_the_bridge() {
        let recording = Arc::new(RecordingBridge::responding_with_body(200, br#"{"status":"applied"}"#));
        let client = BridgeClient::from_transport("http://bridge", "bridge-secret", recording.clone());
        client.sync_core_key_registry(42, vec![("key_opaque".into(), "周的电脑".into(), true)]).unwrap();
        let body = recording.last_body_string();
        assert!(body.contains("key_opaque"));
        assert!(body.contains("周的电脑"));
        assert!(!body.contains("aw_live_"));
        assert!(!body.contains("prefix"));
        assert!(!body.contains("user_id"));
        let headers = recording.last_headers();
        assert_eq!(headers.get("authorization").map(String::as_str), Some("Bearer bridge-secret"));
    }

    #[test]
    fn upload_asset_uses_bridge_authorization_and_returns_aiwork_asset_id() {
        let recording = Arc::new(RecordingBridge::responding_with_asset("bridge-asset-1"));
        let client = BridgeClient::from_transport("http://bridge", "bridge-secret", recording.clone());
        let id = client.upload_asset("ref.png", "image/png", b"png", "asset-request").unwrap();
        assert_eq!(id, "bridge-asset-1");
        assert_eq!(recording.last_path(), "/v1/assets");
        assert_eq!(recording.last_headers()["authorization"], "Bearer bridge-secret");
        assert_eq!(recording.last_headers()["content-type"], "application/json");
        assert!(!recording.last_body_string().contains("user-key"));
    }

    #[test]
    fn malformed_bridge_asset_response_is_an_error() {
        let recording = Arc::new(RecordingBridge::responding_with_body(200, br#"{"object":"asset"}"#));
        let client = BridgeClient::from_transport("http://bridge", "bridge-secret", recording);
        let error = client.upload_asset("ref.png", "image/png", b"png", "asset-request").unwrap_err();
        assert!(error.contains("素材 ID"));
    }

    #[test]
    fn oversized_bridge_body_is_rejected_instead_of_returning_truncated_success() {
        let error = read_bounded_response(std::io::Cursor::new(b"12345"), 4).unwrap_err();
        assert!(error.contains("上限"));
        assert_eq!(read_bounded_response(std::io::Cursor::new(b"1234"), 4).unwrap(), b"1234");
    }

    #[test]
    fn bridge_does_not_follow_video_redirect_with_its_secret() {
        let destination = TcpListener::bind("127.0.0.1:0").unwrap();
        destination.set_nonblocking(true).unwrap();
        let destination_url = format!("http://{}", destination.local_addr().unwrap());
        let destination_thread = thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while std::time::Instant::now() < deadline {
                match destination.accept() {
                    Ok((mut stream, _)) => {
                        let mut request = [0u8; 2048];
                        let _ = stream.read(&mut request);
                        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK");
                        return true;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(10)),
                    Err(error) => panic!("unexpected accept error: {error}"),
                }
            }
            false
        });
        let source = TcpListener::bind("127.0.0.1:0").unwrap();
        let source_url = format!("http://{}", source.local_addr().unwrap());
        let source_thread = thread::spawn(move || {
            let (mut stream, _) = source.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request).unwrap();
            write!(stream, "HTTP/1.1 302 Found\r\nLocation: {destination_url}/secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let response = BridgeClient::new(source_url, "bridge-secret")
            .forward("GET", "/v1/videos/video-1/content", &[], &BTreeMap::new(), "request-1").unwrap();
        source_thread.join().unwrap();
        assert_eq!(response.status, 302);
        assert!(!destination_thread.join().unwrap(), "bridge must not contact the redirect target");
    }
}
