# Seedance 流式兼容与 DeepSeek 辅助调度 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让公网 Core 的 `model=seedance` Chat Completions 流式请求持续返回真实视频结果，并在可核验真实积分的前提下启用后台 DeepSeek 辅助结构化。

**Architecture:** Core 将 Seedance Chat 请求中的 `stream=true` 转成上游非流式视频提交，再把持久化任务状态转换为客户端可解析的 Chat SSE。提交视频前，Core 通过 AI Work Chat 桥接调用默认文字模型（默认 `deepseek-v4-flash`）整理提示词；辅助请求有独立 Core request ID、报价、预留和真实回执，并与视频父请求绑定到同一普通 API Key。参考图只传 Seedance，不传辅助模型。AI Work 源码不做本次部署：GitHub 主分支与线上运行桥接路由有差异，重建覆盖存在回退风险。

**Tech Stack:** Rust、Axum、Tokio、SQLite/rusqlite、SerDe、Core Bridge、AI Work Tauri API server、SSE。

**Spec:** `docs/superpowers/specs/2026-09-24-seedance-stream-deepseek-assist-design.md`

## Global Constraints

- Core 与 AI Work 使用各自隔离的 D 盘 worktree；Core 目标仓库为 `https://github.com/manderzuo/Trae-core.git`，AI Work 仓库为 `https://github.com/manderzuo/trae-maker.git`。MCP 是独立仓库，本次不改。
- 所有构建、临时数据库、Cargo target、Rustup/Cargo 下载缓存和日志放在 `D:/gpt`；测试后只清理**本任务新建且路径经确认**的临时产物，不碰用户数据或整目录。
- 自动化测试仅用本地 Mock/fixture；用户本轮已明确授权使用现有“周”Key 做一次真实公网验收。该请求会分别消耗 DeepSeek 辅助和 Seedance 视频的上游积分；结果不明时不自动重试。
- 对外仍是 `https://api.gemstory.cn/v1` 与 Core 普通 Key；其他文字模型和 `stream=false` Seedance 现有 202 契约不变。
- Core 只用真实上游积分回执结算，不按 token、倍率或固定 1 积分估算；未知结果保留 hold，不自动重试。
- 辅助模型使用 Core `default_model` 配置；空值或误指向 Seedance 时回退到 `deepseek-v4-flash`。上游可用性必须由真实报价/请求验证；辅助与视频回执独立按实际积分结算。
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

## 第一阶段：Core Stream 兼容与 DeepSeek 分开记账辅助

### Task 1: 建立两仓库基线和隔离测试环境

**Files:**
- Read: `Trae-core/starlink-dimension-router/src/user_routes.rs`, `src/server.rs`, `src/bridge_client.rs`, `tests/video_billing.rs`
- Read: `trae-maker/src-tauri/src/api_server/routes.rs`, `seedance_chat.rs`, `video_worker.rs`, `video_store.rs`
- No product file edits.

**Interfaces:** Consumes current `BridgeClient::forward_billed`, Core video job store, AI Work `/v1/videos/*`; produces recorded baseline test results and a validated D-drive test root.

- [x] **Step 1: 验证两仓库工作树、远端和相关入口。** 在各仓库运行 `git status --short`, `git remote -v`, `rg -n 'seedance_stream_unsupported|fn video_task|fn video_content' ...`，记录基线 SHA；若有未提交用户变更，先保留并避让。
- [x] **Step 2: 设置 D 盘任务专属环境。** PowerShell 示例：

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

- [x] **Step 3: 只运行现有定向 Mock 测试并记录基线。** Core：`cargo test --manifest-path starlink-dimension-router/Cargo.toml --test video_billing --offline --locked`；AI Work：`cargo test --manifest-path src-tauri/Cargo.toml seedance_chat --offline --locked`。若离线依赖缺失，先报告缺失，不让 Cargo 自动把缓存写回 C 盘。
- [x] **Step 4: 记录可用代理/负载均衡器的读超时、缓冲配置。** 只检查配置，不修改线上；若单个 SSE 连接不可能覆盖视频最长时长，在 Task 3 中明确“超时后后台继续 + 可恢复查询”，不宣称保持到完成。

