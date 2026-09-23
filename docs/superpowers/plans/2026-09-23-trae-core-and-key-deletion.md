# Trae-core 独立仓库与 Key 删除返还实施计划

**目标：** 将星链维度分流系统（Core）的服务端、管理界面与独立部署材料筛选并发布到 manderzuo/Trae-core；增加普通 API Key 逻辑删除、未用 Core 积分原子返还和列表管理功能。

**架构：** 保留 src-core 与 starlink-dimension-router 两个同级 Rust crate 及现有相对依赖；所有额度与删除决策由 CoreStore 在 SQLite IMMEDIATE 事务中完成；Axum 管理 API 仅暴露管理员会话保护的删除操作；现有静态管理页发起删除并刷新额度视图。

**技术栈：** Rust、SQLite/rusqlite、Axum、静态 HTML/JavaScript、PowerShell、Cargo。

**规格：** docs/superpowers/specs/2026-09-23-trae-core-repository-and-key-deletion-design.md

**全局约束：**

- 目标仓库只接收 Core 所需文件；不复制 AI Work 桌面端、MCP、真实数据、日志、生产密钥、环境文件、release 二进制或临时测试配置。
- 不修改源仓库 E:/AIWORK/workspace/TraeWorkAssistant；它包含用户未提交内容，所有 Core 改动只在 E:/AIWORK/workspace/Trae-core。
- 任何构建、测试、Cargo/Rustup 下载缓存和临时目录都放在 D:/gpt/trae-core-validation 下；设置 CARGO_HOME、RUSTUP_HOME、CARGO_TARGET_DIR、TEMP、TMP、TMPDIR 到该任务目录，不将测试输出写入 C 盘。
- 测试限于本地数据库和模拟回执，不调用真实上游，不操作线上 Key 或积分。
- Key 删除只返还 Key credits 账户中可用的正余额；held、unknown、待对账、已实扣余额不返还。负余额不得变成正向返还；不存在或未配置额度账户时返还 0。操作不增加 AI Work 上游的真实余额。
- 删除采用逻辑标记并撤销 Key，保留请求、账本、结算、审计历史；删除和负向账本事件必须同事务、幂等。
- 仅允许向已确认的 manderzuo/Trae-core 远端普通推送；不 force push，不推主项目或 MCP 仓库。远端连接失败时保留本地结果并报告，不能声称已上传。

**重点审查：**

1. 同一数据库中请求预占与删除竞争时，IMMEDIATE 事务须保证不能在请求持有额度后返还。
2. 缺额度账户、零余额和负余额必须返回 0，不能创建虚假积分或令上游可分配额度增加。
3. 已撤销 Key 再删除、重复删除以及请求重放均不能重复返还。
4. 1.234567 等微积分金额须精确返回，不使用浮点数。
5. held/unknown reservation、活动请求或 reconcile-required 结算存在时，删除必须 409，任何数据均不改变。

## 任务 1：筛选 Core 文件并建立可复现基线

**涉及文件：** 目标仓库的 src-core、starlink-dimension-router、scripts、deploy、docs、.gitignore。

- [ ] 只从源仓库复制 Core crate 的 Cargo.toml、Cargo.lock、src、tests；复制 Router crate 的 Cargo.toml、Cargo.lock、src、static、tests。不得复制 crate 内生成的 D: 路径目录、target、数据目录或其他生成文件。
- [ ] 复制 Core 专属构建、迁移、启动、加密密钥初始化脚本，以及 Core 专属部署文档、systemd 服务、FRP/Nginx 示例。逐份检查脚本和示例，不纳入 AI Work 专用或含真实配置的文件。
- [ ] 新建目标仓库 .gitignore，排除 target、data、数据库/WAL、日志、真实 .env、密钥与发布产物。
- [ ] 设置任务专属 D 盘测试环境，运行两个 crate 的现有全套测试；若依赖或工具链缓存不完整，只在 D 盘 Cargo/Rustup 目录补齐。

    $validation = 'D:\gpt\trae-core-validation'
    New-Item -ItemType Directory -Force -Path $validation, (Join-Path $validation 'tmp'), (Join-Path $validation 'cargo-home'), (Join-Path $validation 'rustup-home'), (Join-Path $validation 'target') | Out-Null
    $env:TEMP = Join-Path $validation 'tmp'
    $env:TMP = $env:TEMP
    $env:TMPDIR = $env:TEMP
    $env:CARGO_HOME = Join-Path $validation 'cargo-home'
    $env:RUSTUP_HOME = Join-Path $validation 'rustup-home'
    $env:CARGO_TARGET_DIR = Join-Path $validation 'target'
    cargo test --manifest-path src-core/Cargo.toml
    cargo test --manifest-path starlink-dimension-router/Cargo.toml

