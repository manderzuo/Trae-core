# 受控无报价视频验收实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让指定的现有“周”Key 在无可信调用前报价时，安全完成一次 DeepSeek 辅助加 Seedance 视频的真实公网验收，并以两个真实回执对同一 Key 只扣一次总额。

**Architecture:** AI Work 桥接层明确识别并持久化受控操作授权，不再用假的报价 ID。Core 在 SQLite 中为父视频请求创建唯一操作级持有，两个步骤分别记录回执，最后一次性提交实际总费；现有报价路径及其他 Key 继续失败关闭。

**Tech Stack:** Rust、Axum、rusqlite/SQLite、serde、现有静态管理页、Windows PowerShell。两个现有 D 盘工作树分别为 `D:\gpt\trae-core-goal-billing`（Core）和 `D:\gpt\aiwork-goal-seedance-stream`（AI Work）。

**Spec:** `docs/superpowers/specs/2026-09-25-controlled-unquoted-billing-acceptance-design.md`，以及已有 `docs/superpowers/specs/2026-09-24-seedance-stream-deepseek-assist-design.md`。

## Global Constraints

- 受控模式只向现有“周”普通 Key 开启；其他 Key 和普通无报价请求继续失败关闭，不扩大到所有用户。
- 授权持有额不是上游价格承诺；真实费用可能超出余额，必须保留完整费用证据、记录差额并冻结该 Key。
- 两个步骤必须分别保留 Core 请求 ID 和可核验的 AI Work 真实回执；不得用 Token、固定积分或上游聚合余额差推算费用。
- 未知提交/回执不得自动重试或凭超时释放预占；断线和进程重启只恢复查询。
- Core 与 AI Work 用桥接认证传输内部关联，不信任普通客户端伪造的关联头；不把 Key、Cookie、JWT、图片原文写入日志。
- 测试输出与临时缓存只放 `D:\gpt`，精确识别后清理新生成的测试可执行文件；不删除用户数据或生产视频。
- 在模拟测试通过、版本和健康核实以前，不开启公网真实请求。只用现有“周”Key做受控真实验收，观察不明时不重复发送。

## Review Focus

1. 同 Key 两个并发 Seedance 请求：仅一个取得操作持有，另一请求在调用 AI Work 前失败（Task 2、Task 4 测试）。
2. DeepSeek 已产生真实费用但视频尚未提交：只结算 DeepSeek，不把整次操作误记为零费或重复扣费（Task 2、Task 4 测试）。
3. 客户端在任务提交后断开并原样重试：复用原父请求/任务，不能重发上游（Task 4、Task 5 测试）。
4. 重启时一个步骤只有未知回执：保留持有和 Key 禁入；恢复后同一请求回执只入账一次（Task 2、Task 5 测试）。
5. 浏览器或客户端伪造受控标记：非桥接 Key 不得取得 AI Work 受控归因，Core 管理权限也不能被普通 Key 调用（Task 1、Task 5 测试）。

---

### Task 1：AI Work 区分真实报价与受控操作授权

**Files:**
- Modify: `D:\gpt\aiwork-goal-seedance-stream\src-tauri\src\api_server\auth.rs`
- Modify: `D:\gpt\aiwork-goal-seedance-stream\src-tauri\src\api_server\bridge_billing.rs`
- Test: 两文件中的现有 `#[cfg(test)]` 模块与 `src-tauri/src/api_server/server.rs` 测试模块

**Interfaces:**
- Consumes: 已验证的 AI Work 桥接 Key、`x-core-request-id`、`x-core-key-id`，以及新内部头 `x-core-controlled-operation-id`。
- Produces: `CoreRequestAttribution { request_id, core_key_id, billing_mode, operation_id }`；`billing_mode` 为 `Quoted | LegacyOneShot | ControlledUnquoted`。同一请求 ID 第二次以不同 Key、模式或操作 ID 登记时返回冲突。

- [ ] **Step 1: 写失败测试。** 在 `auth.rs` 测试模块构造仅携带 `x-core-controlled-operation-id: op-123` 的普通客户端请求，断言没有桥接凭据时不能获得受控归因；构造有效桥接凭据请求，断言归因为 `ControlledUnquoted`；并在 `bridge_billing.rs` 测试同一请求 ID 的模式冲突：