- [x] **Step 5: 对齐线上桥接路由与 AI Work 仓库基线。** 只读探测发现线上桥接路径受保护但 GitHub 主分支缺少路由注册，同时 AI Work 主分支存在 Rust 编译错误。发布前必须先补齐并验证仓库基线，不能用不可构建源码覆盖线上桥接。

**Acceptance:** 基线与失败项有记录，测试 I/O 全在 D 盘，未触达真实上游。

### Task 2: 定义 Chat SSE 格式与 Core 流式入口的失败测试

**Files:**
- Create: `Trae-core/starlink-dimension-router/src/seedance_sse.rs`（只负责编码安全的 Chat SSE 帧）
- Modify: `Trae-core/starlink-dimension-router/src/lib.rs`（注册 `seedance_sse` 模块）
- Create: `Trae-core/starlink-dimension-router/tests/seedance_stream.rs`

**Interfaces:** Produces `pub(crate) enum VideoStreamEvent { Progress(String), Completed { task_id: String, content_url: String, request_id: String }, Failed { code: String, request_id: String } }` and `pub(crate) fn encode_event(id: &str, event: VideoStreamEvent) -> Vec<u8>`; Core route consumes it in Task 3. Keep-alive encoder is `pub(crate) fn keep_alive() -> &'static [u8]`.

- [x] **Step 1: 先写失败测试。** 断言 `encode_event` 的 `object` 是 `chat.completion.chunk`，`choices[0].delta.content` 有用户可见结果；`Completed` 包含完整 HTTPS 内容地址但不包含 Bridge Secret；`Failed` 绝不包含成功字样；终态只发一次 `[DONE]`；心跳是 `: keep-alive\n\n`。将 HTTP fixture 留到 Task 3，与路由实现同一提交。
- [x] **Step 2: 跑新测试确认因缺少行为失败。** `cargo test --manifest-path starlink-dimension-router/Cargo.toml --test seedance_stream --offline --locked`；预期失败在新编码器不存在，而非 fixture 缺依赖。
- [x] **Step 3: 编写最小编码器。** 结构示例（序列化必须用 `serde_json::json!`，不要拼接用户文字）：

```rust
pub(crate) fn keep_alive() -> &'static [u8] { b": keep-alive\n\n" }
fn frame(value: serde_json::Value) -> Vec<u8> {
    format!("data: {}\n\n", value).into_bytes()
}
```

- [x] **Step 4: 跑编码器单测。** 测试通过后提交编码器及测试，不提交测试缓存。

**Acceptance:** SSE 帧可被通用 Chat SSE 解析器读取；所有文案与链接来自验证过的状态，输出无密钥。

### Task 3: Core 视频提交 + 后台观察 + SSE 推送

**Files:**
- Modify: `Trae-core/starlink-dimension-router/src/user_routes.rs`（Seedance 分支、幂等重放、视频状态观察）
- Modify: `Trae-core/starlink-dimension-router/src/state.rs`（若需要每任务观察器/广播器）
- Extend: `Trae-core/starlink-dimension-router/tests/seedance_stream.rs`
- Read: `Trae-core/starlink-dimension-router/src/video_reconciler.rs`

**Interfaces:** Consumes Task 2 `VideoStreamEvent/encode_event/keep_alive`; continues using `BridgeClient::forward_billed("POST", "/v1/chat/completions", ...)`, existing `reconcile_video_job_once`, `/v1/videos/{task_id}` and `/content`. Produces `stream_seedance_video_response(state: Arc<StarlinkRouterState>, principal: Principal, job_id: String, request_id: String) -> Response` after task acceptance.

