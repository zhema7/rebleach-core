#![cfg(feature = "desktop")]
// Rebleach 客户端 CLI（无界面入口，用于被控端调试 / 自动化测试）
// 桌面端见 app/src-tauri：Tauri 2 外壳 + WebView + WebCodecs
// Android 端通过 JNI 入口启动（见 client/src/jni.rs），不需要本文件。
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use winit::event_loop::{EventLoop, EventLoopProxy};

use openh264::formats::YUVSource;

use rebleach_client::protocol::{self, ControlMsg, Role};
use rebleach_client::session::{self, EventSink, SessionConfig, SessionEvent};
use rebleach_client::viewer::{ViewerApp, ViewerEvent};

// ============================================================
// CLI
// ============================================================
#[derive(Debug, Parser)]
#[command(
    name = "rebleach-client",
    about = "Rebleach 客户端（信令 + WebRTC 屏幕流 + 键鼠控制 + 终端/隧道自动化）"
)]
struct Cli {
    /// 信令服务器 WebSocket 地址
    #[arg(long, default_value = "ws://127.0.0.1:8088/ws")]
    server: String,

    /// 本机设备 ID（查看模式需要）
    #[arg(long)]
    id: Option<String>,

    /// 角色（查看模式需要）
    #[arg(long, value_parser = ["controller", "controlled"])]
    role: Option<String>,

    /// 对端设备 ID（controller 模式下需要）
    #[arg(long)]
    peer: Option<String>,

    /// 被控端：采集/编码帧率
    #[arg(long, default_value_t = 15)]
    fps: u64,

    /// STUN 服务器，格式 `stun:host:port`，可重复或逗号分隔（留空则用内置默认）
    #[arg(long, value_delimiter = ',')]
    stun: Vec<String>,

    /// TURN 服务器，格式 `turn:user:pass@host:port`，可重复或逗号分隔（留空则用内置默认）
    #[arg(long, value_delimiter = ',')]
    turn: Vec<String>,

    /// 设备身份密钥文件（TOFU 用），默认 %APPDATA%/rebleach/identity.json
    #[arg(long)]
    key_file: Option<std::path::PathBuf>,

    /// 局域网直连验证码（controller 连接被控端内嵌信令时必填）
    #[arg(long)]
    auth: Option<String>,

