# 星链维度分流系统（Trae-core）

星链维度分流系统是独立的 API 网关与 Core 管理服务。它负责普通 API Key、权限作用域、并发限制、Key 统一积分额度、请求归属与计费账本；管理页面由同一服务提供。

## 仓库边界

- `src-core/`：Rust Core 库，负责 SQLite schema/migration、Key 身份、配额账本、请求结算与审计。
- `starlink-dimension-router/`：可执行服务、管理员 API、外部 API 入口和静态管理页面。`/admin` 是管理页面，`/healthz` 是不含用户数据的健康检查。
- `scripts/`：Core 专属 Windows 构建、启动、迁移和加密密钥初始化脚本。
- `deploy/`：systemd、FRP、Nginx 示例配置。
- `docs/`：Core 部署、密钥保护和网络接入说明。

AI Work Assistant 的桌面程序、MCP 及其发布内容不属于本仓库；不要将它们复制或提交到这里。普通用户使用的 API Key 在 Core 管理页面创建，AI Work 桥接配置只供 Core 内部调用。

## 能力与接口

- OpenAI 兼容文字请求：`POST /v1/chat/completions`。
- 模型列表、素材上传、视频生成与查询/下载：`GET /v1/models`、`POST /v1/assets`、`POST /v1/videos/generations`、`GET /v1/videos/{task_id}`。
- 普通 Key 可独立设置 `chat:invoke`、`videos:submit`、`videos:read`、`assets:write` 作用域、最大并发和积分额度。
- 管理员页面支持 Key 单独复制、轮换、开关、并发调整、用量查询、额度配置及删除。
- 运行概览显示 AI Work 当前统一积分、可分配余额、已核验实扣、持有中、对账状态和按时间排序的实扣趋势。
- 视频实际扣费控制默认为暂停。新部署应同时启用兼容 AI Work 的 `budget_billing_v2`：按请求和步骤预算预占，不持有整个 Key 余额；结果、执行并发与账单结算分别推进，真实回执到达后按唯一请求归属结算并释放差额。未知回执仅保留本步骤预占，不把估算当作实际扣费。

默认监听地址为 `127.0.0.1:7865`。公网部署应使用 HTTPS 反向代理，并限制 Core 后端端口只被本机代理访问。外部客户端使用普通 Key，不能使用管理员密码、管理员会话或 AI Work 桥接凭据。

## Key 删除与积分返还

删除操作只适用于普通 API Key。服务先读取当前可用额度并二次确认；删除在 Core 的 SQLite `IMMEDIATE` 事务中完成：撤销并逻辑标记 Key、记录审计、仅将 Key `credits` 账户尚可用的正余额记一笔负向调整。删除后 Key 从管理列表隐藏，历史请求、结算、账本和审计记录保留。

- 已实扣积分不会退回；返还仅恢复 Core 内可重新分配的额度，不会增加或退款 AI Work 上游真实积分。
- 存在活动请求/视频任务、held/unknown 预留、待对账结算或待迁移额度账户时，删除返回冲突且不修改数据。
- 无额度账户、零余额或负余额时返还 `0.000000`；重复删除返回零，不会二次返还。
- API：`DELETE /admin/v1/api-keys/{key_id}`，需管理员会话。成功响应中的 `returned_credits` 是精确到微积分的十进制字符串；业务冲突返回 `409`。

Key删除功能最初由v20升级到v21；当前预算链路使用schema28。生产升级前必须备份。产生新财务事实后，不得恢复迁移前数据库或切回不识别新schema的旧程序；应保留账本，关闭新准入并使用兼容版本继续查询和结算。

## 构建与本地测试

要求 Rust/Cargo（建议使用已验证的 Rust 1.90 或更新稳定版；Core crate 声明最低 Rust 1.77），Windows 构建需安装 MSVC C++ Build Tools。项目没有根 Cargo workspace，两个 crate 分别测试：

```powershell
cargo test --locked --manifest-path src-core/Cargo.toml
cargo test --locked --manifest-path starlink-dimension-router/Cargo.toml
node scripts/test-starlink-router-ui.mjs
node scripts/test-starlink-router-migration.mjs
```

要把 Windows 测试和构建写入 D 盘，可在当前 PowerShell 会话指定独立路径（不会修改系统环境变量）：

