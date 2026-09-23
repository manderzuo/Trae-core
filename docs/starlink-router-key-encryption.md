# 星链维度分流系统：普通 API Key 加密副本

Core 只保存普通 API Key 的哈希用于认证；为支持管理员在登录后再次复制 Key，数据库另存 AES-256-GCM 加密副本。解密密钥必须与数据库分开保存。数据库备份单独泄露时，不足以恢复普通 Key。

## Windows 初始化与启动

在部署机上以管理员身份打开 PowerShell，在仓库目录运行一次：

```powershell
.\scripts\new-starlink-router-key-encryption-env.ps1
```

脚本默认在仓库/安装根目录的 `secrets\key-encryption.env` 创建随机 32 字节密钥文件，并限制目录和文件 ACL；不会显示密钥，也拒绝覆盖已有文件。数据根目录默认为 `data`，数据库实际保存在 `<DataDir>\data\core.sqlite3`。可通过 `-EnvironmentFile` 和 `-DataDir` 指定其他绝对路径；密钥文件必须位于 Core 数据目录之外。若 Core 服务使用专用 Windows 账户，使用 `-ServiceAccount '机器名\账户名'`，并确保该账户与启动脚本中的运行账户一致。

之后用启动脚本运行 Core：

```powershell
.\scripts\start-starlink-core.ps1
```

启动脚本会读取、校验环境文件中的活动密钥及可选旧版本密钥，并在启动进程前注入环境变量。密钥文件不存在或格式错误时会停止启动，不会临时生成新密钥。备份环境文件应使用与数据库备份同等级或更严格的访问控制；不要提交到 Git、放入发布压缩包、写入日志或发送给用户。

## Linux / systemd

systemd 单元读取 `/etc/starlink-dimension-router/key-encryption.env`。在服务器上生成并严格限制权限：

```sh
sudo install -d -o root -g root -m 0700 /etc/starlink-dimension-router
sudo sh -c 'umask 077; printf "STARLINK_ROUTER_KEY_ENCRYPTION_KEY=%s\\nSTARLINK_ROUTER_KEY_ENCRYPTION_KEY_VERSION=1\\nSTARLINK_ROUTER_KEY_ENCRYPTION_PREVIOUS_KEYS={}\\n" "$(openssl rand -base64 32)" > /etc/starlink-dimension-router/key-encryption.env'
sudo chown root:root /etc/starlink-dimension-router/key-encryption.env
sudo chmod 0600 /etc/starlink-dimension-router/key-encryption.env
```

该文件必须持久保留在服务器本机的安全位置；systemd 服务启动时若文件缺失将失败关闭。不要把实际密钥粘贴到聊天、终端截图或版本库。

## 密钥版本轮换

轮换时不要直接替换旧密钥：旧密钥仍用于解密现有数据库中的 Key 副本。

1. 生成新 32 字节密钥，增加活动版本号，并暂时将旧版本映射写入 `STARLINK_ROUTER_KEY_ENCRYPTION_PREVIOUS_KEYS`，格式为 JSON 对象，例如 `{"1":"<旧密钥的Base64>"}`。新旧版本号必须不同。
2. 安全更新环境文件并重启服务。此时活动版本使用新密钥，旧副本仍可解密。
3. 使用已登录的 Core 管理员会话调用 `POST /admin/v1/key-vault/rewrap`。接口在事务中把全部加密副本重新封装到活动版本；若任何一条无法解密，整批回滚。
4. 确认接口返回的重封装数量与需保留的 Key 数量相符后，从环境文件移除旧密钥映射并再次重启。
5. 在移除旧版本之前，保留受控的密钥备份和数据库备份。丢失活动密钥且无旧版本副本时，已加密保存的 Key 无法恢复，只能重新生成。

`POST /admin/v1/api-keys/{key_id}/copy` 仅允许已登录管理员访问；复制响应禁止缓存。API Key 列表、用量趋势、审计记录和错误日志不得包含明文 Key。
