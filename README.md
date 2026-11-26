# Rebleach

基于 **Rust + WebRTC + Tauri** 的 Windows 远程控制方案。主控端为带界面的桌面程序（GUI），
被控端是纯托盘 / Windows 服务的无窗口 Agent，两者共用同一套信令协议；
同时提供命令行自动化（远程命令 / 端口转发 / SSH / Ansible）与服务端一键安装接入。

> 本项目用于**自有设备或已获明确授权的设备**的远程管理。请勿用于未授权的远程控制。

---

## 核心功能

### 远程画面
- **三级采集后端自动降级**：Windows Graphics Capture（WGC）→ DXGI Desktop Duplication（DDA）→ GDI `BitBlt`，
  无 GPU、无 WGC 的老系统也能出画面。
- **GPU 下采样 + H.264 编码**：采集纹理直接在显卡上缩放到目标分辨率再编码，避免 CPU 逐像素拷贝；
  码率与帧率可实时调整。
- **多显示器支持**：主控端下拉可切换采集目标；虚拟屏在列表中带 `💻` 标识。
- **虚拟屏远程增删**：被控端通过 usbmmidd 驱动的 `IOCTL_PLUG` 插 / 拔虚拟显示器，
  插入后自动把新屏紧贴到物理屏右侧，避免桌面坐标空洞导致鼠标「撞墙」跨不过去。

### 远程操作
- **鼠标 / 键盘注入**（Win32 `SendInput`）：DPI 感知的坐标换算，自动叠加「当前采集屏」在虚拟桌面中的原点偏移，
  多屏 / 200% 缩放下点击依然对得上。
- **双向剪贴板同步**，带回环抑制，不会互相反复覆盖。
- **锁屏解锁**：配合 UIAccess 辅助进程，可在锁屏界面完成输入。
- **Ctrl+Alt+Del 等系统组合键**、滚轮、拖拽等常见操作。

### 连接与网络
- **WebRTC 端到端直连优先**：ICE 打洞成功即走 P2P，只有打不通时才走 TURN 中转。
- **多 STUN 候选**：提升对称 NAT 场景下的直连成功率。
- **局域网直连模式**：UDP 广播发现 + 被控端内置迷你信令，同一网段无需公网即可连接，并用局域网验证码鉴权。
- **断线自动重连**、锁屏期间会话保活（不拆会话，解锁后无需重新注册）。

### 终端与命令行自动化
- **`exec`**：远程执行一次性命令，返回被控端真实退出码与纯文本输出（自动切 UTF-8，无 ANSI 残留）。
- **`tunnel`**：把被控端回环 / 私网端口映射到本机 `127.0.0.1`，目标限定在私网，避免被当作公网跳板。
- **`rebleach-proxy`**：OpenSSH `ProxyCommand` 调用的纯协议桥，被控端无需公网 IP、无需开放 22 端口即可 SSH 接入。
- **`setup` / `ssh-config` / `inventory`**：自动生成操作员密钥并向服务端登记公钥、渲染 `~/.ssh/config.d/rebleach.conf`、输出 Ansible 清单。
- **终端录制**：默认录制 asciicast v2，`term-replay` 可纯文本回放，便于检索与审计。

### 接入与一键部署
- **安装令牌**：管理端生成一次性、带有效期的 enrollment token，可随时吊销。
- **一键安装**：被控端执行 `irm "<server>/bootstrap/install.ps1?token=<token>" | iex`，完成下载、设备登记与静默安装。
- **公钥集中下发**：主控端公钥登记到服务端，被控端每 60 秒同步一次 `ssh_authorized_keys`，授权变更无需重装或重启。

### 服务端与管理
- 基于 `axum` 的信令 / 管理服务：设备注册、在线状态、授权（grants）、审计日志、安装包分发（出包中心）。
- **内置管理网页**：`/admin` 下的设备、授权、出包、审计与接入配置页面，Basic Auth 保护。
- **TURN REST 临时凭据**：服务端用 `static-auth-secret` 签发短期 TURN 账号，避免长期密钥下发到客户端。
- 设备身份使用 **Ed25519** 密钥对签名，不依赖可猜测的设备编号。

### 分发与更新
- Tauri 打包两个独立安装包：`Rebleach Controller`（主控）与 `Rebleach Agent`（被控）。
- Agent 安装时自动注册 Windows 服务、创建托盘自启；驱动类组件由 NSIS 钩子以管理员权限完成安装。
- **签名自动更新**：minisign 公私钥 + NSIS passive 安装，更新包签名校验失败即拒绝。

