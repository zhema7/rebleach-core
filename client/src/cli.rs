//! 无界面 CLI 子命令实现：
//! - `exec`：远程一次性命令（T2，干净输出 + 真实退出码）
//! - `tunnel`：本地端口 → 被控端服务的 TCP 隧道（T2）
//! - `ssh-config`：生成 ~/.ssh/config.d/rebleach.conf（T3，标准 ssh 经 ProxyCommand 接入）
//! - `inventory`：从信令服务器拉设备清单，生成 Ansible inventory（T2）
//! - `keygen`：生成 ed25519 SSH 密钥对（T3，内置 SSH 服务端授权用）
//! - `proxy`：ProxyCommand stdio 工具（T3，见 bin/proxy.rs 调用 proxy_stdio）
//! - `term-replay`：asciicast 录制回放为纯文本（T4，校验/检索用）

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::headless::{HeadlessConfig, LinkEvent};
use crate::term::TermMsg;
use crate::tunnel::TunnelMsg;

/// CLI 通用环境：server/key 文件/ICE 列表
#[derive(Debug, Clone)]
pub struct CliEnv {
    pub server: String,
    pub id: String,
    pub key_file: Option<PathBuf>,
    pub ice_servers: Vec<String>,
}

impl CliEnv {
    pub fn new(
        server: Option<String>,
        id: Option<String>,
        key_file: Option<PathBuf>,
        ice_servers: Vec<String>,
    ) -> Self {
        let server = server
            .or_else(|| std::env::var("RBLEACH_SERVER").ok())
            .unwrap_or_else(|| "ws://127.0.0.1:8088/ws".into());
        let id = id.unwrap_or_else(|| {
            // 默认设备 ID：主机名（与 Agent/管理端展示一致）
            identity_hostname().unwrap_or_else(|| "cli-host".into())
        });
        Self {
            server,
            id,
            key_file,
            ice_servers,
        }
    }

    fn link(&self, peer: &str, auth: Option<String>) -> HeadlessConfig {
        HeadlessConfig {
            server: self.server.clone(),
            id: self.id.clone(),
            peer: peer.to_string(),
            ice_servers: self.ice_servers.clone(),
            key_file: self.key_file.clone(),
            auth,
        }
    }
}

fn identity_hostname() -> Option<String> {
    crate::identity::hostname()
}

fn stdout_flush() {
    let _ = std::io::stdout().flush();
}

// ============================================================
// exec：远程一次性命令
// ============================================================

/// 执行结果（--json 输出用）
#[derive(serde::Serialize)]
pub struct ExecResult {
    pub exit_code: i32,
    pub output: String,
    pub duration_ms: u64,
}

pub async fn exec(
    env: CliEnv,
    device: &str,
    command: &str,
    json: bool,
    timeout_secs: Option<u64>,
    auth: Option<String>,
) -> Result<i32> {
    if command.trim().is_empty() {
        bail!("命令为空：rebleach-client exec <device> -- <command>");
    }
    let start = Instant::now();
    let (link, mut events) = crate::headless::connect(env.link(device, auth), false).await?;
    info!("🚀 exec -> {device}: {command}");

    let sid = 1u16;
    link.send_term(TermMsg::Open {
        sid,
        shell: format!("exec:{command}"),
        cols: 120,
        rows: 30,
        cwd: None,
    })
    .await?;

    let mut out = String::new();
    let mut exit_code: Option<i32> = None;
    let mut err_msg: Option<String> = None;
    let deadline = timeout_secs.map(|t| Instant::now() + Duration::from_secs(t));

    while exit_code.is_none() {
        let ev = tokio::time::timeout(Duration::from_millis(500), events.recv()).await;
        match ev {
            Ok(Some(LinkEvent::Term(TermMsg::Data { sid: s, data }))) if s == sid => {
                let text = String::from_utf8_lossy(&data).into_owned();
                if json {
                    out.push_str(&text);
                } else {
                    print!("{text}");
                    stdout_flush();
                }
            }
            Ok(Some(LinkEvent::Term(TermMsg::Exit { sid: s, code }))) if s == sid => {
                exit_code = Some(code);
            }
            Ok(Some(LinkEvent::Term(TermMsg::Err { sid: s, msg }))) if s == sid => {
                err_msg = Some(msg);
                break;
            }
            Ok(Some(LinkEvent::Status(st))) if st == "term-open" => {}
            Ok(Some(LinkEvent::Status(st))) => {
                bail!("链路中断: {st}");
            }
            Ok(Some(_)) => {}
            Ok(None) => bail!("链路关闭"),
            Err(_) => {
                // 500ms 无消息：检查超时
                if let Some(d) = &deadline
                    && Instant::now() > *d
                {
                    let _ = link.close().await;
                    bail!("执行超时（{}s）", timeout_secs.unwrap_or(0));
                }
            }
        }
    }

    let code = exit_code.unwrap_or(-1);
    let duration_ms = start.elapsed().as_millis() as u64;
    let _ = link.close().await;

    if json {
        let r = ExecResult {
            exit_code: code,
            output: out,
            duration_ms,
        };
        println!("{}", serde_json::to_string(&r)?);
    } else if let Some(m) = err_msg {
        eprintln!("\n[rebleach] 命令执行失败: {m}");
        return Ok(if code == 0 { 1 } else { code });
    } else if !json {
        eprintln!("\n[rebleach] 退出码: {code}，耗时 {duration_ms}ms");
    }
    Ok(code)
}

