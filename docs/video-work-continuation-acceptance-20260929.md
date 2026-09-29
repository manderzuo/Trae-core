# 视频作业上下文与尾帧续写：审计及验收记录

记录日期：2026-09-29。结论：Tasks 1–8 已实施并完成下述本地回归；Task 9 的真实付费、客户端验收未完成，Task 10 未执行。不能据此宣布公网续写已开放。

## 已实现与审计范围

- Key 隔离的作业/版本、加密不可变快照、持久素材、句柄映射及 v29→v30 迁移。
- 精确父版本解析；缺少或冲突的上下文要求澄清，不猜同一 Key 的最近视频。当前明确参数优先，未指定的规格/素材继承父版本。
- 原请求检查点与预算身份复用；不重新规划已绑定版本，不重发结果未知的付费操作。提帧、状态、下载与原视频结算分离。
- AI Work 受控本地尾帧提取及 Core 来源证据校验、保存和重启恢复；以尾帧作普通参考生成新片段。
- MCP 明确改版/续写/状态入口，以及元数据、等待和下载回传；API 流式阶段增加恢复版本、准备尾帧和提交新片段。

本次由主代理另做审计，没有子代理或独立第二位审查者。检查重点为归属、重试、费用、素材/尾帧证据、异步失败及发布边界；本记录不是独立安全认证。

审计修正：显式 `context_handle` 原本没有完整接入解析；MCP 原本拒绝服务端返回的部分上下文元数据。均先复现失败再修复并回归。MCP 只转发作业/父版本/句柄选择器，不把费用身份或任意返回字段传回生成参数。

## 验证证据

日志在本工作区 `.superpowers/sdd/2026-09-28-video-work-context-continuation/`，不是生产日志。

| 命令/范围 | 实际结果 | 日志 |
| --- | --- | --- |
| `cargo test --manifest-path starlink-dimension-router/Cargo.toml --target-dir E:/AIWORK/workspace/TraeWorkAssistant/src-tauri/target --offline --locked` | 297 passed，0 failed | task9-router-final.log |
| `cargo test --manifest-path src-core/Cargo.toml --target-dir D:/gpt/aiwork-core-test-target --offline --locked` | 270 passed，0 failed，1 个既有 ignored | task9-core-final.log |
| AI Work 全量 Rust 回归，隔离运行目录环境变量并串行执行 | 761 passed，0 failed，8 个既有 ignored | task6-aiwork-final.log |
| `node skills/aiwork-seedance/mcp/integration-test.mjs` | 56 passed，0 failed | task9-mcp-final.log |
| MCP smoke / gateway-config | 29 项 smoke 通过；6 项 URL 断言通过（脚本旧汇总文本写 5） | 本轮命令输出 |
| `npm run build` + AI Work locked/offline release build | 成功；既有 chunk/77 个 Rust warning 保留 | task9-aiwork-build.log |

Core/MCP 集成使用假桥接验证真实路由、数据库、Node→PowerShell→HTTP 和本地文件流程。它们不能证明真实模型响应、付费、视觉衔接或三个客户端都通过。

合成 1 秒、8fps 视频已验证末帧时间 875ms。可信提帧程序 SHA256 为 `ff8d9e4fb41c9563022e2e3d4fc040130efb31595959bfc6613f1ae52f039d31`；支持本轮验证的 H264/MJPEG→PNG，不承诺所有源编码。

新 AI Work 构建产物为 `D:/gpt/aiwork-cargo-target/release/ai-work-assistant.exe`，SHA256 `a0c2b4786d6b8a29a26c344b362fdc55c2a6c38d78e60f3f20fe2d902310578e`。尚未安装或启动此产物。

## 未完成的验收与原因

新版作业/续写真实验收仍未完成。2026-09-29 恢复积分授权后，已完成现网基线视频 **1/9**，严格原生样本 **0/1**；这笔占用同一总样本上限，不能重复计算，但不作为新版上下文/续写已通过的证据。

| 客户端 | 实际情况 |
| --- | --- |
| TRAE Work CN API | 已选取窗口，但截图捕获超时；无截图控件树只有通用区域，无法可靠定位工作区/输入框。未发起任务。 |
| DSH API | 已选取窗口，窗口捕获超时。未发起任务。 |
| Codex MCP | 恢复测试时，实际 Codex MCP 已提供7个旧工具；doctor→submit→status→download 完成一笔现网参考图付费基线。新增改版/续写工具尚未部署，未验收。 |
| Chrome 管理页 | 浏览器工具初始化报 `failed to write kernel assets / os error 3`，没有读取到管理页状态。 |

还发现验收环境约束：`key_registry_sync::sync_now` 是全量同步，且 `budget_flow::prepare_step` 每次准备预算都会调用它。用空库测试 Core 直连正在使用的 AI Work，不仅有后台同步风险，也有付费准备时的同步风险，会覆盖/冲突现有 Key 登记。没有这样启动测试实例，也没有清理或改写旧用户账本。

