# Seedance 流式兼容与 DeepSeek 辅助调度 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让公网 Core 的 `model=seedance` Chat Completions 流式请求持续返回真实视频结果，并在可核验真实积分的前提下启用后台 DeepSeek 辅助结构化。

**Architecture:** 第一阶段由 Core 将流式 Chat 请求归一化为现有非流式视频提交，复用视频任务、状态、内容和回执接口，在 Core 对客户端输出 Chat SSE；AI Work 不必接收 `stream=true`。第二阶段在 AI Work 增加可开关的提示词辅助，在 Core/AI Work 之间建立同一普通 Key 下的父请求与独立子回执，不能将辅助模型成本默默并入单一视频回执。两个阶段各自可测试、可独立发布。

**Tech Stack:** Rust、Axum、Tokio、SQLite/rusqlite、SerDe、Core Bridge、AI Work Tauri API server、SSE。

**Spec:** `docs/superpowers/specs/2026-09-24-seedance-stream-deepseek-assist-design.md`

## Global Constraints

- 两仓库分别是 `D:/gpt/trae-core-design-20260924` 与 `D:/gpt/trae-maker-design-20260924` 的文档工作副本；真正实施时重新确认工作树、分支与远端，不能把本次浅克隆当生产工作树。
- 所有构建、临时数据库、Cargo target、Rustup/Cargo 下载缓存和日志放在 `D:/gpt`；测试后只清理**本任务新建且路径经确认**的临时产物，不碰用户数据或整目录。
- 自动化测试仅用本地 Mock/fixture；不得发送真实上游请求或消耗积分。生产部署和小额度真实验收另需用户确认。
- 对外仍是 `https://api.gemstory.cn/v1` 与 Core 普通 Key；其他文字模型和 `stream=false` Seedance 现有 202 契约不变。
- Core 只用真实上游积分回执结算，不按 token、倍率或固定 1 积分估算；未知结果保留 hold，不自动重试。
- DeepSeek V4.1 Flash 的真实模型 ID 必须经过模型目录核验；现有源码默认 `deepseek-v4-flash` 不得冒称 V4.1。
- 客户端本地写文件必须由有工作区权限的本地工具/MCP执行；纯远端 API 只保证安全下载地址。
- 普通用户 Key 不传给 AI Work，Bridge Secret 不出公网响应；素材、状态和内容均按 Core 所有者隔离。
- 子代理如用于执行，仅允许当前模型级别及以下；不允许以更高等级模型审查。

## Review Focus

1. 请求 `stream=true` 且同时传 data URL 图片：应只保存一份归属当前 Key 的素材，AI Work 收到 `stream=false` 与有效 asset ID，不发生重复上传（任务 2、3）。
2. 客户端在任务已提交后断连：任务继续、hold 保留；同幂等键重连不重新生成视频（任务 3、4）。
3. 服务重启后任务已完成但回执缺失：不能发成功终帧或释放额度，必须报待对账并保留任务（任务 3、4）。
4. 辅助调用已实扣、视频调用失败：只记辅助真实成本，视频仅在可信无扣费回执下释放，不能全额退回（任务 6、7）。
5. 不支持自定义 `Idempotency-Key` 的客户端：沿用明确 400 和配置提示，不悄悄无头提交导致重复扣费（任务 2、8）。

---

## 第一阶段：Core Stream 兼容，不依赖辅助模型

### Task 1: 建立两仓库基线和隔离测试环境

**Files:**
- Read: `Trae-core/starlink-dimension-router/src/user_routes.rs`, `src/server.rs`, `src/bridge_client.rs`, `tests/video_billing.rs`
- Read: `trae-maker/src-tauri/src/api_server/routes.rs`, `seedance_chat.rs`, `video_worker.rs`, `video_store.rs`
- No product file edits.

**Interfaces:** Consumes current `BridgeClient::forward_billed`, Core video job store, AI Work `/v1/videos/*`; produces recorded baseline test results and a validated D-drive test root.

- [ ] **Step 1: 验证两仓库工作树、远端和相关入口。** 在各仓库运行 `git status --short`, `git remote -v`, `rg -n 'seedance_stream_unsupported|fn video_task|fn video_content' ...`，记录基线 SHA；若有未提交用户变更，先保留并避让。
- [ ] **Step 2: 设置 D 盘任务专属环境。** PowerShell 示例：

