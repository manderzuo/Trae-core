# FRP + Nginx 部署模板

## 适用拓扑

```text
AI Work / Core 主机（Core: 127.0.0.1:7865，AI Work: 127.0.0.1:7864）
        │ frpc 主动出站
        ▼
共同可达的局域网中转机或腾讯云（frps:7000）
        │
        └── Nginx（80/443） → 127.0.0.1:17865 → Core
```

局域网模式下，中转机必须能被相关路由器网段访问；公网模式下，中转机是腾讯云。生产公网入口应转发 Core，不应直接把 AI Work 暴露给外网。FRP 转发的是 HTTP API，不是完整二层局域网。
AI Work 只作为 Core 的内部执行端，Core 管理用户 Key、作用域、并发和积分账本。

## 部署顺序

1. 在中转机安装与 frpc 同版本的 frps，复制 `frps.toml.example`，生成高强度 token。
2. 在中转机启动 frps，并在防火墙放行控制端口 `7000`；业务端口 `17865` 只允许本机 Nginx 访问。
3. 在 Core 主机安装 frpc，复制 `frpc.starlink-core.toml.example`，填写中转机地址和 FRP token；Core 保持监听 `127.0.0.1:7865`。
4. 在中转机安装 Nginx，复制 `nginx.starlink-core.conf.example`，完成 `api.gemstory.cn` DNS 和 HTTPS 证书后启用 443 配置。
5. 启动顺序：AI Work `7864` → Core `7865` → Core 专用 frpc → Nginx；Core 管理页中把 AI Work Base URL 填为同机的 `http://127.0.0.1:7864`。
6. 从外网请求 `https://api.gemstory.cn/healthz`，必须返回 JSON `status=ok`；再用普通 API Key 请求 `GET https://api.gemstory.cn/v1/models`。
7. 外部客户端统一填写 Base URL `https://api.gemstory.cn/v1`，使用 Core 生成的普通 API Key；不要使用 AI Work 桥接 Key 或管理员凭证。

## 两套 FRP 配置的区别

- `frpc.starlink-core.toml.example`：公网生产入口使用，将 Core `7865` 映射到 `17865`。
- `frpc.toml.example`：兼容旧的 AI Work 直连/局域网场景，不应作为新的公网用户入口。
- `nginx.starlink-core.conf.example`：新的公网 HTTPS 入口，域名建议使用 `api.gemstory.cn`。
- `nginx.aiwork.conf.example`：旧 AI Work 直连模板，仅在明确需要旧链路时使用。

如果 `api.gemstory.cn` 尚未解析，不要把 `www.gemstory.cn` 当作 Core 地址。先完成 DNS、FRP 和 Nginx 切换，再发放公网普通 Key。

## 安全要求

- frps/frpc token 只保存在本机私有配置，不提交 Git，不发到聊天中。
- 公网环境只开放 SSH 和 Nginx 443；不要开放 AI Work 7864 或 FRP 业务端口给公网。
- LAN 也必须启用 AI Work API Key；FRP token 只保护隧道，不能替代应用鉴权。
- 参考图/视频通过 `/v1/assets` 上传；要让 Trae 云端回取，`AIWORK_ASSET_PUBLIC_BASE_URL` 必须是 Trae 可访问的 HTTPS 地址。
- `/health`、`/healthz` 不包含账号数和积分；Core 的详细额度和 Key 管理只通过 `/admin` 会话完成。
- 生产素材基址默认拒绝 HTTP；只有隔离测试显式设置 `AIWORK_ALLOW_INSECURE_ASSET_BASE=true` 才可例外。

## 无积分验收

```text
GET /health
GET /v1/models（携带 API Key）
POST /v1/assets（上传一张小 PNG）
GET /v1/assets/<id>/content（同一个 API Key）
```

先完成上述链路，再进行用户明确授权的最低成本 Seedance 测试。
