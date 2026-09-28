# 视频作业上下文持久化与自动续写实施计划

> For agentic workers: REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. 本项目按用户指定由主代理执行，不启动子代理。步骤使用复选框跟踪；当前为待审阅计划，不是已完成记录。

**Goal:** 在不要求其他电脑修改API配置的前提下，支持同一对话持续改版和自动尾帧准备/续写，同时不破坏素材、扣费、并发与下载链路。

**Architecture:** Core保存Key隔离的作业、不可变版本和私有素材，并把有界上下文交给现行GLM辅助模型。AI Work从已完成的受控视频提取尾帧，按真实验证过的TRAE能力提交下一段；MCP只增加兼容编排入口。

**Tech Stack:** Rust、Axum、SQLite、现有KeyVault与桥接v2、受信任固定路径FFmpeg、Node.js MCP、PowerShell；保持现有平台和依赖版本约束。

**Spec:** [设计说明](../specs/2026-09-28-video-work-context-continuation-design.md)。用户同时要求制定修复计划，因此设计与本计划一起提供审阅；审阅确认前不实施业务代码。

## Global Constraints

- 只有用户明确要求生成、重新生成或续写时才创建付费视频任务；不会自行无限续写。
- 时长4–15秒、分辨率480p/720p、六种现有画幅不变。
- 默认作业素材保留30天、单素材沿用32MiB上限、每Key上限1GiB、Core作业素材总上限10GiB。
- 辅助模型总输入序列化上限32KiB，其中历史文本不超过8KiB；保留当前1024输出token上限。
- 提帧并发2、等待队列最多8、单进程超时30秒、输出上限8MiB。
- 不删除旧账本、不解除未知占用、不冻结整个Key；下载不作为结算释放条件。
- 原生首帧和延长模式默认关闭；不能把普通参考生成冒充严格续写。
- 当前主代理独立执行；不使用子代理。本轮不运行测试、不付费、不推送、不部署。

## Review Focus

1. 客户端截断历史、忽略元数据或丢失作业标记时，澄清而不是复用Key最近任务（Task 3）。
2. 同一Key两对话/同父版本并发分支、乱序完成时，互不覆盖且不串账（Task 5）。
3. 素材刚好过期、清理竞争、密钥轮换或磁盘满时，不丢失在用副本或降为纯文任务（Tasks 2、6）。
4. 付费提交已成功而版本记录尚未完成时重启，按原请求恢复，不重复生成（Task 5）。
5. 工具回传旧的/多份/异常格式上传或下载结果时，只处理原操作，不生成新版本（Tasks 3、8）。

## 仓库、文件和接口边界

主计划保存在Core仓库。以下路径相对于标出的仓库根目录；执行前重新核对工作区状态和AGENTS约束，保留不相关改动。

- Core：D:/gpt/trae-core-goal-billing；基线6638a27。
- AI Work：D:/gpt/aiwork-goal-seedance-stream；基线c8428a8。
- MCP：D:/gpt/trae-maker-MCP-update；基线8f85598。

建议新增模块，而不把全部逻辑继续堆进user_routes.rs或budget_flow.rs。所有新增签名均为计划接口，尚未实现。

## Task 1：TRAE续写能力取证及开关

**Files — AI Work:** 新建src-tauri/src/api_server/video_capabilities.rs；修改bridge_api.rs、server.rs；新建docs/seedance-continuation-capabilities.md。不修改账户策略。

**Interfaces:** VideoCapabilities {tail_reference: bool, native_first_frame: bool, native_video_extend: bool, contract_version: String, evidence_digest: String}；video_capabilities::load(data_dir: &Path) -> Result<VideoCapabilities, String>。Core读取GET /internal/bridge/v2/video-capabilities，未知值一律false；隔离测试Key的待验收tail_reference例外按设计第8节配置。

