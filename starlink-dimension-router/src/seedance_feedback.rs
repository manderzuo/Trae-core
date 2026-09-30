//! Client-visible text is derived from verified workflow events, never invented by a model.
use serde_json::{json,Value};

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub(crate) enum Stage {Received,Restoring,Assistant,References,Tail,SourceVideo,Submitting,SubmittingSegment,Processing,Delivery,QueryDelayed}
impl Stage {
    fn name(self)->&'static str {match self {
        Self::SourceVideo=>"preparing_parent_video",
        Self::Received=>"received",Self::Restoring=>"restoring_version",Self::Tail=>"preparing_tail_frame",Self::SubmittingSegment=>"submitting_segment",Self::Assistant=>"organizing_prompt",Self::References=>"processing_references",
        Self::Submitting=>"submitting_video",Self::Processing=>"processing",Self::Delivery=>"preparing_download",Self::QueryDelayed=>"status_query_delayed",
    }}
    fn message(self)->&'static str {match self {
        Self::SourceVideo=>"正在读取指定父版本的完整视频，用于续写新片段；尚未提交生成。",
        Self::Received=>"已接收请求，正在确认任务。",
        Self::Restoring=>"正在恢复指定版本的提示词、规格与参考素材。",
        Self::Tail=>"正在准备指定父版本的尾帧；新视频片段尚未提交。",
        Self::SubmittingSegment=>"正在提交续写的新片段，原视频任务不会重发。",
        Self::Assistant=>"正在整理提示词。",
        Self::References=>"正在处理参考素材。",
        Self::Submitting=>"正在提交视频任务。",
        Self::Processing=>"任务已提交，正在生成。",
        Self::Delivery=>"视频已生成，正在准备自动下载。",
        Self::QueryDelayed=>"暂时无法获取最新状态，系统会继续查询；无需重新提交。",
    }}
    fn waiting_message(self)->&'static str {match self {
        Self::SourceVideo=>"仍在读取父版本完整视频；新片段尚未提交。",
        Self::Received=>"仍在确认任务状态。",
        Self::Restoring=>"仍在恢复指定版本。",
        Self::Tail=>"仍在准备父视频尾帧；新片段尚未提交。",
        Self::SubmittingSegment=>"新片段尚在提交过程中。",
        Self::Assistant=>"仍在整理提示词。",
        Self::References=>"仍在处理参考素材。",
        Self::Submitting=>"任务尚在提交过程中。",
        Self::Processing=>"上游尚未返回完成结果；系统继续等待，无需重新提交。",
        Self::Delivery=>"仍在准备自动下载。",
        Self::QueryDelayed=>"暂未取得最新状态；系统继续查询，无需重新提交。",
    }}
}
#[derive(Clone)]
pub(crate) struct Progress {pub stage:Stage,pub last_confirmed_at_ms:Option<i64>}
impl Default for Progress {fn default()->Self {Self {stage:Stage::Received,last_confirmed_at_ms:None}}}