- [x] **Step 1: 增加 Mock 失败测试。** 覆盖 `stream=true` 图片参考、桥接只提交一次且 `stream=false`、成功需任务/真实回执/HEAD 产物共同确认、未确认回执与内容 404 均不能成功、缺幂等键不发上游、`stream=false` 维持 202、不同 Key 不能读取。详见 `D:\gpt\seedance-stream-validation-20260924\logs\task-3-red.log`。
- [x] **Step 2: 运行失败测试。** 原始行为按预期失败：成功流返回 400、缺幂等键无错误码、未知回执缺非成功终帧、同用户第二 Key 可读视频。记录于 task-3-red.log。
- [x] **Step 3: 最小改动路由。** Seedance 只在桥接副本设置 `stream=false`，保留原始 body 的 request fingerprint 和 Idempotency-Key；持久化上游 task 后建立受限 SSE 观察器。成功需真实回执结算后 HEAD 内容确认。普通 Key、桥接密钥不进入 SSE URL。

```rust
let client_wants_stream = seedance
    && value.get("stream").and_then(serde_json::Value::as_bool) == Some(true);
if client_wants_stream {
    forward_value["stream"] = serde_json::Value::Bool(false);
}
```

- [x] **Step 4: 将观察器与 HTTP 生命周期解耦。** SSE 观察单任务仅限 1 个、全局最多 128 个；轮询与 10 秒心跳通过 SSE 推送，最长等待 14 分钟。连接断开不撤销持久化 job，现有后台 reconciler 继续查询。
- [x] **Step 5: 跑定向测试。** 最终 `seedance_stream` 为 12/12，通过幂等续接、两笔回执分开入账、图片仅给 Seedance 和未知辅助回执不提交视频。结果见 `D:\gpt\seedance-stream-validation-20260924\logs\seedance-stream-billing-green.log`。全量回归与发布仍待后续任务。

**Acceptance:** Core 流式入口不再出现 `seedance_stream_unsupported`；成功终帧只在实际任务与可信回执都就绪时出现。

### Task 4: 故障恢复、代理约束与端到端 Mock

**Files:**
- Extend: `Trae-core/starlink-dimension-router/tests/seedance_stream.rs`, `tests/video_billing.rs`
- Modify: `Trae-core/starlink-dimension-router/src/user_routes.rs`（仅限恢复缺口）
- Read: `Trae-core/starlink-dimension-router/src/server.rs`, `src/video_reconciler.rs`

**Interfaces:** Uses Task 3 persistent job/observer; no new public endpoint.

- [x] **Step 1: 写故障测试。** 覆盖幂等重连、客户端断开后台继续、不同 Key 读隔离、未知视频回执/缺少 MP4 不发成功，以及辅助回执不明时不提交视频。
- [x] **Step 2: 验证父子账务。** schema v22 持久化同 Key 父子关系；测试分别核验助手与视频 request ID、真实 receipt 和精确余额；未知助手回执保留 hold，释放未提交视频额度。
- [x] **Step 3: 验证 SSE HTTP 头和代理设置。** 测试断言 `text/event-stream`、`cache-control=no-cache, no-transform`、`x-accel-buffering=no`；线上 Nginx 关闭缓冲且 10 秒心跳可穿透。
- [x] **Step 4: 跑 Core 完整离线测试。** `src-core` 与 `starlink-dimension-router` 完整测试均通过；输出保存在 `D:\gpt\seedance-stream-validation-20260924\logs\core-src-core-full-final.log` 与 `core-router-full-final.log`。随后进入 GitHub、服务器部署和一次真实公网验收。

**Acceptance:** 客户端断开或代理超时不改变视频与账本状态；无真实请求、无伪成功。

## 原计划 AI Work 改造阶段（已被 Core 实现路径取代）

> 实施中复核发现：公网运行的 AI Work 桥接路由在当前 GitHub `main` 源码中不存在，直接编译部署会覆盖线上功能。本次改为由 Core 通过已存在的 AI Work Chat 桥接完成提示词辅助与独立结算，所以本节原 Task 5–7 的 AI Work 文件改动、可关闭开关和复合回执协议均不再执行。对应 Core 实现与测试已纳入 Task 3/4；AI Work 主机二进制本次不构建、不替换。

### Task 5: 确认模型 ID、辅助配置和纯文本结构化接口

**Files:**
- Create: `trae-maker/src-tauri/src/api_server/seedance_assist.rs`（配置校验、输出结构、显式字段优先级）
- Modify: `trae-maker/src-tauri/src/api_server/gateway_settings.rs`（持久化配置）
- Modify: `trae-maker/src-tauri/src/api_server/mod.rs`（模块注册）
- Extend: `trae-maker/src-tauri/src/api_server/seedance_chat.rs` tests

