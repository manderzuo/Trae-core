# 参考视频与自然语言规格修复验收记录

## 范围

本轮仅修改 Core 和 AI Work 本地源码、执行隔离测试。没有公网部署、Git 推送、生产重启、启动脚本修改、真实视频生成或生产账务修改。此前 BitBrowser 未提交改动保留不动。

## 修复内容

- 聊天附件上传允许 MP4/WebM，保持 32 MiB/文件与最多 10 个素材限制；上传票据仍绑定 Key、会话、不可变槽位和有效期。
- 图片、视频分别恢复为 image_asset_ids/video_asset_ids。实际文件头决定格式；允许客户端把 JPEG 重命名为 image.png 的已有兼容行为，但禁止图片/视频互换。
- 当前用户输入从 user_input 包装中取最后一段，排除 system-reminder 和附件文件名对规格的影响。
- 区分分镜时间段与总时长；无明确总时长时只从从零开始、连续递增的分镜推导总时长。显式 API 规格优先；无效规格在调用辅助模型前拒绝。
- 明确“截取/提取尾帧做参考”时选择尾帧，不被自动模式升级为原生延长。明确 continuation_mode 字段优先于文字推断。
- 新增内部 reference-last-frame 桥接接口：只接收经过 Core 所有权校验的 MP4 字节，不接受路径或 URL；保持桥接认证，最多 4 个上传处理者，复用已有提帧并发/超时/程序摘要校验。
- Core 核对来源 SHA256、Key、PNG 摘要、大小、尺寸和时间戳。失败不降级为整段视频或纯文字生成。上传源的临时文件在成功和失败后清理。
- 提取出的图片作为持久参考保留，后续同一作业修改不会丢失；明确新上传视频时不会误读旧父视频。
- 反向情形也已覆盖：继续刚生成的视频时，不把继承的原始视频素材误当成本次新上传的视频。尾帧模式不夹带旧的整段视频素材。
- 已检查现有幂等机制与重连测试，保留原请求身份/Key 隔离；没有加入跨用户、跨任务的全局提示词去重。

## 审计与取舍

本轮由实施者单独复查，没有启动子代理或独立审计代理。

- 使用现有加密上传原文确定槽位类型，无需数据库迁移。已有图片会话仍可恢复。
- 上传支持 MP4/WebM，但本轮“上传后截取尾帧”仅开放 MP4；WebM 会明确拒绝提帧，不伪装成功。
- 单次尾帧来源限一个上传视频，多个来源需明确选择后再提交，避免随机取片。
- 本轮验证包含真实本地 FFmpeg 提帧和实际 PowerShell 上传，但不等于已通过公网客户端验收。公网须在用户暂停全部任务并确认部署后验证。
- 没有改变之前的计费余额、冻结记录、未知回执或账号池数据。

## 验证记录

新增回归测试均先复现失败后修复：图片/视频槽位、时长与上下文、尾帧模式、上传视频提帧、修改后参考图保留、错误来源校验、客户端重命名图片兼容。

AI Work：video_frames 8 项通过；last_frame 路由 2 项通过，包括真实 32×32 合成视频末帧时间 875ms、未认证拒绝、损坏视频拒绝、临时源清理、没有新增计费预算。

Core：`cargo test --manifest-path starlink-dimension-router/Cargo.toml --tests` 最终退出码 0，共 22 个测试集、323 项通过、0 失败、0 ignored。日志位于 `.superpowers/reference-repair-temp/router-complete.log`。实际执行 PowerShell 5/7 上传脚本，覆盖图片与视频混合上传；Bash 用例在 Windows 内部提前返回，不视为已验证 Linux/Bash 执行。`src-core` 的 reference_upload 名称过滤未匹配测试，不计作通过证据；实际存储绑定由 Core 路由集成测试覆盖。

Core `git diff --check` 通过。生产端口 7864/8899 的进程仍为 29008/61856，未重启。AI Work 日志为同一测试目录中的 `aiwork-frames.log` 和 `aiwork-frame-routes.log`。

