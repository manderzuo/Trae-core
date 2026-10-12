# TRAE CN 视频迁移与 Core 状态交接

2026-10-12。用户授权将已完成修复推送仓库并更新相关记录。统一客户端登录、文字/视频协议身份和新积分包准入修复位于 [AI Work / trae-maker](https://github.com/manderzuo/trae-maker)；不把桥接实现复制到 Core。

## Core 已部署状态修复的源码补齐

生产仍使用 `20261003-readonly-video-terminal` 运行发布包。原修复在该包中运行，本次将原来停留在本地的三处源码改动、两项回归及交接文档同步仓库，不替换 Core 二进制或财务数据库。

辅助工作已终态且没有视频步骤时，`GET /v1/videos/{request_id}` 返回视频终态 `failed`、`stage=no_video_submitted` 和 `video_not_submitted`。请求与预算操作的成功文字结果保持，已核验的辅助费用仍结算；该投影不等于退款、零扣费或新增派发。未终态辅助、未知费用和实际视频任务继续按原生命周期处理。

`video_read_only_terminal` 两项隔离回归通过：终态视频查询重复两次不会重复计费或继续排队，跨 Key 请求保持 404；普通 Chat 的澄清文本仍返回且不提交视频。检查使用既有 E 盘 target、离线 locked Cargo 和隔离 SQLite/模拟桥接，不调用真实上游。

## 当前桥接迁移结果

- 桌面登录与权益读取使用实际 TRAE CN 3.4.2，文字兼容代码 20260811、视频适配器 0.1.50 / 20260811 分别绑定自己的协议契约；请求的模型选择保留。
- AI Work 的 `bridge-billing.sqlite3` 为 Schema9；这不是 Core 数据库的 Schema 编号。专用账号新增原生 UNUSED 包本金独立记账，旧 P/R/D 与费用事实保留。
- 公网普通视频、尾帧参考、完整视频参考三例均成功、成片读取与完整解码通过，六个辅助/视频步骤全部结算。样本为5秒/480p/16:9，合计179.5364积分；严格首帧锁定仍未开放。
- 原4001失败父请求 `request_2b9rq9OniwECm_VLhF-3gw` 与已结束容量诊断保留原终态，不自动重提。桥接的21笔历史待核费用逐笔保持。

完整请求身份、实扣、成片摘要及独立审查见 [AI Work 视频迁移验收](https://github.com/manderzuo/trae-maker/blob/main/docs/video-migration-repair-20261012.md)。不要把仅目录读取、预算风险占用或本地回归当作视频实际完成和最终扣费。

## 运维边界

Core 的普通 Key、桥接管理员凭据与 Studio 用户设置保持原归属，不输出或写入仓库。关闭 AI Work 新包容量使用同一 Schema9 兼容二进制的 `--disable-new-package-capacity`；不能恢复Schema8备份覆盖新费用，不能通过 Core 手工释放旧未知账单。源码推送不代表全产品/所有视频规格已验收，也不触发新的收费测试或安装包发布。