**Interfaces:** Produces `AssistSettings { enabled: bool, model_id: String, timeout_ms: u64, max_output_bytes: usize, version: u32 }`, `AssistDraft { prompt: String, duration: Option<u32>, resolution: Option<String>, ratio: Option<String> }`, and `merge_explicit_video_fields(original: &serde_json::Value, draft: AssistDraft) -> Result<serde_json::Value, SeedanceChatError>`. No network call in this task.

- [ ] **Step 1: 用 AI Work 当前模型目录和桥接 `/internal/bridge/models` 核验目标 ID。** DeepSeek 官方 ID 是 `deepseek-flash`，当前仓库预设 `deepseek-v4-flash`；由于 AI Work 通过自己的账号池转发，不直接等价于官方端点，只有真实 AI Work 目录确认并经 Mock 路由测试后才能启用；若不接受官方 ID，配置其明确目录映射，不能只改显示名。
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

### Task 8: 测试、GitHub 发布、服务器部署与真实验收

**Files:**
- Create: `Trae-core/docs/seedance-client-compatibility.md`（实测矩阵和配置示例）
- Modify: `Trae-core/README.md`（入口、流式/非流式区别与恢复说明）
- Modify: `trae-maker/docs/` 下相关运维说明（以现有文件为准）

**Interfaces:** Public `POST /v1/chat/completions`, `GET /v1/videos/:task_id`, `GET /v1/videos/:task_id/content`; no new client-side privilege is assumed.

- [ ] **Step 1: Mock 客户端矩阵。** 覆盖 Chat SSE 解析器、参考图 data URL、`/v1/assets`、`Idempotency-Key` 可配置性；Codex/Claude Code 的 MCP 自动保存工作区能力单列，Trae Work CN/Qoder Work/WorkBuddy/DeepSeek harness 逐个标注“仅可读 URL”或“可调用本地下载工具”，不能凭多模态输入能力推断文件写入能力。
- [ ] **Step 2: 加无头客户端测试。** `model=seedance` + `stream=true` + 无 `Idempotency-Key` 仍返回明确 400，不发送上游；文档给出 MCP 或客户端自定义头解决方式。
- [ ] **Step 3: 发布前确认远端与线上文件。** 测试通过后提交并推送 Core；检查 GitHub 远端无并行改动。只备份并替换已确认的 Core 发布文件，不覆盖未知目录、数据库或 AI Work 线上二进制。
- [ ] **Step 4: 先部署 Core，再做 1 次真实公网生成。** 使用现有“周”Key，先查余额与计费闸门，不新建 Key、不改额度。线上闸门当前为“已暂停”；若必须登记精确到 Key 与请求哈希的一次性诊断放行，提交前先向用户确认。核验辅助、视频两笔真实回执、Core 实际扣分、Key 余额与 MP4 内容地址。结果不明时停止，不自动重试。
- [ ] **Step 5: 推送对应仓库。** Core 推送 `Trae-core`；只有确认 AI Work 源码改动必要且测试通过时才独立推 `trae-maker`；不得混推 MCP 仓库。本次预期只发布 Core。

**Acceptance:** 两阶段各有可回滚开关、测试记录和客户端能力表；不会向用户宣称“所有客户端自动下载”。

## 完成前自检

- [ ] 逐条对照规格：流式成功/失败、视频素材、权限隔离、真实积分、断线恢复、辅助模型 ID、工作区保存边界都有测试和运维说明。
- [ ] 在 D 盘运行所有相关定向/全量 Mock 测试，保存结果摘要；检查 `git diff --check`、密钥扫描、构建产物路径与两仓库 `git status --short`。
- [ ] 测试完成后仅清理本轮任务专属 D 盘临时测试目录（确认绝对路径与是否仍被进程占用）；浅克隆源码与文档保留给用户审阅。
- [ ] 真实回执与视频内容均经核验后，再把该计划标记完成。Mock 通过或源码推送不等于已公网部署。