---

## 技术亮点

| 亮点 | 说明 |
|---|---|
| 信令与媒体分离 | H.264 帧不经过 Tauri IPC，而是本地 `127.0.0.1` WebSocket 二进制通道直送前端，省掉 JSON 序列化与 base64 的 CPU 开销 |
| 采集缓冲复用 | 帧池 + 回读缓冲池，每帧复用已分配的 `Vec`，避免 1080p 下每帧约 9 MB 的反复分配 |
| 会话 0 → 用户会话 | 服务以 SYSTEM 身份运行，用 `CreateProcessAsUser` 在登录会话中派生托盘 Agent，既保证开机自启/无窗口常驻，又能操作真实桌面 |
| DPI 正确的输入注入 | 采集端把「编码帧坐标 → 物理坐标」的缩放与屏原点通过 `set_mapping` 下发给注入模块 |
| 显示布局 API | 虚拟屏排布走 `QueryDisplayConfig` + `SetDisplayConfig`（path/mode 数组），而不是会被 IDD 驱动回滚的 `ChangeDisplaySettingsEx` |
| WOW64 安全 | 32 位进程需要 64 位 PowerShell 时走 `Sysnative`，避免 `SysWOW64` 重定向踩坑 |
| 双角色拆分 | 同一个产物支持 `controller`（GUI）与 `agent`（托盘/服务）两种 feature 构建，共享 `client` 协议库 |

---

## 目录结构

```
rebleach-core/
├── client/                 跨平台核心库（会话、WebRTC、采集、注入、协议）+ CLI
│   ├── src/cli.rs          命令行子命令（exec / tunnel / ssh-config / setup ...）
│   ├── src/sshd.rs         内置 SSH 服务端与公钥同步
│   ├── src/bin/            rebleach-client / rebleach-proxy
│   └── examples/           CLI / 局域网端到端测试
├── server/                 信令 / 管理服务（axum）
│   ├── src/main.rs         服务端与公开接入路由（/bootstrap/*、/agent/ssh-keys）
│   └── web/                管理网页（devices / grants / packages / audit / access）
├── app/
│   ├── src-tauri/          Tauri 主控端与被控端 Agent
│   │   ├── src/
│   │   │   ├── controller.rs     主控端会话与媒体通道
│   │   │   ├── agent.rs          被控端会话循环
│   │   │   ├── agent_app.rs      托盘 Agent
│   │   │   ├── service.rs        Windows 服务
│   │   │   ├── child.rs          CreateProcessAsUser 会话派生
│   │   │   ├── virtual_display.rs 虚拟屏增删与自动排布
│   │   │   └── lan.rs            局域网直连
│   │   ├── nsis/                 NSIS 安装钩子
│   │   └── resources/            随包资源（含虚拟屏脚本）
│   ├── ui/                 主控端前端（原生 HTML/JS）
│   ├── ui-agent/           Agent 空窗口
│   └── ui-shim/            UIAccess 辅助进程
├── docs/                   使用说明与部署文档
└── Cargo.toml              workspace（Rust 2024）
```

---

## 构建

前置：Rust 1.80+、WebView2 Runtime、NSIS（Tauri 打包 Windows 安装包时自动下载）。

```bash
# 核心库 / 服务端
cargo build --release

# 主控端安装包
cd app
cargo tauri build

# 被控端安装包（纯托盘 Agent）
cd app
cargo tauri build --config src-tauri/tauri.agent.conf.json \
  -- --no-default-features --features agent
```

### 运行服务端

