# Rebleach 使用说明

版本：v0.5（T1–T4 + 服务端接入配置）
适用系统：Windows 10 / 11 x64
配套文档：《Rebleach 安装说明》（安装、托盘、PIN 等基础操作以该文档为准）

---

## 一、这份说明讲什么

部署完成后，Rebleach 有三种用法：

| 用法 | 适用场景 | 入口 |
|---|---|---|
| 图形界面接管 | 人工远程操作、看屏幕、传文件 | Rebleach Controller 窗口 |
| 命令行自动化 | 批量执行命令、端口转发、SSH 接入、Ansible | `rebleach-client.exe` / `rebleach-proxy.exe` |
| 服务端接入 | 批量部署被控端、集中管理授权 | 管理网页 `/admin` |

图形界面与命令行可以同时使用，互不影响。命令行不会建立屏幕通道，被控端也不会启动屏幕采集。

---

## 二、图形界面：五分钟上手

### 2.1 被控端准备

1. 确认托盘图标存在，右键可看到「复制 ID」「复制 PIN」。
2. 把「ID」发给操作者；PIN 按提示口头/另行告知。
3. 首次运行若弹出防火墙提示，勾选**专用网络**并允许（仅影响局域网直连，走中转不受影响）。

### 2.2 主控端连接

1. 打开 Rebleach Controller。
2. 「远程连接」填入被控端 ID → 点「连接」。
3. 按提示输入 PIN。
4. 连接成功后即可看到屏幕；顶部工具栏提供：
   - 键鼠控制开关
   - 多显示器切换
   - 文件传输（双向拖拽、断点续传）
   - 语音对讲 / 文字消息
   - 终端标签页（等同本机命令行）

### 2.3 局域网直连（可选）

同一局域网时延迟更低：填被控端内网 IP + 6 位验证码，或点「搜索设备」自动发现。

### 2.4 「接入与 SSH」卡片

主控端设置页中的「接入与 SSH」用于对接自建服务端：

1. 填写服务器地址（如 `wss://signal.example.com/ws`），点「保存凭据」可一并保存管理账号与密码。
2. 填写主控端设备 ID，点「生成公钥」，再点「登记公钥」把公钥上传到服务端。
3. 点「生成 SSH 配置」，按服务端当前授权重建 `~/.ssh/config.d/rebleach.conf`。
4. 需要新增被控端时，选择有效期（15 分钟 / 1 小时 / 24 小时）生成安装令牌，复制一键安装命令。

> 管理账号密码以明文保存在 `%APPDATA%\rebleach-controller\config.json`，仅建议在受控的个人设备上使用。

---

## 三、命令行：通用约定

### 3.1 可执行文件

安装目录下有两个程序：

| 程序 | 用途 |
|---|---|
| `rebleach-client.exe` | 主程序：图形界面 + 全部子命令 |
| `rebleach-proxy.exe` | 供 `ssh` 的 ProxyCommand 调用的协议桥 |

建议把安装目录加入 `PATH`，否则 `ssh-config` 生成的 ProxyCommand 会找不到 `rebleach-proxy`。

### 3.2 全局参数

子命令前可加以下参数：

| 参数 | 默认值 | 说明 |
|---|---|---|
| `--server <url>` | `ws://127.0.0.1:8088/ws` | 信令服务器地址，生产环境请显式指定 |
| `--key-file <path>` | `%APPDATA%\rebleach\identity.json` | 设备身份密钥（TOFU 用） |
| `--auth <code>` | 无 | 局域网直连验证码 |
| `--stun <url>` | 内置默认 | 可重复或逗号分隔 |
| `--turn <url>` | 内置默认 | 格式 `turn:user:pass@host:port` |

示例：

```powershell
rebleach-client --server wss://signal.example.com/ws exec dev-0001 -- "hostname"
```

### 3.3 退出码

| 命令 | 退出码含义 |
|---|---|
| `exec` | **被控端命令的真实退出码**（0 成功，非 0 原样返回） |
| `tunnel` / `proxy` | 链路异常或事件流结束时为 1 |
| `ssh-config` / `inventory` / `keygen` / `setup` / `term-replay` | 本地执行成功为 0，参数、网络或 IO 错误为非 0 |

---

## 四、服务端部署与接入配置

### 4.1 启动服务端