- [ ] 检查目标 Git 状态及复制清单，确认只包含 Core 文件，没有凭据、数据、生成缓存或 MCP 文件；记录基线失败项，不掩盖既有失败。

**验收：** 两个 crate 的源码在目标目录独立存在；相对依赖 ../src-core 有效；测试与构建缓存落在 D 盘；复制范围可审查。

## 任务 2：先建立迁移与 HTTP 删除行为的失败测试

**涉及文件：** starlink-dimension-router/tests/admin_key_deletion.rs、现有 Core 测试 fixture（必要时抽取仅限测试的共享 fixture）。

- [ ] 复用 admin_api.rs/admin_login.rs 的本地测试服务器、管理员会话和 SQLite fixture，新增 DELETE /admin/v1/api-keys/{key_id} 行为测试。
- [ ] 写入测试：普通 Key 删除应得到成功响应与精确返还数；未登录/非管理员请求应拒绝；有活动请求/待对账时应得到 409；管理员 Key 不可删除。
- [ ] 先运行新测试，确认当前未实现路由时测试因实际 404/非预期状态失败，而不是因 fixture、编译或网络问题失败。
- [ ] 不连接线上地址，不使用现有真实管理员/普通 Key。

    cargo test --manifest-path starlink-dimension-router/Cargo.toml --test admin_key_deletion

**验收：** 新测试确实针对缺少的删除行为失败，并且所有 fixture 都只使用 D 盘本地临时数据。

## 任务 3：实现 schema v21 与原子额度返还

**涉及文件：** src-core/src/schema.rs、src-core/src/store.rs、src-core/src/quota.rs（按现有模块职责）、src-core/src/error.rs、src-core/src/lib.rs、src-core/tests/key_deletion.rs、src-core/tests/migration.rs。

- [ ] 先添加 CoreStore 级别的测试，覆盖从 v20 升级后已有 Key、账本、使用和结算记录保留；返还余额为 1.234567 时精确返还；已实扣部分不返还；无账户/零余额/负余额返回 0；held、unknown、活动请求、reconcile-required 和额度迁移待处理均拒绝；已撤销/重复删除只返还一次。
- [ ] 将 CURRENT_SCHEMA_VERSION 从 20 升到 21，新增 SCHEMA_V21 为 api_keys 添加 nullable deleted_at_ms；扩展 migrate 让 v20 与更新版本按一次事务迁移至 v21，旧记录默认未删除，并验证迁移重入与失败回滚。
- [ ] 增加可审计的删除错误类型，供 API 将业务冲突映射为 409；保留既有 CoreError 风格。
- [ ] 新增 CoreStore::delete_api_key_as_admin 管理操作：IMMEDIATE 事务内校验管理员身份和普通 Key；检查请求状态 received/validating/reserved/queued/dispatched/completing/unknown、held/unknown reservation、reconcile-required settlement 与待迁移额度账户；取 credits 可用正余额，使用整数微积分并追加关联 Key 的负向 adjust 账本事件；随后撤销并设置 deleted_at_ms，写入操作者、Key ID、返还金额和时间的审计事件；最后提交事务。
- [ ] 若 Key 已删除则幂等返回 0；已撤销但未删除的 Key 可在无活动事务时正常删除。不能物理删除账本或历史。不存在 credits 账户时返还 0；负可用余额不得返还。返还仅释放 Core 内已分配额度，不得篡改 AI Work 返回的真实余额快照。
- [ ] 在 Key 管理查询中排除 deleted_at_ms 非空记录；保留其他历史/统计查询所需数据。
- [ ] 运行 src-core 的定向删除、迁移测试，再运行全部 Core 测试；检查测试数据库路径确实在 D 盘。

    cargo test --manifest-path src-core/Cargo.toml --test key_deletion
    cargo test --manifest-path src-core/Cargo.toml --test migration
    cargo test --manifest-path src-core/Cargo.toml

