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
        continuation_video_media_id: None,
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
fn fenced_helper_json_preserves_paid_decision_and_spec_validation() {
    let raw=r#"{"action":"create","effective_prompt":"从上传视频结尾生成新片段","spec_patch":{"duration":10,"resolution":"720p","ratio":"9:16","watermark":false},"reference_policy":"replace","clarification":null}"#;
    for fenced in [format!("```json\n{raw}\n```"),format!(" \r\n```JSON\r\n{raw}\r\n```\r\n"),format!("```\n{raw}\n```") ] {
        let d=work_planner::parse_decision(&fenced).expect("a single complete JSON fence is a presentation wrapper");
        assert_eq!(d.action,WorkIntent::Create);
        assert_eq!(d.effective_prompt.as_deref(),Some("从上传视频结尾生成新片段"));
        assert_eq!(d.spec_patch,json!({"duration":10,"resolution":"720p","ratio":"9:16","watermark":false}));
        assert_eq!(d.reference_policy,"replace");
    }
    let unsupported=raw.replace("720p","1080p");
    assert!(work_planner::parse_decision(&format!("```json\n{unsupported}\n```")).is_err());
    let unknown=raw.replace("\"clarification\":null","\"clarification\":null,\"charge\":0");
    assert!(work_planner::parse_decision(&format!("```json\n{unknown}\n```")).is_err());
}
#[test]
fn helper_json_fences_cannot_hide_extra_text_or_incomplete_results() {
    let raw=r#"{"action":"create","effective_prompt":"橘猫散步","spec_patch":{},"reference_policy":"replace","clarification":null}"#;
    for bad in [format!("说明\n```json\n{raw}\n```"),format!("```json\n{raw}\n```\n说明"),
        format!("```json\n{raw}\n```\n```json\n{raw}\n```"),format!("```json\n{raw}"),
        format!("```python\n{raw}\n```"),format!("```json\n{raw}\n{raw}\n```"),
        format!("```json\n[{raw}]\n```"),format!("```json\n{}\n```",raw.trim_end_matches('}'))] {
        assert!(work_planner::parse_decision(&bad).is_err(),"must not salvage a partial or ambiguous decision: {bad}");
    }
    let over_limit=format!("```json\n{}\n```", " ".repeat(16*1024));
    assert!(work_planner::parse_decision(&over_limit).is_err());
}
#[test]
fn storyboard_ranges_are_not_total_duration() {
    for text in [
        "接着生成，9:16。0-2秒近景，2-4秒转身，4-7秒奔跑，7-10秒远景。",
        "生成１０秒视频，９：１６。０－２秒近景，２－４秒转身，４－１０秒远景。",
        "生成10秒视频，9:16。0–2秒近景，2–10秒远景。",
    ] {
        let mut d=decision(WorkIntent::Continue,"inherit");d.spec_patch=json!({"duration":10,"ratio":"9:16"});
        let s=work_planner::merge_snapshot(Some(&base()),&d,&body(text)).unwrap();
        assert_eq!((s.duration,s.ratio.as_str()),(10,"9:16"),"{text}");
    }
}
#[test]
fn current_wrapped_input_excludes_old_prompt_and_terminal_reminders() {
    let b=body("<system-reminder>历史任务：16:9，0-2秒近景。终端最多5个。</system-reminder>\n<user_input>之前生成5秒16:9视频</user_input>\n<user_input>从上一帧续写10秒9:16视频，0-2秒近景，2-10秒远景。</user_input>");
    assert_eq!(work_planner::current_text(&b),"从上一帧续写10秒9:16视频，0-2秒近景，2-10秒远景。");
    let s=work_planner::merge_snapshot(Some(&base()),&decision(WorkIntent::Continue,"inherit"),&b).unwrap();
    assert_eq!((s.duration,s.ratio.as_str()),(10,"9:16"));
    let helper=work_planner::build_helper_input(Some(&base()),&b,"glm-5.3-flash").unwrap();
    assert!(!helper.to_string().contains("终端最多"));
}
#[test]
fn explicit_fields_override_text_before_validation() {
    let mut b=body("生成3秒视频");b["duration"]=json!(10);
    assert_eq!(work_planner::merge_snapshot(None,&decision(WorkIntent::Create,"inherit"),&b).unwrap().duration,10);
    b["messages"][0]["content"]=json!("0-2秒近景，5-10秒远景");
    assert_eq!(work_planner::merge_snapshot(None,&decision(WorkIntent::Create,"inherit"),&b).unwrap().duration,10);
    b["messages"][0]["content"]=json!("生成一百秒高清竖屏视频");
    assert_eq!(work_planner::merge_snapshot(None,&decision(WorkIntent::Create,"inherit"),&b).unwrap().duration,10);
}
#[test]
fn attachment_filename_cannot_select_video_spec() {
    let b=body("<uploaded_files><file_path>C:/480p-3秒.png</file_path></uploaded_files>生成10秒720P视频");
    let s=work_planner::merge_snapshot(None,&decision(WorkIntent::Create,"inherit"),&b).unwrap();
    assert_eq!((s.duration,s.resolution.as_str()),(10,"720p"));
}
#[test]
fn invalid_explicit_spec_is_rejected_before_helper_input_is_built() {
    let b=body("生成3秒视频");
    assert_eq!(work_planner::build_helper_input(None,&b,"glm-5.3-flash").unwrap_err(),"work_spec_unsupported");
}
#[test]
fn natural_specs_accept_the_real_night_portrait_helper_result() {
    let b=body("写实电影感夜景人像：一位普通亚洲女性走在小巷中。画面为竖构图，高分辨率，无文字、无水印。");
    let mut d=decision(WorkIntent::Create,"replace");
    d.spec_patch=json!({"ratio":"9:16","resolution":"720p","watermark":false});
    let s=work_planner::merge_snapshot(Some(&base()),&d,&b).unwrap();
    assert_eq!((s.duration,s.resolution.as_str(),s.ratio.as_str()),(5,"720p","9:16"));
}
#[test]
fn natural_specs_apply_even_when_helper_omits_its_patch() {
    for (text,resolution,ratio) in [
        ("生成高清竖屏视频","720p","9:16"),
        ("低分辨率横屏视频","480p","16:9"),
        ("高清正方形构图","720p","1:1"),
        ("HD video in portrait orientation","720p","9:16"),
    ] {
        let s=work_planner::merge_snapshot(None,&decision(WorkIntent::Create,"inherit"),&body(text)).unwrap();
        assert_eq!((s.resolution.as_str(),s.ratio.as_str()),(resolution,ratio),"{text}");
    }
}
#[test]
fn chinese_seconds_are_normalized_without_reading_shot_counts_as_duration() {
    for (text,seconds) in [("生成五秒高清竖屏视频",5),("生成十秒视频，三个镜头",10),("生成十五秒视频",15),("生成十四 秒视频",14)] {
        let mut d=decision(WorkIntent::Create,"inherit");d.spec_patch=json!({"duration":seconds});
        let s=work_planner::merge_snapshot(None,&d,&body(text)).unwrap();
        assert_eq!(s.duration,seconds,"{text}");
    }
    for text in ["生成三秒视频","生成十六秒视频"] {
        assert_eq!(work_planner::build_helper_input(None,&body(text),"glm-5.3-flash").unwrap_err(),"work_spec_unsupported");
    }
}
#[test]
fn chinese_storyboard_ranges_are_not_misread_as_total_duration() {
    let b=body("生成十秒视频，零至二秒近景，二至五秒转身，五至十秒远景");
    assert_eq!(work_planner::merge_snapshot(None,&decision(WorkIntent::Create,"inherit"),&b).unwrap().duration,10);
    let b=body("零至二秒近景，二至五秒转身，五至十秒远景");
    assert_eq!(work_planner::merge_snapshot(None,&decision(WorkIntent::Create,"inherit"),&b).unwrap().duration,10);
}
#[test]
fn helper_input_contains_authorized_natural_specs_and_preserved_defaults() {
    let input=work_planner::build_helper_input(Some(&base()),&body("改为低分辨率横屏，动作放慢"),"glm-5.3-flash").unwrap();
    let payload:Value=serde_json::from_str(input["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(payload["normalized_spec"],json!({"resolution":"480p","ratio":"16:9"}));
    assert_eq!(payload["create_defaults"],json!({"duration":5,"resolution":"720p","ratio":"16:9","watermark":false}));
}
#[test]
fn helper_can_echo_unchanged_defaults_but_not_invent_changes() {
    let mut d=decision(WorkIntent::Create,"inherit");
    d.spec_patch=json!({"duration":5,"resolution":"720p","ratio":"16:9","watermark":false});
    let s=work_planner::merge_snapshot(None,&d,&body("生成一只猫走路的视频")).unwrap();
    assert_eq!((s.duration,s.resolution.as_str(),s.ratio.as_str()),(5,"720p","16:9"));
    let mut parent=base();parent.resolution="480p".into();
    d.action=WorkIntent::Revise;d.spec_patch=json!({"duration":10,"resolution":"480p","ratio":"9:16"});
    let s=work_planner::merge_snapshot(Some(&parent),&d,&body("动作放慢，其他不变")).unwrap();
    assert_eq!((s.duration,s.resolution.as_str(),s.ratio.as_str()),(10,"480p","9:16"));
    d.spec_patch=json!({"resolution":"720p"});
    assert!(work_planner::merge_snapshot(Some(&parent),&d,&body("动作放慢，其他不变")).is_err());
}
#[test]
fn explicit_specs_override_natural_hints_and_negated_hints_do_not_apply() {
    let mut b=body("高清竖屏生成5秒视频，480P 1:1");
    let s=work_planner::merge_snapshot(None,&decision(WorkIntent::Create,"inherit"),&b).unwrap();
    assert_eq!((s.resolution.as_str(),s.ratio.as_str()),("480p","1:1"));
    b["resolution"]=json!("720p");b["ratio"]=json!("16:9");
    let s=work_planner::merge_snapshot(None,&decision(WorkIntent::Create,"inherit"),&b).unwrap();
    assert_eq!((s.resolution.as_str(),s.ratio.as_str()),("720p","16:9"));
    let mut parent=base();parent.resolution="480p".into();
    let s=work_planner::merge_snapshot(Some(&parent),&decision(WorkIntent::Revise,"inherit"),&body("不要高清，不要横屏，动作放慢")).unwrap();
    assert_eq!((s.resolution.as_str(),s.ratio.as_str()),("480p","9:16"));
    let s=work_planner::merge_snapshot(Some(&parent),&decision(WorkIntent::Revise,"inherit"),&body("piano HD video in portrait orientation")).unwrap();
    assert_eq!((s.resolution.as_str(),s.ratio.as_str()),("720p","9:16"),"the word piano does not negate HD");
}
#[test]
fn conflicting_natural_specs_are_rejected_before_paid_helper() {
    assert_eq!(work_planner::build_helper_input(None,&body("生成竖屏或横屏视频"),"glm-5.3-flash").unwrap_err(),"work_spec_unsupported");
    assert_eq!(work_planner::build_helper_input(None,&body("高清1080P视频"),"glm-5.3-flash").unwrap_err(),"work_spec_unsupported");
}
#[test]
fn helper_may_echo_effective_watermark_but_cannot_change_it() {
    let mut d=decision(WorkIntent::Create,"replace");
    d.spec_patch=json!({"watermark":false});
    assert!(!work_planner::merge_snapshot(None,&d,&body("延长视频")).unwrap().watermark);
    d.spec_patch=json!({"watermark":true});
    assert!(work_planner::merge_snapshot(None,&d,&body("延长视频")).is_err());
    let mut parent=base();parent.watermark=true;d.action=WorkIntent::Revise;
    assert!(work_planner::merge_snapshot(Some(&parent),&d,&body("动作放慢")).unwrap().watermark);
    d.spec_patch=json!({"watermark":false});
    assert!(work_planner::merge_snapshot(Some(&parent),&d,&body("动作放慢")).is_err());
}
#[test]
fn uploaded_video_extension_uses_owned_source_instead_of_guessing_parent() {
    let raw=json!({"action":"continue","effective_prompt":"延长上传的视频，保留前5秒，再续拍5秒","spec_patch":{"duration":10},"reference_policy":"replace","clarification":null});
    let mut d=work_planner::parse_decision(&raw.to_string()).unwrap();
    let b=json!({"video_asset_ids":["owned-video"],"duration":10,"resolution":"480p","ratio":"16:9"});
    work_planner::resolve_uploaded_video_action(&mut d,false,&b);
    let snapshot=work_planner::merge_snapshot(None,&d,&b).unwrap();
    assert_eq!(d.action,WorkIntent::Create);
    assert_eq!(snapshot.duration,10);
    assert!(snapshot.effective_prompt.contains("保留前5秒"));
    for (has_parent,input) in [(true,b.clone()),(false,json!({})),(false,json!({"image_asset_ids":["image"]})),(false,json!({"action":"continue","video_asset_ids":["owned-video"]}))] {
        let mut d=work_planner::parse_decision(&raw.to_string()).unwrap();
        work_planner::resolve_uploaded_video_action(&mut d,has_parent,&input);
        assert_eq!(d.action,WorkIntent::Continue,"explicit parent continuation must not downgrade");
    }
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
    assert!(work_planner::read_only_decision(&body("这个视频不满意"), true).is_none());
    assert!(work_planner::read_only_decision(&body("不满意，改成夜景"), true).is_none());
}
#[test]
fn status_download_have_no_video_step() {
    for (text, action) in [
        ("查看任务状态", WorkIntent::Status),
        ("重新下载刚才的视频", WorkIntent::Download),
    ] {
        assert!(work_planner::read_only_decision(&body(text), true).is_none());
        let d = work_planner::read_only_decision(&json!({"action":format!("{action:?}").to_lowercase(),"messages":[{"role":"user","content":text}]}), true).unwrap();
        assert_eq!(d.action, action);
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
    assert_eq!(input["max_tokens"], 4096);
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

fn attachment_followup() -> Value {
    json!({"messages":[
        {"role":"user","content":"<user_input>使用本次上传的视频作为参考，从它的结尾继续生成一个新片段。新片段10秒720P，竖屏9:16。感染者扑向镜头，警车驶近。</user_input>"},
        {"role":"assistant","content":"请明确要续写哪个视频版本；本次未提交视频。"},
        {"role":"user","content":[
            {"type":"text","text":"<uploaded_files><file_path>C:\\clips\\source.mp4</file_path></uploaded_files>"},
            {"type":"text","text":"<system-reminder>Use PowerShell 5</system-reminder>"},
            {"type":"text","text":"REQUIREMENT:\n- Detect the language XX( such english ,chinese ) of the user's query.All outputs throughout the entire workflow must use the language XX.\n- You MUST NOT spawn more than 3 Explore subagents at the same time"}
        ]}
    ]})
}
#[test]
fn video_attachment_followup_recovers_current_conversation_script_and_specs() {
    let mut b=attachment_followup();
    for uploaded in [false,true] {
        if uploaded {
            b["messages"][2]["content"][0]["text"]=json!("");
            b["video_asset_ids"]=json!(["owned-video"]);
        }
        let input=work_planner::build_helper_input(None,&b,"glm-5.3-flash").unwrap();
        let payload:Value=serde_json::from_str(input["messages"][1]["content"].as_str().unwrap()).unwrap();
        assert!(payload["current"].as_str().unwrap().contains("感染者扑向镜头"));
        assert!(!payload["current"].as_str().unwrap().contains("REQUIREMENT"));
        assert_eq!(payload["normalized_spec"],json!({"duration":10,"resolution":"720p","ratio":"9:16"}));
        let s=work_planner::merge_snapshot(None,&decision(WorkIntent::Create,"replace"),&b).unwrap();
        assert_eq!((s.duration,s.resolution.as_str(),s.ratio.as_str()),(10,"720p","9:16"));
    }
}
#[test]
fn attachment_followup_never_overrides_a_new_user_instruction() {
    let mut b=attachment_followup();
    b["messages"][2]["content"].as_array_mut().unwrap().push(json!({"type":"text","text":"<user_input>先别生成，查看任务状态</user_input>"}));
    assert_eq!(work_planner::current_text(&b),"先别生成，查看任务状态");
}
#[test]
fn attachment_followup_does_not_skip_cancellation_or_replay_completed_work() {
    let mut b=attachment_followup();
    b["messages"][0]["content"]=json!("取消刚才的视频任务");
    assert_eq!(work_planner::current_text(&b),"");
    let mut b=attachment_followup();
    b["messages"][1]=json!({"role":"assistant","content":"视频已完成","video_task":{"status":"completed"},"work_context":{"base_version_id":"version-done"}});
    assert_eq!(work_planner::current_text(&b),"");
    let mut b=attachment_followup();
    b["messages"][2]["content"][0]["text"]=json!("");
    assert_eq!(work_planner::current_text(&b),"","no fresh video means no history fallback");
}
#[test]
fn video_without_script_is_read_only_without_a_paid_helper() {
    let mut b=attachment_followup();
    b["messages"].as_array_mut().unwrap().drain(0..2);
    let d=work_planner::read_only_decision(&b,false).expect("empty attachment intent is a free clarification");
    assert_eq!(d.action,WorkIntent::Clarify);
    assert!(d.paid_action().is_none());
}
#[test]
fn null_reference_policy_is_tolerated_only_for_read_only_decisions() {
    for action in ["clarify","status","download"] {
        let d=work_planner::parse_decision(&json!({"action":action,"effective_prompt":null,"spec_patch":null,"reference_policy":null,"clarification":"请提供视频剧本"}).to_string()).unwrap();
        assert_eq!(d.reference_policy,"inherit");
        assert!(d.paid_action().is_none());
    }
    for action in ["create","revise","continue"] {
        assert!(work_planner::parse_decision(&json!({"action":action,"effective_prompt":"生成视频","spec_patch":{},"reference_policy":null,"clarification":null}).to_string()).is_err());
    }
}
#[test]
fn helper_receives_uploaded_source_evidence_without_client_paths() {
    let mut b=body("使用上传视频，从结尾继续生成10秒的新片段");
    b["video_asset_ids"]=json!(["verified-local-video"]);
    let input=work_planner::build_helper_input(None,&b,"glm-5.3-flash").unwrap();
    let payload:Value=serde_json::from_str(input["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(payload["current_references"],json!({"video_count":1,"image_count":0}));
    assert!(!input.to_string().contains("verified-local-video"));
}
#[test]
fn unknown_requirement_blocks_remain_user_instructions() {
    let b=body("REQUIREMENT:\n- 从视频尾部生成10秒720P新片段，不要改服装");
    assert_eq!(work_planner::current_text(&b),"REQUIREMENT:\n- 从视频尾部生成10秒720P新片段，不要改服装");
}
#[test]
fn negative_visual_constraints_are_not_a_task_cancellation() {
    let mut b=attachment_followup();
    b["messages"][0]["content"]=json!("使用上传的视频继续生成10秒720P竖屏视频。负面约束：不要生成水印、logo和多余肢体。");
    assert!(work_planner::current_text(&b).contains("负面约束"));
}
#[test]
fn supplemental_video_cannot_restart_a_previously_submitted_task() {
    let mut b=attachment_followup();
    b["messages"][1]=json!({"role":"assistant","content":"视频任务已提交，正在生成中。"});
    assert_eq!(work_planner::current_text(&b),"");
}