测试目录清理命令被执行工具策略拦截，未执行删除，也未换方式绕过；隔离测试残留暂时保留。此限制不同于程序内临时视频源清理，后者已由测试验证。

## 追加修复：自然语言规格与规划错误反馈

根据公网只读诊断，夜景人物请求的辅助模型输出了合法的 `9:16/720p`，但旧版本只认可数字规格，拒绝了“竖构图、高分辨率”的对应结果。飞盘与感染者续写还受旧上下文和分镜区间解析影响；这部分已在前述本地修复中覆盖。

- 将“竖屏/竖构图/纵向构图”规范为 9:16，“横屏/横构图/横向构图”规范为 16:9，正方形规范为 1:1。
- 当前能力下“高清/高分辨率/高画质”规范为 720p，“标清/低分辨率”规范为 480p；支持对应的常见英文表达。明确否定的描述不改动原规格。
- 支持“五秒、十秒、十五秒”等中文时长及连续中文分镜区间；镜头数量仍保留为叙述内容。
- API 显式字段优先，其次数字规格，再其次自然表达；不明确的字段在修改/续写时沿用父版本，新建时保持原默认 5 秒、720p、16:9、无水印。
- 辅助模型接收经过确认的 `normalized_spec` 与默认值，允许回显不变的继承值/默认值；服务端仍拒绝无依据的参数改变。
- 冲突描述与超范围规格在付费辅助调用前拒绝；例如 1080P 不会自动降为 720P。规划结果、规格、缺失父版本分别返回可读原因，保持请求编号和真实结算状态。
- 没有改动历史失败任务、未知回执或冻结账务，没有自动重发收费请求。

新增回归包括真实失败提示词与辅助结果的本地路由重放、最终视频提交参数、后续修改、同请求重试不重复付费，以及错误反馈和费用保留；外部桥接采用隔离替身，没有发起真实视频生成。最终版本完整命令 `cargo test --manifest-path starlink-dimension-router/Cargo.toml --tests` 退出码 0，22 个测试集共 334 项通过、0 失败、0 ignored，日志为 `.superpowers/reference-repair-temp/router-natural-spec-final.log`。原共享测试夹具存在 `unused import: work_context` 编译警告，测试运行不受影响。`git diff --check` 通过，生产端口 7864/8899 进程仍为 29008/61856，没有重启、推送或部署；公网客户端验收仍待用户暂停任务并确认部署之后进行。

## 原部署门槛（现已获得确认）

1. 本地全部必要测试完成。
2. 通知用户暂停所有提交、生成和续写任务，等待用户确认；当前尚未部署。
3. 后续确认无在途任务，再按 AI Work 桥接 → Core 顺序协调更新，验证公网带素材、尾帧、连续修改与原任务重连。

## 2026-09-29 公网部署与检查

用户明确确认“可以部署了，我这边没有运行中的任务”后执行。本轮仅部署、检查；未进行 Git 提交或推送，未提交收费视频。前述“暂不部署”均为确认前的历史状态。