```rust
assert_eq!(store.record_core_request_with_mode("req-a", "key-a", CoreBillingMode::ControlledUnquoted, Some("op-a")).unwrap(), CoreRequestRecord::Created);
assert_eq!(store.record_core_request_with_mode("req-a", "key-a", CoreBillingMode::ControlledUnquoted, Some("op-a")).unwrap(), CoreRequestRecord::Duplicate);
assert_eq!(store.record_core_request_with_mode("req-a", "key-a", CoreBillingMode::ControlledUnquoted, Some("op-b")).unwrap(), CoreRequestRecord::Conflict);
```

- [ ] **Step 2: 运行失败测试。** 在 AI Work 工作树使用 D 盘 Cargo 缓存运行对应 `cargo test --manifest-path src-tauri/Cargo.toml core_controlled_attribution --offline`；预期新枚举/参数尚不存在，编译失败。
- [ ] **Step 3: 实现最小兼容扩展。** 增加 `CoreBillingMode` 与 `bridge_core_request_modes(request_id PRIMARY KEY, mode, operation_id)` 扩展表；旧行按既有 `bridge_core_one_shot_test_requests` 推导模式。`core_attribution_headers` 仅在已验证桥接凭据后接受新头，要求它与 `x-core-quote-id` 互斥；普通 A 路径行为不变。模式/Key/操作 ID 冲突必须标记并拒绝，不默默覆盖。

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CoreBillingMode { Quoted, LegacyOneShot, ControlledUnquoted }