- [ ] 读取本机官方GenerateVideo实际工具定义、现有已脱敏代理记录或官方契约；只查询，不提交视频。记录字段、素材角色、输出是新片段还是整段、计费维度。没有取得契约时明确记录not_verified，不猜mode_type或方舟字段。
- [ ] 添加capability_unknown_defaults_false、capability_provider_version_mismatch_disables_native测试，断言原生两个能力false，且普通视频功能保持可用。
- [ ] 运行：cargo test --manifest-path src-tauri/Cargo.toml video_capabilities；预期新增测试先失败，实施后全部通过。
- [ ] 实现版本化能力配置与只读查询，只有经过Task 9验收的模式才能标记verified。普通参考图的功能存在与“尾帧参考续写验收完成”分开记录。
- [ ] 独立审计并提交该能力边界；不把取证报告写成成功生成报告。

## Task 2：作业存储、迁移与持久素材

**Files — Core:** 新建src-core/src/video_work.rs、src-core/tests/video_work.rs、src-core/tests/video_work_migration.rs；修改src-core/src/schema.rs、store.rs、lib.rs；新建starlink-dimension-router/src/work_media.rs，修改assets.rs、key_vault.rs、config.rs、state.rs、lib.rs。

**Interfaces:** 导出VideoWork、VideoWorkVersion、VideoWorkSnapshot、WorkMediaRef及设计第4节列明的字段；WorkAction = Create | Revise | Continue；WorkVersionState = Preparing | Running | Completed | Failed | Unknown；WorkMutation {version: VideoWorkVersion, created: bool}；EncryptedWorkSnapshot {key_version: u32, ciphertext: Vec<u8>, snapshot_sha256: String}。CoreStore::create_video_work(principal: &Principal, conversation_ref: &str) -> Result<VideoWork, CoreError>；owned_video_work(principal: &Principal, work_id: &str) -> Result<Option<VideoWork>, CoreError>；work_media::pin(state: &StarlinkRouterState, principal: &Principal, work_id: &str, asset: &ParsedAssetUpload, now_ms: i64) -> Result<WorkMediaRef, String>；materialize(state: &StarlinkRouterState, principal: &Principal, media: &WorkMediaRef, now_ms: i64) -> Result<CoreAsset, String>；rotate(state: &StarlinkRouterState) -> Result<usize, String>；cleanup(state: &StarlinkRouterState, now_ms: i64) -> Result<usize, String>。媒体租约在video_work.rs定义持久获取/释放方法，并纳入重启恢复测试。

- [ ] 添加v29_to_v30_preserves_ledger_and_unknown_holds、work_owner_isolation、snapshot_encrypted_and_restart_readable、work_media_survives_upload_ttl、work_media_capacity_rejects_before_eviction、media_rotation_and_inflight_cleanup测试。校验原数据哈希/行数不变；32MiB、1GiB、10GiB和30天边界使用小型模拟，不造大文件。
- [ ] 运行：cargo test --manifest-path src-core/Cargo.toml --test video_work；cargo test --manifest-path src-core/Cargo.toml --test video_work_migration；cargo test --manifest-path starlink-dimension-router/Cargo.toml work_media。确认失败原因，再实现schema30、归属检查、KeyVault封装和原子文件发布。
- [ ] 限制DB只持有短事务；初始化空库及已有29库均进入30，重复迁移不变更账本。建立设计第4节的四张表，context_handle原值只存密文、哈希用于索引；密钥轮换也覆盖关联句柄。测试删除一个版本不删除其他版本引用，Key停用后不能读副本，断电孤儿可恢复/清理。
- [ ] 重跑上述测试通过；检查轮换路径同时覆盖新快照/媒体和旧密钥记录。提交该存储单元。

## Task 3：无配置对话关联与上下文选择

**Files — Core:** 新建starlink-dimension-router/src/work_context.rs、starlink-dimension-router/tests/work_context.rs；修改reference_context.rs、reference_upload.rs、video_delivery.rs、user_routes.rs、lib.rs；src-core/src/video_work.rs新增句柄哈希映射。

