# 缺档预冻结与失败原因持久化：发布记录

2026-09-30，北京时间。用户明确授权推送和部署；两仓独立发布，MCP仓库未修改，停用的监控没有重启。

## 运行版本

- Core源码提交：`21304db2379ba75400cca4f53015b63e289eaff0`，已推送 Trae-core 的 main 与 fix/seedance-billing-20260925。
- 助手源码提交：`cf034fc575a119e554837a57f808d4752ab0fa02`，已推送 trae-maker 的 main 与 fix/seedance-stream-billing-20260925。
- 公网Core运行于 `/opt/gemstory/starlink-dimension-router/releases/20260930-fallback-risk-first-cause/starlink-dimension-router`，SHA256：`dec8c4b5a7c075b2fdbd7003a0d149cb1b949bb113bbfff579d03a7eef98f2d5`。
- 助手仍在用户本机，运行于 `E:/AIWORK/releases/20260930-fallback-risk-first-cause/ai-work-assistant.exe`，SHA256：`86b676a45481fed84c590d833dda8c45c02692e4cba09dacbfab53d1c2268e66`。发布目录同时包含Python运行时、当前业务脚本和PowerShell资源；不是只替换exe。

## 生产策略

保留原29个profiles，仅增加video_fallback。支持范围内缺档视频使用397积分的临时预占，不冻结整个Key余额；有效期到北京时间2026-10-06 23:59:59。该值来自此前只读核实的360.6392积分基准及10%余量，不是最高实扣保证。真实回执仍按原规格及账号结算并学习，超出预占不截断账单。素材验证、所有权、余额、上游容量保护不变。

## 发布验证

- Linux release离线构建成功；Linux work_client_feedback回归12通过、0失败。
- Windows运行exe及公网 `/proc/<pid>/exe` 路径和SHA256与构建产物匹配。
- 本机API7864与代理8899均恢复监听；本机health、公网health与admin页面HTTP200。
- 经普通Core Key认证的公网 `/v1/models` 返回HTTP200且包含seedance。
- 查询旧失败任务返回failed及budget_failure_reason_unavailable；没有猜测或回填历史失败原因。
- 发布前后公网0个真实在途请求/操作/步骤。助手桥接charge_ready=true、recovery_required=false。
- Core余额/预占/流水/预算操作/预算步骤/结算表和Key表的行数与全行哈希均未变；仅新增恢复后的回执游标元数据。助手17张表中只有桥接代际元数据变化，费用、预算、容量及Key记录均未变。250条预算回执保留。
- 旧5个unknown请求与13个未决桥接预算保留，未删除、未释放未知冻结，也没有重发任务。

第一次切换触发强制停止后的桥接恢复保护，发布检查拒绝放行并回滚。随后核对无运行执行、无待提交rebase，使用正式认证恢复接口按当前实例及代际显式接管，保持未决预算及账务不变。没有直接写库解除保护。助手资源补齐后再次切换，确认API和代理均正常。

## 回滚和限制

本机原启动器、原策略及桥接SQLite备份保存在 `E:/AIWORK/backups/20260930-fallback-risk-first-cause`；公网配置、服务drop-in及SQLite备份保存在 `/var/lib/starlink-dimension-router/backups/20260930-fallback-risk-first-cause`。旧版本程序保留。运行时审计证据保存在新Core发布目录deployment-evidence.json，权限600；不提交凭据或完整生产数据。

本轮没有新建付费视频，也没有验证真实参考视频的最新上游账单延迟或自动下载。本轮完成的是推送、部署与不提交付费任务的发布验证，不能据此声称所有真实生成场景已验收。397兜底过期后若仍缺档，会重新受报价保护限制。