// ============================================================
// tunnel：本地端口 → 被控端目标服务
// ============================================================

/// 一条隧道规则：本地端口 → 被控端目标（host:port 或 ssh）
#[derive(Debug, Clone)]
pub struct TunnelSpec {
    pub local_port: u16,
    pub target: String,
}

/// 解析 `13306:127.0.0.1:3306` / `13306:ssh` 形式的规则
pub fn parse_tunnel_spec(s: &str) -> Result<TunnelSpec> {
    let (local, target) = s
        .split_once(':')
        .with_context(|| format!("隧道规则格式应为 本地端口:目标，收到: {s}"))?;
    let local_port: u16 = local
        .parse()
        .with_context(|| format!("本地端口非法: {local}"))?;
    if target != "ssh" && target.rsplit_once(':').is_none() {
        bail!("目标应为 host:port 或 ssh，收到: {target}");
    }
    Ok(TunnelSpec { local_port, target: target.to_string() })
}

struct TunnelState {
    /// cid → 本地 socket 写半边
    conns: std::collections::HashMap<u16, mpsc::Sender<Vec<u8>>>,
    next_cid: u16,
}

pub async fn tunnel(
    env: CliEnv,
    device: &str,
    specs: Vec<TunnelSpec>,
    auth: Option<String>,
) -> Result<()> {
    if specs.is_empty() {
        bail!("至少一条 --local 规则，如 --local 13306:127.0.0.1:3306");
    }
    // connect 直接返回分离的事件流（Receiver 需要 &mut），link 进 Arc 共享
    let (link, mut events) = crate::headless::connect(env.link(device, auth), true).await?;
    let link = Arc::new(link);
    info!(
        "🔌 隧道会话 {} -> {device}（审计 ID {}）",
        specs.iter().map(|s| s.local_port.to_string()).collect::<Vec<_>>().join("/"),
        link.session_id()
    );

    let state = Arc::new(parking_lot::Mutex::new(TunnelState {
        conns: Default::default(),
        next_cid: 1,
    }));

    // 被控端 → 本地：按 cid 分发 DATA 到对应写半边
    {
        let state2 = state.clone();
        tokio::spawn(async move {
            while let Some(ev) = events.recv().await {
                match ev {
                    LinkEvent::Tunnel(TunnelMsg::Data { cid, data }) => {
                        let tx = state2.lock().conns.get(&cid).cloned();
                        if let Some(tx) = tx {
                            let _ = tx.send(data).await;
                        }
                    }
                    LinkEvent::Tunnel(TunnelMsg::Err { cid, msg }) => {
                        warn!("隧道 cid={cid} 被控端错误: {msg}");
                        if let Some(tx) = state2.lock().conns.remove(&cid) {
                            let _ = tx; // drop → 写任务退出 → socket 关闭
                        }
                    }
                    LinkEvent::Tunnel(TunnelMsg::Close { cid }) => {
                        if let Some(tx) = state2.lock().conns.remove(&cid) {
                            let _ = tx;
                        }
                    }
                    LinkEvent::Tunnel(TunnelMsg::Open { cid, target }) => {
                        info!("🔌 隧道 cid={cid} 已接通: {target}");
                    }
                    LinkEvent::Status(st) if st == "term-open" => {}
                    LinkEvent::Status(st) => {
                        warn!("链路状态: {st}，隧道退出");
                        std::process::exit(1);
                    }
                    _ => {}
                }
            }
            warn!("隧道链路事件流结束");
            std::process::exit(1);
        });
    }

    // 每条规则一个监听器
    let mut listeners = Vec::new();
    for spec in specs {
        let l = tokio::net::TcpListener::bind(("127.0.0.1", spec.local_port))
            .await
            .with_context(|| format!("绑定本地端口 {} 失败", spec.local_port))?;
        info!(
            "🛣️ 127.0.0.1:{} -> {device}:{}",
            spec.local_port, spec.target
        );
        listeners.push((l, spec));
    }

    // accept 循环（并发跑所有监听器）
    let mut tasks = Vec::new();
    for (l, spec) in listeners {
        let state3 = state.clone();
        let link3 = link.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = l.accept().await else { break };
                // target 先克隆：spec 不能被 move 进任务（accept 循环要复用）
                let target = spec.target.clone();
                let cid = {
                    let mut st = state3.lock();
                    let cid = st.next_cid;
                    st.next_cid = st.next_cid.wrapping_add(1).max(1);
                    cid
                };
                info!("🔌 新隧道连接 cid={cid} -> {target}");
                let (mut rd, mut wr) = socket.into_split();
                let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
                state3.lock().conns.insert(cid, tx);

                // 本地 socket → 被控端
                let link4 = link3.clone();
                tokio::spawn(async move {
                    if link4
                        .send_tunnel(TunnelMsg::Open { cid, target })
                        .await
                        .is_err()
                    {
                        return;
                    }
                    let mut buf = vec![0u8; 8192];
                    loop {
                        match rd.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if link4
                                    .send_tunnel(TunnelMsg::Data {
                                        cid,
                                        data: buf[..n].to_vec(),
                                    })
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                    }
                    let _ = link4
                        .send_tunnel(TunnelMsg::Close { cid })
                        .await;
                });

                // 被控端 → 本地 socket
                tokio::spawn(async move {
                    while let Some(data) = rx.recv().await {
                        if wr.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    let _ = wr.shutdown().await;
                });
            }
        }));
    }

    // Ctrl-C 优雅退出
    let _ = tokio::signal::ctrl_c().await;
    info!("👋 收到退出信号，关闭隧道");
    for t in tasks {
        t.abort();
    }
    link.close().await;
    Ok(())
}

