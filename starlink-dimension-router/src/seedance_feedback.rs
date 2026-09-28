//! Client-visible text is derived from verified workflow events, never invented by a model.
use serde_json::{json,Value};

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub(crate) enum Stage {Received,Assistant,References,Submitting,Processing,Delivery,QueryDelayed}
impl Stage {
    fn name(self)->&'static str {match self {
        Self::Received=>"received",Self::Assistant=>"organizing_prompt",Self::References=>"processing_references",
        Self::Submitting=>"submitting_video",Self::Processing=>"processing",Self::Delivery=>"preparing_download",Self::QueryDelayed=>"status_query_delayed",
    }}
    fn message(self)->&'static str {match self {
        Self::Received=>"请求已接收，正在确认任务状态。",
        Self::Assistant=>"正在识别请求并整理提示词。",
        Self::References=>"正在处理本次请求携带的参考素材。",
        Self::Submitting=>"正在准备额度并提交视频任务。",
        Self::Processing=>"视频任务已提交，最近一次成功查询显示任务在处理中；继续等待同一任务。",
        Self::Delivery=>"视频已生成，正在准备本机下载工具；是否保存成功以本机工具回报为准。",
        Self::QueryDelayed=>"暂时无法取得最新任务状态，系统正在继续查询同一任务；本次查询异常尚不能判断视频生成失败。",
    }}
}
#[derive(Clone)]
pub(crate) struct Progress {pub stage:Stage,pub last_confirmed_at_ms:Option<i64>}
impl Default for Progress {fn default()->Self {Self {stage:Stage::Received,last_confirmed_at_ms:None}}}

pub(crate) fn progress_event(request:&str,progress:&Progress,elapsed_secs:u64)->Vec<u8> {
    let text=if elapsed_secs>=20 {format!("{} 本次连接已等待{elapsed_secs}秒。\n",progress.stage.message())}
        else {format!("{}\n",progress.stage.message())};
    let value=json!({"id":format!("chatcmpl-{request}"),"object":"chat.completion.chunk","model":"seedance",
        "created":chrono::Utc::now().timestamp(),"request_id":request,
        "task_progress":{"stage":progress.stage.name(),"last_confirmed_at_ms":progress.last_confirmed_at_ms,"connection_wait_seconds":elapsed_secs},
        "choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}]});
    format!("data: {value}\n\n").into_bytes()
}

pub(crate) fn video_failure(result:&Value)->&'static str {
    let reason=result["error"].as_str().or_else(||result.pointer("/error/message").and_then(Value::as_str)).unwrap_or("").trim().to_ascii_lowercase();
    match reason.as_str() {
        "video security check failed"=>"video_safety_check_failed",
        "image security check failed"=>"reference_safety_check_failed",
        "text security check failed"=>"prompt_safety_check_failed",
        _=>"video_execution_failed",
    }
}
pub(crate) fn message(code:&str)->&'static str {match code {
    "video_safety_check_failed"=>"上游返回：视频安全检查未通过，本次生成失败。",
    "reference_safety_check_failed"=>"上游返回：参考图片安全检查未通过，本次生成失败。",
    "prompt_safety_check_failed"=>"上游返回：提示词安全检查未通过，本次生成失败。",
    "video_execution_failed"=>"上游已确认视频生成失败，但未提供可公开确认的具体原因。",
    "budget_execution_wait_timeout"=>"本次等待已超时，尚未取得最终生成结果；可继续查询同一任务。",
    "key_concurrency_exceeded"=>"当前 Key 的任务并发已达上限，请等待已有任务结束。",
    "quota_insufficient"=>"当前 Key 可用积分不足，无法提交本次任务。",
    "budget_policy_unconfigured"=>"当前模型缺少计费配置，无法继续提交任务。",
    "video_billing_paused"=>"视频提交目前处于暂停状态。",
    "budget_not_sent"=>"本次任务未发送到视频上游。",
    "stream_observer_limit"=>"系统当前等待连接已达全局上限，请稍后连接同一任务。",
    "budget_preparation_busy"=>"系统正在处理较多任务，请稍后重试。",
    _=>"暂时无法完成本次任务结果的核验，请保留任务编号以便继续查询。",
}}
pub(crate) fn failure_value(request:&str,code:&str,settled:bool)->Value {
    let billing=if settled {"积分已结算，可在 Key 使用记录查看实际扣费。"} else {"积分尚在核对，系统会按真实回执结算。"};
    let text=format!("{} {billing}",message(code));
    json!({"id":format!("chatcmpl-{request}"),"object":"chat.completion.chunk","model":"seedance","created":chrono::Utc::now().timestamp(),
        "request_id":request,"error":{"type":"api_error","code":code,"message":text,"request_id":request,"billing_state":if settled {"settled"} else {"pending"}},
        "choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":"stop"}]})
}
