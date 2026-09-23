# Trae-core 独立仓库与 Key 删除返还设计

## 目标

建立独立、可构建和可部署的 `manderzuo/Trae-core` 仓库，承载星链维度分流系统（Core）的服务端、管理界面及其运行所需的额度/账本核心代码，并在该版本中加入普通 API Key 的安全删除与未用积分返还。

用户已选择保留现有目录关系的发布方案。GitHub 目标仓库当前为空；源项目工作区含有其他未提交改动，发布必须按 Core 文件清单筛选，不能把整个主项目工作区推送过去。

## 仓库边界

目标仓库保留两个同级 Rust crate，以维持现有本地依赖关系：

```text
Trae-core/
├── src-core/                    # aiwork-core：Core 持久化、额度、账本与请求域逻辑
├── starlink-dimension-router/   # 可运行服务、管理 API、静态管理页面
├── scripts/                    # 仅 Core 的构建、迁移、启动和密钥初始化脚本
├── deploy/                     # Core 的 systemd、FRP/Nginx 示例配置
├── docs/                        # Core 安全与部署文档
└── README.md                    # 中文使用与部署说明
```

`starlink-dimension-router/Cargo.toml` 继续通过 `../src-core` 引用 Core crate。构建和启动脚本须能从仓库根目录运行；应移除开发机专属 Cargo 可执行文件路径和不必要的机器专属输出路径，改为可配置参数或使用 `cargo` 的 PATH 解析。

纳入 Core 当前实现所需的 Rust 源码、静态资源、Cargo 清单/锁文件、Core 专属测试、配置示例和部署文档。排除 AI Work 桌面应用与 React 客户端、MCP 仓库内容、数据库/用户数据、生产密钥、真实环境文件、日志、release 二进制和临时测试配置目录。

## Key 删除与积分返还

### 用户可见行为

- 普通 Key 列表增加“删除”操作。点击后确认框显示该 Key 当前可返还的剩余积分，并说明已实扣积分不返还、删除后无法再使用。
- 删除成功后从普通 Key 列表移除，刷新可分配积分；界面显示实际返还数量。
- 有处理中、预占中、未知或待对账请求时，删除被拒绝并说明需先等待结算/完成对账。
- 管理员 Key 不属于普通 Key 管理列表，不允许通过此接口删除。

### API 与持久化

- 新增管理员会话保护的 `DELETE /admin/v1/api-keys/{key_id}`。
- 使用单个 SQLite `IMMEDIATE` 事务完成授权、待处理请求检查、额度冲回、Key 撤销/逻辑删除和审计事件写入；任一步失败均整体回滚。
- 不物理删除 `api_keys`、账本、结算或请求记录。通过新增 nullable `deleted_at_ms` 标记逻辑删除；数据库迁移由 schema v20 升至 v21，老 Key 默认未删除。普通 Key 管理列表默认隐藏已删除项。
- 仅冲回 Key `credits` 预算账户中尚可用的正余额：追加一条关联 Key 的负向额度账本事件，将该余额清零。已结算实扣保留，预占/未知余额绝不返还。返还只增加 Core 可重新分配空间，不会增加 AI Work 上游账户的真实积分。
- Key 删除同时将其状态设为 revoked，防止新请求；既有历史、使用统计、结算与审计仍可追溯。
- 如果存在以下任一情况，返回冲突且不修改任何数据：请求仍处于 received/validating/reserved/queued/dispatched/completing/unknown；Key 有 held/unknown reservation；请求结算标记 reconcile-required；Key 额度账户需要迁移对账。
- 审计事件记录 Key ID、操作者、各资源余额冲回量与时间。重复删除不得重复冲回；对已删除 Key 的重复调用保持安全、无二次返还。

## README 内容

中文 README 说明：项目边界和架构、Core 管理页如何访问、支持的 API/模型用途、所需 Rust 工具链、离线/联网构建方式、本地 Windows 启动、Linux systemd 与反向代理部署、数据目录与环境变量、管理员初始化/登录、Key 配额及删除规则、数据库备份/迁移和密钥保管。示例不得包含真实密码、Key、密钥文件或公网用户数据；所有地址和密钥示例均使用占位值。

## 安全与兼容约束

- 不更改 MCP 仓库或 AI Work 桌面端代码。
- 不改写源主仓库中其他功能，也不重置、暂存或删除其现有未提交内容。
- 不向上游 AI Work 发出真实请求，不触碰生产 Key 或真实积分。
- 不对 GitHub 进行 force push；仅将筛选后的 Core 文件和 README 发布到 Trae-core。
- 迁移须保留 v20 数据与 API Key；升级后旧数据保持可读。

## 验收标准

1. 空仓库形成自包含的 Core Rust 源码树，两个 crate 可按 README 命令构建，管理页面由 Core 服务正常提供。
2. 从 schema v20 启动会迁移到 v21，旧 Key、额度账本、使用与结算记录保持完整。
3. 删除一个有未用积分、无待结算请求的普通 Key 后，Key 不再可认证，列表隐藏该 Key，剩余积分只返还一次，可分配额增加相同数量，已实扣数不变。
4. 存在 held/unknown/reconcile-required 请求时删除返回冲突，Key 与额度账本保持原状。
5. 未授权删除失败；管理员 Key 不可被普通 Key 删除接口删除；重复删除不产生二次返还。
6. Core 自动化测试与构建通过；测试不调用真实上游，不在 C 盘生成测试数据。
7. README 给出可复现的本地运行和部署流程，公开仓库内容不含秘密或运行数据。
8. Git 变更仅进入 `manderzuo/Trae-core`；MCP 内容仍留在其独立仓库。