**Interfaces:** WorkResolution = New | Existing {work: VideoWork, base_version: Option<VideoWorkVersion>} | Clarify {text: String}；work_context::resolve(state: &StarlinkRouterState, principal: &Principal, headers: &HeaderMap, body: &Value) -> Result<WorkResolution, String>；issue_handle(state: &StarlinkRouterState, principal: &Principal, work_id: &str, version_id: Option<&str>) -> Result<String, String>；decorate_reply(reply: &mut Value, handle: &str, context: &Value)。待生成作业句柄允许version_id为空；存储层为作业/版本保存句柄哈希与版本化句柄密文。句柄只作关联，所有读取仍要求Key认证。

- [ ] 添加history_marker_resumes_exact_version、same_key_new_chat_has_no_global_fallback、foreign_key_or_tampered_handle_is_rejected、conflicting_parent_markers_clarify、history_without_marker_does_not_guess、reference_tool_resume_preserves_work、download_receipt_does_not_create_work、handle_stable_after_retry_and_restart、work_reference_restored_before_missing_reference_check测试。
- [ ] 运行：cargo test --manifest-path starlink-dimension-router/Cargo.toml --test work_context；预期先失败，再实现设计第5节的优先级与紧凑标记。
- [ ] 明确处理缺失client id和完全相同首次请求的不可区分边界，保留既有幂等保护，不通过文本哈希制造“永久对话ID”。明确指定父版本与句柄冲突时不付费提交。
- [ ] 为流式最终消息、非流式结果、上传确认和下载回退补回传信息；剥离标记后再交给模型。重跑通过并提交。
- [ ] 入口顺序保持上传/下载工具回执优先；其后解析作业，按已验证归属恢复工作副本，再执行现有reference_context检查。禁止先被reference_image_missing拦住而没有机会读取作业素材。

## Task 4：有界上下文规划与参数继承

**Files — Core:** 新建starlink-dimension-router/src/work_planner.rs、starlink-dimension-router/tests/work_planner.rs；修改budget_flow.rs、user_routes.rs、seedance_feedback.rs。不修改普通文字模型转发链路。

**Interfaces:** 定义独立WorkIntent = Create | Revise | Continue | Status | Download | Clarify；只把前三个转换为Task 2的WorkAction。WorkDecision {action: WorkIntent, effective_prompt: Option<String>, spec_patch: Value, reference_policy: String, clarification: Option<String>}；build_helper_input(snapshot: Option<&VideoWorkSnapshot>, body: &Value, model: &str) -> Result<Value, String>；merge_snapshot(base: Option<&VideoWorkSnapshot>, decision: &WorkDecision, body: &Value) -> Result<VideoWorkSnapshot, String>。

- [ ] 添加revise_inherits_unspecified_fields、current_explicit_spec_wins、new_reference_replaces_unless_merge_requested、continue_inherits_duration_without_new_value、fullwidth_ratio_normalized、vague_dissatisfaction_clarifies、status_download_have_no_video_step测试。
- [ ] 添加helper_excludes_base64_tickets_and_terminal_logs、helper_limit_32k_history_8k、invalid_json_or_hallucinated_parent_never_dispatches测试；断言max_tokens仍为1024且模型取现行配置。
- [ ] 运行：cargo test --manifest-path starlink-dimension-router/Cargo.toml --test work_planner；确认RED，实施六种意图、有界辅助输入和服务端字段合并；规格合法性由服务端裁决。
- [ ] 模型建议不能改变Key、账户和账本。结构化只读操作直接执行；自由文本需要模型判断时记录真实文字费用，不宣传零费用。重跑GREEN并提交。

## Task 5：版本生成接入、幂等和恢复

**Files — Core:** 修改src-core/src/video_work.rs、starlink-dimension-router/src/budget_flow.rs、budget_continuation.rs、user_routes.rs；新建starlink-dimension-router/tests/work_execution.rs；扩展tests/budget_bridge_recovery.rs、tests/budget_video_admission.rs。

