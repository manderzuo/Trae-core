use aiwork_core::VideoWorkSnapshot;
use serde_json::{json, Value};
use starlink_dimension_router::work_planner::{self, WorkDecision, WorkIntent};
fn base() -> VideoWorkSnapshot {
    VideoWorkSnapshot {
        effective_prompt: "女子穿黑裙撑红伞走在雨夜".into(),
        duration: 10,
        resolution: "720p".into(),
        ratio: "9:16".into(),
        watermark: false,
        user_media_ids: vec!["old-image".into()],
        tail_frame_media_id: None,
        parent_version_id: None,
        source_request_id: None,
        reference_mode: "user_reference".into(),
        summary: "雨夜街道".into(),
        dispatch_body:None,
    }
}
fn decision(action: WorkIntent, policy: &str) -> WorkDecision {
    WorkDecision {
        action,
        effective_prompt: Some("女子穿黑裙撑红伞在雨夜放慢脚步".into()),
        spec_patch: json!({}),
        reference_policy: policy.into(),
        clarification: None,
    }
}
fn body(text: &str) -> Value {
    json!({"messages":[{"role":"user","content":text}]})
}
#[test]
fn revise_inherits_unspecified_fields() {
    let s = work_planner::merge_snapshot(
        Some(&base()),
        &decision(WorkIntent::Revise, "inherit"),
        &body("动作放慢，其他不变"),
    )
    .unwrap();
    assert_eq!(
        (s.duration, s.resolution.as_str(), s.ratio.as_str()),
        (10, "720p", "9:16")
    );
    assert_eq!(s.user_media_ids, vec!["old-image"]);
    assert!(s.effective_prompt.contains("放慢"));
}