```powershell
$validation = 'D:\gpt\seedance-stream-validation-20260924'
New-Item -ItemType Directory -Force -Path $validation, "$validation\tmp", "$validation\cargo", "$validation\rustup", "$validation\target" | Out-Null
$env:TEMP = "$validation\tmp"
$env:TMP = $env:TEMP
$env:TMPDIR = $env:TEMP
$env:CARGO_HOME = "$validation\cargo"
$env:RUSTUP_HOME = "$validation\rustup"
$env:CARGO_TARGET_DIR = "$validation\target"
```

- [ ] **Step 3: 只运行现有定向 Mock 测试并记录基线。** Core：`cargo test --manifest-path starlink-dimension-router/Cargo.toml --test video_billing --offline --locked`；AI Work：`cargo test --manifest-path src-tauri/Cargo.toml seedance_chat --offline --locked`。若离线依赖缺失，先报告缺失，不让 Cargo 自动把缓存写回 C 盘。
- [ ] **Step 4: 记录可用代理/负载均衡器的读超时、缓冲配置。** 只检查配置，不修改线上；若单个 SSE 连接不可能覆盖视频最长时长，在 Task 3 中明确“超时后后台继续 + 可恢复查询”，不宣称保持到完成。

**Acceptance:** 基线与失败项有记录，测试 I/O 全在 D 盘，未触达真实上游。

### Task 2: 定义 Chat SSE 格式与 Core 流式入口的失败测试

**Files:**
- Create: `Trae-core/starlink-dimension-router/src/seedance_sse.rs`（只负责编码安全的 Chat SSE 帧）
- Modify: `Trae-core/starlink-dimension-router/src/lib.rs`（注册 `seedance_sse` 模块）
- Create: `Trae-core/starlink-dimension-router/tests/seedance_stream.rs`

**Interfaces:** Produces `pub(crate) enum VideoStreamEvent { Progress(String), Completed { task_id: String, content_url: String, request_id: String }, Failed { code: String, request_id: String } }` and `pub(crate) fn encode_event(id: &str, event: VideoStreamEvent) -> Vec<u8>`; Core route consumes it in Task 3. Keep-alive encoder is `pub(crate) fn keep_alive() -> &'static [u8]`.

- [ ] **Step 1: 先写失败测试。** 断言 `encode_event` 的 `object` 是 `chat.completion.chunk`，`choices[0].delta.content` 有用户可见结果；`Completed` 包含完整 HTTPS 内容地址但不包含 Bridge Secret；`Failed` 绝不包含成功字样；终态只发一次 `[DONE]`；心跳是 `: keep-alive\n\n`。将 HTTP fixture 留到 Task 3，与路由实现同一提交。
- [ ] **Step 2: 跑新测试确认因缺少行为失败。** `cargo test --manifest-path starlink-dimension-router/Cargo.toml --test seedance_stream --offline --locked`；预期失败在新编码器不存在，而非 fixture 缺依赖。
- [ ] **Step 3: 编写最小编码器。** 结构示例（序列化必须用 `serde_json::json!`，不要拼接用户文字）：

```rust
pub(crate) fn keep_alive() -> &'static [u8] { b": keep-alive\n\n" }
fn frame(value: serde_json::Value) -> Vec<u8> {
    format!("data: {}\n\n", value).into_bytes()
}
```

- [ ] **Step 4: 跑编码器单测。** 测试通过后提交编码器及测试，不提交测试缓存。

**Acceptance:** SSE 帧可被通用 Chat SSE 解析器读取；所有文案与链接来自验证过的状态，输出无密钥。

### Task 3: Core 视频提交 + 后台观察 + SSE 推送

**Files:**
- Modify: `Trae-core/starlink-dimension-router/src/user_routes.rs`（Seedance 分支、幂等重放、视频状态观察）
- Modify: `Trae-core/starlink-dimension-router/src/state.rs`（若需要每任务观察器/广播器）
- Extend: `Trae-core/starlink-dimension-router/tests/seedance_stream.rs`
- Read: `Trae-core/starlink-dimension-router/src/video_reconciler.rs`