**Interfaces:** CoreStore::bind_work_version(principal: &Principal, work_id: &str, parent_version_id: Option<&str>, request_id: &str, action: WorkAction, sealed_snapshot: &EncryptedWorkSnapshot) -> Result<WorkMutation, CoreError>；set_work_version_state(principal: &Principal, request_id: &str, state: WorkVersionState) -> Result<WorkMutation, CoreError>。sealed_snapshot仅含版本化密文与摘要；未加密快照不得写库。

- [ ] 添加request_unique_binding、same_retry_reuses_immutable_snapshot、different_keys_never_share_budget、parallel_branches_keep_parents_and_balances、late_completion_does_not_overwrite_selected_version、restart_after_dispatch_does_not_resubmit、resume_ignores_expired_transient_asset、unknown_execution_keeps_original_hold测试。
- [ ] 运行：cargo test --manifest-path starlink-dimension-router/Cargo.toml --test work_execution；先RED，后将作业绑定接入现有预算身份和检查点。已有request的恢复先于重新规划或重新提帧。
- [ ] 保留辅助模型独立计费与单次视频提交CAS；提帧、状态和下载不占视频预算/并发。不同生成遵循Key实际并发设置，不能引入整个对话的长互斥锁。
- [ ] 运行现有budget_bridge_recovery和budget_video_admission集成测试；通过后提交。记录持久化失败发生在发送前/后的不同恢复规则。

## Task 6：AI Work受控尾帧提取

**Files — AI Work:** 新建src-tauri/src/api_server/video_frames.rs；修改bridge_v2_api.rs、bridge_artifacts.rs、video_store.rs、server.rs、mod.rs；补充本机FFmpeg可信路径配置和启动检查，不能要求调用API的其他电脑安装FFmpeg。

**Interfaces:** FrameArtifact {path: PathBuf, mime: String, width: u32, height: u32, timestamp_ms: i64, source_sha256: String, frame_sha256: String}；video_frames::extract_last(data_dir: &Path, owned_video_path: &Path, extractor_path: &Path) -> Result<FrameArtifact, String>。HTTP路由返回受控PNG及来源头。Core的bridge_client.rs定义FrameDownload {bytes: Vec<u8>, width: u32, height: u32, timestamp_ms: i64, source_sha256: String, frame_sha256: String}，新增BridgeClient::last_frame(&self, step: &BudgetStepView) -> Result<FrameDownload, String>，读取上限8MiB。

- [ ] 在本机检查FFmpeg是否已有可用可信安装；缺失时实施阶段才安装固定版本并验证摘要。新增short_clip_returns_last_decodable_frame、invalid_or_missing_video_fails_without_generation、no_shell_or_remote_protocol、timeout_kills_process_and_releases_slot、same_source_digest_reuses_frame、cleanup_waits_for_media_lease测试。
- [ ] 运行：cargo test --manifest-path src-tauri/Cargo.toml video_frames；先用小型本地合成夹具验证最后帧与时间戳，不使用付费生成。断言30秒超时、8MiB输出上限、并发2及队列8。
- [ ] 复用预算/执行/归属/加密结果校验和open_completed资源恢复；未完成或其他Key的视频不得提帧。PNG通过魔数和尺寸检查才发布。
- [ ] 原有视频清理增加可恢复租约判断；提帧进程使用固定参数、私有目录和局部解码，不加载整段进内存。异常时只清理本次临时文件。
- [ ] 路由测试断言跨budget/request拒绝，重复提帧不新增账单、不新增上游生成。回归通过并提交。

## Task 7：Core自动续写与公开作业接口

**Files — Core:** 新建starlink-dimension-router/src/work_routes.rs、work_continuation.rs、tests/work_continuation.rs；修改lib.rs、bridge_client.rs、budget_flow.rs、config.rs。AI Work修改bridge_planner.rs、video.rs时仅增加Task 1确认的原生字段映射。

