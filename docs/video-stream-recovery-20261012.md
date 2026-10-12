# 视频未知状态传递补丁（2026-10-12）

AI Work 已修复视频 SSE 读取错误被忽略及缺少整体等待期限的问题。Core 的原 GET 视频状态接口此前把 unknown + not_ready 转成 processing，现在返回 unknown，并透传闭合的视频响应流诊断。原任务身份、发送次数、预算预占和费用核对规则保持一致；ready 真实结果优先，迟到结果仍可确认。未新增模型调用或重发路径。

已执行 23 项 Core 回归：新增 unknown / 保留 Held / 查询不重发 / 跨 Key 隔离 / 迟到结果 1，既有 readonly terminal 2、work client feedback 19、闭合错误码与阶段映射 1。使用已有 Linux 缓存离线锁定构建，部署至 `20261012-video-stream-recovery` 目录，健康检查与真实原请求只读查询通过。

Core 运行二进制 SHA256：`116404f22490003ff44ac8b4cc0c390bfcdf3a6a30fd3959ec353dcb6c2bf9b9`。

原故障视频尚无最终成片或最终收费证据；未知状态不能改为失败退款，不能自动重新生成。Studio 已有 unknown / 查询中断契约，本次无需改动其交互布局。