- 本机 AI Work：`E:/AIWORK/releases/20260929-reference-video-natural-spec/ai-work-assistant.exe`，PID 65920；SHA256 `3da7a3f62b8f7c7e2c299b2ec0498fdc42e5fbff798292968158b6225f0734af`。
- 公网 Core：`/opt/gemstory/starlink-dimension-router/releases/20260929-reference-video-natural-spec/starlink-dimension-router`，PID 2377640；SHA256 `c1ff4eeccc0f5ab9a91049ef08a7ac7629629310499aed43603109febd42a124`，实际运行文件与本地 Linux 构建一致。
- AI Work 仍在本机；持久启动脚本指向新包，7864 网关、8899 代理均自动恢复监听。保留原 BitBrowser 本地修改及 Python/PowerShell 运行资源。
- 两端 release 构建退出码均为 0。新增 Linux 实际 Bash 上传测试执行 1 项并通过，不再只是 Windows 提前返回的用例；首轮 exact 过滤匹配 0 项不计入证据。
- 助手正常关闭请求未退出，数据库备份后受控停止旧进程。重新启动触发了已有恢复保护；通过认证接口按当前实例/代际确认恢复，13 条历史未决预算保留，charge_ready=true、recovery_required=false，没有释放未知扣费或重发任务。
- 公网服务器通过 FRP 对新内部接口上传真实本地合成 MP4，返回 32×32 PNG、875ms 尾帧；Key、来源 SHA256、图片 SHA256 校验一致。匿名访问 403。未新增计费步骤。
- 使用已有“周”普通 Key 对公网 `/v1/models` 验证 200 且包含 seedance。模拟聊天附件得到上传工具调用，MP4（2674 字节）和 PNG（120 字节）上传与重复上传均返回 200、摘要一致、重复上传返回同一资产；图片上传到视频槽位返回 400。
- 中文“三秒”、明确 1080P、竖屏横屏冲突均在付费辅助调用前返回 400/work_spec_unsupported。所有部署检查前后 budget_steps 计数未增加，没有视频生成或积分支出。
- 公网服务切换时曾短暂返回 502，切换后公网及本机健康检查均恢复 200。数据库 quick_check=ok、schema_version=30 未变；部署时 api_keys、余额账户、预占、积分账本、预算步骤和结算表逐行摘要均未变化。
- Core 数据库与服务配置备份：`/var/lib/starlink-dimension-router/backups/20260929-reference-video-natural-spec`。AI Work 桥接数据库与旧启动脚本备份保存在本机新 release；旧 release 保留。两端源码快照均随 release 保存。

限制：以上公网验证是接口级、不收费检查，不等于在用户的 TRAE/DSH 桌面客户端完成了带视频附件、尾帧续写和连续修改的整条付费验收。WebM 上传可用，但上传视频提尾帧本轮仍仅支持 MP4。历史账号容量核对警告及未知费用事实保持原样，本轮没有清除或伪造结算。

## 2026-09-29 追加：上传视频续写与补发素材丢失意图

公网只读诊断已确认：17:29 的首次视频与剧本合并请求在上传前被父版本追问截断，上传槽位保持空；随后单独补发的 MP4 已成功上传（5,189,621 字节），但辅助输入仅剩 TRAE 的 REQUIREMENT 模板，没有上一条 10 秒、720P、9:16 剧本。GLM 返回只读追问且 reference_policy=null，旧解析器拒绝。父请求 `request_BbQ2kr8zF2XkR0OSPl6w3Q` 失败，没有视频预算步骤；辅助请求实际结算 0.0552 积分。SSH 维护开关不是本次故障原因。

本次只修改 Core 本地源码，保留此前未提交改动，不更新生产数据库、服务配置或历史失败请求：

- 有经过路径校验的当前 MP4/WebM 附件时，允许先上传，不因文字里的“继续生成”要求猜测已有父版本；明确 action=continue/revise、外 Key 上下文、冲突句柄的限制保留。实际素材所有权与内容校验仍在付费调度前执行。
- 排除已知 TRAE 系统提醒、附件路径和两个固定 REQUIREMENT 模板行。未知 REQUIREMENT 内容保留，不擅自删除用户指令。
- 仅在当前明确补发视频、当前没有新指令、最近一条助手明确追问且声明未提交视频时，恢复同一请求历史中最近用户剧本。不会越过取消、新用户指令或已提交/完成的视频，不做 Key 全局历史查询。剧本、规格在密封上传原文内恢复，工具回执无法覆盖。
- “不要生成水印、logo”等负面视觉约束不被当作取消任务。没有可恢复剧本时直接只读追问，不调度付费辅助或视频步骤。
- GLM 收到服务端校验后的图片/视频数量，不收到素材 ID、客户端路径或授权链接；上传视频可直接作为新片段来源，不要求虚构 Core 父版本，不宣称严格锁帧或原生延长能力。
- reference_policy=null 或缺省只在 clarify/status/download 只读动作下规范为 inherit；create/revise/continue 仍严格拒绝空策略和未知字段。普通 JSON 与 SSE 追问均正常返回，不再因此抛泛化服务器错误。