```powershell
$validation = 'D:\gpt\trae-core-validation'
New-Item -ItemType Directory -Force -Path "$validation\tmp", "$validation\target", "$validation\cargo-home" | Out-Null
$env:TEMP = "$validation\tmp"
$env:TMP = $env:TEMP
$env:TMPDIR = $env:TEMP
$env:CARGO_HOME = "$validation\cargo-home"
$env:CARGO_TARGET_DIR = "$validation\target"
cargo test --locked --manifest-path src-core/Cargo.toml
cargo test --locked --manifest-path starlink-dimension-router/Cargo.toml
cargo build --locked --release --manifest-path starlink-dimension-router/Cargo.toml
```

若使用自定义 `CARGO_HOME`，需先在该目录下载 Cargo 依赖；不要在 Cargo 缓存未准备好时使用 `--offline`。Rust 测试 fixture 使用系统临时目录，设置 `TEMP/TMP/TMPDIR` 后，测试数据库与上传文件会留在上述 D 盘目录。所有自动化测试使用本地 SQLite 和模拟桥接响应，不会请求真实上游或消耗真实积分。

Windows 发布脚本默认从 PATH 查找 `cargo`，也支持覆盖输出目录和 Cargo target：

```powershell
.\scripts\build-starlink-router.ps1 `
  -OutputRoot 'D:\gpt\starlink-router-release' `
  -CargoTargetDir 'D:\gpt\trae-core-validation\target'
```

脚本默认离线构建；首次构建请先用上面的 `cargo build --release` 联网补齐依赖，再运行脚本。生成物为 `<OutputRoot>\release\starlink-dimension-router.exe`，同时写入 SHA-256 清单。仓库本地的 `target/`、`dist/`、运行数据和密钥目录已加入 `.gitignore`。

## Windows 启动与持久化配置

先准备独立数据目录及其外部加密密钥文件：

```powershell
.\scripts\new-starlink-router-key-encryption-env.ps1 `
  -DataDir 'D:\Gemstory\starlink-core-data' `
  -EnvironmentFile 'D:\Gemstory\secrets\key-encryption.env'
```

密钥文件只生成一次，脚本拒绝覆盖；请用备份软件安全保管，数据库和密钥必须分开备份。启动程序时指定部署根目录、数据目录和同一密钥文件：

```powershell
.\scripts\start-starlink-core.ps1 `
  -ReleaseRoot 'D:\Gemstory\starlink-core' `
  -DataDir 'D:\Gemstory\starlink-core-data' `
  -EnvironmentFile 'D:\Gemstory\secrets\key-encryption.env'
```

脚本会校验加密密钥后，为本次 Core 进程注入 `STARLINK_ROUTER_KEY_ENCRYPTION_KEY`、版本及旧密钥映射，不依赖终端关闭后仍存在的临时环境变量。也可追加 `-Background` 以隐藏窗口运行。数据目录内的 `router.json` 保存常规设置；环境变量 `STARLINK_ROUTER_DATA_DIR`、`STARLINK_ROUTER_HOST`、`STARLINK_ROUTER_PORT`、`STARLINK_ROUTER_PUBLIC_BASE_URL` 可覆盖对应值。显式设置的数据目录优先于 `router.json` 中的旧目录。

初次启动后访问 `http://127.0.0.1:7865/admin` 并按页面初始化管理员。管理员密码只用于 Core 管理登录；生成的普通 API Key 才发给外部客户端。初始化密钥与桥接配置详情见[Key 加密文档](docs/starlink-router-key-encryption.md)。

## Linux / 腾讯云部署

服务示例见 [`deploy/systemd/starlink-dimension-router.service`](deploy/systemd/starlink-dimension-router.service)，公网反代与 FRP 拓扑见 [`deploy/frp/README.md`](deploy/frp/README.md)。systemd 模板使用版本稳定入口：

```text
/opt/gemstory/starlink-dimension-router/current/starlink-dimension-router
```

腾讯云现有生产服务目前通过 `starlink-dimension-router.service.d/` 下的版本化 drop-in 选择具体 release，而不是 `current` 链接。升级时应保留主 unit 和已有版本/drop-in，新增一个排序更后的版本化 drop-in 指向新 release；不要为套用模板而重写现有服务配置。