续测需要可操作的客户端，及与正式 Key 登记隔离的桥接/账户，或经过审计的同一正式 Core 测试灰度环境；不能临时忽略登记、关闭计费保护或把旧用户 Key 放到影子账本中冒充隔离测试。

## 现网真实付费基线（恢复授权后）

使用已配置的“周”普通 Key，经真实 Codex MCP 调用公网 `https://api.gemstory.cn/v1`，不是假网关。未新建 Key，未清理旧账本、旧预占或重启程序。

- doctor：网关和鉴权成功，模型目录19项；该检查没有收费。
- submit 仅调用一次：`request_54EFRGUQID9Xa-Drevu34w`；固定幂等键 `work-continuation-20260929-baseline-reference-01`。
- 5秒、480p、16:9；显式上传本计划红色合成参考图，120字节，SHA256 `171303000410d291b70cf61208be8ee6fab3361ff9a719a0df9005d5ece89410`。AI Work 素材 `asset-1790642456-87b4c478ef59` 的大小/MIME/摘要与本地相同，证明传至 AI Work；该记录本身不证明所有上游请求字段或真实人物一致性。
- 唯一视频预算 `budget_EjxHFaboez0THglSRQ2csobfDPwZ3FFR-bCx3IY8rrY`，唯一上游任务 `video-1790642457627-26`；Core dispatch_attempted=1，执行 succeeded、财务 settled、debt=0。此 MCP 直接视频接口只有一个视频步骤，没有辅助文字步骤。
- 预占62积分，真实 final 回执扣费56.208，释放差额5.792。AI Work 与 Core 的实际扣费一致。AI Work finished_at_ms=1790642557844，Core settled_at_ms=1790642561232，记录时间差3.388秒；两机时钟相关的小样本，不外推到负载P95或所有模型。
- Core Key 可用余额从6723.79变为6667.582，差值恰为56.208。原有1433积分未决预占、1个未决执行保持原样。注意 SUM(delta) 已经是扣除预占后的 available，不能再次减 held；测试探针首版的展示解释已纠正，不涉及业务代码改动。
- status completed 后，实际 MCP download 保存至 `C:/Users/StarLink/Downloads/aiwork-seedance-20260929-084319-request_54EFRGUQID9Xa-Drevu34w.mp4`，707862字节，MP4 `ftyp` 签名，SHA256 `0dae4b14682c71efaf2ce423ed3e96f687db8d622f1872cca2034e197fcbef80`。
- 产物 H264/AAC、约5.09秒、864×496（上游480p档实际编码尺寸），实际解码出第2秒画面，观察为红色绸布，符合这笔基线提示词。首个整流转 null 解码命令因该 FFmpeg 未提供默认PCM音频编码器失败，随后显式只取视频PNG成功；未重新生成视频，也未将失败命令写成通过。

本次无503、观察者竞争、参考图缺失或下载工具错误。结论只适用于这一现网 MCP 基线；没有发送“改成夜景”和“续写”去旧版本盲耗积分。新版端到端验收需先进入安全隔离/灰度运行环境。

## 发布状态与边界

- 三个对应仓库均只提交本地代码，未推送；公网 Core 未替换，本地正在运行的 AI Work 未重启。
- 本轮只读健康检查：公网 Core `/health` 为 HTTP 200；本地 `127.0.0.1:7864/health` 为 HTTP 200。7864 对应 PID 9100，8899 对应 PID 17052。健康检查不证明运行了本次新功能。
- `work_context_enabled`、`continuation_enabled` 默认关闭。真实验收例外限最多 8 个显式测试 Key、24 小时内到期，不允许原生模式；本轮未配置该例外。
- `native_first_frame`、`native_video_extend` 未取证、未验收、未开放。尾帧参考是近似衔接的新片段，不是严格首帧锁定或原生延长。
- v30 只从正式 v29 迁移；更早开发版本的 v30 夹具需重建，不能将其当成正式迁移来源。
- 已保存尾帧过期/损坏/丢失时明确失败，不以无图视频替代。缺少客户端工具时仍准确返回下载地址，不假称本地保存成功。

## 续测与发布顺序

1. 恢复客户端操作或人工配合，建立安全测试环境并记录测试 Key、余额、参考图摘要，运行 doctor。
2. 主客户端最多三笔：参考图 V1（5秒480p）→同对话“改成夜景，其他不变”V2→“接着再生成5秒”C1。逐笔记录 work/version/parent/request/budget、素材/尾帧摘要、真实回执、额度释放和实际下载。
3. 真实异常先定位，不盲重发；未知结果停止新增付费。三客户端共最多9笔，复用已有可核验父视频以减少消耗。
4. 只开放验收通过的能力。备份正式数据库/配置/包，AI Work 仅本机先更新，再发布公网 Core，最后推送对应 MCP 工具。
5. 记录三个仓库提交、实际运行路径与哈希；回滚用 schema30 兼容包并关闭功能开关，不以旧数据库覆盖新增账本。