回归先复现实际失败后修复，包括合并上传、补发剧本与最终 10 秒竖屏提交、幂等重放、取消/已提交/已完成任务保护、负面约束、只读空策略。定位与测试过程未消费真实积分，外部 AI Work/模型响应采用隔离替身。

验证过程曾因同时编译同一个 Windows 测试程序而出现 os error 32，另一次完整运行包含当时仍在 RED 阶段的 `supplemental_video_cannot_restart_a_previously_submitted_task`。两次均不计作最终验收通过；停止源文件变更后独占重跑完整命令。原有 work_client_feedback 共享夹具 unused import 警告保留。

最终验证（最后一次源码变更后执行）：

- Windows 完整命令：`cargo test --locked --offline --manifest-path starlink-dimension-router/Cargo.toml`，退出码 0；共 348 项通过、0 失败、0 ignored，包含库单元测试、路由集成测试与文档测试阶段。相较先前 334 项新增 14 项回归/边界用例。原有 unused import 警告仍存在。
- Linux/WSL：`cargo test --locked --offline --manifest-path starlink-dimension-router/Cargo.toml --test work_planner --test work_context`，退出码 0；分别 35、13 项通过，0 失败。
- Linux release 构建退出码 0；待部署文件 `D:/gpt/starlink-router-linux-build-20260923/release/starlink-dimension-router`，SHA256 `18dde1de65fbf586e4932e9b06a7b0529f64c55c76ca4fb396d838dbf7222d18`。
- `git diff --check` 退出码 0。

本轮尚未 Git 提交/推送或部署公网；须用户再次确认已暂停所有在途任务后更新 Core。AI Work 的本机部署位置与监听服务保持不变。公网 TRAE 实际发送附件、GLM 新规划输入及生成结果的真实付费验收尚待部署后完成；不能把隔离替身通过当作公网客户端已验收。若客户端没有携带上一条剧本，或上一轮已经提交任务，单独补发视频仍会追问，不擅自猜测或重发收费任务。

## 2026-09-29 追加部署与一次真实付费检查

用户再次明确“可以部署”后执行。部署前 Core 没有 ready/running 预算操作或步骤，本机 AI Work 同样没有 running 执行；5 条旧请求属于历史 unknown，不做额度释放或状态改写。桥接 charge_ready=true、recovery_required=false，保留 13 条历史未决预算。

- Core 已切换为 `/opt/gemstory/starlink-dimension-router/releases/20260929-source-video-context/starlink-dimension-router`，PID 2448623；运行文件 SHA256 `18dde1de65fbf586e4932e9b06a7b0529f64c55c76ca4fb396d838dbf7222d18` 与本地待部署构建一致。
- 停止服务后完成 SQLite 一致性备份与配置备份，目录 `/var/lib/starlink-dimension-router/backups/20260929-source-video-context`；旧 release 留存可回退。quick_check=ok、schema_version=30；切换前后 Key、余额账户、预占、账本、预算步骤和结算表逐行摘要均未变化。
- 本机 AI Work 未重启或部署，7864/8899 监听仍为 65920/36720。公网健康检查、普通 Key 模型列表和 Seedance 上传流程正常。
- `git archive HEAD` 因 promisor remote 取缺失对象失败，未将失败的部分归档上传；改用实际工作区的源码、锁文件、测试和审计文档创建隔离源码包，随 Core release 保存。此操作不是 Git 提交或推送。本轮仍未提交/推送，所有原有未提交改动保留。

使用已有“周”普通 Key，经公网 HTTPS 发起一组受控检查，不使用管理员 Key 生成视频，不打印密钥或授权链接：