```bash
cargo run -p rebleach-server --release -- \
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
| `--db` | `./rebleach.db` | SQLite 数据库（首次启动自动建表） |
| `--admin-user` / `--admin-pass` | `admin` / `admin` | 管理端 Basic Auth 账号，**上线前务必修改** |
| `--packages-dir` | `./packages` | 出包中心目录，同时用于 `/bootstrap/agent` 分发 |
| `--public-url` | 空 | 对外地址，用于拼接一键安装命令；留空时按请求头推导 |
| `--turn-url` / `--turn-secret` | 空 | coturn 地址与 `static-auth-secret`，留空则不下发 TURN 凭据 |
| `--turn-ttl` | `600` | 临时 TURN 凭据有效期（秒） |

管理网页（Basic Auth）：

| 页面 | 用途 |
|---|---|
| `/admin/devices` | 设备列表、在线状态 |
| `/admin/grants` | 授权关系（controller ↔ device） |
| `/admin/packages` | 出包中心 |
| `/admin/audit` | 审计日志 |
| `/admin/access` | 接入配置：安装令牌、主控端公钥 |

### 接入被控端（一键安装）

1. 打开 `/admin/access` 生成安装令牌（15 分钟 / 1 小时 / 24 小时），得到一次性 token。
2. 在目标机器上以管理员身份运行 PowerShell：

   ```powershell
   irm "<server>/bootstrap/install.ps1?token=<token>" | iex
   ```

3. 脚本向 `/bootstrap/claim` 换取设备 ID，随后下载并静默安装 Agent。

令牌单次有效、到期自动失效；未使用的令牌可在页面随时删除，已使用的令牌再删除会返回 404。
`--packages-dir` 中需放置文件名同时包含 `rebleach` 与 `agent` 的 `.exe` 安装包，否则 `/bootstrap/agent` 返回 404。

### 命令行 SSH 接入

```powershell
# 首次：生成操作员密钥、登记公钥、生成 ssh config
rebleach-client --server wss://signal.example.com/ws setup --device <controller_id> --user admin --pass <密码>

# 之后授权变化时重新渲染配置
rebleach-client --server wss://signal.example.com/ws ssh-config --controller <controller_id>

# 连接
ssh rbl-<device_id>
```

`setup` 会：

- 在 `%APPDATA%\rebleach\ssh\<controller_id>.key` 生成（或复用）操作员密钥；
- 调用 `POST /api/v1/ssh-keys` 把公钥登记到服务端；
- 按 `GET /api/v1/grants` 中的有效授权渲染 `~/.ssh/config.d/rebleach.conf`，并确保 `~/.ssh/config` 含 `Include ~/.ssh/config.d/*.conf`。

被控端 Agent 每 60 秒从 `/agent/ssh-keys` 同步一次授权公钥到 `ssh_authorized_keys`，
新增授权最迟一分钟后生效，无需重启服务。完整操作见 [使用说明](docs/Rebleach-使用说明.md)。

---

## 配置说明（部署时按需替换）

- **默认中转地址**：通过构建时环境变量注入，未设置时使用占位地址：

  ```bash
  REBLEACH_DEFAULT_SERVER=wss://your-server.example.com/ws cargo build --release
  ```

- **自动更新地址**：`tauri.conf.json` / `tauri.agent.conf.json` 中的 endpoint 指向本项目的更新服务，
  自建部署时请改成自己的地址；更新签名私钥由构建环境变量 `TAURI_SIGNING_PRIVATE_KEY` 提供，**不入库**。
- **STUN / TURN**：默认使用公共 STUN；生产环境建议自建 coturn，并通过服务端
  `--turn-url` / `--turn-secret` 下发短期凭据。
- **TLS 根证书**：`app/src-tauri/certs/rebleach-root.crt` 与 `client/certs/rebleach-root.crt`
  只是**公开证书**（用于校验证书链），替换成自己的 CA 证书即可。
- **虚拟显示驱动**：usbmmidd 为第三方软件，仓库只保留调用脚本，驱动文件需自行放置，
  见 [virtual-display/README.md](app/src-tauri/resources/virtual-display/README.md)。

---

## 说明

- 仓库不包含私钥、令牌或设备编号；文档中的地址均为示例，实际部署按自己的环境替换。
- 设备身份密钥在首次运行时由客户端本地生成，仅保存在用户数据目录。

## 文档

- [使用说明（GUI / CLI / SSH / 服务端接入）](docs/Rebleach-使用说明.md)

## License

本项目采用 [PolyForm Strict License 1.0.0](LICENSE)：源码公开可见，但**不是 OSI 开源许可**。

| 行为 | 是否允许 |
|---|---|
| 个人学习、研究、测试、非商业自用 | ✅ 允许 |
| 非营利组织、教育 / 科研 / 政府机构内部使用 | ✅ 允许 |
| 商业使用、企业内部生产使用 | ❌ 需书面授权 |
| 复制、分发、再发布 | ❌ 需书面授权 |
| 修改、二次开发、基于本项目创作衍生作品 | ❌ 需书面授权 |

如需商业授权、二次开发或分发，请通过 GitHub [@zhema7](https://github.com/zhema7) 联系作者取得书面许可。

第三方组件（如 `app/ui/vendor/xterm`、usbmmidd 相关脚本与驱动）仍遵循其各自许可，不受本项目许可影响。

Copyright (c) 2026 zhema7. 未明确授予的权利均予保留。
