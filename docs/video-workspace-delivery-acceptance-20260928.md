# Seedance 工作区自动交付验收（2026-09-28）

## 修复

此前 Core 仅在视频完成后返回内容 URL，没有发送客户端的本地工具调用。现在根据请求中实际声明的 Trae `RunCommand`（PowerShell 5）生成下载工具调用：视频完成 → `tool_calls` → 客户端工作区下载 → 回传保存路径 → Core 确认。

非流式与 SSE 使用同一响应；SSE 工具带 index、`finish_reason=tool_calls`、`[DONE]`。失败或不认识的工具回传不会再次提交原视频，不调用付费辅助模型。未声明已适配工具、关闭工具或本地执行失败时提供明确降级。

下载使用短期任务限定能力，不向命令暴露永久 Key。Key/用户停用、视频权限撤销、任务不匹配、授权过期或篡改都拒绝访问。后台输入缓存过期不会导致下载授权依赖临时 checkpoint。

## 验证证据

- RED：原实现新集成测试失败，实际 tool_calls 为 Null。
- RED：初版不认识的工具编号会进入旧生成流程；修复为无付费拒绝，GREEN。
- 视频/计费集成：20 项通过。
- Router 全套：185 项通过，0 失败。
- Core 全套：261 项通过，0 失败，1 个原有生产快照测试忽略。
- PowerShell 5 实际执行：正常文件、不覆盖、HTML 错误页、截断传输与临时文件清理通过。
- Linux release 构建通过。

测试日志保留于 `E:\AIWORK\core-audit-20260926\fast-settlement-implementation\tmp\task-3a2a\delivery-*.log`。

## 公网交付

复用已经生成的视频 `request_Xyu-CyJtAY0WXVqUf0ijkQ`。从该请求的加密业务 checkpoint 仅取出真实 RunCommand 工具 schema，在内存中处理；不导出凭据/完整提示词。用已有普通 Key 执行交付接口，不重新生成视频。

- 非流式与 SSE 交付返回标准 RunCommand 调用。
- 实际执行返回的 PowerShell 命令，保存到 E 盘隔离验收工作区。
- 文件长度：3,708,392 字节。
- SHA-256：`8c0e61d3efdeb2aecad1e088d0dcc194ff61cc30a2ccf508d50a93f56539264e`。
- 与 Bearer 鉴权原内容接口的流式下载哈希完全一致。
- 工具回传到 Chat Completions 后，SSE 正确确认本地保存路径，不再生成。
- 验收前后 requests=141、budget_steps=36 完全不变；原视频与辅助请求实扣仍为 120.8792 / 0.12 积分。
- 原未知图片请求 `request_DW9ftvbctUeE1vWnypLssA` 的 10 积分占用未修改。
- 本轮新增付费请求：0。

结果记录：`E:\AIWORK\video-delivery-20260928\acceptance-result.json`。验收 MP4 属于本轮临时文件，验收后删除；原用户视频与账本保留。

## 部署

公网仍为 `https://api.gemstory.cn/v1`，无需更改客户端 Base URL/Key/model。

Core release：`/opt/gemstory/starlink-dimension-router/releases/20260928-video-delivery/starlink-dimension-router`。

二进制 SHA-256：`b856b0179d5e39d6471d104b306f4e26e1498e6f40ec55a1edf869e3cfef9f02`。

备份：`/var/lib/starlink-dimension-router/backups/release-20260928-video-delivery`，保存原 Nginx、systemd release、router 配置及一致性 SQLite 快照。部署前暂停新增付费入口，保留已知未知扣费，不以清空账本换取发布。

Nginx 两个 server 的 `/v1/videos/[A-Za-z0-9_-]+/download` 专用 location 禁用 access/error 日志；HTTP 不重定向此能力 URL，HTTPS 流式代理下载。发布健康检查 200。

## 限制与自审

由实现者另做自审，未启用子代理或独立第二审阅者。仅保证已核验 Trae RunCommand 契约；本轮验收使用真实工具 schema + 实际本地 PowerShell + 公网协议，**没有执行 Trae 原生聊天界面的完整 UI 回合**。客户端自身的工具审批/权限策略仍然生效，其他 Agent 未声明相同工具时降级，需要分别适配，不能声称所有客户端均自动下载。

MCP 与 AI Work 仓库本轮没有改动；代码归属 `manderzuo/Trae-core` 现有修复分支，不直接合并 main。