1. 视频与文字一起发送，提示从上传视频结尾继续生成：HTTP 200，返回上传工具调用，不再在上传前追问 Core 父版本；本步未回传工具结果、未收费。
2. 模拟同一对话中前一条剧本、明确“未提交视频”的追问，当前仅补发视频及 TRAE 系统模板。上传真实合成 H264 MP4，16,098 字节，SHA256 `0d4a7fe2e58efa2ee5a0ada19789b5fe1175a449a81f03de8efbcfce5db73216`；HTTP 200，上传摘要相符，未新增预算步骤。
3. 仅一次提交上传回执开始真实付费规划，父请求 `request_02lwYf-vCgCgfflDDaUEcw`。SSE 返回接收、整理提示词、处理参考素材、提交视频阶段，随后返回 `reference_video_metadata_invalid`；没有重复提交或重新生成。
4. GLM 辅助步骤 `request_EkFr_w1JEPXlEvnOnxFJ2Q` 成功并真实结算 115,600 microcredits，即 **0.1156 积分**；预占 2 积分，释放 1.8844 积分，debt=0。没有 video 预算步骤或视频 task_ref，因此未消耗视频生成积分，也没有生成或下载成品。
5. 已持久化的版本为 action=create、parent_version_id=null，5 秒、480p、16:9，参考素材数量 1；规划后的提示词保留原剧本的深蓝、金黄、青绿三种元素。证明补发素材时原剧本与规格已恢复，但不证明视频上游生成成功。

本次检查发现的限制/待办：

- 合成验收 MP4 的 video mdhd 为 37,376 ticks，timescale=12,288；stts 为 72×512=36,864 ticks，解码/播放时长为 3 秒，时间头与样本表相差一帧。AI Work 的保守视频预算元数据校验在此拒绝；不能绕过校验或手填更短时长。此次未修改 AI Work 解析器，也未换素材再发起第二次收费任务。
- Core `budget_flow::finish_definite_failure` 缺少 `reference_video_metadata_invalid` 等对应确定性拒绝的收尾分支。版本已为 failed，辅助费已经结算且释放差额，但父请求仍 reserved、预算操作仍 running。它会占用逻辑并发名额；需要补充有证据的确定性失败收尾与回归验证，不能把传输超时等未知结果一并当作失败，不能直接改历史数据库掩盖问题。本轮未修复这处新发现的缺口。
- 公网接口级“视频补发恢复剧本”已验证；整条参考视频生成与下载验收 **未通过**。没有操作用户的 TRAE/DSH 桌面完成真实客户端验收，不能声称已实现所有客户端无感下载。

生产部署证据与受控检查证据分别保存在本次 release 的 `deployment-evidence.json`、`context-acceptance.json`；不含 API Key、JWT 或下载 ticket。历史失败任务未重发，账务未手动校正。

## 2026-09-29 追加：辅助模型 JSON 代码块兼容修复（本地）