```bash
rebleach-server \
  --addr 0.0.0.0:8088 \
  --db ./rebleach.db \
  --admin-user admin --admin-pass <管理员密码> \
  --packages-dir ./packages \
  --public-url https://signal.example.com \
  --turn-url turn:signal.example.com:3478 \
  --turn-secret <coturn static-auth-secret>
```

| 参数 | 默认值 | 说明 |
|---|---|---|
| `--addr` | `0.0.0.0:8088` | 监听地址 |
| `--db` | `./rebleach.db` | SQLite 数据库，首次启动自动建表 |
| `--admin-user` / `--admin-pass` | `admin` / `admin` | 管理端 Basic Auth 账号，**上线前务必修改** |
| `--packages-dir` | `./packages` | 出包中心目录，同时用于一键安装分发 Agent |
| `--public-url` | 空 | 对外地址，用于拼接一键安装命令；留空按请求头推导 |
| `--turn-url` / `--turn-secret` | 空 | coturn 地址与 `static-auth-secret`；留空则不下发 TURN 凭据 |
| `--turn-ttl` | `600` | 临时 TURN 凭据有效期（秒） |

反向代理后运行时，`--public-url` 建议显式填写，否则一键安装命令可能拼出内网地址。

### 4.2 管理网页

访问 `http://<host>:8088/admin`，浏览器会弹出 Basic Auth，输入 `--admin-user` / `--admin-pass`。

| 页面 | 用途 |
|---|---|
| `/admin/devices` | 设备列表、在线状态 |
| `/admin/grants` | 授权关系（主控端 ↔ 被控端） |
| `/admin/packages` | 出包中心，可上传 / 下载安装包 |
| `/admin/audit` | 审计日志 |
| `/admin/access` | 接入配置：安装令牌、主控端公钥 |

### 4.3 授权与公钥

1. 在 `/admin/devices` 确认主控端与被控端都已注册。
2. 在 `/admin/grants` 建立「主控端 → 被控端」的有效授权。
3. 在 `/admin/access` 登记主控端公钥（也可以由客户端 `setup` 自动登记）。
4. 被控端 Agent 每 60 秒拉取一次有效授权对应的公钥，写入本地 `ssh_authorized_keys`。

撤权时删除对应 grant 或公钥即可，被控端最迟一分钟后同步移除。

---

## 五、被控端一键安装

### 5.1 生成安装令牌

在 `/admin/access` 选择有效期（15 分钟 / 1 小时 / 24 小时）生成令牌：

- 令牌一次性使用，安装成功后立即失效；
- 未使用的令牌可随时删除，已使用的令牌再删除返回 404；
- 令牌泄露时请直接吊销，重新生成。

### 5.2 执行安装

在目标机器上以**管理员身份**打开 PowerShell：

```powershell
irm "<server>/bootstrap/install.ps1?token=<token>" | iex
```

脚本会：

1. 调用 `POST /bootstrap/claim` 用令牌换取设备 ID；
2. 从 `/bootstrap/agent` 下载 Agent 安装包；
3. 静默安装并注册 Windows 服务，随后自动上线。

> `--packages-dir` 中需放置文件名同时包含 `rebleach` 与 `agent` 的 `.exe` 安装包，否则 `/bootstrap/agent` 返回 404。

### 5.3 验证

安装完成后回到 `/admin/devices`，应能看到新设备及在线状态。若未出现，检查：

- 目标机器能否访问 `--public-url` 对应地址；
- 安装令牌是否已过期或被使用；
- `RebleachAgent` 服务是否启动。

---

## 六、exec：远程执行一次性命令

### 6.1 基本用法

```powershell
rebleach-client exec <设备ID> -- "<命令>"
```

示例：

```powershell
# 查看系统信息
rebleach-client exec dev-0001 -- "systeminfo"

# 执行 PowerShell
rebleach-client exec dev-0001 -- "powershell -NoProfile -Command Get-Process | Sort-Object CPU -Descending | Select-Object -First 10"

# 带超时（秒），超时直接报错退出
rebleach-client exec dev-0001 --timeout 60 -- "ping -n 10 127.0.0.1"

# JSON 输出，便于脚本解析
rebleach-client exec dev-0001 --json -- "whoami"
```

### 6.2 行为说明

- 采用**管道模式**执行，不创建窗口，不会在被控端弹出黑框。
- `stdout` 与 `stderr` 合并为同一路输出返回。
- 命令前缀自动设置 UTF-8 代码页（`chcp 65001`），输出为纯文本，**不含 ANSI 转义序列**。
- 后台常驻进程（如 `start /b`）不会被等待；`exec` 只等待命令本身结束。
- **不加 `--timeout` 时会一直等待**，自动化脚本里建议始终显式设置。
- `--json` 输出字段：`exit_code`、`output`、`duration_ms`。

