# 持久视频作业与续写

Core `work_context_enabled` 默认关闭。开启后，每次付费生成绑定独立 request/budget 与不可变加密版本快照；同作业可以从明确父版本改版或形成并发分支。状态、下载和工具回执不创建新版本。没有可靠父版本时澄清，不猜当前 Key 的最近视频。

返回 `work_context` 以及文本 `[AIWORK_WORK:…]` 句柄。句柄不是凭据，读取仍要求原 Key 认证。API 可明确传 `work_context.work_id/base_version_id`；客户端保留历史标记也可以续接。新请求当前显式规格优先，未指定规格继承，新的用户素材默认替换旧素材。永久 Key、JWT、ticket 和终端输出不进入辅助模型上下文。

`GET /v1/video-works/:work_id` 只读版本状态，不返回加密快照。
`POST /v1/video-works/:work_id/continue` 接受 `base_version_id`、`prompt` 和可选 `duration/resolution/ratio/continuation_mode`，返回同现有异步协议的新 task id。使用新的幂等键；重试同一新片段复用原键。

`continuation_enabled` 也默认关闭。当前仅实现 `tail_reference`：从精确父任务提取尾帧，保存带请求/预算/来源摘要的加密证据，作为图片参考生成独立片段。不是原生延长，不自动拼接，不保证首帧严格一致。两个 native 模式仍未验证、始终拒绝，不借用其他供应商参数。

AI Work 的证据能力接口通过后才能普遍启用。真实验收期间，可给最多8个明确测试 Key 设置 `continuation_test_key_ids` 及绝对 `continuation_test_expires_at_ms`（未来不超过24小时）。只允许上游明确返回 not_verified 的待验收尾帧参考，不开放 native、不放松扣费/素材保护，不制造 verified 证据。验收后清空例外配置。

预提帧异步进行，最多10个 Core 所有者，对同版本不堆积后台等待者；AI Work 实际解码并发2、队列8、超时30秒。重启后可以根据已成功预算修复尚未完成的版本记录，并恢复预取。提帧失败不阻塞交付或积分结算；不可核验的已保存副本失败关闭，不以无图请求替代。副本超出保留期不能承诺仍能续写。

迁移为 schema30，保留旧账本及 unknown 占用。媒体默认30天，Key1GiB/总10GiB，单素材32MiB；文件/快照/句柄/尾帧证据都支持密钥轮换。新功能关闭时旧生成入口保持可用。

回滚必须使用 schema30 兼容程序，关闭新功能，不用旧库覆盖新增账单、不直接回退 schema29 二进制。