最新真实客户端请求 `request_x72qit3EpvQm3PvQtowSDg` 的视频上传回执为 `ef327fe90bbdd8ebd35200ed4b401574`。只读核查确认 5,189,621 字节 MP4 已上传，Core 原请求包含完整剧本和一个视频资产。GLM 成功返回 action=create、10 秒、720p、9:16、reference_policy=replace，但将整个 JSON 包在 Markdown 的 ```json 代码块里。原入口直接按裸 JSON 解析而失败；去掉完整外层代码块的只读探针确认内部对象合法。这次没有 video 预算步骤；辅助请求实际结算 0.1212 积分，预占 2 积分释放 1.8788 积分，父请求和预算操作均已为 failed。

用户要求修复后，只修改本地 Core，不改生产数据库或重新提交上述失败任务：

- 新建统一 `assistant_json::object`：接受裸 JSON，或单个完整的 ```json / ```JSON / 无语言标记代码块。支持 CRLF 和外围空白，整体仍有 16 KiB 字节上限。
- 不搜索说明文字里的 JSON，不修补截断对象，不接受额外前后说明、多个代码块、多个对象、数组、未知语言代码块；JSON 字符串内的反引号是数据，不被替换或删除。
- 作业规划、新旧调度路径和后台结算/恢复读取辅助结果都使用同一规则。WorkDecision 原有未知字段、动作、付费提示词、规格与参考策略校验保留。
- 错误文字明确归因为辅助返回格式或字段校验，提示本次未提交视频，保留请求编号与真实结算状态，不把服务器解析错误归咎于用户重新填写参数。

测试先复现裸解析失败；首次只更改前台解析后，完整路由测试仍失败。进一步追踪到后台 `budget_reconciler` 仍直接按裸 JSON 解析，在辅助成功后将预算操作提前标为 failed，阻止 video 步骤准入。将后台读取一并接入同一解析规则后通过；没有修改测试原有素材、规格或放宽生产账务状态校验来消除失败。

验证（最后一次生产源码变更后）：

- Windows 完整 `cargo test --locked --offline --manifest-path starlink-dimension-router/Cargo.toml`：退出码 0，356 项通过、0 失败、0 ignored；日志 `.superpowers/reference-repair-temp/router-fenced-json-final.log`。新增 8 项回归/边界测试。
- Linux `cargo test --locked --offline --manifest-path starlink-dimension-router/Cargo.toml --test work_execution --test work_planner --test work_client_feedback`：退出码 0，19+37+8=64 项通过、0 失败。
- 真实路由/存储配合隔离桥接替身，确认视频素材摘要、10 秒/720p/9:16、只有一次辅助和视频发送，幂等重放沿用同一请求；只读代码块追问不提交视频，非法字段在 JSON/SSE 下返回明确错误且父操作终止。
- Linux release 构建退出码 0，SHA256 `f8d687564b905937fa8e18246f790f1b27c3714b9832e1c3d34fed78e9802000`；`git diff --check` 通过。原共享夹具 unused import: work_context 警告保留，不影响测试结果。

状态与限制：本轮没有积分消费、没有公网付费重试，没有 Git 提交/推送或生产部署；本机 7864/8899 监听仍是 65920/36720。公网还是 `20260929-source-video-context`，尚不包含这次 JSON 兼容修复。素材时间元数据拒绝后的收尾缺口仍属于上一轮已记录待办，本轮没有顺带改动。部署及整条付费验收仍须在用户暂停在途任务后进行；本地桥接替身通过不等于真实视频生成与下载已验收。

## 2026-09-29 追加：确定性素材拒绝的任务收尾（本地）

用户要求“一并修复”后补齐上述收尾缺口，保留辅助 JSON 代码块兼容修复。

- 排障追踪确认：AI Work 的 prepare 在发视频前返回 `reference_video_metadata_invalid`、`reference_video_format_unsupported` 或 `reference_asset_type_mismatch` 时，Core 原确定性失败名单未覆盖；已有辅助步骤成功，版本可为 failed，但预算操作仍 running、父请求仍 reserved，继续占用逻辑并发。
- 三种错误接入现有 `finish_budget_execution(...Failed)` 事务。该事务要求所有已记录步骤的执行已结束，拒绝终止仍有 running/unknown 付费步骤的操作；只改变执行状态，不退款或伪造账单。
- 事务成功或幂等确认失败终态后统一进行作业版本和素材租用的收尾。后台 continuation 也经过该入口，不再依赖原 HTTP 观察者完成这一动作。
- 不将 `reference_asset_unavailable`、`bridge_state_unavailable`、`budget_preparation_requires_recovery` 纳入确定性终止：这些错误可能来自读取或恢复状态异常，继续保留待核对状态与预算身份。
- 素材元数据/类型拒绝给出明确文字与真实结算状态，不声称“没有任何扣费”。辅助模型已产生的费用仍正常结算。
- 回归中额外发现同一失败请求重放会因为 `video_continuation_not_active` 未公开映射而变成泛化 503。补齐安全错误码、非流式 HTTP 409 与文字“任务已结束，不能再次提交”；流式在既有 SSE 连接中返回相同终态信息。同一请求不会重复发送辅助或视频步骤。

新增四项真实路由/SQLite 集成回归，桥接外部操作使用隔离替身：

1. 三种确定性错误分别在 JSON/SSE 下验证：父请求和操作 failed、作业版本 failed、没有视频预算步骤或发送；Key 并发上限为 1，旧辅助账单仍 pending 时下一条独立请求即能成功提交。
2. 原失败请求的幂等重放保持原 request_id，不产生第二笔辅助调用；其后收到真实格式的辅助回执仍可结算 1.25 积分并释放 0.75 积分差额，重复回执返回 Duplicate、不重复扣款。
3. 旧调度路径和 `/v1/videos/generations` 异步入口同样结束被明确拒绝的任务，不永久 queued/running。
4. 可恢复的素材读取/桥接错误不能伪装成终止证据：操作仍 running、辅助财务 held、未虚构实际扣费或退款，不绕过原并发限制。

先运行新增回归确认 RED：普通及旧调度操作仍 running，异步父任务未结束。补齐收尾后前两条入口通过；重放测试进一步实际复现 HTTP 503（期望 409），补齐映射后通过。一次早期测试夹具编译失败（引用未公开的默认并发常量）已纠正，不作为 RED 或验收证据。

最终验证（最后一次代码变更后执行）：

- Windows 完整 `cargo test --locked --offline --manifest-path starlink-dimension-router/Cargo.toml`：退出码 0，360 项通过、0 失败、0 ignored；日志 `.superpowers/reference-repair-temp/metadata-closure-final.log`。
- Linux `cargo test --locked --offline --manifest-path starlink-dimension-router/Cargo.toml --test work_client_feedback --test work_execution --test work_planner`：退出码 0，12+19+37=68 项通过、0 失败；日志 `.superpowers/reference-repair-temp/metadata-closure-linux-tests.log`。
- Linux release 构建退出码 0；待部署文件 `D:/gpt/starlink-router-linux-build-20260923/release/starlink-dimension-router`，SHA256 `a371e349406e61cc92431ed715efe9bc0586947a2f533eb323ec4e196016a2df`。
- `git diff --check` 通过。共享测试夹具原 unused import 警告及新构造方法在其他集成测试模块未使用的 dead_code 警告保留；无生产编译错误。

限制与部署边界：没有修改 AI Work MP4 时间元数据的保守校验，不能声称先前时间表不一致的测试视频已经能生成。本轮未消费积分、未重发旧任务、未修改生产账务或历史残留任务，未 Git 提交/推送、未部署公网。新收尾规则防止新请求再次残留，不擅自依据旧版本 failed 状态就释放历史未知任务。历史已核实残留的处置、公网真实参考视频生成和下载验收仍须在用户确认暂停在途任务后部署阶段处理。

## 2026-09-29 追加：部署 JSON 兼容与确定性失败收尾版本

用户明确要求“部署”后，将上述已通过回归的 Core 构建部署为 `20260929-helper-json-terminal-cleanup`。

- 原预检正确阻止了未经区分的 running 状态。进一步核查唯一记录 `request_02lwYf-vCgCgfflDDaUEcw`：已有验收证据对应同一 Key 和请求；只有辅助步骤 `request_EkFr_w1JEPXlEvnOnxFJ2Q`，succeeded/settled，实际 115,600 microcredits、释放 1,884,400、debt=0；无 video 步骤/task_ref，作业版本 failed。再次通过桥接 execution/billing 的完整请求、Key、预算、账号与实例绑定确认辅助完成及 final 0.1156 账单。因此它被单独认定为此前已核实拒绝的残留，而非在途生成，不依据年龄或 failed 版本泛化排除其他请求。
- 部署预检和停止服务后的复检均没有实际 active requests/operations/steps。五条历史 unknown 请求和上述残留全部保留，未修订生产任务状态、余额、账本或未决预算，没有重发付费请求。桥接 charge_ready=true、recovery_required=false，13 条历史未决预算保留。
- 停服后 SQLite 一致性备份和配置、systemd drop-in 备份保存在 `/var/lib/starlink-dimension-router/backups/20260929-helper-json-terminal-cleanup`。旧 release 保留；切换失败自动恢复旧运行文件。
- 公网运行文件 `/opt/gemstory/starlink-dimension-router/releases/20260929-helper-json-terminal-cleanup/starlink-dimension-router`，PID 2474466，SHA256 `a371e349406e61cc92431ed715efe9bc0586947a2f533eb323ec4e196016a2df`，与本地发布构建相同。actual workspace 源码包保存为同目录 `source.tar.gz`；不是 Git 提交或推送。
- 切换前后 quick_check=ok、schema_version=30；Key、余额账户、预占、账本、预算操作、步骤与结算表逐行摘要均未改变。部署证据保存为该 release 下 `deployment-evidence.json`。
- 本机 AI Work 不需要此次 Core 修复的程序更新，未重启；7864/8899 仍由原 65920/36720 监听。桥接未执行 recovery acknowledgement 写入。

部署后经公网 HTTPS 使用普通“周”Key 做非付费验收：health HTTP 200、models HTTP 200 且包含 seedance；真实视频尾帧提取 HTTP 200、PNG 120 字节、来源及输出摘要/Key 绑定匹配，未授权调用 HTTP 403；视频 2,674 字节与图片 120 字节均上传成功且幂等重放同一资产，互换素材类型被 HTTP 400 拒绝；非法时长、分辨率及冲突比例均以 `work_spec_unsupported` HTTP 400 拒绝。验证前后预算步骤数量相同，未提交视频、未消费积分。

本次部署包含辅助模型 JSON 代码块统一解析、前后台一致读取、确定性素材拒绝的任务收尾及失败请求重放的清晰终态反馈。不包含视频轨相差一帧的时间信息兼容。没有本轮完整真实付费生成/下载验收，没有 Git 提交/推送；旧已核实残留仍保持历史原状，不能声称旧占用已全部清除。

## 2026-09-29 追加：助手分级时间审查上线与 Core 推送前验证

用户授权“部署，然后git推送”后，本机助手已切换至 `E:/AIWORK/releases/20260929-reference-timeline-graded/ai-work-assistant.exe`，SHA256 `b1f60aa7f591f199300a28b105708ab6854900866be1ca028a9f8977c5e9bb43`。Core 继续使用上述已经部署的构建，不重复停服。待提交 Core 生产源码与公网 release 保存的源码包按统一换行核对，18 个文件一致，0 个不一致。

- Core 推送前重新执行完整 Windows 测试：360 项通过，0 失败，退出码 0；日志 `.superpowers/reference-repair-temp/prepush-core-full.log`。测试使用独立临时目录，不使用生产账号或账务数据库。
- 助手完整串行 Rust 测试：777 项通过、0 失败、8 ignored；前端 50 项通过。此前测试环境下的并行互斥冲突不通过放宽生产锁规避。
- 新助手 API 7864、代理 8899 均恢复；公网普通 Key 的模型列表、图片和视频上传及幂等重放、授权尾帧提取均通过，未经授权与错误类型仍拒绝。
- 重启前后一致性备份与活动桥接数据库的四类预算/执行/回执表逐行摘要保持不变，quick_check=ok；13 条历史未决预算保留。公网验收前后没有新增预算步骤，没有付费生成或积分消费。

助手修复已经分别提交为 `9d40cae`（账号接管与续期凭据边界）和 `e12392c`（参考视频分级时间审查及受限尾帧提取），推送到 trae-maker 的既有修复分支和 main。这里的 Core 源码、测试和设计/审计记录独立提交到 Trae-core；MCP 仓库未改动。整条真实付费视频生成及客户端下载本轮未再次执行，不能由非付费链路检查推断其已经全面验收。
