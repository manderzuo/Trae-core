# Seedance 流式兼容与 DeepSeek 辅助调度设计

日期：2026-09-24

状态：书面设计，待用户确认后实施

适用仓库：`manderzuo/Trae-core`（公网 Core）、`manderzuo/trae-maker`（AI Work）；MCP 仓库本阶段不改。

## 1. 目标与现状

用户在支持多模态的客户端中配置同一个 Core Base URL 与普通 API Key：普通文字模型继续走文字服务，选择 `seedance` 则可在 `POST /v1/chat/completions` 通过 `stream=true` 提交视频请求，等待真实生成结果，并取得可访问的 MP4。参考图既可使用现有 `image_url` data URL，也可先上传到 `/v1/assets` 后传 asset ID。Core 的权限、并发、幂等、额度预占与真实积分结算仍是唯一对外账本。

源码核对结果：

- Core 的 `starlink-dimension-router/src/user_routes.rs` 在 Seedance `stream=true` 时直接返回 `seedance_stream_unsupported`；非流式仅提交任务，成功后返回任务编号。
- AI Work 的 `src-tauri/src/api_server/routes.rs::seedance_chat_completions` 也拒绝 `stream=true`，当前 `wrap_seedance_video_response` 只输出异步任务 envelope。
- AI Work 的 `seedance_chat.rs` 已能提取最后一条用户文字、内联参考图和顶层视频参数，当前不调用辅助文字模型。
- Core 已有 `GET /v1/videos/:task_id`、`GET /v1/videos/:task_id/content` 与视频真实回执结算；AI Work 已有视频任务、受所有者约束的状态/内容接口和视频后台工作器。这些机制要复用，不再设计一套平行任务账本。
- 两仓库当前默认模型 ID 是 `deepseek-v4-flash`，不是已核实的“DeepSeek V4.1 Flash” ID。实施前须从实际 AI Work 模型目录与上游可用性确认目标模型；未确认不得以旧 ID 冒称 V4.1。

旧规格 `trae-maker/docs/superpowers/specs/2026-09-20-seedance-chat-completions-compatibility-design.md` 的“`stream=true` 返回 400”与本设计冲突；本设计实施后以本设计为准。

## 2. 边界与成功定义

1. `seedance` 是视频任务选择器，不是文字模型。请求中的 `stream` 只决定**下游响应协议**，不改变 Seedance 上游的异步生成性质。
2. 程序承担素材验证、任务提交、持久化、状态轮询、真实回执结算和错误分类。DeepSeek 辅助模型仅负责**可选的提示词理解/结构化**，不负责轮询、不执行下载、不决定扣费或是否重试。
3. 默认启用辅助模型时，用户只需配置一个对外模型 `seedance`；内部辅助模型由管理员在 AI Work 桥接设置中指定并验证。默认目标为用户要求的 DeepSeek V4.1 Flash；若实际目录不支持该 ID，部署保持辅助功能关闭并显式报配置不就绪，不静默改用其他模型。流式兼容改造不依赖辅助模型上线。
4. 对客户端“可用”的最低保证是：流式请求不会再因 `stream=true` 被拒；连接存活到任务完成时，最终 SSE 给出成功/失败及可下载的公网内容地址。**纯远端 API 无法直接写入另一台电脑的工作区**；自动落地需要客户端现有工具权限或 MCP/本地接收器。不能写入工作区时，返回受 Key 保护的下载路径，不能宣称已保存本地。
5. 本阶段不承诺所有客户端都能保持长连接、理解视频结果或自动下载；Codex/Claude Code 继续用 MCP 完成“下载到工作区”，其他客户端做协议兼容测试并如实标注能力。`stream=false` 的现有异步任务 API 保持兼容，不伪装成已完成。

## 3. 对外接口与响应

统一公网 Base URL：`https://api.gemstory.cn/v1`。接口为 `POST /chat/completions`，`model=seedance`；其它模型无行为变化。认证仍是 Core 普通 Key。Seedance 要求 `videos:submit`，结果读取要求 `videos:read`；内联/上传素材仍受相应素材权限及所有者隔离。现有客户端不一定暴露自定义 `Idempotency-Key`；见第 6 节，不能因为无该头就暗自重复提交。

流式响应使用 `Content-Type: text/event-stream`，按 OpenAI Chat Completions 常见 SSE 帧格式输出 `data: {"id":"chatcmpl-...","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"..."},"finish_reason":null}]}`；成功终帧 `finish_reason="stop"` 后输出 `data: [DONE]`。过程文本只描述“正在准备/生成”，不能含虚假的最终链接。可使用 SSE 注释帧 `: keep-alive\n\n` 保持连接；代理/CDN 要关闭响应缓冲，明确心跳间隔、总等待上限与空闲超时。

终帧的 `delta.content` 提供人可读摘要与 `https://api.gemstory.cn/v1/videos/{task_id}/content` 下载地址；具体地址由可信的 `public_base_url` 构造，不把上游临时原始 URL、管理员 Key、Bridge Key 或内部地址暴露给用户。对于能处理扩展元数据的客户端，可在同一帧附加 `video_task`，含 `id`、`status=completed`、`content_url`、`request_id`。不得依赖扩展字段才能获得结果。若生成失败，发送内容明确的失败终帧并 `[DONE]`；内部 `reconcile_required` 时不得伪装成功，提示同一请求 ID 对账，不建议重新提交。

`stream=false` 保持现有 202 + `video_task` 的异步契约，以免破坏已接入者；后续如要让非流式也阻塞到完成，另行设计显式参数与超时策略。`/v1/videos/{task_id}` 和 `/content` 仍可供客户端恢复查询。返回的下载地址须经普通 Key 授权；不在 URL 查询参数里放 Key。

