# Rebleach

基于 **Rust + WebRTC + Tauri** 的 Windows 远程控制方案。主控端为带界面的桌面程序（GUI），
被控端是纯托盘 / Windows 服务的无窗口 Agent，两者共用同一套信令协议。

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

### 服务端与管理
- 基于 `axum` 的信令 / 管理服务：设备注册、在线状态、授权（grants）、审计日志、安装包分发（出包中心）。
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
├── server/                 信令 / 管理服务（axum）+ 管理网页
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
cargo run -p rebleach-server --release -- --bind 0.0.0.0:8088
```

管理界面：`http://<host>:8088/admin/devices`。

---

## 配置说明（仓库内不含任何真实基础设施 / 密钥）

- **默认中转地址**：通过构建时环境变量注入，未设置时使用占位地址：

  ```bash
  REBLEACH_DEFAULT_SERVER=wss://your-server.example.com/ws cargo build --release
  ```

- **自动更新地址**：`tauri.conf.json` / `tauri.agent.conf.json` 中的 endpoint 为占位域名，
  部署时改成自己的更新服务；更新签名私钥由构建环境变量 `TAURI_SIGNING_PRIVATE_KEY` 提供，**不入库**。
- **STUN / TURN**：默认使用公共 STUN；生产环境建议自建 coturn，并通过服务端
  `--turn-url` / `--turn-secret` 下发短期凭据。
- **TLS 根证书**：`app/src-tauri/certs/rebleach-root.crt` 与 `client/certs/rebleach-root.crt`
  只是**公开证书**（用于校验证书链），替换成自己的 CA 证书即可。
- **虚拟显示驱动**：usbmmidd 为第三方软件，仓库只保留调用脚本，驱动文件需自行放置，
  见 [virtual-display/README.md](app/src-tauri/resources/virtual-display/README.md)。

---

## 说明

- 仓库不包含任何私钥、令牌、设备编号或真实服务器地址。
- 设备身份密钥在首次运行时由客户端本地生成，仅保存在用户数据目录。

## License

Apache-2.0
