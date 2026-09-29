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
    let reason=result["error"].as_str().or_else(||result.pointer("/error/message").and_then(Value::as_str)).unwrap_or("").trim().to_ascii_lowercase();
    match reason.as_str() {
        "video security check failed"=>"video_safety_check_failed",
        "image security check failed"=>"reference_safety_check_failed",
        "text security check failed"=>"prompt_safety_check_failed",
        _=>"video_execution_failed",
    }
}
pub(crate) fn message(code:&str)->&'static str {match code {
    "work_spec_unsupported"=>"本轮视频规格超出支持范围或存在冲突；时长支持4至15秒，分辨率支持480P或720P，也可用横屏、竖屏、高清等描述。本次未提交视频。",
    "work_decision_invalid"|"assist_result_invalid"=>"辅助模型返回的规划格式或字段未通过校验；本次未提交视频，请保留请求编号供排查。",
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