**Interfaces:** ContinuationMode = Auto | TailReference | NativeFirstFrame | NativeVideoExtend；prepare_continuation(state: Arc<StarlinkRouterState>, principal: Principal, base: VideoWorkVersion, input: Value) -> Result<VideoWorkSnapshot, String>；WorkContinueRequest {base_version_id: String, prompt: String, duration: Option<i64>, resolution: Option<String>, ratio: Option<String>, continuation_mode: Option<ContinuationMode>}；warm_tail_frame(state: Arc<StarlinkRouterState>, principal: Principal, version: VideoWorkVersion) -> Result<WorkMediaRef, String>。实现GET /v1/video-works/:work_id和POST /v1/video-works/:work_id/continue。

- [ ] 添加last_frame_bound_to_parent_video、auto_uses_only_verified_capability、strict_mode_rejects_without_silent_fallback、missing_parent_never_text_only、reference_pricing_uses_actual_counts、continue_new_request_not_parent_retry、old_client_create_unchanged、saved_tail_survives_original_mp4_cache_expiry、preextract_failure_does_not_block_delivery_or_settlement测试。
- [ ] 运行：cargo test --manifest-path starlink-dimension-router/Cargo.toml --test work_continuation；确认RED，实施“验证父版本→提帧→保存派生素材→继承并合并本轮要求→有限预占→提交新片段”。
- [ ] tail_reference明确标注近似参考；native_first_frame在契约取证和Task 9验证前保持关闭。native_video_extend只预留禁用能力，不把其实现作为本期前置依赖。
- [ ] 已完成版本异步调用warm_tail_frame并保存来源绑定的尾帧副本；续写优先用已保存副本，缺失再提取。后台任务重启可恢复，不修改已提交预算快照；异常单独标记，不阻塞视频交付或结算。
- [ ] 未支持、父视频丢失或素材不全在视频预算前返回准确原因；普通视频链路不被全局关闭。完成相关回归并提交。

## Task 8：MCP、真实状态与客户端下载回执

**Files — MCP:** 修改skills/aiwork-seedance/mcp/server.mjs、integration-test.mjs、fake-gateway.mjs、README.md；修改skills/aiwork-seedance/scripts/aiwork-seedance.ps1及仓库README.md。Core修改video_delivery.rs、reference_upload.rs、seedance_feedback.rs，新增tests/work_client_feedback.rs。

**Interfaces:** MCP新增seedance_continue(work_id, base_version_id, prompt, 可选规格/模式)与seedance_work_status(work_id)；原submit/generate增加可选work_context/action。PowerShell新增continue/work-status动作，默认下载和secret配置不变。现有普通工具的旧输入保持兼容，续写规格范围以Core 4–15秒/480p/720p为准。

- [ ] 添加假网关集成断言：old_submit_compatible、continue_has_exact_parent_and_new_request、wait_and_download_do_not_generate、utf8_prompts_and_paths_preserved、work_metadata_survives_tools。
- [ ] 运行：node skills/aiwork-seedance/mcp/integration-test.mjs；node skills/aiwork-seedance/mcp/gateway-config-test.mjs。先RED后GREEN，确保没有把API永久Key写入命令输出或作业快照。
- [ ] Core添加progress_truthful_and_not_repeated、stream_and_nonstream_context_roundtrip、native_upload_receipt_keeps_pending_version、delivery_failure_keeps_completed_video、delivery_retry_does_not_charge_video测试；运行cargo test --manifest-path starlink-dimension-router/Cargo.toml --test work_client_feedback。
- [ ] API直连仍只调用客户端声明的工具；缺少工具时提供原任务下载地址，不假称落盘。新增回执兼容不得破坏之前修好的invalid_delivery_tool_result处理。通过后分别提交MCP/Core改动。

## Task 9：整链路审计与受控真实付费验收

**Files:** 三仓库相关测试；Core新增docs/video-work-continuation-acceptance-20260928.md；AI Work能力证据文档。日期应在实际执行日更新。