#[test]
fn glm_null_spec_patch_means_no_change_not_invalid_revision() {
    let raw=json!({"action":"revise","effective_prompt":"红色绸布在柔和月光下起伏","spec_patch":null,"reference_policy":"inherit","clarification":null}).to_string();
    let d=work_planner::parse_decision(&raw).unwrap();
    assert_eq!(d.spec_patch,json!({}));
    let s=work_planner::merge_snapshot(Some(&base()),&d,&body("改成夜景，其他不变")).unwrap();
    assert_eq!((s.duration,s.resolution.as_str(),s.ratio.as_str()),(10,"720p","9:16"));
    assert_eq!(s.user_media_ids,vec!["old-image"]);
    for invalid in [json!([]),json!(""),json!({"core_key_id":"foreign"})] {
        let mut value:Value=serde_json::from_str(&raw).unwrap();value["spec_patch"]=invalid;
        assert!(work_planner::parse_decision(&value.to_string()).is_err());
    }
}
#[test]
fn current_explicit_spec_wins() {
    let mut b = body("改成5秒480P 16:9");
    b["duration"] = json!(7);
    b["resolution"] = json!("720p");
    b["ratio"] = json!("1:1");
    let s =
        work_planner::merge_snapshot(Some(&base()), &decision(WorkIntent::Revise, "inherit"), &b)
            .unwrap();
    assert_eq!(
        (s.duration, s.resolution.as_str(), s.ratio.as_str()),
        (7, "720p", "1:1")
    );
}
#[test]
fn new_reference_replaces_unless_merge_requested() {
    let mut b = body("使用新图片");
    b["work_user_media_ids"] = json!(["new-image"]);
    let s =
        work_planner::merge_snapshot(Some(&base()), &decision(WorkIntent::Revise, "replace"), &b)
            .unwrap();
    assert_eq!(s.user_media_ids, vec!["new-image"]);
    b["messages"][0]["content"] = json!("合并新旧参考图");
    let s = work_planner::merge_snapshot(Some(&base()), &decision(WorkIntent::Revise, "merge"), &b)
        .unwrap();
    assert_eq!(s.user_media_ids, vec!["old-image", "new-image"]);
    let s = work_planner::merge_snapshot(
        Some(&base()),
        &decision(WorkIntent::Revise, "clear"),
        &body("不用参考图"),
    )
    .unwrap();
    assert!(s.user_media_ids.is_empty());
    assert!(
        work_planner::merge_snapshot(
            Some(&base()),
            &decision(WorkIntent::Revise, "clear"),
            &body("动作放慢")
        )
        .is_err(),
        "model cannot silently drop original reference"
    );
}
#[test]
fn continue_inherits_duration_without_new_value() {
    let s = work_planner::merge_snapshot(
        Some(&base()),
        &decision(WorkIntent::Continue, "inherit"),
        &body("接着上一段生成"),
    )
    .unwrap();
    assert_eq!(s.duration, 10);
}
#[test]
fn fullwidth_ratio_normalized() {
    let s = work_planner::merge_snapshot(
        Some(&base()),
        &decision(WorkIntent::Revise, "inherit"),
        &body("改成５Ｓ ４８０Ｐ １６：９"),
    )
    .unwrap();
    assert_eq!(
        (s.duration, s.resolution.as_str(), s.ratio.as_str()),
        (5, "480p", "16:9")
    );
}
#[test]
fn vague_dissatisfaction_clarifies() {
    assert_eq!(
        work_planner::read_only_decision(&body("这个视频不满意"), true)
            .unwrap()
            .action,
        WorkIntent::Clarify
    );
    assert!(work_planner::read_only_decision(&body("不满意，改成夜景"), true).is_none());
}
#[test]
fn status_download_have_no_video_step() {
    for (text, action) in [
        ("查看任务状态", WorkIntent::Status),
        ("重新下载刚才的视频", WorkIntent::Download),
    ] {
        let d = work_planner::read_only_decision(&body(text), true).unwrap();
        assert_eq!(d.action, action);
        assert!(d.paid_action().is_none());
    }
}
#[test]
fn helper_excludes_base64_tickets_and_terminal_logs() {
    let b = json!({"messages":[{"role":"system","content":"aw_live_secret-system"},{"role":"tool","content":"SECRET_TERMINAL_LOG"},{"role":"assistant","content":"https://api.example.test/download?ticket=SECRET_TICKET"},
        {"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,SECRET_IMAGE"}},{"type":"text","text":"<uploaded_files><file_path>C:/private/SECRET_PATH.png</file_path></uploaded_files>动作放慢。aw_live_SECRET_KEY https://api.example.test/video?ticket=SECRET_TICKET"}]}]});
    let input = work_planner::build_helper_input(Some(&base()), &b, "glm-5.3-flash").unwrap();
    let raw = input.to_string();
    for secret in [
        "SECRET_TERMINAL_LOG",
        "SECRET_IMAGE",
        "SECRET_PATH",
        "SECRET_KEY",
        "SECRET_TICKET",
        "secret-system",
    ] {
        assert!(!raw.contains(secret), "{secret} leaked");
    }
    assert_eq!(input["model"], "glm-5.3-flash");
    assert_eq!(input["max_tokens"], 1024);
    assert_eq!(input["stream"], false);
}
#[test]
fn helper_limit_32k_history_8k() {
    let mut messages: Vec<Value> = (0..20)
        .map(|_| json!({"role":"user","content":"字".repeat(1500)}))
        .collect();
    messages.push(json!({"role":"user","content":"改成夜景"}));
    let input = work_planner::build_helper_input(
        Some(&base()),
        &json!({"messages":messages}),
        "glm-5.3-flash",
    )
    .unwrap();
    assert!(input.to_string().len() <= 32 * 1024);
    let user: Value =
        serde_json::from_str(input["messages"][1]["content"].as_str().unwrap()).unwrap();
    let history = user["history"].as_array().unwrap();
    assert!(history.len() <= 4);
    assert!(
        history
            .iter()
            .map(|v| v.as_str().unwrap().len())
            .sum::<usize>()
            <= 8 * 1024
    );
    assert!(
        work_planner::build_helper_input(None, &body(&"字".repeat(5000)), "glm-5.3-flash").is_err()
    );
}
#[test]
fn invalid_json_or_hallucinated_parent_never_dispatches() {
    for raw in [
        "```json {} ```",
        "{\"action\":\"create\"",
        r#"{"action":"continue","effective_prompt":"x","spec_patch":{},"reference_policy":"inherit","parent_version_id":"guessed"}"#,
    ] {
        assert!(work_planner::parse_decision(raw).is_err());
    }
    let d = decision(WorkIntent::Continue, "inherit");
    assert!(work_planner::merge_snapshot(None, &d, &body("接着上一段")).is_err());
    for spec in [
        json!({"duration":3}),
        json!({"duration":16}),
        json!({"resolution":"1080p"}),
        json!({"ratio":"3:2"}),
    ] {
        let mut b = body("生成视频");
        b.as_object_mut()
            .unwrap()
            .extend(spec.as_object().unwrap().clone());
        assert!(
            work_planner::merge_snapshot(None, &decision(WorkIntent::Create, "inherit"), &b)
                .is_err()
        );
    }
    let mut d = decision(WorkIntent::Revise, "inherit");
    d.spec_patch = json!({"account_ref":"attacker"});
    assert!(work_planner::merge_snapshot(Some(&base()), &d, &body("动作放慢")).is_err());
    for text in [
        "改成3秒视频",
        "改成16秒视频",
        "改成1080P视频",
        "改成3:2画幅",
    ] {
        assert!(
            work_planner::merge_snapshot(
                Some(&base()),
                &decision(WorkIntent::Revise, "inherit"),
                &body(text)
            )
            .is_err(),
            "{text}"
        );
    }
}