**验收：** 成功删除仅冲回可用余额且只冲回一次；待结算/未知状态冲突时事务无副作用；v20 数据完整迁至 v21。

## 任务 4：接通受保护的管理员删除 API

**涉及文件：** starlink-dimension-router/src/admin_routes.rs、starlink-dimension-router/src/dto.rs（仅当响应需要专用 DTO）、starlink-dimension-router/tests/admin_key_deletion.rs。

- [ ] 注册 DELETE /admin/v1/api-keys/:key_id，继续使用现有管理员会话认证和 Principal。
- [ ] 调用 CoreStore::delete_api_key_as_admin；成功返回 no-store JSON，包含 deleted 与十进制字符串 returned_credits；额度精度复用 CreditAmount 的序列化，不经过浮点数。
- [ ] 为普通业务冲突返回 409，并给出不泄露敏感数据的中文提示；认证失败沿用现有会话错误行为；数据库/内部错误按现有管理 API 错误规范处理。
- [ ] 完成任务 2 的 HTTP 测试，并额外断言重放不会二次返还、未授权请求不能删除；运行 Router 专项测试与完整测试。

    cargo test --manifest-path starlink-dimension-router/Cargo.toml --test admin_key_deletion
    cargo test --manifest-path starlink-dimension-router/Cargo.toml

**验收：** 已登录管理员可删除普通 Key；无会话拒绝；冲突为 409；重复调用没有第二笔返还。

## 任务 5：重构普通 Key 列表操作并提供安全确认

**涉及文件：** starlink-dimension-router/static/index.html、starlink-dimension-router/tests/admin_key_deletion.rs 或已有 UI 静态检查测试。

- [ ] 在普通 API Key 每行操作区增加“删除”按钮，与现有复制、额度、并发、启停、使用情况风格一致。
- [ ] 删除前通过现有 GET /admin/v1/api-keys/:key_id/quota?resource_kind=credits 读取最新可用余额，再显示确认信息：显示名称、可返还余额、已实扣不返还、删除后不可再用；取消时不发送 DELETE 请求。并发请求导致余额变化时以删除事务和返回值为准。
- [ ] 确认后调用 DELETE endpoint；成功提示真实 returned_credits、刷新 Key 列表与可分配额度；冲突时展示需先完成结算/对账的中文说明，且保留该行。
- [ ] 保持 Key 秘密只在既有复制流程中使用；删除确认、列表和日志不得展示或记录完整 Key。
- [ ] 运行现有 Core UI 检查脚本（若其依赖与 Core 无关则不复制/不运行），并在浏览器本地 fixture 上手动核对删除/取消/冲突/成功四种状态。

**验收：** 界面实时反映删除和额度返还；取消无副作用；完整 Key 不会进入页面提示或普通日志。

## 任务 6：补齐可独立构建的脚本、部署材料和中文 README