    /// 无界面自动化子命令（缺省 = 传统的图形查看模式）
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Debug, clap::Subcommand)]
enum Cmd {
    /// 远程执行一次性命令：exec <device> -- <command>（干净输出 + 真实退出码）
    Exec {
        /// 目标被控端设备 ID
        device: String,
        /// 命令行（`--` 之后，由被控端 cmd 解析，支持管道/重定向）
        #[arg(last = true, required = true)]
        command: Vec<String>,
        /// 输出 JSON（exit_code/output/duration_ms）
        #[arg(long)]
        json: bool,
        /// 超时秒数（超时报错退出）
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// TCP 隧道：--local 13306:127.0.0.1:3306 或 --local 13306:ssh，可重复
    Tunnel {
        /// 目标被控端设备 ID
        device: String,
        /// 规则：本地端口:目标（目标= host:port 或 ssh）
        #[arg(long, required = true)]
        local: Vec<String>,
    },
    /// 生成 ~/.ssh/config.d/rebleach.conf，让标准 ssh 经 ProxyCommand 接入（T3）
    SshConfig {
        /// 设备 ID，可重复
        #[arg(long, required = true)]
        device: Vec<String>,
        /// 后端：builtin（Agent 内置 SSH 服务端，默认）或 sshd（被控端本机 sshd）
        #[arg(long, default_value = "builtin")]
        backend: String,
        /// ssh 登录用户（可空）
        #[arg(long)]
        user: Option<String>,
        /// ProxyCommand 里引用的可执行名
        #[arg(long, default_value = "rebleach-proxy")]
        proxy_bin: String,
        /// 主控端设备 ID（有值时写入 ProxyCommand --id，与授权校验身份一致）
        #[arg(long)]
        controller: Option<String>,
        /// 输出文件路径（默认 ~/.ssh/config.d/rebleach.conf）
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        /// 只打印不写文件
        #[arg(long)]
        print: bool,
    },
    /// 从信令服务器拉设备清单，生成 Ansible inventory
    Inventory {
        /// Basic Auth 用户名
        #[arg(long, default_value = "admin")]
        user: String,
        /// Basic Auth 密码（必填；也可用环境变量 RBLEACH_PASS）
        #[arg(long, env = "RBLEACH_PASS", hide_env_values = true)]
        pass: String,
        /// 只包含在线设备
        #[arg(long)]
        only_online: bool,
        /// ProxyCommand 里引用的可执行名
        #[arg(long, default_value = "rebleach-proxy")]
        proxy_bin: String,
        /// 输出文件路径（默认 %APPDATA%/rebleach/inventory.ini）
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        /// 只打印不写文件
        #[arg(long)]
        print: bool,
    },
    /// 生成 ed25519 SSH 密钥对（内置 SSH 服务端授权用）
    Keygen {
        /// 设备标识（写入密钥注释与文件名）
        #[arg(long)]
        device: String,
        /// 私钥输出路径（默认 %APPDATA%/rebleach/ssh/<device>.key）
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        /// 同时把公钥追加到本机被控端授权列表（仅本机即被控端时有效）
        #[arg(long)]
        add: bool,
    },
    /// 回放 asciicast v2 终端录制为纯文本（校验/检索）
    TermReplay {
        /// .cast 文件路径
        path: String,
    },
    /// 一键初始化主控端：登记 SSH 公钥 + 按有效授权生成 ~/.ssh/config.d/rebleach.conf
    Setup {
        /// 主控端设备 ID（纯数字）
        #[arg(long)]
        device: String,
        /// Basic Auth 用户名
        #[arg(long, default_value = "admin")]
        user: String,
        /// Basic Auth 密码（必填；也可用环境变量 RBLEACH_PASS）
        #[arg(long, env = "RBLEACH_PASS", hide_env_values = true)]
        pass: String,
        /// 后端：builtin（Agent 内置 SSH 服务端，默认）或 sshd（被控端本机 sshd）
        #[arg(long, default_value = "builtin")]
        backend: String,
        /// ssh 登录用户（可空）
        #[arg(long)]
        ssh_user: Option<String>,
        /// ProxyCommand 里引用的可执行名
        #[arg(long, default_value = "rebleach-proxy")]
        proxy_bin: String,
        /// 输出文件路径（默认 ~/.ssh/config.d/rebleach.conf）
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        /// 只打印不写文件
        #[arg(long)]
        print: bool,
    },
}

fn collect_ice_servers(stun: &[String], turn: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(stun.len() + turn.len());
    out.extend(stun.iter().cloned());
    out.extend(turn.iter().cloned());
    out
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();

    // 无界面自动化子命令：不进 winit 循环
    if let Some(cmd) = &cli.command {
        return run_cli_cmd(&cli, cmd);
    }

    // 传统图形模式：--id/--role 必填
    let id = cli
        .id
        .clone()
        .context("图形查看模式需要 --id <本机设备ID>（或使用 exec/tunnel 等子命令）")?;
    let role_s = cli
        .role
        .clone()
        .context("图形查看模式需要 --role controller|controlled")?;
    let role = protocol::role_from_str(&role_s);
    let cli = LegacyArgs { cli, id, role };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    match cli.role {
        Role::Controller => run_controller(&rt, cli),
        Role::Controlled => run_controlled(&rt, cli),
    }
}

/// 图形模式参数（补全必填字段后的 Cli）
struct LegacyArgs {
    cli: Cli,
    id: String,
    role: Role,
}

impl std::ops::Deref for LegacyArgs {
    type Target = Cli;
    fn deref(&self) -> &Cli {
        &self.cli
    }
}

/// 无界面子命令分发
fn run_cli_cmd(cli: &Cli, cmd: &Cmd) -> Result<()> {
    let env = rebleach_client::cli::CliEnv::new(
        Some(cli.server.clone()),
        cli.id.clone(),
        cli.key_file.clone(),
        collect_ice_servers(&cli.stun, &cli.turn),
    );
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    match cmd {
        Cmd::Exec {
            device,
            command,
            json,
            timeout,
        } => {
            let command = command.join(" ");
            let (json, timeout, device) = (*json, *timeout, device.clone());
            let auth = cli.auth.clone();
            let code = rt.block_on(rebleach_client::cli::exec(
                env, &device, &command, json, timeout, auth,
            ))?;
            std::process::exit(code);
        }
        Cmd::Tunnel { device, local } => {
            let specs: Vec<_> = local
                .iter()
                .map(|s| rebleach_client::cli::parse_tunnel_spec(s))
                .collect::<Result<Vec<_>>>()?;
            let auth = cli.auth.clone();
            rt.block_on(rebleach_client::cli::tunnel(env, device, specs, auth))
        }
        Cmd::SshConfig {
            device,
            backend,
            user,
            proxy_bin,
            controller,
            out,
            print,
        } => {
            let backend = match backend.as_str() {
                "builtin" | "ssh" | "russh" => rebleach_client::cli::SshBackend::BuiltIn,
                "sshd" | "tcp" => rebleach_client::cli::SshBackend::Sshd,
                other => anyhow::bail!("未知后端: {other}（builtin / sshd）"),
            };
            rebleach_client::cli::ssh_config(
                device.clone(),
                backend,
                user.clone(),
                proxy_bin.clone(),
                cli.server.clone(),
                controller.clone(),
                out.clone(),
                *print,
            )
        }
        Cmd::Inventory {
            user,
            pass,
            only_online,
            proxy_bin,
            out,
            print,
        } => rebleach_client::cli::inventory(
            &env,
            user.clone(),
            pass.clone(),
            *only_online,
            proxy_bin.clone(),
            out.clone(),
            *print,
        ),
        Cmd::Keygen { device, out, add } => {
            rebleach_client::cli::keygen(device.clone(), out.clone(), *add)
        }
        Cmd::TermReplay { path } => rebleach_client::cli::term_replay(path),
        Cmd::Setup {
            device,
            user,
            pass,
            backend,
            ssh_user,
            proxy_bin,
            out,
            print,
        } => {
            let backend = match backend.as_str() {
                "builtin" | "ssh" | "russh" => rebleach_client::cli::SshBackend::BuiltIn,
                "sshd" | "tcp" => rebleach_client::cli::SshBackend::Sshd,
                other => anyhow::bail!("未知后端: {other}（builtin / sshd）"),
            };
            rebleach_client::cli::setup(
                &env,
                device.clone(),
                user.clone(),
                pass.clone(),
                backend,
                ssh_user.clone(),
                proxy_bin.clone(),
                out.clone(),
                *print,
            )
        }
    }
}

fn session_config(cli: &LegacyArgs) -> SessionConfig {
    SessionConfig {
        server: cli.server.clone(),
        id: cli.id.clone(),
        role: cli.role.clone(),
        peer: cli.peer.clone(),
        fps: cli.fps,
        ice_servers: collect_ice_servers(&cli.stun, &cli.turn),
        key_file: cli.key_file.clone(),
        auth: cli.auth.clone(),
    }
}

// ============================================================
// 主控端：网络任务 spawn 到 rt，主线程跑 winit 事件循环
// ============================================================
fn run_controller(rt: &tokio::runtime::Runtime, cli: LegacyArgs) -> Result<()> {
    info!("🖥️  主控端模式：窗口渲染 + 输入捕获");
    // EventLoop 必须在主线程创建（Windows 平台约束）
    let event_loop = EventLoop::<ViewerEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    // UI 输入 → 网络（unbounded：鼠标移动高频，不阻塞 UI 线程）
    let (input_tx, input_rx) = mpsc::unbounded_channel::<ControlMsg>();
    // 网络 → 解码任务（H.264 裸流，解码后再送 UI 线程）
    let (frame_tx, frame_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    let proxy_net = proxy.clone();
    let sink: EventSink = Arc::new(move |ev| match ev {
        SessionEvent::Status(s) => {
            let _ = proxy_net.send_event(ViewerEvent::Status(s));
        }
        SessionEvent::Video { data, .. } => {
            let _ = frame_tx.send(data);
        }
        SessionEvent::Monitors(list) => {
            info!("🖥️ 对端显示器: {}", list.len());
            for m in &list {
                info!(
                    "   #{} {} {}x{}{}",
                    m.index,
                    m.name,
                    m.width,
                    m.height,
                    if m.primary { "（主）" } else { "" }
                );
            }
        }
        SessionEvent::Clipboard(text) => {
            rebleach_client::control::note_clip(&text);
            if let Err(e) = rebleach_client::control::set_clipboard(&text) {
                warn!("写入本机剪贴板失败: {e:#}");
            }
        }
        SessionEvent::Stats {
            fps,
            kbps,
            buffered,
        } => {
            debug!(
                "📶 链路 {fps:.0} fps / {kbps:.0} kbps / 缓冲 {} KB",
                buffered / 1024
            );
        }
        SessionEvent::Log { level, msg } => {
            let mark = match level.as_str() {
                "warn" => "⚠️",
                "error" => "❌",
                _ => "ℹ️",
            };
            info!("{mark} 被控端: {msg}");
        }
        SessionEvent::DriverStatus {
            has_driver,
            active_count,
            driver_kind,
            hint,
        } => {
            info!(
                "🖥️ 驱动状态: has_driver={has_driver} kind={driver_kind} active={active_count}"
            );
            if !has_driver {
                warn!("💡 {hint}");
            }
        }
        SessionEvent::Link { kind, detail, rtt_ms } => {
            info!("🔗 链路 {kind} · {detail} · RTT {rtt_ms:.0}ms");
        }
        SessionEvent::Ended(reason) => {
            info!("🔚 会话结束: {reason}");
        }
        SessionEvent::TermReady => info!("🖥️ 对端支持终端"),
        SessionEvent::TermOpened { sid } => info!("🖥️ 终端 {sid} 已打开"),
        SessionEvent::TermData { sid, data } => {
            let _ = sid;
            print!("{data}");
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
        SessionEvent::TermExit { sid, code } => info!("🖥️ 终端 {sid} 退出 code={code}"),
        SessionEvent::TermErr { sid, msg } => warn!("🖥️ 终端 {sid} 错误: {msg}"),
    });

    let cfg = session_config(&cli);
    rt.spawn(async move {
        if let Err(e) = session::run_session(cfg, input_rx, sink).await {
            error!("会话失败: {e:#}");
        }
    });

    let proxy_decode = proxy.clone();
    rt.spawn(async move {
        if let Err(e) = decode_loop(frame_rx, proxy_decode).await {
            error!("解码任务退出: {e:#}");
        }
    });

    let mut app = ViewerApp::new(input_tx);
    event_loop.run_app(&mut app)?;
    info!("窗口关闭，客户端退出");
    Ok(())
}

// ============================================================
// 主控端解码循环：openh264 Decoder → I420 → RGBA → UI
// ============================================================
async fn decode_loop(
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
    proxy: EventLoopProxy<ViewerEvent>,
) -> Result<()> {
    let mut decoder = openh264::decoder::Decoder::new().context("H.264 解码器初始化失败")?;
    let t0 = std::time::Instant::now();
    let mut n: u64 = 0;

    while let Some(frame) = rx.recv().await {
        match decoder.decode(&frame) {
            Ok(Some(yuv)) => {
                let (w, h) = yuv.dimensions();
                if w == 0 || h == 0 {
                    continue;
                }
                // 用 openh264 自带转换（正确处理内部 stride，手写紧凑索引会花屏）
                let mut rgba = vec![0u8; w * h * 4];
                yuv.write_rgba8(&mut rgba);
                let _ = proxy.send_event(ViewerEvent::Frame {
                    w: w as u32,
                    h: h as u32,
                    rgba: Arc::new(rgba),
                });
                n += 1;
                if n % 60 == 1 {
                    info!(
                        "🎬 已解码 {n} 帧 ({:.1} fps)",
                        n as f64 / t0.elapsed().as_secs_f64().max(1e-6)
                    );
                }
            }
            Ok(None) => {} // 丢弃帧
            Err(e) => warn!("解码失败: {e}"),
        }
    }
    info!("解码循环结束");
    Ok(())
}

// ============================================================
// 被控端：会话主循环（状态只打日志）
// ============================================================
fn run_controlled(rt: &tokio::runtime::Runtime, cli: LegacyArgs) -> Result<()> {
    info!("🖱️  被控端模式：屏幕采集 + 控制注入 (fps={})", cli.fps);
    let cfg = session_config(&cli);
    let (_input_tx, input_rx) = mpsc::unbounded_channel::<ControlMsg>();
    let sink: EventSink = Arc::new(|ev| {
        if let SessionEvent::Status(s) = ev {
            info!("{s}");
        }
    });
    rt.block_on(session::run_session(cfg, input_rx, sink))
}