**Interfaces:** Consumes Task 2 `VideoStreamEvent/encode_event/keep_alive`; continues using `BridgeClient::forward_billed("POST", "/v1/chat/completions", ...)`, existing `reconcile_video_job_once`, `/v1/videos/{task_id}` and `/content`. Produces `stream_seedance_video_response(state: Arc<StarlinkRouterState>, principal: Principal, job_id: String, request_id: String) -> Response` after task acceptance.

- [ ] **Step 1: 增加 Mock 失败测试。** `stream=true` + 合法视频权限/幂等键/参考图应只调用一次 `forward_billed`，上游请求体强制 `"stream":false`；核对报价 `request_fingerprint` 与桥接对请求体的校验仍一致；queued/running 产生心跳，completed + 可读产物 + 真实最终回执才输出成功终帧和 `[DONE]`；failed、unknown、回执不明、产物 404 分别输出非成功结果。`stream=false` 仍返回 202，不同 Key 的任务读取 404。
- [ ] **Step 2: 运行失败测试。** `cargo test --manifest-path starlink-dimension-router/Cargo.toml --test seedance_stream --offline --locked`，确认是当前 400 或缺少 SSE 生命周期行为。
- [ ] **Step 3: 最小改动路由。** 先鉴权并做原有视频准入/额度预占；`seedance && stream` 时，仅对桥接副本改为 `stream=false`，保留原请求哈希与 `Idempotency-Key` 的归属。若 AI Work 实际校验转发体与报价指纹，必须明确升级桥接 quote/投影协议并同时测试，不能绕过校验；拿到 task ID 后持久化 job，再创建观察 SSE。不得在 AI Work 上再次用 `stream=true` 调用旧入口；不要把视频任务作为普通文字 `stream_chat_response` 处理。

```rust
let client_wants_stream = seedance
    && value.get("stream").and_then(serde_json::Value::as_bool) == Some(true);
if client_wants_stream {
    forward_value["stream"] = serde_json::Value::Bool(false);
}
```

- [ ] **Step 4: 将观察器与 HTTP 生命周期解耦。** 每任务唯一的后台状态观察由持久化 job 与现有 reconciler 驱动；SSE 只订阅状态、限制观察者数量及内存队列，发送失败不触发取消/退款/重试。心跳频率和最长等待低于已测代理限制；超时返回非成功终帧与 `request_id`，后台继续运行。
- [ ] **Step 5: 跑定向测试并提交。** 包括“断开 SSE 后任务仍可查询”“同幂等键只提交一次”“`stream=false` 原 202 未变”“图片归属不串 Key”。

**Acceptance:** Core 流式入口不再出现 `seedance_stream_unsupported`；成功终帧只在实际任务与可信回执都就绪时出现。

### Task 4: 故障恢复、代理约束与第一阶段端到端 Mock

**Files:**
- Extend: `Trae-core/starlink-dimension-router/tests/seedance_stream.rs`, `tests/video_billing.rs`
- Modify: `Trae-core/starlink-dimension-router/src/user_routes.rs`（仅限恢复缺口）
- Read: `Trae-core/starlink-dimension-router/src/server.rs`, `src/video_reconciler.rs`

**Interfaces:** Uses Task 3 persistent job/observer; no new public endpoint.

- [ ] **Step 1: 先写重启与故障测试。** 服务重启后的 job 仍可由原 Key 查询；上游已接受但 task ID 丢失维持 `reconcile_required`；真实回执重复只结算一次；完成任务但下载内容不可读不发成功；同一请求的重放不得再发上游。
- [ ] **Step 2: 跑失败测试，再补最小恢复代码。** 必须依据持久化 job 恢复观察，不能在内存广播器丢失后自动重新提交。Mock 中插入客户端断线、桥接超时、服务重启三个注入点。
- [ ] **Step 3: 验证 SSE HTTP 头与代理。** 断言 `content-type=text/event-stream`、`cache-control=no-cache`、`x-accel-buffering=no`；在本地代理 fixture 验证心跳不会被缓冲。线上代理超时若低于生成上限，文档和终帧必须说明按 request ID 恢复查询。
- [ ] **Step 4: 跑 Core 全量离线测试和 AI Work 现有 Seedance 测试。** `cargo test --manifest-path src-core/Cargo.toml --offline --locked`；`cargo test --manifest-path starlink-dimension-router/Cargo.toml --offline --locked`；AI Work 同步运行定向 `seedance_chat` 测试。提交第一阶段可独立发布的代码。