// ============================================================
// proxy：ProxyCommand stdio 工具（ssh 的传输层替身）
// ============================================================

/// ssh → rebleach-proxy --stdio：stdin/stdout 即 ssh 协议字节流，
/// 经 tunnel 通道打到被控端（`ssh` 目标 = 内置 SSH 服务端，或 host:port = TCP）。
pub async fn proxy_stdio(
    env: CliEnv,
    device: &str,
    target: String,
    auth: Option<String>,
) -> Result<()> {
    use std::io::Read;
    // connect 直接返回分离的事件流（Receiver 需要 &mut），link 进 Arc 共享
    let (link, mut events) = crate::headless::connect(env.link(device, auth), true).await?;
    let link = Arc::new(link);
    info!("🔗 proxy stdio -> {device}:{target}");

    const CID: u16 = 1;
    link.send_tunnel(TunnelMsg::Open {
        cid: CID,
        target: target.clone(),
    })
    .await?;

    // 被控端 → stdout
    {
        tokio::spawn(async move {
            while let Some(ev) = events.recv().await {
                match ev {
                    LinkEvent::Tunnel(TunnelMsg::Data { cid, data }) if cid == CID => {
                        let mut so = std::io::stdout();
                        if so.write_all(&data).and_then(|_| so.flush()).is_err() {
                            std::process::exit(0); // ssh 关闭了管道
                        }
                    }
                    LinkEvent::Tunnel(TunnelMsg::Close { .. }) => {
                        std::process::exit(0);
                    }
                    LinkEvent::Tunnel(TunnelMsg::Err { msg, .. }) => {
                        eprintln!("rebleach-proxy: {msg}");
                        std::process::exit(2);
                    }
                    LinkEvent::Status(st) if st == "term-open" => {}
                    LinkEvent::Status(_) => std::process::exit(3),
                    _ => {}
                }
            }
            std::process::exit(3);
        });
    }

    // stdin → 被控端（阻塞读放独立线程，经无界通道转发到 tokio 写侧）
    {
        let (in_tx, mut in_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut sin = std::io::stdin();
            let mut buf = vec![0u8; 8192];
            loop {
                match sin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if in_tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
            // stdin EOF：关掉隧道（半关语义：读侧继续等被控端收尾）
        });
        let link3 = link.clone();
        tokio::spawn(async move {
            while let Some(data) = in_rx.recv().await {
                if link3
                    .send_tunnel(TunnelMsg::Data {
                        cid: CID,
                        data,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            // stdin EOF → 关闭隧道连接
            let _ = link3.send_tunnel(TunnelMsg::Close { cid: CID }).await;
        });
    }

    // 主线程挂在事件流上（写侧任务退出时整个进程随之退出）
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

// ============================================================
// ssh-config：标准 ssh 接入配置（T3）
// ============================================================

#[derive(Debug, Clone, serde::Serialize)]
pub struct SshHostEntry {
    /// ssh Host 别名（rbl-<device>）
    pub alias: String,
    /// 设备 ID
    pub device: String,
    /// 后端：内置 SSH 服务端 或 被控端 127.0.0.1:22 的 sshd
    pub backend: SshBackend,
    /// 登录用户（可空）
    pub user: Option<String>,
    /// 主控端设备 ID（有值时写入 ProxyCommand --id，确保授权校验用的是同一身份）
    pub controller_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub enum SshBackend {
    /// Agent 内置 russh 服务端（无需被控端装 sshd）
    BuiltIn,
    /// 被控端本机 sshd（Linux/Windows OpenSSH Server）
    Sshd,
}

/// 渲染 ~/.ssh/config.d/rebleach.conf 的内容（纯函数，便于测试）
pub fn render_ssh_config(entries: &[SshHostEntry], proxy_bin: &str, server: &str) -> String {
    let mut out = String::from(
        "# Rebleach SSH 接入配置（由 rebleach-client ssh-config 生成）\n\
         # 用法：确认 ~/.ssh/config 里有 'Include ~/.ssh/config.d/*.conf' 后：\n\
         #   ssh rbl-<device-id>\n\n",
    );
    for e in entries {
        let (hostname, stdio_target) = match e.backend {
            // %h 占位：rblssh = 内置 SSH 服务端；其余按 %h:%p 原样转发
            SshBackend::BuiltIn => ("rblssh", "rblssh"),
            SshBackend::Sshd => ("127.0.0.1", "%h:%p"),
        };
        out.push_str(&format!("Host {alias}\n", alias = e.alias));
        out.push_str(&format!("    HostName {hostname}\n"));
        out.push_str("    Port 22\n");
        if let Some(u) = &e.user {
            out.push_str(&format!("    User {u}\n"));
        }
        let id_part = e
            .controller_id
            .as_ref()
            .map(|c| format!(" --id {c}"))
            .unwrap_or_default();
        out.push_str(&format!(
            "    ProxyCommand \"{proxy_bin}\" --server {server}{id_part} --device {device} {target}\n",
            device = e.device,
            target = stdio_target,
        ));
        out.push_str("    StrictHostKeyChecking accept-new\n\n");
    }
    out
}

/// ssh-config 子命令：渲染并（可选）写入 ~/.ssh/config.d/rebleach.conf
#[allow(clippy::too_many_arguments)]
pub fn ssh_config(
    devices: Vec<String>,
    backend: SshBackend,
    user: Option<String>,
    proxy_bin: String,
    server: String,
    controller: Option<String>,
    out: Option<PathBuf>,
    print_only: bool,
) -> Result<()> {
    if devices.is_empty() {
        bail!("至少一个 --device <id>");
    }
    let entries: Vec<SshHostEntry> = devices
        .iter()
        .map(|d| SshHostEntry {
            alias: format!("rbl-{d}"),
            device: d.clone(),
            backend,
            user: user.clone(),
            controller_id: controller.clone(),
        })
        .collect();
    let content = render_ssh_config(&entries, &proxy_bin, &server);

    let path = out.unwrap_or_else(|| {
        home_dir()
            .join(".ssh")
            .join("config.d")
            .join("rebleach.conf")
    });

    if print_only {
        println!("{content}");
        return Ok(());
    }
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)
            .with_context(|| format!("创建目录失败: {}", p.display()))?;
    }
    std::fs::write(&path, &content)
        .with_context(|| format!("写入失败: {}", path.display()))?;
    info!("✅ 已写入 {}", path.display());
    info!("提示：在 ~/.ssh/config 中加入 'Include ~/.ssh/config.d/*.conf' 后即可 ssh {}", entries[0].alias);
    Ok(())
}

// ============================================================
// inventory：设备清单 → Ansible inventory（T2）
// ============================================================

#[derive(Debug, Clone, serde::Serialize)]
pub struct DeviceRow {
    pub id: String,
    pub hostname: String,
    pub os: String,
    pub role: String,
    pub online: bool,
    pub ip: String,
}

impl DeviceRow {
    pub fn from_json(v: &serde_json::Value) -> Option<Self> {
        Some(Self {
            id: v.get("id")?.as_str()?.to_string(),
            hostname: v
                .get("hostname")
                .and_then(|x| x.as_str())
                .unwrap_or("-")
                .to_string(),
            os: v
                .get("os")
                .and_then(|x| x.as_str())
                .unwrap_or("unknown")
                .to_string(),
            role: v
                .get("role")
                .and_then(|x| x.as_str())
                .unwrap_or("controlled")
                .to_string(),
            online: v
                .get("online")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
            ip: v
                .get("ip")
                .and_then(|x| x.as_str())
                .unwrap_or("-")
                .to_string(),
        })
    }
}

/// ws(s):// → http(s)://，去掉 /ws 后缀
pub fn signaling_to_http(server: &str) -> String {
    let s = server
        .replacen("ws://", "http://", 1)
        .replacen("wss://", "https://", 1);
    s.trim_end_matches("/ws").trim_end_matches('/').to_string()
}

/// GET /api/v1/devices（Basic Auth）
pub fn fetch_devices(server: &str, user: &str, pass: &str) -> Result<Vec<DeviceRow>> {
    let url = format!("{}/api/v1/devices", signaling_to_http(server));
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .build();
    // ureq 2.x 没有 basic_auth 辅助方法，手写 Basic 认证头
    let creds = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
    let resp = agent
        .get(&url)
        .set("Authorization", &format!("Basic {creds}"))
        .call()
        .map_err(|e| anyhow!("查询设备清单失败: {e}"))?;
    let body: serde_json::Value = resp
        .into_json()
        .context("设备清单响应 JSON 解析失败")?;
    let arr = body
        .get("data")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(arr.iter().filter_map(DeviceRow::from_json).collect())
}

/// 渲染 Ansible inventory（纯函数，便于测试）
pub fn render_inventory(rows: &[DeviceRow], proxy_bin: &str) -> String {
    let mut out = String::from("# Rebleach Ansible inventory（由 rebleach-client inventory 生成）\n\n[rebleach]\n");
    for r in rows {
        if r.role != "controlled" {
            continue;
        }
        let alias = format!("rbl-{}", r.id);
        // 每台主机一条 ProxyCommand，走 Agent 内置 SSH 服务端
        out.push_str(&format!(
            "{alias} ansible_connection=ssh ansible_ssh_common_args='-o ProxyCommand=\"{proxy_bin} --device {id} --stdio rblssh\"'\n",
            id = r.id
        ));
    }
    out.push_str("\n[rebleach:vars]\nansible_python_interpreter=auto_silent\n\n");
    // 明细注释块：机器名/系统/在线状态，便于人看
    out.push_str("# 设备明细：\n");
    for r in rows {
        out.push_str(&format!(
            "# {} host={} os={} role={} online={} ip={}\n",
            r.id, r.hostname, r.os, r.role, r.online, r.ip
        ));
    }
    out
}

pub fn inventory(
    env: &CliEnv,
    user: String,
    pass: String,
    only_online: bool,
    proxy_bin: String,
    out: Option<PathBuf>,
    print_only: bool,
) -> Result<()> {
    let rows = fetch_devices(&env.server, &user, &pass)?;
    let rows: Vec<DeviceRow> = rows
        .into_iter()
        .filter(|r| !only_online || r.online)
        .collect();
    if rows.is_empty() {
        warn!("服务器无设备记录");
    }
    let content = render_inventory(&rows, &proxy_bin);
    let path = out.unwrap_or_else(|| crate::identity::default_base_dir().join("inventory.ini"));
    if print_only {
        println!("{content}");
        return Ok(());
    }
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&path, &content)
        .with_context(|| format!("写入失败: {}", path.display()))?;
    info!("✅ inventory 已写入 {}（{} 台设备）", path.display(), rows.len());
    Ok(())
}

// ============================================================
// keygen：ed25519 SSH 密钥对（T3）
// ============================================================

/// 生成一把 ed25519 密钥，返回 (私钥 OpenSSH 格式, 公钥 OpenSSH 格式)
pub fn generate_keypair(comment: &str) -> Result<(String, String)> {
    crate::sshd_impl::generate_keypair_openssh(comment)
}

pub fn keygen(device: String, out: Option<PathBuf>, add: bool) -> Result<()> {
    let (priv_pem, pub_ssh) = generate_keypair(&format!("rebleach-{device}"))?;
    let path = out.unwrap_or_else(|| {
        crate::identity::default_base_dir()
            .join("ssh")
            .join(format!("{device}.key"))
    });
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&path, &priv_pem).with_context(|| format!("写入私钥失败: {}", path.display()))?;
    #[cfg(windows)]
    restrict_key_file(&path);

    info!("🔑 私钥已生成: {}", path.display());
    info!("🔑 公钥:");
    println!("{pub_ssh}");

    if add {
        let ak = crate::sshd::authorized_keys_path();
        if let Some(p) = ak.parent() {
            std::fs::create_dir_all(p)?;
        }
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&ak)
            .with_context(|| format!("打开授权文件失败: {}", ak.display()))?;
        writeln!(f, "{pub_ssh}")?;
        info!("✅ 已追加到 {}", ak.display());
        info!("提示：--add 只在**本机就是被控端**时有效；远程设备请把公钥拷到其 %APPDATA%/rebleach/ssh_authorized_keys");
    }
    Ok(())
}

