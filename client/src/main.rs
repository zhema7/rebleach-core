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
    about = "Rebleach 客户端（信令 + WebRTC 屏幕流 + 键鼠控制）"
)]
struct Cli {
    /// 信令服务器 WebSocket 地址
    #[arg(long, default_value = "ws://127.0.0.1:8088/ws")]
    server: String,

    /// 本机设备 ID
    #[arg(long)]
    id: String,

    /// 角色
    #[arg(long, value_parser = ["controller", "controlled"])]
    role: String,

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
    let role = protocol::role_from_str(&cli.role);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    match role {
        Role::Controller => run_controller(&rt, cli),
        Role::Controlled => run_controlled(&rt, cli),
    }
}

fn session_config(cli: &Cli) -> SessionConfig {
    SessionConfig {
        server: cli.server.clone(),
        id: cli.id.clone(),
        role: protocol::role_from_str(&cli.role),
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
fn run_controller(rt: &tokio::runtime::Runtime, cli: Cli) -> Result<()> {
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
        SessionEvent::Ended(reason) => {
            info!("🔚 会话结束: {reason}");
        }
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
fn run_controlled(rt: &tokio::runtime::Runtime, cli: Cli) -> Result<()> {
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