pub(crate) struct CoreRequestAttribution {
    pub request_id: String,
    pub core_key_id: String,
    pub billing_mode: CoreBillingMode,
    pub operation_id: Option<String>,
}
```

- [ ] **Step 4: 运行 AI Work 桥接鉴权/回执测试。** 测试新模式、旧 one-shot、无报价普通请求、同 request ID 冲突；现有最终回执查询仍按请求 ID 与唯一 session 查账，不随模式改写实际费用。
- [ ] **Step 5: 仅在 AI Work 仓库提交这些文件。** 提交说明 `feat: support controlled Core billing attribution`；不推送和部署。

### Task 2：Core v23 操作级持有和逐步骤回执

**Files:**
- Modify: `src-core/src/schema.rs`、`src-core/src/store.rs`、`src-core/src/lib.rs`
- Create: `src-core/src/controlled_billing.rs`
- Test: `src-core/tests/controlled_billing.rs`、`src-core/tests/migration.rs`

**Interfaces:**
- Consumes: 父请求、本地 Key 身份、新鲜上游余额快照、真实的辅助/视频 `BillingReceipt`。
- Produces: `CoreStore::begin_controlled_operation(parent_request_id, snapshot) -> Result<ControlledOperation, CoreError>`；`CoreStore::record_controlled_step(parent_request_id, step_request_id, kind, receipt) -> Result<ControlledStepResult, CoreError>`；`CoreStore::finish_controlled_operation(parent_request_id, video_submitted) -> Result<ControlledSettlement, CoreError>`。`kind` 为 `Assist | Video`；回执未知返回 `Held`，最终金额仅从 `Final` 或可证实无扣费回执取值。

- [ ] **Step 1: 写迁移与账本失败测试。** `schema_v22_to_v23_preserves_old_reservations` 先构造 v22 数据库和原有报价预占；迁移后它仍可查询。`controlled_assist_and_video_commit_once` 分配 100 积分，操作持有 100，辅助回执 0.5、视频回执 45.25，结束后 Key 已用 45.75、余额 54.25；重复两张回执和重复完成不再扣费。`controlled_assist_only_failure_commits_assist` 在视频未提交且已有辅助 0.5 时只扣 0.5。`controlled_unknown_receipt_survives_restart` 重开数据库仍持有全部可用积分。

```rust
let fresh_snapshot = aiwork_core::UpstreamCreditSnapshot {
    total: aiwork_core::CreditAmount::parse("1000.000000", "credits").unwrap(),
    updated_at_ms: chrono::Utc::now().timestamp_millis(),
};
let operation = store.begin_controlled_operation(&parent_id, fresh_snapshot).unwrap();
assert_eq!(operation.held.as_microcredits(), 100_000_000);
assert_eq!(store.record_controlled_step(&parent_id, &child_id, ControlledStepKind::Assist, assist_receipt).unwrap(), ControlledStepResult::Verified);
assert_eq!(store.record_controlled_step(&parent_id, &parent_id, ControlledStepKind::Video, video_receipt).unwrap(), ControlledStepResult::Verified);
assert_eq!(store.finish_controlled_operation(&parent_id, true).unwrap().actual_credits.as_microcredits(), 45_750_000);
```

- [ ] **Step 2: 运行失败测试。** `cargo test --manifest-path src-core/Cargo.toml --test controlled_billing --offline` 应因接口/表不存在失败。
- [ ] **Step 3: 建立 v23 表与存储边界。** 在 `SCHEMA_V23` 创建 `controlled_billing_operations`（唯一父请求、Key、操作级持有 ID、初始持有、状态）与 `controlled_billing_steps`（唯一步骤请求、操作 ID、步骤类别、回执哈希/金额/任务引用/状态）。v22→v23 事务保留原有记录；同 Key 受控运行操作使用 `CREATE UNIQUE INDEX ... ON controlled_billing_operations(api_key_id) WHERE state IN ('held','submitted','unknown')`。只在父请求建一笔 `quota_reservations`；子请求以关系表归属而不建第二笔全额持有。原有 A 路径 `billing_quotes` 与 `apply_credit_receipt` 不变。

```sql
CREATE TABLE IF NOT EXISTS controlled_billing_operations (
  operation_id TEXT PRIMARY KEY,
  parent_request_id TEXT NOT NULL UNIQUE REFERENCES requests(id),
  api_key_id TEXT NOT NULL REFERENCES api_keys(id),
  hold_reservation_id TEXT NOT NULL UNIQUE REFERENCES quota_reservations(id),
  held_microcredits INTEGER NOT NULL CHECK(held_microcredits > 0),
  state TEXT NOT NULL CHECK(state IN ('held','submitted','unknown','settled','released')),
  created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS controlled_billing_steps (
  request_id TEXT PRIMARY KEY REFERENCES requests(id),
  operation_id TEXT NOT NULL REFERENCES controlled_billing_operations(operation_id),
  kind TEXT NOT NULL CHECK(kind IN ('assist','video')),
  receipt_hash BLOB, actual_microcredits INTEGER CHECK(actual_microcredits >= 0),
  task_ref TEXT, state TEXT NOT NULL CHECK(state IN ('pending','verified','unknown','conflict')),
  UNIQUE(operation_id, kind)
);
```

- [ ] **Step 4: 原子入账。** `record_controlled_step` 先验证本地请求与 Key/关系、单位、来源、视频 task_ref、唯一回执，再把每个观察写入现有 `billing_receipts`，并更新步骤摘要；相同回执幂等，不同最终金额保留两份证据并冻结 Key。`finish_controlled_operation` 只在所有可能收费步骤可证明终态时，原子提交一次总费用并释放余量；实际费用超过持有时记账、记录差额并冻结 Key。v23 迁移在事务中重建 `api_key_billing_blocks`，保留旧行并把 `reason` CHECK 扩为 `over_quote | receipt_conflict | over_authorized_hold`，不把受控持有记作可信报价。
- [ ] **Step 5: 跑完整 Core Store 测试。** 包括同 Key 并发、不同 Key 独立、超额/重复/冲突、进程重启和旧 v22 数据迁移；测试 `quote_unavailable`、认证失败、上游快照过期各自的拒绝路径。
- [ ] **Step 6: 仅在 Core 仓库提交账本与迁移。** 提交说明 `feat(core): add controlled operation hold and real receipt settlement`。

### Task 3：Core 桥接明确转发受控操作

**Files:**
- Modify: `starlink-dimension-router/src/bridge_client.rs`
- Test: `starlink-dimension-router/src/bridge_client.rs` 中现有的 `RecordingBridge` 单元测试

**Interfaces:**
- Consumes: Task 2 的 `operation_id` 与两个步骤 `request_id`。
- Produces: `BridgeClient::forward_controlled_for_key(method, path, body, headers, request_id, operation_id, core_key_id)`；对应流式方法。只发桥接 Key、`x-core-request-id`、`x-core-key-id`、`x-core-controlled-operation-id`，不发假的 `x-core-quote-id`。

- [ ] **Step 1: 写失败测试。** 复用该文件的 `RecordingBridge`，调用新方法后断言受控头使用 Core 的操作 ID、普通客户端传入的 `x-core-controlled-operation-id` 被覆盖，且不出现 `x-core-quote-id`。

```rust
let recording = Arc::new(RecordingBridge::default());
let client = BridgeClient::from_transport("http://bridge", "bridge-secret", recording.clone());
let headers = BTreeMap::from([("x-core-controlled-operation-id".into(), "client-forged".into())]);
client.forward_controlled_for_key("POST", "/v1/chat/completions", b"{}", &headers, "request-assist", "operation-parent", "key-server").unwrap();
let seen = recording.last_headers();
assert_eq!(seen.get("x-core-controlled-operation-id").map(String::as_str), Some("operation-parent"));
assert_eq!(seen.get("x-core-request-id").map(String::as_str), Some("request-assist"));
assert!(!seen.contains_key("x-core-quote-id"));
```

- [ ] **Step 2: 运行失败测试。** `cargo test --manifest-path starlink-dimension-router/Cargo.toml bridge_client::tests::controlled_bridge_headers --offline` 应找不到新方法。
- [ ] **Step 3: 实现桥接方法。** 维持 `safe_headers` 现有白名单（仅 `accept`、`content-type`、`idempotency-key`），再插入经 Core 生成的三项内部关联；普通报价方法维持旧头。空 ID 或控制字符直接拒绝。
- [ ] **Step 4: 跑桥接模拟测试并提交 Core 仓库。** 提交说明 `feat(core): forward controlled operation attribution`。

### Task 4：Core 两个视频入口与 DeepSeek 辅助的同一操作准入

**Files:**
- Modify: `starlink-dimension-router/src/video_billing.rs`、`starlink-dimension-router/src/user_routes.rs`、`starlink-dimension-router/src/state.rs`
- Test: `starlink-dimension-router/tests/video_billing.rs`、`starlink-dimension-router/tests/seedance_stream.rs`、`src-core/tests/seedance_assist_billing.rs`

**Interfaces:**
- Consumes: Tasks 2–3 的操作级持有、逐步骤回执与桥接转发。
- Produces: 受控 Key 的 Chat Seedance 和原生视频入口；普通文字、其他 Key、现有 A 报价路径行为不变。

- [ ] **Step 1: 写失败测试。** 模拟视频报价 `quote_unavailable`，指定 Key 的 Chat Seedance 在辅助前建立持有并转发一次辅助 Chat、一次 Seedance Chat；非指定 Key 与不同请求哈希在第一步前拒绝。辅助已结算、视频明确拒绝只扣辅助；辅助回执未知时 Seedance Chat 转发计数为零；客户端断开/重连不二次转发；原生视频入口不调用辅助。

```rust
fixture.bridge.set_quote_unavailable(true);
let response = post_seedance_chat(&fixture).await;
assert_eq!(response.status(), StatusCode::OK);
assert_eq!(fixture.bridge.request_count("/v1/chat/completions"), 2);
assert_eq!(fixture.bridge.request_count("/v1/videos/generations"), 0);
```

- [ ] **Step 2: 运行失败测试。** 现有诊断路径会出现先辅助、后视频预占，新断言失败；确认失败点是准入顺序而非 FakeBridge 返回错误。
- [ ] **Step 3: 先完成本地验证再付费。** `chat_completions` 在 `run_seedance_assist` 前完成素材归属、参数与上游快照检查，创建父操作持有；`video_generations` 同样先准入但不创建辅助步骤。只在父视频报价明确不可用且管理员受控登记匹配 Key/哈希时启用 B，其他报价失败不降级。辅助与视频使用 Task 3 的方法，回执使用 Task 2 的操作分账而非现有普通 `apply_credit_receipt`；已知无费拒绝、未知提交、重启恢复分别按设计处理。
- [ ] **Step 4: 保留流式协议与幂等。** 保持既有 SSE 帧与 `[DONE]` 规则；成功终帧需 MP4 可读及操作账单终态，未知费用不伪装成功。无 `Idempotency-Key` 的窗口重连复用原请求；断线不重发生成。持久化视频 job 时保存受控操作 ID，以便重启继续查同一任务与回执。
- [ ] **Step 5: 跑相关集成测试并提交 Core 仓库。** 覆盖两个入口、参考素材权限、流式/非流式、并发、回执未知和重启；提交说明 `feat(core): admit controlled Seedance operation before paid assist`。

### Task 5：管理员受控开关、状态与审计

**Files:**
- Modify: `starlink-dimension-router/src/admin_routes.rs`、`src-core/src/video_billing.rs`、`starlink-dimension-router/static/index.html`
- Test: `starlink-dimension-router/tests/admin_api.rs`、`starlink-dimension-router/tests/video_billing_display.test.cjs`

**Interfaces:**
- Consumes: Task 2 的操作状态与 Task 4 的准入模式。
- Produces: 管理员可按现有 Key 内部 ID 和请求哈希登记一次受控验收、查看持有/步骤/真实费用/异常、关闭闸门；普通 Key 无法调用管理员接口。

- [ ] **Step 1: 写失败测试。** 管理员登记指定 Key 后状态显示 `armed`；请求被领取后显示 `claimed` 与请求 ID；同 Key 还有未结清操作时第二次登记返回 409；普通 Key 调用登记接口返回 401/403；刷新页面后状态与已核验辅助费用仍可见。
- [ ] **Step 2: 运行失败测试。** `cargo test --manifest-path starlink-dimension-router/Cargo.toml --test admin_api controlled_video --offline` 及 `node --test starlink-dimension-router/tests/video_billing_display.test.cjs` 应在新增断言处失败。
- [ ] **Step 3: 扩展现有视频闸门而非新增平行管理后台。** 管理 API 与静态管理页新增中文“受控无报价验收”状态、指定 Key、操作级持有、步骤真实费用和待对账原因；默认关闭。启停和领取写审计，关闭闸门不能释放已提交操作的持有。
- [ ] **Step 4: 跑管理员 API/页面测试并提交 Core 仓库。** 提交说明 `feat(core): expose controlled billing acceptance and status`。

### Task 6：隔离验收、部署与一次真实公网核对

**Files:**
- Modify: Core 与 AI Work 各自的部署说明/README（只记录已验证的启动、版本检查、回退和受控操作步骤）
- Test: 两仓库上文列出的现有与新增测试；公网 `/healthz`、Core 管理页状态、AI Work `/health`、任务/回执/MP4 接口

**Interfaces:**
- Consumes: Tasks 1–5 的兼容构建和现有“周”Key；不创建新的用户 Key，也不删除旧账本。
- Produces: 一条父/子请求 ID、两个真实回执、Core Key 实扣与余额、可下载 MP4 的可审计验收记录。

- [ ] **Step 1: 全量本地验证。** 在 D 盘指定 `CARGO_HOME`、`CARGO_TARGET_DIR`、`RUSTC`、`TEMP`、`TMP` 后分别运行两仓库 Rust 测试及 Core 页面测试；失败时先定位根因，不跳过安全测试。测试结束只清理精确新建的测试程序/符号和临时目录，保留共享依赖缓存与生产数据。
- [ ] **Step 2: 核对部署前状态。** 记录两仓库 commit、生产可执行文件版本/哈希、Core 与 AI Work 健康、Key 状态及旧任务持有数；有未知旧任务时先对账，不覆写数据库。
- [ ] **Step 3: 先部署 AI Work 兼容桥接，再部署 Core。** 每一步都核对服务健康与旧请求兼容；GitHub 只推送对应仓库的代码。部署失败恢复前一个可执行版本，不回滚数据库迁移或清除持有。
- [ ] **Step 4: 一次真实公网请求。** 管理端只为“周”Key 登记受控请求；发送一个最小 Seedance 任务，若辅助或视频已提交但响应不明则只按原请求 ID 查询，不再发送新生成请求。对照两个 AI Work 真实回执、Core 操作总费、Key 余额、任务 ID 与 MP4 内容，记录证据；任何一环缺失即保持待对账并报告失败，不声称通过。
- [ ] **Step 5: 验收后关闭受控登记。** 确认无新付费请求可进入，同时已提交任务仍可查询/结算；清理 D 盘精确的本次测试产物，保留视频和账本。仅当所有链路证据完整、无已知 BUG 且旧接口无回归时，才报告目标完成。