**Acceptance:** 客户端断开或代理超时不改变视频与账本状态；无真实请求、无伪成功。

## 第二阶段：可关闭的 DeepSeek 辅助与真实子回执

### Task 5: 确认模型 ID、辅助配置和纯文本结构化接口

**Files:**
- Create: `trae-maker/src-tauri/src/api_server/seedance_assist.rs`（配置校验、输出结构、显式字段优先级）
- Modify: `trae-maker/src-tauri/src/api_server/gateway_settings.rs`（持久化配置）
- Modify: `trae-maker/src-tauri/src/api_server/mod.rs`（模块注册）
- Extend: `trae-maker/src-tauri/src/api_server/seedance_chat.rs` tests

**Interfaces:** Produces `AssistSettings { enabled: bool, model_id: String, timeout_ms: u64, max_output_bytes: usize, version: u32 }`, `AssistDraft { prompt: String, duration: Option<u32>, resolution: Option<String>, ratio: Option<String> }`, and `merge_explicit_video_fields(original: &serde_json::Value, draft: AssistDraft) -> Result<serde_json::Value, SeedanceChatError>`. No network call in this task.

- [ ] **Step 1: 用 AI Work 当前模型目录和桥接 `/internal/bridge/models` 核验目标 ID。** 若仅见 `deepseek-v4-flash`，明确在配置中标记“V4.1 未确认”，功能保持 `enabled=false`。不得将显示名当 API ID。
- [ ] **Step 2: 先写纯函数失败测试。** 用户明确给的 `duration=5`、`ratio=16:9`、素材 ID 不被模型输出覆盖；模型输出过长、非法枚举、空提示词一律拒绝；原始图片字节与 data URL 不进入辅助输入。
- [ ] **Step 3: 跑失败测试。** `cargo test --manifest-path src-tauri/Cargo.toml seedance_assist --offline --locked`，确认失败原因是新结构化接口缺失。
- [ ] **Step 4: 实现并保存配置。** 复用 `gateway_settings.rs` 的原子配置保存约定；默认 `enabled=false`，`model_id` 只能填实测可用 ID。对普通 API 用户不开放修改。参数合并核心代码：

```rust
for field in ["duration", "resolution", "ratio", "image_asset_ids", "video_asset_ids"] {
    if original.get(field).is_some() { output[field] = original[field].clone(); }
}
```

- [ ] **Step 5: 跑纯函数测试并提交。** 该任务结束时辅助仍关闭，生产行为不变。

**Acceptance:** 已确认真实模型 ID 与配置持久化；未确认时不能开启；显式参数与素材不被模型改写。

### Task 6: Core/AI Work 的父子请求和真实积分回执协议

**Files:**
- Modify: `Trae-core/src-core/src/requests.rs`, `src/store.rs`, `src/credits.rs`（持久化父子关系与两个独立预占/结算）
- Modify: `Trae-core/starlink-dimension-router/src/bridge_client.rs`, `src/user_routes.rs`（报价、子回执校验）
- Modify: `trae-maker/src-tauri/src/api_server/core_bridge.rs`, `core_executor.rs`（按父请求记录辅助子请求与真实回执）
- Create: `Trae-core/src-core/tests/seedance_assist_billing.rs`
- Create: `Trae-core/starlink-dimension-router/tests/seedance_assist_billing.rs`

**Interfaces:** `parent_request_id` 绑定当前 Core `Principal`; 子请求键固定为 `(parent_request_id, "assist")` 和 `(parent_request_id, "video")`。每个子回执包含 `child_request_id`, `kind`, `model`, `actual_credits`, `unit="credits"`, `source_ref`, `observed_at_ms`, `task_ref`（视频子请求必需）。Core 现有单回执 `/internal/bridge/requests/{id}/billing` 对旧请求维持原样；新增版本化复合回执能力，不让旧解析器误读聚合成本。