- [ ] 确认AI Work最新程序、7864监听、代理及Core桥接正常；只使用授权测试Key，不修改旧用户账本。选择可用测试素材，记录摘要并检查可归属的余额/预占基线。
- [ ] 运行Core两crate全量测试、AI Work相关模块与现有视频/桥接回归、MCP假网关集成；所有命令和输出进入验收记录。Tauri宿主限制不能伪装为通过；只测试本改造必要边界。
- [ ] 复用已有可验证父视频，或生成V1：5秒480p、清晰参考图。随后同对话“改成夜景，其他不变”生成V2；检查确实继承原规格/素材，且新任务只提交一次。
- [ ] 从V2“接着再生成5秒”生成C1；比对尾帧来源、实际送给上游的素材摘要和新片段内容。若已取得原生首帧契约，再单独最多1次验证首帧控制，不凭HTTP成功打开开关。
- [ ] 主客户端按上述V1/V2/C1最多3次先验证主链；完整验收三种客户端各最多3次常规生成，共最多9次，严格首帧额外最多1次。上线前后共享上限，能够复用可验证V1时减少次数。失败先审日志、归属和回执，不循环盲重试。逐笔检查真实消耗、临近预估校正、额度释放，确认没有默认扣1或整Key冻结。
- [ ] 仅给隔离测试Key开放待验收tail_reference，预算/素材保护照常运行；原生首帧必须已取得契约。客户端验收分别覆盖TRAE Work CN API、DSH API、Codex MCP：连续改版、素材继承、续写、状态、自动下载/准确回退。不能操控某客户端时明确记为未验收，不用合成请求冒充通过。
- [ ] 进行同Key两对话、同父版本并发分支和两Key隔离验证；优先复用产物/假上游，真实付费并发若超出样本上限另行说明具体必要性。审计完成后列出各能力通过/未通过，不统一报“全支持”。

## Task 10：分仓推送、本机更新、公网灰度与回滚

**Files:** 三仓库README/验收记录；Core持久配置与部署说明；AI Work本机启动配置。正式执行前核对当前origin、分支和SSH目标，不在文档中保存凭据。

- [ ] 只有Task 9通过的能力可启用。确认Core/AI Work/MCP三个提交分别归对应仓库；本机AI Work仍部署在本机，公网只部署Core服务。
- [ ] 备份Core数据库、配置和现有发布包；本地迁移夹具验证v29→v30不改变旧账本。AI Work先部署兼容桥接版本，重启并检查7864/能力接口；Core再发布，MCP最后发布可选工具，旧客户端保持可用。
- [ ] 公网记录二进制哈希、Git提交、实际运行路径与持久配置。先灰度一个测试Key，执行状态读取、参考复用、改版和续写验证；剩余真实样本按Task 9总额度记录，不重复无必要生成。
- [ ] 若回滚，关闭work_context_enabled/continuation_enabled/native能力开关，保留新增库和新付费记录；使用schema30兼容的已验证包，禁止以旧库覆盖新账本或盲回退v29二进制。
- [ ] 归档必要的脱敏证据；删除本次可确认不再使用的临时测试文件，保留正式版本、用户产物和回滚备份。遇到删除政策拒绝不换方式绕过。
- [ ] 最终汇报：三项能力各自状态、客户端验收结果、实际积分、版本/仓库/运行位置，以及没有验证的原生能力边界。

## 执行顺序与停止条件

先Tasks 1–5打通持久化和连续改版，再Tasks 6–8接通尾帧参考续写与客户端编排，之后Task 9审计/真实验收，最后Task 10上线。Tasks 2–8采用小步RED→GREEN并提交，不能写完全部功能后才发现身份或扣费接口不兼容。

严格首帧取证失败不阻塞已验证的上下文改版和tail_reference交付，但要明确降级能力名称。Key额度不足、不可归属扣费、素材未送达或上游结果未知时停止新增付费视频，继续安全诊断；不能通过关闭计费保护、清空旧占用或更换账户盲重发来“跑通”。

当前交付仅为可审阅设计和实施计划。确认后由主代理按此顺序实施，不需要用户再次选择子代理模式。
