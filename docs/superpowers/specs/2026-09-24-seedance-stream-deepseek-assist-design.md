# Seedance 流式兼容与 DeepSeek 辅助调度设计

日期：2026-09-24

状态：已按 Core-only 路径实施，本地 Core 全量测试通过；待线上部署与一次真实公网验收

适用仓库：`manderzuo/Trae-core`（公网 Core）、`manderzuo/trae-maker`（AI Work）；MCP 仓库本阶段不改。

## 1. 目标与现状

用户在支持多模态的客户端中配置同一个 Core Base URL 与普通 API Key：普通文字模型继续走文字服务，选择 `seedance` 则可在 `POST /v1/chat/completions` 通过 `stream=true` 提交视频请求，等待真实生成结果，并取得可访问的 MP4。参考图既可使用现有 `image_url` data URL，也可先上传到 `/v1/assets` 后传 asset ID。Core 的权限、并发、幂等、额度预占与真实积分结算仍是唯一对外账本。

源码核对结果：

- Core 的 `starlink-dimension-router/src/user_routes.rs` 在 Seedance `stream=true` 时直接返回 `seedance_stream_unsupported`；非流式仅提交任务，成功后返回任务编号。
- AI Work 的 `src-tauri/src/api_server/routes.rs::seedance_chat_completions` 也拒绝 `stream=true`，当前 `wrap_seedance_video_response` 只输出异步任务 envelope。
- AI Work 的 `seedance_chat.rs` 已能提取最后一条用户文字、内联参考图和顶层视频参数，当前不调用辅助文字模型。
- Core 已有 `GET /v1/videos/:task_id`、`GET /v1/videos/:task_id/content` 与视频真实回执结算；AI Work 已有视频任务、受所有者约束的状态/内容接口和视频后台工作器。这些机制要复用，不再设计一套平行任务账本。
- DeepSeek 官方在 2026-09-10 发布 V4.1 Flash，官方 API 当前模型 ID 为 `deepseek-flash`；`deepseek-v4-flash` 是暂时兼容名。两仓库现有默认值是 `deepseek-v4-flash`，但 AI Work 使用自己的 Trae/WorkBuddy 上游目录，不等同于直连 DeepSeek 官方 API，因此启用辅助前仍须核对 AI Work 实际模型目录；模型别名未被该目录接受时，应通过明确映射适配，不得仅改展示名。来源：[DeepSeek 更新日志](https://api-docs.deepseek.com/updates/)。

旧规格 `trae-maker/docs/superpowers/specs/2026-09-20-seedance-chat-completions-compatibility-design.md` 的“`stream=true` 返回 400”与本设计冲突；本设计实施后以本设计为准。

## 2. 边界与成功定义

1. `seedance` 是视频任务选择器，不是文字模型。请求中的 `stream` 只决定**下游响应协议**，不改变 Seedance 上游的异步生成性质。
2. 程序承担素材验证、任务提交、持久化、状态轮询、真实回执结算和错误分类。当前 Core 在所有 Seedance Chat 请求（流式与非流式）前调用 DeepSeek 辅助整理提示词；助手不负责轮询、不执行下载、不决定扣费或是否重试。
3. 用户仍只配置对外模型 `seedance`。内部辅助模型取 Core 的默认文字模型设置；空值或误设为 Seedance 时回退到 `deepseek-v4-flash`。AI Work 仓库主分支缺少线上已运行的桥接路由，故这次不重建/覆盖 AI Work 服务；公网真实请求将验证当前上游目录是否接受该模型 ID。辅助不可用时在提交视频前失败关闭。
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
  → 独立计费的 DeepSeek 提示词整理
  → Seedance 提交并记录 task_id/request_id/key_id
  → Core/AI Work 后台观察 queued/running
  → completed + MP4 可读 + 真实回执已确认 → SSE 成功终帧
  → failed + 真实回执已确认 → SSE 失败终帧
  → unknown / 回执不明 / 产物不可读 → 保留 hold，reconcile_required
```

流连接只是观察者：客户端断线、代理 504、取消等待或服务重启，均不能取消已提交任务、释放未确认的预占或再次提交视频。后台观察/结算必须独立于连接生命周期；SSE 发送失败只停止推送，任务继续运行。轮询采用有上限的指数退避与抖动，截止时间由配置给出，并低于已核实的公网代理总时限；超时时发送“任务仍在后台进行/可凭同一 ID 查询”，不制造新的请求。只有在任务与产物均可用、实际扣费回执完成或明确无扣费时，才允许发最终“视频已完成”。

对下游“生成完成”与“本地保存成功”必须分开：Core/AI Work 只能确认服务端可下载；由客户端适配器确认工作区写入。下载使用临时文件 + 原子重命名、文件大小/哈希校验与文件名去路径化，失败才显示下载地址。通用 Chat API 本身不具备客户端文件系统权限。

## 5. DeepSeek 辅助模型

辅助模型接收文本提示词及显式视频参数的最小必要副本，仅返回 JSON 对象中的 `prompt`。Core 再次验证非空及长度，并只替换用户文字；显式时长、分辨率、比例、参考素材保持原样。参考图原始字节和私有素材 URL 不发送给辅助模型；素材归属/大小/格式必须在任何付费助手调用之前验证。助手不能上传素材、执行工具或选择账本。

当前实现没有独立 `enabled` 开关；模型由 Core 默认文字模型配置选择，回退 ID 为 `deepseek-v4-flash`。普通 API 用户不能自行覆盖辅助模型或辅助提示词。若模型不可达、结果格式不合法或回执无法核定，在**提交视频之前**失败；不能自动再次运行辅助或更换账号。若需独立管理开关，应作为后续单独改造，不得在部署说明中宣称已存在。

辅助调用产生的真实积分归属同一 Core 普通 Key，并有独立子 request ID、报价、预留、真实上游回执与结算记录；schema v22 的 request relation 将其与父视频请求关联。账本不以 token 用量、预估倍率或固定“1 积分”代替真实扣费。两项各自独立结算、按请求 ID 去重；视频失败时不退还已实扣辅助成本。辅助回执未知时保留助手预留额度等待对账，视频不提交，视频父请求额度按未发出释放。任何真实成本都不从一笔回执猜另一笔。

## 6. 身份、并发、幂等与异常

- Core 将请求绑定 `principal.key_id`、`user_id`、`request_id` 与视频 `task_id`；任务查询与内容读取只接受同一所有者，不能靠猜任务号跨 Key 访问。
- 有 `Idempotency-Key` 时，按 Key + 规范化请求哈希稳定复用原任务；同 Key 不同内容 409。无该头的客户端，应由服务端在**一次连接内部**生成并持久化请求 ID，但跨连接自动重试无法保证去重，故此版本是否开放无头提交必须经过客户端矩阵与风险确认；默认维持现有 400，不为“兼容任何客户端”而牺牲不重复扣费。
- 流式连接占用并发名额的时长与视频任务名额分离：提交额度与任务并发直到终态才释放；观察连接断开只释放连接资源。对同一任务多观察者设上限。
- Core 的额度预占必须覆盖可能产生的辅助 + 视频上限，或采用先辅助后视频的两阶段受控预占。任何阶段的实际回执都只对本 Key 落账。可用积分不足时在上游调用前拒绝；上游已接受但回执不明保持 hold 和人工/后台对账，不自动释放、不重新提交。
- 不记录 Key、JWT、Cookie、原始提示词、图片字节或完整上游请求体；日志只记录脱敏 key_id、父/子 request_id、task_id、状态、延迟、回执来源与积分数。

## 7. 发布与验收门槛

先部署通过全量测试的 Core。AI Work 运行源码与 GitHub 主分支存在桥接路由差异，不能用当前仓库构建物覆盖线上服务。部署后只用现有“周”Key 做一次真实公网请求，确认 DeepSeek 与 Seedance 分别返回真实回执、Core 分别结算、SSE 得到 MP4 内容地址；结果不明即停止且不自动重试。不得改变旧文本模型行为或旧非流式视频契约。当前线上视频安全闸门为暂停；若要用按 Key 与请求哈希限定的一次性诊断放行进行真实测试，操作前需要用户在此刻再次确认，不得擅自启用全局计费。

本地验收使用 D 盘隔离目录和 Mock 上游，不消耗真实积分；覆盖流式成功、失败、客户端断线、请求重放、素材隔离、未知回执不伪成功、素材权限先于付费助手调用、辅助请求与视频请求不同 ID 且同 Key 分账、旧非流式契约以及 v21→v22 schema 迁移。用户已允许一次真实公网验收，按两个真实回执、Core Key 余额和 MP4 内容地址核对。

本设计不是“任意客户端自动保存到工作区”的保证。客户端是否可自动下载，要按 Codex/Claude Code（MCP）及 Trae Work CN、Qoder Work、WorkBuddy、DeepSeek harness 等直接 API 客户端逐一实测，形成能力表；无文件写入能力者只返回下载链接。
