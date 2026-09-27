# 请求预算 v2：接线与灰度边界

状态：本地接线/隔离测试阶段，不是公网已启用公告。AI Work继续运行在用户的Windows主机；Core运行于公网服务器。

## 请求路径

`seedance` Chat请求经默认文字助手判断意图；连接测试/问候只执行助手，不误生成视频。视频意图沿用原请求时长、清晰度与参考图，准备独立视频预算后派发。普通文字模型仍沿用旧路由，不宣称已经迁移v2早SSE。

Core的`router.json`新增`budget_billing_v2`，默认false。必须在两端相容、审计/真实验收通过后才启用。不能仅改此开关解决旧公网503。

内部鉴权使用桥接管理员Key，普通用户Key仅用于Core鉴权；Key元数据镜像不包含明文秘钥。AI Work内部端点：

Key 镜像版本与元数据摘要在同一数据库事务内持久化：内容不变复用版本，内容变化单调递增（不受时钟回拨影响）。网络发送在事务外；旧快照被新版本超越时最多重新读取并同步一次，不让全局网络锁阻塞其他请求。

| 路径 | 用途 |
| --- | --- |
| POST `/internal/bridge/v2/budgets/prepare` | 服务端选账号、规范参数、读取权益/估价，持久准备单笔预算 |
| POST `/internal/bridge/v2/budgets/dispatch` | 全授权与加密原件核验后，consume/send CAS唯一派发 |
| POST `/internal/bridge/v2/budgets/cancel` | 只取消尚未消费的原预算，返回持久no-send证据 |
| GET `/internal/bridge/v2/requests/{request_id}/{execution,result,billing,content}?budget_id=...` | 按完整预算归属恢复状态、结果、账单与视频 |
| POST `/internal/bridge/v2/requests/{request_id}/refresh?budget_id=...` | 请求级幂等刷新提示，不新发付费任务 |
| GET `/internal/bridge/v2/receipt-events?generation=...&after=...&limit=100` | 有界回执事件与历史重放 |

## 资金与并发

只预占每个步骤的H，不冻结整把Key。Helper风险预算2积分；视频H来自参数绑定原生估价或显式风险政策。真实消费可能超过H，必须照实记账，不能截断或固定扣1。

执行终态释放并发，财务待结仍保留本笔H。结果、下载不等待账单。并发满返回429，不包装成quote_unavailable/503。Core未派发且有桥接持久no-send证据时释放预约，不编造上游账单；已派发的NoSend按0回执处理。未知派发不自动重试、退款或释放执行槽。

Chat流式请求无需客户端另配Idempotency-Key；Core内部用Key+请求指纹复用在途请求。后台worker不依赖HTTP观察连接存活。结果用OpenAI SSE帧及DONE结束。文件下载沿用普通Key鉴权，不能承诺任意客户端都自动执行本地下载。

## 回执恢复

金额确认与事件由AI Work同事务写入。Core先幂等应用实扣/冲突，再CAS持久推进游标（`schema_meta`的`budget-receipts-v2:`命名空间）；两个提交之间崩溃只导致重放，不漏记。游标按桥接实例与活动世代隔离。

空generation仅用于发现当前世代；非空错误世代409。页头绑定当前活动世代，单条事实保留原世代；按全库递增sequence返回历史记录。因此正常重启后的新世代可从0重放旧事实，不丢重启前尚未消费的冲突。未知本地步骤事件（如Core拒绝准入后AI Work取消准备）记录审计后推进，不生成虚拟Core任务。

Core重开后，同一原请求的客户端重试可以取回已保存Helper输出并继续视频，不再次付费调用Helper。尚未实现无需客户端重试的完整后台工作流恢复；无原始正文/可信输出时不可猜测重发。

## 素材与价格边界

支持最后一条用户消息中的图片data URL：真实文件格式验证、合计6MiB/10张限制、素材权限及限流，上传到桥接后传入视频预算。已有Core素材ID先验证所有权。此入口不任意抓取远程URL或客户端本地路径；参考视频尚缺可信时长预算适配。

AI Work原生档案当前限无参考图/视频720p16:9的10/15秒。其他规格读取本机显式`bridge-budget-policy.json`，未配置拒绝而不全额冻结。其格式为version1+profiles数组，每条包含profile、policy_version、source、expires_at_ms、hold_credits；不能把研究推导表宣称为官方实扣表。

## 发布前仍须完成

- 按账号的静止重基线与崩溃lease恢复，不能任意清除D占用。
- 事件/账单大历史有界处理与错误诊断复核；Key 镜像并发版本修复已通过本地回归。
- 参考规格政策、真实参考图采样、普通Chat新账本早SSE。
- 本机新运行器与公网Core的受控付费联调，记录产物、唯一会话、实际扣费与Key前后余额；实测回执确认到Core入账P95。
- 正式部署的备份/迁移与回滚演练，之后分别推送AI Work和Core仓库，MCP仓库不混入。

本地模拟外部上游的测试不是公网付费验收。不能删除旧账或开启无限信用来让验收通过。