#[cfg(windows)]
fn restrict_key_file(path: &std::path::Path) {
    use std::os::windows::process::CommandExt;
    // 用 icacls 收紧权限为仅当前用户（失败不致命，密钥本就在用户目录下）
    let _ = std::process::Command::new("icacls")
        .arg(path)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(format!(
            "{}:R",
            std::env::var("USERNAME").unwrap_or_default()
        ))
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output();
}

// ============================================================
// setup：一键初始化主控端（登记公钥 + 生成 ssh config）
// ============================================================

fn home_dir() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

fn basic_auth_header(user: &str, pass: &str) -> String {
    let creds = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
    format!("Basic {creds}")
}

/// 主控端 SSH 密钥对（用于服务端登记与 ssh 接入）
#[derive(Debug, Clone, serde::Serialize)]
pub struct OperatorKey {
    pub pubkey: String,
    pub private_path: PathBuf,
    pub public_path: PathBuf,
}

/// 读取或生成主控端 SSH 密钥
pub fn ensure_operator_key(controller_id: &str) -> Result<OperatorKey> {
    let dir = crate::identity::default_base_dir().join("ssh");
    let key_path = dir.join(format!("{controller_id}.key"));
    let pub_path = dir.join(format!("{controller_id}.pub"));
    if key_path.exists() && pub_path.exists() {
        let pubkey = std::fs::read_to_string(&pub_path)
            .map(|s| s.trim().to_string())
            .context("读取已有 SSH 公钥失败")?;
        return Ok(OperatorKey {
            pubkey,
            private_path: key_path,
            public_path: pub_path,
        });
    }
    let (priv_pem, pub_ssh) = generate_keypair(&format!("rebleach-{controller_id}"))?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&key_path, &priv_pem)
        .with_context(|| format!("写入私钥失败: {}", key_path.display()))?;
    std::fs::write(&pub_path, format!("{pub_ssh}\n"))?;
    #[cfg(windows)]
    restrict_key_file(&key_path);
    info!("🔑 已生成 SSH 密钥: {}", key_path.display());
    Ok(OperatorKey {
        pubkey: pub_ssh,
        private_path: key_path,
        public_path: pub_path,
    })
}

