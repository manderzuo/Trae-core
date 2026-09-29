# 完整父视频自动续写接入（2026-09-29）

## 契约

沿用 `native_video_extend` 模式名，含义为以完整父视频作为参考生成独立新片段。不是保留原视频前缀的原生拼接延长，不覆盖父版本、不自动拼接、不保证严格首帧。`duration` 是新片段时长。

`auto` 在证据绑定的 `video-reference-continuation-v1` 能力可用时优先完整视频；显式 `tail_reference` 保持尾帧流程。完整视频读取失败时必须报错，不能无声改成文生视频或尾帧。

## 接入和审计

- 服务器从归属当前 Key/作业的完成父版本取 MP4；核对 request、budget、key、account、bridge instance、task 六项身份，限制32MiB并检查MP4格式。
- MP4 加密持久化至作业素材，版本快照固定 `continuation_video_media_id`；仅传直接父视频，替代历史视频参考，保留原始图片。
- 复用既有幂等执行、真实结算和素材租约；版本继续携带父版本编号。同一幂等请求不得再次提交视频。
- 桥接内容接口先验证持久执行和结果身份，再读取文件；内部身份响应头不会通过公开下载接口泄露。
- 能力清单必须匹配实际测试证据文件摘要，原生首帧能力仍关闭。Core 灰度仍限“周”Key，不扩大到其他 Key。
- 审计由当前执行者完成，未使用子代理。没有改动历史未知账目、没有迁移数据库结构。

## 本地验证

Core 312 passed；存储270 passed / 1 ignored；AI Work762 passed / 8 ignored；MCP集成57 passed、冒烟29 passed。
新增回归覆盖自动选择、显式尾帧保留、错误身份/文件、跨Key父版本拒绝、失败不降级、同请求重放不重提交、多代续写只传直接父视频，以及真实HTTP传输保留身份响应头。

## 发布验收

已通过公网 MCP `seedance_continue` 的 `auto` 模式完成一次真实付费验收，没有由客户端上传视频：

- 父版本 `version_eQMYy8pah1DgoUgVMBOENg`，父请求 `request_vxaGh0i2QQhr6CFE3XiVpg`。
- 新请求 `request_2RdFz7kJqXG28sD3XZudqg`，新版本 `version_KRTl_afX5kjywnSUUuL-hA`，上游任务 `video-1790657192949-1`。
- 保持同一个作业 `work_CaMY7ZyNU4FzX5ScViRryw`，父子关系正确，公开状态 `reference_mode=native_video_extend`。
- 实际送往上游：1张原参考图、1段完整父视频；父视频805103字节，SHA256 `a115deaa6fe689c8b4e83b8bf27b8cdaf3dce65bf88c2d749a865792e676b50f`，与指定父版本MP4一致。没有尾帧替代。
- 产物5.09秒、864×496、24fps，921112字节；SHA256 `9a8ced0cfd338fcca91ca2c04d3d9b5634b2c5ccb346f962b1e12b20b6b8fa57`。MCP成功下载至系统Downloads。实际解码首帧观察到夜景中的红色绸布；不据此声称严格无缝衔接。
- 视频实际66.5664积分，辅助GLM0.0728积分，总计66.6392。视频预占84、返还17.4336；辅助预占2、返还1.9272；无欠额。
- 桥接完成时间1790657354960，Core真实结算时间1790657359929，间隔4.969秒。周Key可用余额从6385.1956变为6318.5564，差额与两笔实际费用严格一致；历史预占1433未改变。
- 原样重放同一幂等键仍返回同一个请求，账本中仅1笔辅助+1笔视频、余额不变；AI Work重启后查询仍返回completed和同一版本关系。
- 本规格已从84积分临时风险预占校准为74积分（本次真实66.5664加10%后向上取整），仅是实测样本预算，不是上游官方价格上限；结算仍按真实回执。其他未校准的参考视频时长/规格不因本次样本被宣称已全部支持。

## 发布记录

Core源代码 `883c633dc72234f20cd04d373dee148f54802057`，公网二进制SHA256 `0cd8a682f3c10335aa5bd785d20aef6319da84510762c763b813dc214435d008`，release `20260929-native-video-continuation`。

AI Work源代码 `4dcdd312b9ea8bbd13e73ce9f94ca402be4b46e5`，本机二进制SHA256 `8935265c0e58315b308fb8f83be436f2d98020b3085cca4ef5179ee81bd49e23`，release `E:/AIWORK/releases/20260929-native-video-continuation-4dcdd31`。发布目录包含原有Python/PowerShell运行资源，持久启动器已更新；重启后7864网关和8899代理均监听。AI Work仍在本机，不是移至公网。

MCP `9e7d5c8` 已推送并安装本机副本，保留现有DPAPI凭据。三个仓库均正常快进推送main和工作分支。

能力证据摘要 `37b507bbf7f9160875e07cdf7c2753085616ec17d91ac3ed1e82129036a8d7e7`；Core仅周Key灰度不变。部署前备份配置和一致性SQLite，验证全Key余额、预占及旧unknown记录不变。重启桥接通过已有恢复协议保留11笔历史未决记录，没有清账解锁。

本轮完成自动取父视频→续写→下载→结算的真实链路；没有额外测试所有桌面客户端UI，也没有开放严格首帧或自动拼接。

## 后续全量开放（2026-09-29）

用户明确要求现有和新增Key全部可用后，公网持久配置的 `work_context_key_ids` 从周Key白名单调整为空数组（代码契约：对所有Key生效），保留 `work_context_enabled=true`、`continuation_enabled=true`。这是功能开放范围变化，不是删除Key的权限、额度或并发校验；停用/删除的Key不会因此恢复有效，也不允许跨Key访问父视频。

变更时数据库共有4个Key且无ready/running任务；配置已备份到 `/var/lib/starlink-dimension-router/backups/20260929-continuation-all-keys/router-before.json`。只修改白名单字段，重启Core后健康检查200，公网MCP doctor鉴权通过，桥接charge_ready=true、完整视频续写能力为true。部署检查确认余额、历史预占、unknown请求、Key状态与并发配置未变。

重新执行配置回归 `work_context_gray_key_isolation_and_global_disable`，1项通过。没有新建Key、没有付费生成，也没有逐个客户端实测；新增Key自动适用源于空白名单对任意Key执行同一判断，无需单独登记。既有灰度阶段记录保留作为历史验收证据，本节为当前开放范围。
