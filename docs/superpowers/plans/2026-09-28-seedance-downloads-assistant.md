# Seedance Downloads Assistant Implementation Plan

> **For agentic workers:** Use superpowers:executing-plans; user explicitly prohibits subagents.

**Goal:** 直接 API 使用 glm-5.3-flash 调度客户端现有工具，无预装脚本自动保存系统 Downloads。

**Architecture:** 沿用 V2 计费和下载凭证。下载工具采用有限适配器，辅助模型仅做受约束的工具选择；Core 注入经测试的下载命令。下载规划独立幂等、回执无付费重入。

**Tech Stack:** Rust/axum/SQLite、PowerShell 5、Bash/curl、systemd/Nginx。

**Spec:** ../specs/2026-09-28-seedance-downloads-assistant.md

## Global Constraints

- glm-5.3-flash；系统 Downloads；不安装 MCP/Skill；不重做视频、不伪造计费。
- 凭证和其他用户数据不进入日志；保留未知账单；当前代理自行执行。
- 使用已存在的 D 盘隔离工作树；测试目录和已有编译缓存使用 E 盘，清理本轮临时产物。

## Review Focus

- 客户端更改工具编号或包装输出：仍能关联自己的已完成视频。
- 无工具、禁止联网、辅助模型失败：不误报已保存、不重新生成。
- Key/用户停用、他人任务、票据篡改：不能下载或确认他人视频。
- 系统 Downloads 重定向、路径有空格：准确保存、不覆盖。
- SSE 工具参数与幂等重试：真实工具契约、无重复辅助付费。

### Task 1: 助手与工具交付

Files: config.rs, budget_flow.rs, video_delivery.rs, new delivery_assist.rs, budget_video_admission.rs.
Interfaces: async completion produces standard OpenAI response; planner returns one supported tool name, never arbitrary commands.

- [x] RED：受控桥接的辅助调用计数断言先失败；工具适配与回执覆盖加入集成测试。
- [x] 实现 Seedance 专用模型配置、有限工具适配、独立幂等计费规划；注入受控命令。
- [x] GREEN：Router 协议与现有视频/权限/并发测试通过。

### Task 2: 系统下载目录与本地落盘

Files: video_download.ps1, new video_download.sh, delivery command tests.
Interfaces: local command prints SEEDANCE_DELIVERY_RECEIPT, default system Downloads, only owned .part cleanup.

- [x] RED：默认目录与 Bash 实际执行测试发现长命令解析失败；压缩内嵌命令后通过。
- [x] Windows Downloads 注册表/系统目录解析；Git Bash 自动选择 PowerShell，Unix 使用 curl；不需安装脚本。
- [x] GREEN：正常、不覆盖、错误类型、截断、路径和清理测试；全套 Core/Router。

### Task 3: 自审、部署和公网验收

Files: acceptance report and narrowly scoped deployment/acceptance scripts outside C.
Interfaces: publish verified Core binary/config; existing public Base URL unchanged.

- [x] 当前代理自审安全与重试路径（未使用独立审查代理）；Linux release 构建。
- [ ] 推送 Trae-core 修复分支；带一致性快照/回滚发布 Core，不部署 AI Work 到云。
- [ ] 普通 Key 复用已有视频跑 GLM + 本机工具 + SSE + 回执；核对仅新增文字计费、没有新增视频。
- [ ] 清理本轮测试临时视频/目录；报告实际验证和未验证客户端边界。

## Execution Ledger

- Preflight: existing isolated worktree D:/gpt/trae-core-goal-billing, baseline dd69e24, clean; local AI Work PID 49268 and public Core PID 1706392.
- User start instruction authorizes the agreed design, implementation, repository push and deployment; no additional approval loop.
- 2026-09-28: targeted RED 19 passed / 1 failed, followed by 20 passed / 0 failed. Full Router 187 passed; Core 261 passed, 1 existing ignored production snapshot; both exit 0.
- Actual PowerShell 5 and Windows Git Bash commands passed, including apostrophe/CJK paths. Test Downloads was redirected per process to E; no registry change or C test video. Actual WSL Linux Bash: saved twice without overwrite, wrong MIME and truncation failed, own temporary parts cleaned.
- Self-review declined explicitly network-disabled tools and generic terminal tools with unknown shell. A failing assertion for unknown exec_command shell was fixed before the final full suite.
- Real public GLM tool-calling preflight returned Bash/tool_calls (request_h3XQrO_BLovmCGPkjgRTRw); no new video. No client-native UI acceptance claimed.
- Linux release SHA-256: 2f081f987c693a5849a6737162df47e8c3a7558a3bf64ac3e9c935fe5e1db660. Deployment and existing-video acceptance pending at implementation commit.