pub fn register_ssh_key(
    server: &str,
    user: &str,
    pass: &str,
    controller_id: &str,
    pubkey: &str,
) -> Result<()> {
    let url = format!("{}/api/v1/ssh-keys", signaling_to_http(server));
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .build();
    let body = serde_json::json!({
        "controller_id": controller_id,
        "pubkey": pubkey,
    });
    let resp = agent
        .post(&url)
        .set("Authorization", &basic_auth_header(user, pass))
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
        .map_err(|e| anyhow!("登记 SSH 公钥失败: {e}"))?;
    let v: serde_json::Value = resp.into_json().context("登记公钥响应解析失败")?;
    if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        bail!("登记 SSH 公钥被拒绝: {v}");
    }
    Ok(())
}

/// GET /api/v1/grants → data 数组
pub fn fetch_grants(server: &str, user: &str, pass: &str) -> Result<Vec<serde_json::Value>> {
    let url = format!("{}/api/v1/grants", signaling_to_http(server));
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .build();
    let resp = agent
        .get(&url)
        .set("Authorization", &basic_auth_header(user, pass))
        .call()
        .map_err(|e| anyhow!("查询授权列表失败: {e}"))?;
    let body: serde_json::Value = resp.into_json().context("授权列表响应解析失败")?;
    Ok(body
        .get("data")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default())
}