- [ ] **Step 1: 先写 CoreStore 失败测试。** 两子请求只归属同一个 Key；实际微积分分别精确入账，重复回执不重复扣；辅助已实扣而视频 `failed_no_charge` 只保留辅助成本；任一回执 unknown/无可信 `source_ref` 时只结算已证实子项、未证实子项继续 hold；另一 Key 不能按 request ID 读明细。
- [ ] **Step 2: 先写桥接协议失败测试。** 新旧版本兼容；`child_request_id` 不属于父请求、积分单位非 credits、模型不一致、视频 `task_ref` 不一致、回执金额超过预占上限均拒绝并标记待对账。
- [ ] **Step 3: 跑两仓库失败测试后实现 schema/事务。** 同一父请求下，在上游调用前为辅助和视频分别取得可信最大报价并预占；若总额度不足，两个子请求都不发送上游。独立最终回执按 Core 已有 `CreditAmount` 微积分精度入账；事务失败不部分修改账本。AI Work 的桥接只提供观察与真实回执，不自行给用户 Key 扣第二份账。

```sql
-- 在既有 Core request/quota 表之上扩展，不另建第二个用户积分池。
CREATE TABLE composite_request_legs (
  parent_request_id TEXT NOT NULL,
  child_request_id TEXT NOT NULL UNIQUE,
  kind TEXT NOT NULL CHECK(kind IN ('assist', 'video')),
  owner_key_id TEXT NOT NULL,
  model TEXT NOT NULL,
  PRIMARY KEY(parent_request_id, kind)
);
-- 每个 child_request_id 对应既有 request/reservation/receipt 行；
-- INSERT 父子关系 + 两笔预占在 SQLite IMMEDIATE 事务中完成。
```

```json
{
  "version": 2,
  "parent_request_id": "req-parent",
  "legs": [
    {"child_request_id":"req-assist", "kind":"assist", "status":"final", "actual_credits":"0.100000", "unit":"credits", "source_ref":"provider-assist-1", "observed_at_ms":1790200000000},
    {"child_request_id":"req-video", "kind":"video", "status":"unresolved"}
  ]
}
```
- [ ] **Step 4: 完成版控协议与模型/回执能力探测。** Core 启动辅助开关前检查桥接双方都支持复合回执；不支持时拒绝开启，继续旧视频单回执模式。加健康检查断言，不向公网暴露 Bridge Secret。
- [ ] **Step 5: 跑定向及全量离线测试，提交两仓库对应改动。** 提交记录分别在各仓库；不得混推到 MCP 仓库。

**Acceptance:** 任何真实积分都能追溯到父请求、子请求、普通 Key 与上游来源；总余额不按 token 推算、不跨 Key、不因视频失败把辅助消耗退掉。

### Task 7: 调用辅助模型并接入 Seedance 提交

**Files:**
- Modify: `trae-maker/src-tauri/src/api_server/seedance_assist.rs`, `routes.rs`, `seedance_chat.rs`
- Modify: `trae-maker/src-tauri/src/api_server/core_executor.rs`（使用 Task 6 子请求计费）
- Create: `trae-maker/src-tauri/src/api_server/seedance_assist_tests.rs` 或扩展模块内 `#[cfg(test)]`

**Interfaces:** Consumes Task 5 `AssistSettings/AssistDraft/merge_explicit_video_fields` and Task 6 `BilledLeg { parent_request_id: String, child_request_id: String, quote_id: String }`（在 `core_bridge.rs` 定义并绑定 Core 校验后的 owner）；produces 规范化视频输入交给现有 `videos_generations`，不得重复上传参考图。

- [ ] **Step 1: 写 Mock 失败测试。** `enabled=false` 时绝无文字模型请求；启用且目录验证成功时只调用一次指定辅助模型，输入只含文字/公开参数，无图片字节；输出 JSON schema 校验后保留用户显式参数；辅助超时或回执不明时视频提交数为 0，已发生的辅助账务保持待核/已扣；视频提交后断线不重跑辅助。
- [ ] **Step 2: 跑失败测试。** `cargo test --manifest-path src-tauri/Cargo.toml seedance_assist --offline --locked`，确认新行为缺失。
- [ ] **Step 3: 最小实现：** 在进入原有 `videos_generations` 前执行单次有界辅助调用，指定经验证模型 ID、低超时和 JSON schema；通过 Task 6 子请求引用获得真实回执并记账；只在辅助已得可核定结果、视频预占仍有效时提交视频。辅助调用不能调工具、不能自行下载、不能更改 Key、不能直接决定真实扣费金额。

