# Seedance Workspace Delivery Implementation Plan

> **For agentic workers:** Use superpowers:executing-plans; user requests native execution, no subagents.

**Goal:** Seedance 完成后自动通过客户端工具保存视频到工作区，并防止工具结果触发重复生成。

**Architecture:** Core 返回标准工具调用，短期加密能力只允许下载原任务。客户端执行 PowerShell 下载并返回回执；Core 无付费步骤地确认保存。不存在适配工具时提供明确降级。

**Tech Stack:** Rust/Axum/SQLite、现有 KeyVault、PowerShell 5。

**Spec:** docs/superpowers/specs/2026-09-28-video-workspace-delivery.md

## Global Constraints

- 不传出永久 Key，不改账本，不释放未知扣费。
- 仅使用客户端声明的工具，遵从工具审批，不声称远端文件已保存而无回执。
- 15 分钟、单任务下载授权，测试临时文件放 E 盘。

## Review Focus

- SSE 的工具 index / finish_reason 与非流式保持一致。
- 工具回传、失败、伪造调用编号均不得新增付费任务。
- 过期、篡改、跨 Key、停用 Key 与撤销作用域均拒绝下载。
- 工作区含空格/单引号、已有文件、HTML 错误页、断线时不覆盖文件。
- 授权 URL 不进入访问日志；只支持已核验工具，不默认适配所有 Agent。

### Task 1: 工具交付与无付费回传

**Files:** create `starlink-dimension-router/src/video_delivery.rs`; modify `budget_flow.rs`, `lib.rs`; test `tests/budget_video_admission.rs`.

**Interfaces:** `completion(state, principal, request, body) -> Result<Value, String>`; `follow_up(state, principal, body) -> Option<Response>`; `sse_completion(value) -> Vec<u8>`.

- [x] 写并运行已完成 Seedance + RunCommand 工具的集成测试；旧实现失败：tool_calls 为 Null，期待 RunCommand。
- [x] 实现 schema 校验、PowerShell 命令与客户端回执处理；非流式/SSE 都返回标准调用。
- [x] 验证工具成功/失败、无工具、tool_choice=none；确认 sends 不增加、无新预算。

### Task 2: 短期下载授权与重试入口

**Files:** modify `server.rs`, `src-core/src/identity.rs`; extend `video_delivery.rs` and integration tests.

**Interfaces:** `CoreStore::active_principal_for_request(request) -> Result<Option<Principal>, CoreError>`; authenticated POST `/v1/videos/:task_id/delivery`; public GET `/v1/videos/:task_id/download?ticket=...`.

- [x] 所有权/授权生命周期集成测试通过；未知 tool_call_id 回传测试先失败，再修复通过。
- [x] 复用 KeyVault 密封任务限定授权；GET 重查当前身份，委托现有有界 MP4 下载。
- [x] 验证过期/篡改/错任务/错 Key/撤销授权及重复交付没有计费派发。

### Task 3: 审计与上线验收

**Files:** README/API 文档、验收记录；部署脚本和测试输出存 E 盘。

- [x] 实际运行生成的 PowerShell 5 命令，验证文件、失败清理和不覆盖。
- [x] 完整 Core 261 + Router 185 测试通过，作者单独自审协议/授权/账本边界，Linux release 构建通过。
- [x] 备份并部署 Core 与 Nginx 下载日志限制，已知未知请求的 10 积分占用保持不变。
- [x] 公网复用既有视频验收下载和回传，账本无变化、无新增生成请求；推送/清理结果见验收记录。

## Progress

- 现有隔离 worktree 干净，基线沿用本次对话已通过的 Core 261 + Router 181 测试。
- Ruling: 实际工具只核验到 Trae RunCommand，先保证此契约；其他工具缺失时降级，避免虚构跨客户端支持。
- Ruling: 不使用子代理；由当前作者另做独立自审，强度低于第二位审阅者。
- Task 1/2: complete — `run-core-check.ps1 -Test budget_video_admission` 20 passed; `delivery-red.log` 与 `delivery-fail-closed-red.log` 保存 RED→GREEN 证据。
- Task 3: complete — `run-core-check.ps1 -All` 185 passed/0 failed；`run-core-library-check.ps1` 261 passed/0 failed/1 ignored（已有生产快照测试）；Linux release exit 0；公网工作区交付哈希一致。
- Final review: self-review (user requested no subagents). 对照 spec 检查 SSE/tool_calls、当前授权、仅下载 capability、失败/无效工具回传、账本无变更；没有未处理的重要发现。
- Ruling: 不再额外付费生成；复用已完成视频验收真实公网交付。原生 Trae UI 是否自动接受本地工具仍须用户复测，不以脚本验收冒充 UI 验收。