/// 确保 ~/.ssh/config 包含 config.d 的 Include（幂等）
pub fn ensure_ssh_include(home: &std::path::Path) -> Result<()> {
    let cfg = home.join(".ssh").join("config");
    if let Some(p) = cfg.parent() {
        std::fs::create_dir_all(p)?;
    }
    let mut text = std::fs::read_to_string(&cfg).unwrap_or_default();
    let line = "Include ~/.ssh/config.d/*.conf";
    let has = text.lines().any(|l| {
        let l = l.trim();
        l.eq_ignore_ascii_case(line) || l.eq_ignore_ascii_case("Include config.d/*.conf")
    });
    if !has {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(line);
        text.push('\n');
        std::fs::write(&cfg, text).with_context(|| format!("写入失败: {}", cfg.display()))?;
        info!("✅ 已向 {} 追加 Include", cfg.display());
    }
    Ok(())
}

/// 从 grant 列表里挑出「当前生效」的被控端设备 ID（纯函数，便于测试）
pub fn pick_active_devices(
    grants: &[serde_json::Value],
    controller_id: &str,
    now: i64,
) -> Vec<String> {
    let mut devices: Vec<String> = grants
        .iter()
        .filter_map(|g| {
            if g.get("controller_id")?.as_str()? != controller_id {
                return None;
            }
            if g.get("revoked").and_then(|v| v.as_bool()).unwrap_or(false) {
                return None;
            }
            let start = g.get("start_at")?.as_i64()?;
            let end = g.get("end_at")?.as_i64()?;
            if !(start <= now && now < end) {
                return None;
            }
            Some(g.get("controlled_id")?.as_str()?.to_string())
        })
        .collect();
    devices.sort();
    devices.dedup();
    devices
}