### 6.3 在脚本里判断结果

```powershell
rebleach-client exec dev-0001 --timeout 30 -- "sc query RebleachAgent"
if ($LASTEXITCODE -ne 0) {
    Write-Host "服务异常，退出码 $LASTEXITCODE"
}
```

---

## 七、tunnel：端口转发

### 7.1 基本用法

```powershell
rebleach-client tunnel <设备ID> --local <本地端口>:<目标>
```

目标有两种写法：

| 写法 | 含义 |
|---|---|
| `127.0.0.1:3306` | 被控端本机端口 |
| `10.0.0.8:80` | 被控端所在内网的其他机器（**必须是 IP**） |
| `ssh` | 被控端内置 SSH 服务端（推荐） |

示例：

```powershell
# 把被控端的 MySQL 映射到本机 13306
rebleach-client tunnel dev-0001 --local 13306:127.0.0.1:3306

# 多条规则可以同时开
rebleach-client tunnel dev-0001 `
  --local 13306:127.0.0.1:3306 `
  --local 18080:10.0.0.8:80 `
  --local 2222:ssh
```

连接本机端口即可：

```powershell
mysql -h 127.0.0.1 -P 13306 -u root -p
ssh -p 2222 Administrator@127.0.0.1
```

### 7.2 行为说明

- 本地监听地址固定为 **127.0.0.1**，只允许本机访问，不会暴露到局域网。
- 每条规则一个独立通道，同时支持多个连接（每个连接分配独立 `cid`）。
- **目标限制**：只允许回环地址和私网 IP（10/8、172.16/12、192.168/16、127/8、169.254/16、IPv6 私网段）。域名和公网 IP 会被拒绝，这是为了防止被当作公网跳板。
- 需要转发域名时，请先在被控端解析出 IP 再填写。
- 按 `Ctrl+C` 结束，所有本地监听随之关闭。

---

## 八、SSH 接入

### 8.1 原理

`rebleach-proxy` 把本机 `ssh` 的 stdin/stdout 当作一条 TCP 连接，经 tunnel 通道送到被控端：

```
ssh → ProxyCommand(rebleach-proxy) → 信令 → tunnel 通道 → 被控端
                                                          ├─ rblssh：内置 SSH 服务端
                                                          └─ 127.0.0.1:22：本机 sshd
```

不需要被控端有公网 IP，也不需要开放 22 端口。授权统一由服务端管理，主控端不再逐台拷贝公钥。

### 8.2 一键初始化（推荐）

```powershell
rebleach-client --server wss://signal.example.com/ws setup `
  --device <主控端设备ID> --user admin --pass <管理员密码>
