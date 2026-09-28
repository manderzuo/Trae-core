# Seedance GLM 调度与 Downloads 交付验收

## 本轮范围

- Core 为 Seedance 单独使用 `glm-5.3-flash` 做意图辅助和下载工具选择；普通文字默认模型不变。
- 支持客户端声明的 Bash、明确标识 Bash/PowerShell 的终端工具；默认系统 Downloads。内嵌受控命令，不需要 AI Work MCP/Skill 脚本。
- 辅助模型只选择受限工具和参数；命令由 Core 注入。永久 Key 不进入模型、命令或报告。
- 同一视频和工具契约复用已计费规划；下载确认不重新生成视频，不调用付费助手。
- 保留现有 V2 账本、未知扣费及 10 积分持有，不清账、不切换到估算实扣。

## 验证证据

- 首次集成 RED：19 通过、1 失败；实现后 20 通过。完整 Router 187 通过、Core 261 通过，Core 原有生产快照用例 1 项未启用。
- 实际 PowerShell 5 与 Git Bash：正常 MP4、重定向 Downloads、中文/空格/单引号路径、不覆盖、错误类型和截断清理通过。
- 实际 WSL Linux Bash：两次保存不覆盖，错误类型和截断失败并清理 .part。
- 测试目录均在 E；Downloads 注册表解析采用进程内文件夹 fixture，未修改用户注册表，未向 C 写测试视频。
- 自审发现未知 shell 的通用终端不能直接收到 Bash 命令，加入失败断言并修正。禁止联网或未知必填字段的工具不适配。
- 真实 GLM 工具调用预检：`request_h3XQrO_BLovmCGPkjgRTRw`，`tool_calls/Bash`，实际结算 0.011600 积分。
- 首次公网已完成视频交付：`request_Xyu-CyJtAY0WXVqUf0ijkQ`，GLM 规划 `request_Dz1jeJbgErvhkdXFDVvCww`，实际结算 0.014800 积分。工具在本机下载 3,708,392 字节，SHA-256 `8c0e61d3efdeb2aecad1e088d0dcc194ff61cc30a2ccf508d50a93f56539264e`。没有新增视频生成。
- 公网验收发现 SSE 确认漏带 `video_delivery` 结构化状态，文字确认已经返回。新增集成断言得到 19 通过、1 失败；补齐元数据后该集成套件 20 通过，Router 全套重跑 187 通过、0 失败（exit 0）。

## 发布与边界

- 初次功能提交：`44de515`，已推送 Trae-core 的 `fix/seedance-billing-20260925` 分支。
- 公网 Core 初次 release 为 `20260928-glm-downloads`；数据库一致性快照、旧配置和二进制版本保留，健康检查 200。SSE 补丁最终发布和公网重验待记录。
- AI Work 仍运行本机（PID 49268、127.0.0.1:7864），没有部署到云。
- 未操作真实 Agent 原生界面；公网验证调用真实协议和操作系统工具，系统 Downloads 解析重定向到 E 的隔离目录以避免 C 测试写入。
- 仅配置模型不能开放客户端未提供的终端或联网权限。客户端禁止执行/联网时会给出下载链接，不假报本地保存成功。
- 本轮临时下载媒体由验收脚本 finally 删除，构建缓存和用户原视频保留。