/// 生成并写入 SSH 配置的结果
#[derive(Debug, Clone, serde::Serialize)]
pub struct SshConfigOutcome {
    pub path: PathBuf,
    pub aliases: Vec<String>,
}

/// 渲染「按授权生成」的 ssh config 文本
pub fn build_grants_ssh_config(
    server: &str,
    controller_id: &str,
    devices: &[String],
    backend: SshBackend,
    ssh_user: Option<String>,
    proxy_bin: &str,
) -> String {
    let entries: Vec<SshHostEntry> = devices
        .iter()
        .map(|d| SshHostEntry {
            alias: format!("rbl-{d}"),
            device: d.clone(),
            backend,
            user: ssh_user.clone(),
            controller_id: Some(controller_id.to_string()),
        })
        .collect();
    render_ssh_config(&entries, proxy_bin, server)
}

/// 拉取有效授权 → 渲染 → 写文件 → 确保 Include（供 CLI setup 与主控端 GUI 复用）
#[allow(clippy::too_many_arguments)]
pub fn write_grants_ssh_config(
    server: &str,
    user: &str,
    pass: &str,
    controller_id: &str,
    backend: SshBackend,
    ssh_user: Option<String>,
    proxy_bin: &str,
    out: Option<PathBuf>,
) -> Result<SshConfigOutcome> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("系统时间异常")?
        .as_secs() as i64;
    let devices = pick_active_devices(&fetch_grants(server, user, pass)?, controller_id, now);
    if devices.is_empty() {
        warn!("当前没有生效中的授权（grants），生成的 SSH 配置为空");
    }
    let content = build_grants_ssh_config(
        server,
        controller_id,
        &devices,
        backend,
        ssh_user,
        proxy_bin,
    );
    let home = home_dir();
    let path = out.unwrap_or_else(|| home.join(".ssh").join("config.d").join("rebleach.conf"));
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).with_context(|| format!("创建目录失败: {}", p.display()))?;
    }
    std::fs::write(&path, &content).with_context(|| format!("写入失败: {}", path.display()))?;
    ensure_ssh_include(&home)?;
    Ok(SshConfigOutcome {
        path,
        aliases: devices.iter().map(|d| format!("rbl-{d}")).collect(),
    })
}