## 4. 服务内状态机与时间边界

```text
鉴权/幂等/额度预占
  → [可选] DeepSeek 辅助结构化
  → Seedance 提交并记录 task_id/request_id/key_id
  → Core/AI Work 后台观察 queued/running
  → completed + MP4 可读 + 真实回执已确认 → SSE 成功终帧
  → failed + 真实回执已确认 → SSE 失败终帧
  → unknown / 回执不明 / 产物不可读 → 保留 hold，reconcile_required
```

流连接只是观察者：客户端断线、代理 504、取消等待或服务重启，均不能取消已提交任务、释放未确认的预占或再次提交视频。后台观察/结算必须独立于连接生命周期；SSE 发送失败只停止推送，任务继续运行。轮询采用有上限的指数退避与抖动，截止时间由配置给出，并低于已核实的公网代理总时限；超时时发送“任务仍在后台进行/可凭同一 ID 查询”，不制造新的请求。只有在任务与产物均可用、实际扣费回执完成或明确无扣费时，才允许发最终“视频已完成”。

对下游“生成完成”与“本地保存成功”必须分开：Core/AI Work 只能确认服务端可下载；由客户端适配器确认工作区写入。下载使用临时文件 + 原子重命名、文件大小/哈希校验与文件名去路径化，失败才显示下载地址。通用 Chat API 本身不具备客户端文件系统权限。

## 5. DeepSeek 辅助模型

辅助模型接收文本提示词及显式视频参数的最小必要副本，返回受 JSON Schema 约束的 `prompt`、`duration`、`resolution`、`ratio` 与可选场景说明。其响应只作候选；程序再次验证字段范围。用户显式提交的时长、分辨率、比例、参考素材 ID 始终优先。参考图原始字节和私有素材 URL **默认不发送给辅助模型**；它不能自己上传素材、执行工具或选择账本。

配置包含 `enabled`、`model_id`、`timeout_ms`、`max_output_bytes` 与版本号；设置只由管理员修改，不在普通 Key 请求中接受替代模型或辅助提示词。DeepSeek V4.1 Flash 模型 ID 与可用账户须由 AI Work `/internal/bridge/models` 或实际模型目录确认。若配置关闭，就沿用已存在的直接文本投影。若配置启用但模型不可达或回执无法核定，在**提交视频之前**失败并给出可识别错误；不能在视频已经提交后自动再次运行辅助模型或更换账号。

辅助调用如产生真实积分，必须归属同一 Core 普通 Key 和同一父视频请求，形成可审计的子请求 ID、真实上游回执和微积分金额。账本不以 token 用量、预估倍率、固定“1 积分”代替真实扣费。父请求的最终成本为已核定的辅助调用成本与视频调用成本之和，但两项各自独立结算、去重；视频失败时，已实扣的辅助成本不应被错误退还。现有 Core 单一视频回执接口不能未经设计就把两类成本混为一个 `task_ref`；实施任务必须先扩展多子回执/聚合方案并验证原子性、上下限与重放行为。若无法可靠取到辅助调用真实成本，则不能启用收费的辅助模式。

## 6. 身份、并发、幂等与异常

- Core 将请求绑定 `principal.key_id`、`user_id`、`request_id` 与视频 `task_id`；任务查询与内容读取只接受同一所有者，不能靠猜任务号跨 Key 访问。
- 有 `Idempotency-Key` 时，按 Key + 规范化请求哈希稳定复用原任务；同 Key 不同内容 409。无该头的客户端，应由服务端在**一次连接内部**生成并持久化请求 ID，但跨连接自动重试无法保证去重，故此版本是否开放无头提交必须经过客户端矩阵与风险确认；默认维持现有 400，不为“兼容任何客户端”而牺牲不重复扣费。
- 流式连接占用并发名额的时长与视频任务名额分离：提交额度与任务并发直到终态才释放；观察连接断开只释放连接资源。对同一任务多观察者设上限。
- Core 的额度预占必须覆盖可能产生的辅助 + 视频上限，或采用先辅助后视频的两阶段受控预占。任何阶段的实际回执都只对本 Key 落账。可用积分不足时在上游调用前拒绝；上游已接受但回执不明保持 hold 和人工/后台对账，不自动释放、不重新提交。
- 不记录 Key、JWT、Cookie、原始提示词、图片字节或完整上游请求体；日志只记录脱敏 key_id、父/子 request_id、task_id、状态、延迟、回执来源与积分数。

## 7. 发布与验收门槛

先单独发布 `stream=true` 兼容功能，模拟任务通过后再在功能开关后启用 DeepSeek 辅助；后一阶段失败可以关开关回退到现有直接投影。Core 与 AI Work 要按兼容版本顺序发布，在桥接双方能力探测通过前不对外宣传已支持流式。不得改变旧文本模型行为或旧非流式视频契约。

本地验收使用 D 盘隔离目录和 Mock 上游，不消耗真实积分；覆盖：流式成功、长任务心跳、失败、客户端断线、重启恢复、请求重放、素材隔离、缺 scope、超额并发、缺积分、真实回执未到、回执重复、辅助调用已扣而视频失败、无头客户端、代理超时及同一 Key/不同 Key 的任务隔离。最后仅在用户另行允许时做小额度公网真实验收，并按真实回执核对 AI Work 与 Core 两侧余额及产物。

本设计不是“任意客户端自动保存到工作区”的保证。客户端是否可自动下载，要按 Codex/Claude Code（MCP）及 Trae Work CN、Qoder Work、WorkBuddy、DeepSeek harness 等直接 API 客户端逐一实测，形成能力表；无文件写入能力者只返回下载链接。