```

`setup` 会依次完成：

1. 在 `%APPDATA%\rebleach\ssh\<主控端ID>.key` 生成（或复用）操作员密钥，Windows 上自动收紧 ACL；
2. 调用 `POST /api/v1/ssh-keys` 把公钥登记到服务端；
3. 读取 `GET /api/v1/grants` 中当前有效的授权，渲染 `~/.ssh/config.d/rebleach.conf`；
4. 检查 `~/.ssh/config` 是否包含 `Include ~/.ssh/config.d/*.conf`，没有则提示。

密码也可以放在环境变量里，避免留在命令历史：

```powershell
$env:RBLEACH_PASS = "<管理员密码>"
rebleach-client --server wss://signal.example.com/ws setup --device <主控端设备ID>
```

只想看配置不写文件：追加 `--print`；指定输出位置：`--out <path>`。

### 8.3 手动生成密钥与登记

```powershell
# 1. 生成密钥（也可直接从「接入与 SSH」卡片复制公钥）
rebleach-client keygen --device dev-0001

# 2. 把打印出的公钥贴到 /admin/access 页面登记（或调用 POST /api/v1/ssh-keys）
```

- 私钥默认写到 `%APPDATA%\rebleach\ssh\dev-0001.key`。
- 公钥一行一把，登记时关联到主控端设备 ID。
- 如果本机就是被控端，可以用 `keygen --device dev-0001 --add` 直接写入本地授权文件。

> 内置 SSH 服务端只接受 publickey 认证，不支持密码登录。

### 8.4 公钥同步

被控端每 60 秒从 `GET /agent/ssh-keys` 同步一次授权公钥：

| 运行形态 | 写入路径 |
|---|---|
| Agent 安装版（Windows 服务） | `%ProgramData%\Rebleach\ssh_authorized_keys` |
| 独立 CLI / 手动运行 | `%APPDATA%\rebleach\ssh_authorized_keys` |

新增授权最迟一分钟后生效，无需重启服务；删除授权同样最迟一分钟后移除。

### 8.5 生成 ssh config

```powershell
rebleach-client ssh-config --controller <主控端设备ID>
```

- `--controller` 会写入 ProxyCommand 的 `--id`，保证授权校验使用的是主控端身份。
- 默认按服务端当前授权自动收集设备；也可显式指定 `--device dev-0001 --device dev-0002`。
- 生成内容形如：

```
Host rbl-dev-0001
    HostName rblssh
    Port 22
    User Administrator
    ProxyCommand "rebleach-proxy" --server wss://signal.example.com/ws --id <主控端ID> --device dev-0001 rblssh
    StrictHostKeyChecking accept-new
```

然后直接：

```powershell
ssh rbl-dev-0001
```

如果被控端已经装了标准 sshd，改用本机 sshd 后端：

```powershell
rebleach-client ssh-config --controller <主控端ID> --backend sshd
```

---

## 九、inventory：生成 Ansible 清单

```powershell
$env:RBLEACH_PASS = "你的管理密码"
rebleach-client inventory --user admin --only-online
```

- 从信令服务器 `GET /api/v1/devices` 拉设备列表，只收录角色为 `controlled` 的在线设备。
- 默认输出 `%APPDATA%\rebleach\inventory.ini`，每台主机一条 `ProxyCommand`。
- `--print` 只打印不写文件；`--pass` 也可直接传参（不推荐，会留在命令历史里）。

生成后即可：

```powershell
ansible -i $env:APPDATA\rebleach\inventory.ini rebleach -m ping
ansible -i $env:APPDATA\rebleach\inventory.ini rebleach -m win_command -a "whoami"
```

依赖：控制机上的 `ssh` 能通过 `rebleach-proxy` 连到被控端（见第八节），且对应公钥已授权。

---

## 十、proxy：单独使用

`rebleach-proxy` 是一个纯协议桥，只做三件事：连信令、开 tunnel、把 stdin/stdout 与远端字节流对接。

```powershell
rebleach-proxy --server wss://signal.example.com/ws --device dev-0001 --stdio rblssh
```

| 参数 | 说明 |
|---|---|
| `--server` | 信令地址 |
| `--device` | 目标被控端 ID |
| `--id` | 本机在信令上的设备 ID（建议填主控端 ID） |
| `--auth` | 局域网直连验证码（可选） |
| `<target>` | 位置参数，默认 `rblssh`；也可填 `127.0.0.1:22` |

注意：**stdout 是纯协议数据**，所有日志都走 stderr，因此可以安全地被 `ssh`、`nc`、数据库客户端等程序调用。

---

## 十一、终端录制与回放

### 11.1 自动录制

主控端与终端交互时默认录制，目录：

```
%APPDATA%\rebleach\recordings\<会话ID>\term-<标签页>.cast
```

格式为 asciicast v2（JSONL），头部带关联信息：

```json
{"version":2,"width":120,"height":30,"vars":{"rebleach_session":"...","rebleach_device":"dev-0001","rebleach_sid":"1"}}
```

之后每行是一条事件：`[秒, "o", 输出文本]`。

关闭录制（启动主控端前设置）：

```powershell
$env:RBLEACH_TERM_RECORD = "0"
```

### 11.2 回放

```powershell
# 纯文本回放（适合检索、审计）
rebleach-client term-replay "$env:APPDATA\rebleach\recordings\sess-xxxx\term-1.cast"

# 带时间轴回放
asciinema play "$env:APPDATA\rebleach\recordings\sess-xxxx\term-1.cast"
```

---

## 十二、网络与安全

### 12.1 连接路径

1. 优先 **P2P 直连**（延迟最低，不消耗中继）。
2. 打洞失败自动切换 **TURN 中继**，对使用者透明。
3. 中继只负责转发加密后的字节流，服务器不持有设备私钥。

### 12.2 端口清单

| 端口 | 方向 | 用途 |
|---|---|---|
| TCP 7681 | 被控端监听 | 局域网直连信令 |
| UDP 7682 | 被控端监听 | 局域网设备发现 |
| TCP 8088 | 服务器 | 信令 WebSocket + 管理网页 + 接入 API |
| UDP 3478 / TCP 3478 | 服务器 | TURN |
| UDP 49160–49360 | 服务器 | TURN relay 端口段 |

跨网使用只需能访问服务器的信令与 TURN 端口，被控端无需任何入站端口。

### 12.3 凭据与令牌

- **管理端 Basic Auth**：默认 `admin/admin`，上线前必须修改；建议只在内网或 VPN 内暴露 `/admin`。
- **安装令牌**：一次性、带有效期，只用于换取设备身份；泄露后立即删除并重新生成。
- **TURN 凭据**：仅短期有效（默认 600 秒），由服务端按需签发，客户端不保存长期密钥。

### 12.4 TOFU 信任

首次连接时，对端公钥会被固定到 `%APPDATA%\rebleach\known_peers.json`。之后如果对端公钥变化，连接会被拒绝（提示 `pubkey_mismatch`）。

- 正常重装系统、更换设备时，需要删除对应记录后重连。
- **不要随意删除该文件**，它用于防止中间人替换设备身份。

---

## 十三、常见问题

| 现象 | 排查 |
|---|---|
| 连接一直停在「等待对端」 | 被控端是否在线（托盘图标 / `services.msc` 中 `RebleachAgent`）；`--server` 是否指向正确服务器 |
| 提示 `pubkey_mismatch` | 对端重装或换了身份密钥；确认无误后删除 `known_peers.json` 中对应条目 |
| 管理网页 / API 返回 401 | Basic Auth 账号密码错误；确认与 `--admin-user` / `--admin-pass` 一致 |
| 一键安装报令牌无效 | 令牌过期或已被使用（一次性）；回 `/admin/access` 重新生成 |
| 一键安装下载 404 | `--packages-dir` 中没有文件名包含 `rebleach` 与 `agent` 的 `.exe` |
| 登记公钥后 ssh 仍 Permission denied | 被控端需等待最长 60 秒同步；确认 grant 有效、公钥关联的主控端 ID 正确 |
| `exec` 中文乱码 | 命令前缀已设 UTF-8；若目标程序强制输出 GBK，请在命令里自行转码，例如 PowerShell 中设置 `[Console]::OutputEncoding` |
| `tunnel` 报「目标应为 host:port 或 ssh」 | 检查 `--local` 格式，例如 `13306:127.0.0.1:3306` |
| `tunnel` 报目标被拒绝 | 只允许回环/私网 IP；域名和公网 IP 需先解析为内网 IP |
| `ssh` 提示 `ProxyCommand` 找不到 | 把 Rebleach 安装目录加入 `PATH`，或 `--proxy-bin` 指定完整路径 |
| `inventory` 返回空列表 | 设备需要处于在线状态；确认 `--user/--pass` 与服务器管理账号一致 |
| 长会话约 10 分钟后中继断开 | 服务端 TURN 临时凭据 TTL 偏短，将 `--turn-ttl` 调大（建议 43200） |
| 找不到录制文件 | 确认未设 `RBLEACH_TERM_RECORD=0`；检查 `%APPDATA%\rebleach\recordings` |

---

## 十四、命令速查

```powershell
# 主控端一键初始化（登记公钥 + 生成 ssh config）
rebleach-client --server <url> setup --device <controller_id> --user admin --pass <password>

# 远程执行
rebleach-client exec <device> -- "<command>"
rebleach-client exec <device> --json --timeout 60 -- "<command>"

# 端口转发（回环/私网 IP，或 ssh）
rebleach-client tunnel <device> --local 13306:127.0.0.1:3306
rebleach-client tunnel <device> --local 2222:ssh

# SSH 接入
rebleach-client keygen --device <device> [--add]
rebleach-client ssh-config --controller <controller_id> [--backend builtin|sshd] [--user <u>] [--print]

# Ansible
$env:RBLEACH_PASS = "<password>"
rebleach-client inventory --user admin --only-online [--print]

# 录制回放
rebleach-client term-replay <path>.cast

# 协议桥（一般由 ssh 自动调用）
rebleach-proxy --server <url> --id <controller_id> --device <device> --stdio rblssh
```

---

*本文与 v0.5 代码行为保持一致；后续版本新增参数会同步更新。*