/// POST /api/v1/enroll-tokens → (token, expires_at)
pub fn create_enroll_token(
    server: &str,
    user: &str,
    pass: &str,
    note: &str,
    ttl_secs: u64,
) -> Result<(String, i64)> {
    let url = format!("{}/api/v1/enroll-tokens", signaling_to_http(server));
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .build();
    let body = serde_json::json!({"note": note, "ttl_secs": ttl_secs});
    let resp = agent
        .post(&url)
        .set("Authorization", &basic_auth_header(user, pass))
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
        .map_err(|e| anyhow!("创建安装令牌失败: {e}"))?;
    let v: serde_json::Value = resp.into_json().context("安装令牌响应解析失败")?;
    let token = v
        .get("token")
        .and_then(|x| x.as_str())
        .context("响应缺少 token")?
        .to_string();
    let expires = v
        .get("expires_at")
        .and_then(|x| x.as_i64())
        .context("响应缺少 expires_at")?;
    Ok((token, expires))
}

/// setup 子命令：登记主控端公钥 → 拉取有效授权 → 生成 ssh config
#[allow(clippy::too_many_arguments)]
pub fn setup(
    env: &CliEnv,
    controller_id: String,
    user: String,
    pass: String,
    backend: SshBackend,
    ssh_user: Option<String>,
    proxy_bin: String,
    out: Option<PathBuf>,
    print_only: bool,
) -> Result<()> {
    if controller_id.trim().is_empty() || !controller_id.chars().all(|c| c.is_ascii_digit()) {
        bail!("--device 必须是纯数字的主控端设备 ID");
    }
    let key = ensure_operator_key(&controller_id)?;
    register_ssh_key(&env.server, &user, &pass, &controller_id, &key.pubkey)?;
    info!("✅ 已登记主控端公钥（{controller_id}）");

    if print_only {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("系统时间异常")?
            .as_secs() as i64;
        let devices =
            pick_active_devices(&fetch_grants(&env.server, &user, &pass)?, &controller_id, now);
        if devices.is_empty() {
            warn!("当前没有生效中的授权（grants），生成的 SSH 配置为空");
        }
        println!(
            "{}",
            build_grants_ssh_config(
                &env.server,
                &controller_id,
                &devices,
                backend,
                ssh_user,
                &proxy_bin
            )
        );
        return Ok(());
    }

    let outcome = write_grants_ssh_config(
        &env.server,
        &user,
        &pass,
        &controller_id,
        backend,
        ssh_user,
        &proxy_bin,
        out,
    )?;
    info!(
        "✅ 已写入 {}（{} 台可接入设备）",
        outcome.path.display(),
        outcome.aliases.len()
    );
    if outcome.aliases.is_empty() {
        info!("提示：先由管理员创建授权后重跑 setup，或执行 rebleach-client ssh-config");
    } else {
        info!("可用主机：");
        for a in &outcome.aliases {
            println!("  ssh {a}");
        }
    }
    Ok(())
}

// ============================================================
// term-replay：asciicast 回放为纯文本（T4）
// ============================================================

pub fn term_replay(path: &str) -> Result<()> {
    let p = std::path::Path::new(path);
    let text = crate::asciicast::replay_to_text(p)?;
    print!("{text}");
    stdout_flush();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::pick_active_devices;
    use serde_json::json;

    fn grant(controller: &str, controlled: &str, start: i64, end: i64, revoked: bool) -> serde_json::Value {
        json!({
            "controller_id": controller,
            "controlled_id": controlled,
            "start_at": start,
            "end_at": end,
            "revoked": revoked,
        })
    }

    #[test]
    fn picks_only_active_grants_of_controller() {
        let grants = vec![
            grant("111111111", "900000001", 100, 200, false), // 有效
            grant("111111111", "900000002", 100, 200, false), // 有效
            grant("111111111", "900000003", 100, 50, false),  // 已过期
            grant("111111111", "900000004", 300, 400, false), // 尚未开始
            grant("111111111", "900000005", 100, 200, true),  // 已撤销
            grant("222222222", "900000006", 100, 200, false), // 他人授权
        ];
        let devices = pick_active_devices(&grants, "111111111", 150);
        assert_eq!(devices, vec!["900000001".to_string(), "900000002".to_string()]);
    }

    #[test]
    fn dedups_and_ignores_malformed() {
        let grants = vec![
            grant("111111111", "900000001", 100, 200, false),
            grant("111111111", "900000001", 100, 200, false),
            json!({"controller_id": "111111111", "controlled_id": "900000002"}),
        ];
        let devices = pick_active_devices(&grants, "111111111", 150);
        assert_eq!(devices, vec!["900000001".to_string()]);
    }
}