```rust
pub(crate) trait AssistInvocation {
    async fn invoke(
        &self,
        model_id: &str,
        text: &str,
        leg: &BilledLeg,
    ) -> Result<AssistDraft, SeedanceChatError>;
}

pub(crate) async fn prepare_video_input(
    client: &impl AssistInvocation,
    settings: &AssistSettings,
    original: &serde_json::Value,
    text: &str,
    assist_leg: &BilledLeg,
) -> Result<serde_json::Value, SeedanceChatError> {
    let draft = client.invoke(&settings.model_id, text, assist_leg).await?;
    merge_explicit_video_fields(original, draft)
}
```

路由在获得 `prepare_video_input` 的结果后调用现有 `videos_generations`；`AssistInvocation::invoke` 的生产实现只用现有 AI Work 模型执行/真实回执适配器，不创建第二个公网入口。
- [ ] **Step 4: 测试失败策略及恢复。** 注入模型 429、超时、非法 JSON、未知回执、视频提交接受后断线；每个场景核对上游调用次数和 Core hold/settlement。运行 AI Work 与 Core 定向测试并分别提交。

**Acceptance:** 开关关闭时与第一阶段完全一致；开关开启时按指定可用模型只调一次，最终视频结果与真实积分都可追踪。

### Task 8: 客户端矩阵、发布、回滚与真实验收准备

**Files:**
- Create: `Trae-core/docs/seedance-client-compatibility.md`（实测矩阵和配置示例）
- Modify: `Trae-core/README.md`（入口、流式/非流式区别与恢复说明）
- Modify: `trae-maker/docs/` 下相关运维说明（以现有文件为准）

**Interfaces:** Public `POST /v1/chat/completions`, `GET /v1/videos/:task_id`, `GET /v1/videos/:task_id/content`; no new client-side privilege is assumed.

- [ ] **Step 1: Mock 客户端矩阵。** 覆盖 Chat SSE 解析器、参考图 data URL、`/v1/assets`、`Idempotency-Key` 可配置性；Codex/Claude Code 的 MCP 自动保存工作区能力单列，Trae Work CN/Qoder Work/WorkBuddy/DeepSeek harness 逐个标注“仅可读 URL”或“可调用本地下载工具”，不能凭多模态输入能力推断文件写入能力。
- [ ] **Step 2: 加无头客户端测试。** `model=seedance` + `stream=true` + 无 `Idempotency-Key` 仍返回明确 400，不发送上游；文档给出 MCP 或客户端自定义头解决方式。
- [ ] **Step 3: 按兼容顺序准备发布。** 先发布 AI Work 可读取旧非流式提交/状态/内容的版本，再发布 Core 第一阶段；第二阶段独立配置开启。发布前备份配置与账本，确认代理不缓冲 SSE、心跳可穿透、下载地址公开域名正确、视频计费闸门健康。
- [ ] **Step 4: 限流灰度与回滚演练。** 先仅允许测试 Key；发现 SSE 异常关闭 Core 流式功能标志，保留已创建视频任务与 hold；发现辅助异常关闭 `AssistSettings.enabled`，不删除任何子回执。检查旧 `stream=false` 及普通文字模型。
- [ ] **Step 5: 书面列出真实验收请求。** 需用户另外明确批准消耗多少真实积分、使用哪个测试 Key 与是否保存产物；获批后只做 1 次小额度公网生成，并对照父/子 request ID、AI Work 原始回执、Core 实际扣分、Key 余额及 MP4 下载结果。未获批不发送。

**Acceptance:** 两阶段各有可回滚开关、测试记录和客户端能力表；不会向用户宣称“所有客户端自动下载”。

## 完成前自检

- [ ] 逐条对照规格：流式成功/失败、视频素材、权限隔离、真实积分、断线恢复、辅助模型 ID、工作区保存边界都有测试和运维说明。
- [ ] 在 D 盘运行所有相关定向/全量 Mock 测试，保存结果摘要；检查 `git diff --check`、密钥扫描、构建产物路径与两仓库 `git status --short`。
- [ ] 测试完成后仅清理本轮任务专属 D 盘临时测试目录（确认绝对路径与是否仍被进程占用）；浅克隆源码与文档保留给用户审阅。
- [ ] 用户确认后再执行代码改造；生产发布与真实积分验收需分别确认。文档完成本身不等于功能已上线。