schema 26 与新 v2 账本的恢复规则：一旦新 v2 账本已写入任何新财务事实，先关闭新收费准入；保留并继续运行能够识别 schema 26 的兼容结算进程和生产数据库，让在途任务继续结算，并确保旧任务结果与账单仍可查询。此后不得恢复迁移前数据库，也不得盲目切回不识别 schema 26 的旧 release。迁移前只读备份和回滚准备可以保留，但只有确认尚未产生任何新财务事实时，才允许将数据库、实际使用的 release 选择入口（`current` 链接或版本化 drop-in）及相关服务配置作为一套一致状态整体还原；不得只恢复数据库或只切换程序。本文其他位置的 v20→v21 回滚说明仅适用于对应历史迁移且尚未产生任何新财务事实的场景，不得套用于当前 schema 26 数据库或已产生新财务事实的场景。

推荐每个版本放在独立 `releases/<版本号>/` 目录，再把 `current` 指向已验收版本；数据目录固定在 `/var/lib/starlink-dimension-router`，加密环境文件固定在 `/etc/starlink-dimension-router/key-encryption.env`，二者不可随版本目录轮换。不要把生产密钥、数据库、FRP token 或真实配置提交到 Git。

带数据库 schema 迁移的发布流程：

1. 维护窗口内先关闭新收费准入；确认没有未完成写入，且在途任务状态已持久化并可由 schema 26 兼容结算进程恢复后，再停止服务并确认没有进程打开 SQLite。
2. 在服务器持久卷的受限备份目录建立迁移前只读备份，包含整个 Core 数据目录（数据库及存在的 WAL/SHM）、实际使用的 release 选择入口和服务配置；该备份仅在尚未产生任何新财务事实时可用于整套还原。不要下载生产数据库到开发机。
3. 将新二进制放入版本目录并校验 SHA-256；腾讯云现有服务应新增排序更后的版本化 drop-in 指向新 release，保留主 unit 与已有 drop-in，不要切换 `current` 或重写现有服务配置。启动能够识别 schema 26 的版本，执行对应迁移并启用新 v2 账本。
4. 检查 `systemctl status`、`journalctl`、`curl --fail https://<域名>/healthz` 和 `/admin`；确认旧任务结果与账单仍可查询，且在途任务由兼容结算进程继续处理。新收费准入保持关闭，直到发布门槛验收通过。
5. 如迁移或健康检查失败，关闭新收费准入并保留故障现场副本。若新财务事实已经产生，保留生产数据库并继续运行 schema 26 兼容结算进程，使在途任务继续结算、旧任务结果和账单保持可查；不得恢复迁移前数据库，也不得盲目切回不识别 schema 26 的旧 release。只有确认尚未产生任何新财务事实时，才可将迁移前数据库、release 选择入口和服务配置作为一套整体还原；核对无误后再开放新收费准入。

健康检查只证明 HTTP 服务启动，不会发送真实上游请求或验证真实积分扣费。公网反向代理样例使用 `api.gemstory.cn`，上线前需核验服务器实际域名、FRP/Nginx 路由、TLS 证书、持久卷、数据库路径及管理员登录状态；不能仅凭模板推定生产拓扑。

### Seedance 流式客户端

客户端沿用 Core Base URL、普通 API Key 和模型名 seedance，无需额外配置请求头。对于不发送 Idempotency-Key 的 stream=true 请求，Core 会为该次请求生成内部编号，按同一 Key 分别记录提示词辅助和视频的真实回执，并在 SSE 中返回结果。同一 Key、同一内容的立即重试会复用运行中的任务或两分钟内完成的任务；显式发送 Idempotency-Key 的客户端仍可用同一键重连并复用原任务。

没有客户端提供的稳定幂等键时，Core 无法可靠区分断线后的自动重试和用户主动再次提交：短时间内主动重复同一内容也会复用旧任务，超过窗口的重试则可能创建并计费新视频。服务端不会因 SSE 断开而自动重发生成请求。生产视频仍受持久化计费闸门约束：闸门为 paused 时返回 video_billing_paused，不执行上游生成，不能把兼容流式请求误认为已放开视频计费。

### 旧版兼容：无报价请求的受控实扣（非推荐新部署路径）

以下说明仅适用于 `budget_billing_v2=false` 的历史 v1 受控链路。新版 v2 使用 schema 28，并要求本机 AI Work 提供 v2 预算、执行及回执接口；配置开关写在 `router.json` 中，默认关闭以防两端版本不兼容。v2 不会在预算准备失败后偷偷降回旧版整额冻结。正常账单延迟不占执行并发；仍有未决步骤的预占不允许被当成可用余额。开启前核验实际预算政策覆盖的模型与视频规格，未配置档位不可伪称已支持。