**涉及文件：** README.md、.gitignore、starlink-dimension-router/src/main.rs、starlink-dimension-router/src/config.rs、starlink-dimension-router/src/admin_routes.rs、scripts/build-starlink-router.ps1、scripts/start-starlink-core.ps1、scripts/migrate-starlink-router.ps1、scripts/new-starlink-router-key-encryption-env.ps1、Core 测试 fixture、deploy/**、docs/**。

- [ ] 修正 Core 启动/构建脚本与运行入口：使用 PATH 上的 cargo 或可覆盖参数；移除 C:\Users\StarLink 专属 Cargo 路径；将 main/config、静态资源 fixture 和迁移管理 API 中 D:\gpt 的机器专属运行路径改为 STARLINK_ROUTER_DATA_DIR 或明确的可配置路径；不得让 Linux 部署把 D:\gpt 当相对目录。数据目录和构建输出允许参数/env 覆盖，不将用户数据写入仓库。缺省行为给出清晰提示，不覆盖已有数据。
- [ ] 将已有 Core 测试中硬编码 D:\gpt 根路径的 fixture 改为系统临时目录接口；测试进程用 TEMP/TMP/TMPDIR 指向任务专属 D 盘目录，测试清理只针对自己生成的唯一子目录。
- [ ] 更新迁移脚本与 systemd/FRP/Nginx 示例，保证仓库根目录结构下路径正确；所有敏感配置只使用占位符或环境变量。
- [ ] 编写中文 README：项目/仓库边界、Core 管理页、API 用途、Rust 版本与 Windows 构建启动、Linux systemd/反代、数据目录/env/admin 初始化、额度与 Key 生命周期、删除返还/不返还规则、迁移备份、密钥保护和常见排障。
- [ ] README 明确普通 Key 删除是逻辑删除；返还的是 Core 可重新分配额度而非 AI Work 上游真实积分；待对账、占用和未知请求必须先处理；MCP 与 AI Work 客户端不在本仓库。
- [ ] 逐项检查 .gitignore、示例配置和 README 中无真实密码、API Key、Core 加密密钥、生产数据或内部私密地址。

**验收：** 新环境按 README 可复现构建和启动；构建脚本不依赖 StarLink 用户 C 盘路径；文档与实际脚本参数一致。

## 任务 7：全量验收、审阅与发布到正确仓库

**涉及文件：** 目标仓库全部产品文件；不得改动源主仓库或 MCP 仓库。

- [ ] 在 D 盘隔离目录重新运行 cargo fmt 检查、src-core 全测试、Router 全测试、release 构建，并复核没有任何测试访问公网真实上游；本轮 shell 进程显式设置 CARGO_HOME、RUSTUP_HOME、CARGO_TARGET_DIR、TEMP、TMP、TMPDIR 到 D:\gpt\trae-core-validation。

    cargo fmt --manifest-path src-core/Cargo.toml -- --check
    cargo fmt --manifest-path starlink-dimension-router/Cargo.toml -- --check
    cargo test --manifest-path src-core/Cargo.toml
    cargo test --manifest-path starlink-dimension-router/Cargo.toml
    cargo build --release --manifest-path starlink-dimension-router/Cargo.toml

- [ ] 将构建输出和临时目录限定到 D:\gpt\trae-core-validation；检查工作树差异、秘密扫描结果、复制清单和 README 命令；确认 schema 20→21、退款精度、幂等、冲突保护、管理员会话与 UI 全有测试证据。
- [ ] 只在 D:\gpt\trae-core-validation 下清理由本计划创建的临时构建/测试目录；删除前解析并验证绝对路径确实位于该目录内。保留目标仓库中的源码、README、测试和 Git 历史。
- [ ] 查看 origin 必须精确为 https://github.com/manderzuo/Trae-core.git，检查当前分支及远端状态；分阶段显式暂存目标仓库文件并创建清晰提交，不使用全局暂存。
- [ ] 远端可用时只向 Trae-core 的 main 分支普通推送，绝不 force push；远端不可用或有他人新提交时停止推送并报告，保留本地提交，不能改推 TraeWorkAssistant 或 MCP 仓库。

**验收：** 全量测试和 release 构建通过；仓库无秘密/运行数据；只发布到 Trae-core；最终报告提交号、远端状态及仍存在的限制。

## 完成后复核

- 将上述 8 项验收标准逐项与规格文件核对，并在最终交付说明中标出通过证据或未通过原因。
- 查看最终 Git diff，确认没有源仓库/MCP 修改、误删用户文件、密钥或运行时数据库。
- 未经用户另行提出，不执行真实积分请求、不连接生产环境，也不清理 D:\gpt 下不属于本计划创建的文件。
