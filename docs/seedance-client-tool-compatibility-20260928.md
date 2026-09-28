# Seedance 客户端终端兼容修补

## 本次范围

只修改 Core 仓库，不修改 AI Work 的上游执行、额度账本或数据库 schema；辅助模型保持 `glm-5.3-flash`，默认保存到客户端操作系统 Downloads。没有配置或发布生产服务，没有提交新视频任务，也没有消耗真实积分。

真实请求核查发现：Trae Work 提供 `RunCommand`；另一台 DSH 客户端提供 `bash` 和 `dev_tool_search`，但 Bash 的说明禁止联网。不能把手动 PowerShell 访问成功当成 Agent Bash 已获联网授权。

## 行为

1. 适配原生 `pwsh` / `bash` 的 `command`、`description`；已声明的 `timeoutMs` 不超过五分钟及 schema 上限，不主动开启后台模式。
2. 先过滤合法终端，再限制候选数，避免大工具目录丢失后面的终端。PowerShell 描述中的“禁止使用 Bash”不再使其被选为 Bash。
3. 保留 `tool_choice=none`、强制工具选择和网络限制。不认识的必填字段仍拒绝，绝不猜测权限或会话参数。
4. 没有直接可用终端但提供 `dev_tool_search` 时，进行一次 `powershell` 目录搜索。仅解锁目录实际返回的已知终端名称；后续必须由客户端带回完整工具声明，才允许生成下载调用。搜索或解锁失败后停止，不循环。
5. 发现、解锁、下载回传都绑定已经完成的视频及原 Key。未知回传不会重新进入视频意图解析；相同终端 schema 的下载规划复用原幂等结果，不重复生成视频。
6. 接受标准 `tool` 消息及单个结构化 `tool_result`。保存回执允许 `data` 等嵌套包装，但错误退出、超时、取消、沙箱拒绝不报告成功。成功路径只代表客户端回执，不代表服务器核验了远端磁盘。

## 结果状态

`video_delivery.status` 可为 `discovering_tools`、`unlocking_tools`、`download_requested`、`saved`、`download_failed` 或 `download_unavailable`。不可用时的 `reason` 区分未传工具、工具关闭、明确联网限制、未支持的终端/schema，以及未配置公网地址。SSE 保留该元数据。

发现流程是有限的交付续接，不是通用 Agent 工具循环。Seedance 的非视频意图仍只返回文字；用正常文字模型执行一般命令测试。客户端真正拒绝联网或不允许写 Downloads 时，Core 不换工具绕过拒绝，只有下载链接兜底。

## 验证与剩余边界

回归先验证旧代码失败，再验证修补。接口集成使用完整原生终端 schema 与真实 DSH 搜索结果格式，覆盖 SSE、发现/解锁、有限停止、错误回执、跨 Key 隔离及下载重试不新增视频派发。Windows 原生 PowerShell / Git Bash 的受控下载测试使用临时 HTTP 服务，不访问真实上游。

2026-09-28 本地完整回归：Router 194 项通过、0 失败；Core 261 项通过、0 失败，既有 `production_backup_migrates_without_changing_historical_rows` 因需要明确授权的私有不可变生产备份而跳过（1 项）。修改后另一次完整 Router 回归再次通过，包含新增的跨 Key 工具发现拒绝校验。

执行命令为 `cargo test --offline --manifest-path starlink-dimension-router/Cargo.toml --jobs 2` 和 `cargo test --offline --manifest-path src-core/Cargo.toml --jobs 2`。测试使用 D 盘临时目录和现有 D 盘编译缓存，未读取生产数据库。

两台远端 Agent 的 UI 端到端验收尚未执行，不能声称所有客户端已经自动下载成功。生产部署与 Git 推送不包含在本次本地修改中。