文字 API 和视频 API 在收到明确的 `quote_unavailable` 时，可进入受控实扣；视频仍须先由管理员将视频计费状态设为 `active`，默认 `paused` 会在任何付费请求前拦截。处于 `paused` 时，管理员也可为一个普通 Key 登记精确的一次性诊断请求（Key 内部 ID 与请求摘要）。报价超时、鉴权失败、余额快照不可用及其他错误都不降级。受控视频支持 Seedance Chat（DeepSeek 辅助 + 视频）与原生 `/v1/videos/generations`（仅视频）。

Core 在调用任何付费步骤前，先检查上游积分快照、素材归属和 Key 额度，并将该 Key 全部可用积分持有到同一个操作；同一 Key 同时只能有一个未结清的受控请求，不同 Key 各自隔离。这个数值是授权持有额，**不是**上游价格上限；上游真实扣费可能超出它。Seedance 的 DeepSeek 辅助与视频步骤各用独立请求编号，并与发起请求的 Core Key 绑定；普通文字请求按其唯一请求编号查回执。只有真实回执核验成功才记入实际消耗并释放持有余量；超额时照实记账并冻结 Key。未知/冲突回执或断线时保持持有，只查询原请求，不自动重发。

视频提交前被 AI Work 本地拒绝时，只有收到与该 Core 请求编号完全一致的零扣费证明，才结清视频步骤并释放剩余持有；已产生的文字辅助费用仍按其真实回执入账。单独的 HTTP 400/503、任务状态不明或聚合余额变化均不能代替该证明。重启恢复只查询原任务与回执，不再次提交视频。

此策略不应被描述为上游价格上限：若单次真实扣费高于 Key 的全部可用余额，Core 会如实记录超额并冻结该 Key。v23→v24 迁移扩展受控步骤类型以记录普通文字请求，并保留既有受控操作与步骤；部署前须备份数据库及 WAL/SHM，不能只回退二进制。回执不明时停止该 Key 的新请求并保留现场，不能凭聚合余额差、固定积分或视频文件单独宣称计费通过。

## 迁移、备份与安全

- SQLite 实际文件为 `<STARLINK_ROUTER_DATA_DIR>/data/core.sqlite3`；运行时数据、临时上传内容和生成文件不得放进 Git。
- 备份须使用停写一致性副本或SQLite在线备份接口，并保留独立加密密钥。只有尚未产生新财务事实时，才可整体恢复迁移前备份；否则保留新账本与兼容结算进程。
- 历史 Key 只显示脱敏前缀；明文仅在生成、轮换或管理员复制操作时短暂返回。日志、确认对话框和普通列表均不展示完整 Key。
- 删除保留历史账本和审计数据，不物理删记录；任何待结算/未知余额都 fail-closed，先对账，不自动退款。
- 迁移工具不会覆盖已有目标数据库；使用前先核对源目录、目标目录与迁移报告。迁移脚本不删除源数据。

如需多网段或公网接入，请先阅读 [`deploy/frp/README.md`](deploy/frp/README.md)；不要把 Core 后端端口、AI Work 内部桥接 Key 或加密密钥直接暴露给互联网客户端。
## v2 参考素材错误与迁移演练

v2 桥接的参考素材类型、摘要、时长或格式无效时，Core 保留明确的 HTTP 400
错误码，不将这些输入错误变为笼统的 503。参考视频预算必须包含由 AI Work
验证的参考时长；仅上传成功不表示该规格已配置风险预算或通过真实价格校准。

`src-core/tests/production_migration.rs` 提供显式启用的私有备份迁移演练：
从 `CORE_MIGRATION_SNAPSHOT` 指定的不可变 SQLite 一致性备份复制到临时目录，
升级 schema 28 后核对全部旧表旧列内容和完整性。必须额外设置
`CORE_MIGRATION_SNAPSHOT_ACK=1` 并运行 ignored 测试；默认回归不读取生产数据。
日志只记录表名/数量，不打印财务行或密钥材料。切勿把运行中数据库文件直接复制
当作一致性备份，也不要将任何测试备份提交到仓库。