pub(crate) fn progress_event(request:&str,progress:&Progress,elapsed_secs:u64)->Vec<u8> {
    event(request,progress,elapsed_secs,"stage",progress.stage.message())
}
pub(crate) fn waiting_event(request:&str,progress:&Progress,elapsed_secs:u64)->Vec<u8> {
    let text=format!("本次已等待{}分钟，{}",elapsed_secs/60,progress.stage.waiting_message());
    event(request,progress,elapsed_secs,"waiting",&text)
}
fn event(request:&str,progress:&Progress,elapsed_secs:u64,kind:&str,text:&str)->Vec<u8> {
    // Chat clients append content deltas and treat a single newline as a soft
    // break. Complete paragraphs prevent stage updates from becoming one wall.
    let text=format!("{text}\n\n");
    let value=json!({"id":format!("chatcmpl-{request}"),"object":"chat.completion.chunk","model":"seedance",
        "created":chrono::Utc::now().timestamp(),"request_id":request,
        "task_progress":{"kind":kind,"stage":progress.stage.name(),"last_confirmed_at_ms":progress.last_confirmed_at_ms,"connection_wait_seconds":elapsed_secs},
        "choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}]});
    format!("data: {value}\n\n").into_bytes()
}

pub(crate) fn video_failure(result:&Value)->&'static str {
    let detail=aiwork_core::UpstreamFailure::from_value(result);
    let reason=detail.message.as_deref().unwrap_or("").trim().to_ascii_lowercase();
    match reason.as_str() {
        "video security check failed"=>"video_safety_check_failed",
        "image security check failed"=>"reference_safety_check_failed",
        "text security check failed"=>"prompt_safety_check_failed",
        _=>"video_execution_failed",
    }
}

/// Only verified result payloads may populate upstream details. Infrastructure
/// errors use the safe code allowlist and never expose raw Rust/network errors.
#[derive(Clone,Debug,PartialEq,Eq)]
pub(crate) struct Failure {
    pub code: &'static str,
    pub upstream: Option<aiwork_core::UpstreamFailure>,
    pub stage: Option<&'static str>,
}
impl From<&str> for Failure {
    fn from(code:&str)->Self {Self {code:crate::budget_errors::public_code(code).unwrap_or("seedance_budget_execution_failed"),upstream:None,stage:None}}
}
impl From<String> for Failure {fn from(code:String)->Self {Self::from(code.as_str())}}
impl Failure {
    pub fn video(result:&Value)->Self {
        Self {code:video_failure(result),upstream:Some(aiwork_core::UpstreamFailure::from_value(result)),stage:Some("video_execution")}
    }
    pub fn diagnostic(value:&Value)->Self {
        let mut failure=Self::from(value["code"].as_str().unwrap_or("assist_result_unconfirmed"));
        failure.stage=match value["stage"].as_str() {
            Some("assistant_connection")=>Some("assistant_connection"),
            Some("assistant_http")=>Some("assistant_http"),
            Some("assistant_stream")=>Some("assistant_stream"),
            _=>None,
        };
        if value["upstream_error"].is_object() {failure.upstream=Some(aiwork_core::UpstreamFailure::from_value(value));}
        failure
    }
    pub fn assistant(result:&Value)->Self {
        if result["error"].is_object() {
            let mut diagnostic=result["error"].clone();
            if result["upstream_error"].is_object() {diagnostic["upstream_error"]=result["upstream_error"].clone();}
            Self::diagnostic(&diagnostic)
        } else {Self::from("assist_execution_failed")}
    }
}
pub(crate) fn message(code:&str)->&'static str {match code {
    "chat_upstream_error"=>"文字模型上游明确返回错误，本轮文字模型执行未完成；不会自动重发付费请求。",
    "chat_connection_timeout"=>"请求文字模型上游时网络超时（连接、写入或等待响应阶段），未取得有效响应；本轮结果尚未确认。",
    "chat_dns_failed"=>"文字模型上游域名解析失败，未取得有效响应；本轮结果尚未确认。",
    "chat_tls_failed"=>"连接文字模型上游时 TLS 校验失败，未取得有效响应；本轮结果尚未确认。",
    "chat_transport_failed"=>"与文字模型上游通信失败，未取得最终结果。",
    "chat_stream_read_timeout"=>"文字模型响应流读取超时，未收到完整结束信号；不能据此判定上游未执行或未扣费。",
    "chat_stream_read_failed"=>"文字模型响应流中断，未收到完整结果；不能据此判定上游未执行或未扣费。",
    "chat_stream_incomplete"=>"文字模型响应流已结束，但没有完整结束信号；结果尚未确认。",
    "chat_stream_invalid_event"=>"文字模型上游返回了无法解析的流事件，未取得完整结果。",
    "chat_stream_too_large"=>"文字模型响应超过安全处理长度，未取得可核验的完整结果。",
    "chat_stream_progress_timeout"=>"文字模型长时间没有新增有效内容，已停止本轮等待；执行和扣费仍待核对，不会自动重发。",
    "chat_stream_total_timeout"=>"文字模型响应超过本轮总时限，已停止等待；执行和扣费仍待核对，不会自动重发。",
    "chat_execution_unknown"=>"文字模型响应流已关闭，但完整执行结果尚未确认；积分保留待核对，不会自动重发。",
    "chat_stream_unavailable"|"chat_result_unavailable"=>"暂时无法读取文字模型响应或执行结果；这是查询链路异常，不代表上游任务未执行。",
    "chat_stream_invalid_page"|"chat_stream_cursor_unavailable"=>"文字模型响应流的读取位置或数据未通过校验；本轮已停止输出，执行和扣费仍待核对。",
    "chat_result_invalid"=>"文字模型返回的执行结果未通过格式校验；不会将不完整结果当成成功。",
    "chat_invalid_tool_calls"|"chat_invalid_tool_index"|"chat_tool_limit"|"chat_invalid_tool_function"|
    "chat_invalid_tool_identity"|"chat_tool_identity_conflict"|"chat_invalid_tool_arguments"|"chat_incomplete_tool_identity"=>
        "文字模型上游返回的工具调用格式、标识或参数未通过校验；未执行这些工具，执行和扣费仍待核对。",
    "chat_output_after_completion"=>"文字模型上游在结束信号后仍返回输出，响应未通过完整性校验。",
    "assistant_json_invalid"=>"辅助模型返回的 JSON 语法不合法（如台词引号未转义或多余内容）；本次未提交视频，请保留请求编号供排查。",
    "assistant_json_duplicate_key"=>"辅助模型返回了重复的 JSON 字段，无法确定唯一规划；本次未提交视频。",
    "assistant_json_invalid_escape"=>"辅助模型返回的 JSON 含非法转义或损坏的 Unicode 编码；本次未提交视频。",
    "assistant_json_control_character"=>"辅助模型返回的 JSON 字符串含未转义的换行或控制字符；本次未提交视频。",
    "assistant_json_not_object"=>"辅助模型返回的 JSON 不是规定的单一对象；本次未提交视频。",
    "assistant_output_truncated"=>"辅助模型输出被截断或未完整结束，无法安全读取规划；本次未提交视频。",
    "assistant_output_too_large"=>"辅助模型返回的规划超过允许长度；本次未提交视频。",
    "assistant_output_blocked"=>"辅助模型输出被内容过滤阻止；本次未提交视频，这不是视频生成阶段的安全检查结果。",
    "assistant_output_invalid"=>"辅助模型未返回唯一、正常结束的文本规划（可能返回了工具调用或拒绝）；本次未提交视频。",
    "assistant_schema_invalid"=>"辅助模型返回的 JSON 格式或字段不符合规定结构，可能缺少字段、类型错误或出现未允许字段；本次未提交视频。",
    "work_spec_unsupported"=>"本轮视频规格超出支持范围或存在冲突；时长支持4至15秒，分辨率支持480P或720P，也可用横屏、竖屏、高清等描述。本次未提交视频。",
    "work_decision_invalid"|"assist_result_invalid"=>"辅助模型返回的规划格式或字段未通过校验；本次未提交视频，请保留请求编号供排查。",
    "assist_execution_failed"=>"辅助模型执行失败；本次尚未提交视频。积分仍按该辅助步骤的真实账单核对。",
    "assist_result_unconfirmed"=>"辅助模型的执行结果尚未确认，视频尚未提交；系统保留原请求与积分预占继续核对，不会自动重发付费请求。",
    "work_parent_required"=>"本次续写或修改没有找到可用的上一版视频，请在原对话继续或指定已有视频版本；本次未提交视频。",
    "reference_video_format_unsupported"=>"当前视频处理不支持该素材格式；提取尾帧请上传 MP4，本次未提交视频。",
    "reference_video_metadata_invalid"=>"参考视频的时长或时间信息未通过校验；本次未提交视频，请检查素材文件或重新导出。",
    "reference_asset_type_mismatch"=>"参考素材的实际类型与请求声明不一致；本次未提交视频，请重新选择对应类型的素材。",
    "source_video_not_ready"=>"父视频尚未确认完成，未提交续写片段。",
    "source_video_unavailable"|"source_video_invalid"|"source_video_identity_invalid"=>"无法读取并核验指定父视频，已停止本次续写；没有改用尾帧或无素材生成，原视频不受影响。",
    "continuation_disabled"=>"续写功能尚未启用；本次未提交新的视频片段。",
    "continuation_mode_unsupported"=>"上游尚未验证所选续写模式；本次未提交新的视频片段。",
    "work_parent_unavailable"=>"指定的父视频版本不可用，不能续写；请检查版本编号与权限。",
    "frame_result_not_ready"=>"父视频尚未确认完成，暂不能提取尾帧；本次未提交新片段。",
    "frame_extractor_busy"=>"尾帧处理资源暂忙，请稍后继续同一请求；不会重新生成父视频。",
    "frame_extractor_unconfigured"=>"AI Work 尚未配置独立的尾帧提取工具；本次已停止、未提交新视频，请管理员安装并配置专用 FFmpeg。",
    "frame_extractor_unavailable"=>"AI Work 的尾帧提取工具文件缺失、损坏或不可读取；本次已停止、未提交新视频，请管理员修复专用 FFmpeg。",
    "frame_extractor_digest_mismatch"=>"AI Work 的尾帧提取程序与固定版本指纹不一致；这不是上传视频的指纹问题。本次已停止、未提交新视频，请管理员核验专用 FFmpeg。",
    "frame_extraction_unavailable"|"frame_output_invalid"|"frame_identity_invalid"=>"未取得来源可核验的尾帧，已停止续写；原视频和原任务结算不受影响。",
    "continuation_capabilities_unavailable"=>"暂时无法读取上游续写能力，未提交新的视频片段。",
    "video_safety_check_failed"=>"上游返回：视频安全检查未通过，本次生成失败。",
    "reference_safety_check_failed"=>"上游返回：参考图片安全检查未通过，本次生成失败。",
    "prompt_safety_check_failed"=>"上游返回：提示词安全检查未通过，本次生成失败。",
    "video_execution_failed"=>"上游已确认视频生成失败，但未提供可公开确认的具体原因。",
    "budget_execution_wait_timeout"=>"本次等待已超时，尚未取得最终生成结果；可继续查询同一任务。",
    "key_concurrency_exceeded"=>"当前 Key 的任务并发已达上限，请等待已有任务结束。",
    "quota_insufficient"=>"当前 Key 可用积分不足，无法提交本次任务。",
    "budget_policy_unconfigured"=>"当前模型缺少计费配置，无法继续提交任务。",
    "video_billing_paused"=>"视频提交目前处于暂停状态。",
    "video_continuation_not_active"=>"该任务已结束，不能再次提交视频；请查询原任务状态。需要重新生成时请发起新请求。",
    "budget_not_sent"=>"本次任务未发送到视频上游。",
    "budget_failure_reason_unavailable"=>"本次视频任务未提交成功，具体失败原因未保存；请保留请求编号供排查。",
    "budget_policy_expired"=>"当前视频预冻结基准已过期，尚未提交视频。",
    "budget_policy_invalid"=>"当前视频预冻结配置未通过校验，尚未提交视频。",
    "stream_observer_limit"=>"系统当前等待连接已达全局上限，请稍后连接同一任务。",
    "budget_preparation_busy"=>"系统正在处理较多任务，请稍后重试。",
    _=>"暂时无法完成本次任务结果的核验，请保留任务编号以便继续查询。",
}}
pub(crate) fn failure_value(request:&str,failure:&Failure,settled:bool)->Value {
    let code=failure.code;
    let billing=if settled {"积分已结算，可在 Key 使用记录查看实际扣费。"} else {"积分尚在核对，系统会按真实回执结算。"};
    let reason=match &failure.upstream {
        Some(detail) if code=="video_execution_failed"=>format!("上游返回：{}。",detail.description()),
        Some(detail)=>format!("{} 上游原始说明：{}。",message(code),detail.description()),
        None=>message(code).into(),
    };
    let text=format!("{reason} {billing}");
    let mut value=json!({"id":format!("chatcmpl-{request}"),"object":"chat.completion.chunk","model":"seedance","created":chrono::Utc::now().timestamp(),
        "request_id":request,"error":{"type":"api_error","code":code,"message":text,"request_id":request,"billing_state":if settled {"settled"} else {"pending"}},
        "choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":"stop"}]});
    if let Some(detail)=&failure.upstream {value["error"]["upstream"]=json!(detail);}
    if let Some(stage)=failure.stage {value["error"]["stage"]=json!(stage);}
    value
}

/// Once SSE is open, provider failures are application results, not broken
/// OpenAI protocol frames. Clients often discard content in a top-level error.
pub(crate) fn readable_failure(mut value:Value)->Value {
    if let Some(error)=value.as_object_mut().and_then(|v|v.remove("error")) {value["task_error"]=error;}
    value
}

#[cfg(test)]
mod work_feedback_tests {
    #[test]
    fn continuation_stages_are_truthful_and_do_not_claim_submission_early() {
        let tail=super::Stage::Tail.message();
        assert!(tail.contains("尾帧"));assert!(tail.contains("尚未提交"));
        assert!(super::Stage::Restoring.message().contains("指定版本"));
        assert!(super::Stage::SubmittingSegment.message().contains("新片段"));
        assert!(super::message("frame_extraction_unavailable").contains("原任务结算不受影响"));
    }
}
