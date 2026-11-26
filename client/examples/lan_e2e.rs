// 本机端到端自检：充当 LAN 主控端，连被控端内嵌迷你信令，验证「连得上 + 真收帧 + 控制通道可用」。
// 用法：cargo run -p rebleach-client --example lan_e2e -- --peer 999000111 --pin 123456
// 退出码：0 = 收到足够视频帧；1 = 超时/被拒绝/无帧（附状态轨迹，便于定位）
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use rebleach_client::protocol::{ControlMsg, Role};
use rebleach_client::session::{self, EventSink, SessionConfig, SessionEvent};
use tokio::sync::mpsc;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "ws://127.0.0.1:7681/ws")]
    server: String,
    /// 主控端设备 ID（默认使用隔离的测试身份）
    #[arg(long, default_value = "999000222")]
    id: String,
    /// 可选主控端身份密钥；不传则使用 %TEMP%/rebleach-e2e/identity.json
    #[arg(long)]
    key_file: Option<std::path::PathBuf>,
    /// 被控端设备 ID
    #[arg(long)]
    peer: String,
    /// 局域网验证码（被控端 %ProgramData%/Rebleach/lan-pin.txt）
    #[arg(long)]
    pin: String,
    /// 总超时（秒）
    #[arg(long, default_value_t = 40)]
    seconds: u64,
    /// 收到这么多帧即判定成功
    #[arg(long, default_value_t = 30)]
    frames: u32,
    /// 控制测试用的鼠标目标（编码帧坐标）
    #[arg(long, default_value_t = 12)]
    mx: u32,
    #[arg(long, default_value_t = 12)]
    my: u32,
    /// 保持阶段使用的第二个鼠标目标（缺省同 --mx/--my）；与首个目标不同才能区分
    /// 「锁屏期间失败的移动」和「解锁后成功的移动」
    #[arg(long)]
    mx2: Option<u32>,
    #[arg(long)]
    my2: Option<u32>,
    /// 收满 frames 后切换到该显示器再收一轮（验证多显示器采集与坐标原点）
    #[arg(long)]
    switch: Option<u32>,
    /// 收满 frames 后继续保持连接 N 秒，并每 2 秒重发一次 MouseMove
    /// （用于跨锁屏/解锁验证注入线程是否恢复）
    #[arg(long, default_value_t = 0)]
    hold: u64,
    /// 有画面后额外发一次键盘按键（单字符或命名键，如 q / Enter），验证键盘注入
    #[arg(long)]
    key: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,webrtc=warn".into()),
        )
        .init();
    let a = Args::parse();

    // 独立身份目录：绝不碰真实主控端/被控端的密钥与 TOFU 记录
    let dir = std::env::temp_dir().join("rebleach-e2e");
    std::fs::create_dir_all(&dir).ok();

    let frames = Arc::new(AtomicU32::new(0));
    let key_frames = Arc::new(AtomicU32::new(0));
    let bytes = Arc::new(AtomicU64::new(0));
    let monitors = Arc::new(AtomicU32::new(0));
    let ended = Arc::new(parking_lot::Mutex::new(None::<String>));
    let statuses = Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));

    let sink: EventSink = {
        let (frames, key_frames, bytes, monitors, ended, statuses) = (
            frames.clone(),
            key_frames.clone(),
            bytes.clone(),
            monitors.clone(),
            ended.clone(),
            statuses.clone(),
        );
        Arc::new(move |ev| match ev {
            SessionEvent::Video { key, data } => {
                frames.fetch_add(1, Ordering::Relaxed);
                bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                if key {
                    key_frames.fetch_add(1, Ordering::Relaxed);
                }
            }
            SessionEvent::Monitors(list) => {
                monitors.store(list.len() as u32, Ordering::Relaxed);
                println!("[e2e] 显示器清单：{} 台", list.len());
            }
            SessionEvent::Stats { fps, kbps, .. } => {
                println!("[e2e] 链路统计：{fps:.1} fps / {kbps:.0} kbps");
            }
            SessionEvent::Status(s) => {
                println!("[e2e] 状态：{s}");
                statuses.lock().push(s);
            }
            SessionEvent::Ended(r) => {
                println!("[e2e] 会话结束：{r}");
                *ended.lock() = Some(r);
            }
            SessionEvent::Clipboard(_) => {}
            SessionEvent::Log { level, msg } => {
                println!("[e2e] 被控端日志({level}): {msg}");
            }
            SessionEvent::DriverStatus {
                has_driver,
                active_count,
                driver_kind,
                hint,
            } => {
                println!("[e2e] 驱动状态: driver={driver_kind} has={has_driver} active={active_count} hint={hint}");
            }
            SessionEvent::Link { .. } => {}
            SessionEvent::TermReady
            | SessionEvent::TermOpened { .. }
            | SessionEvent::TermData { .. }
            | SessionEvent::TermExit { .. }
            | SessionEvent::TermErr { .. } => {}
        })
    };

    let (input_tx, input_rx) = mpsc::unbounded_channel::<ControlMsg>();
    let cfg = SessionConfig {
        server: a.server.clone(),
        id: a.id.clone(),
        role: Role::Controller,
        peer: Some(a.peer.clone()),
        fps: 30,
        ice_servers: Vec::new(),
        key_file: Some(
            a.key_file
                .clone()
                .unwrap_or_else(|| dir.join("identity.json")),
        ),
        auth: Some(a.pin.clone()),
    };

    let task = tokio::spawn(async move { session::run_session(cfg, input_rx, sink).await });

    let start = Instant::now();
    let mut control_sent = false;
    let mut ok = false;
    let mut hold_until: Option<Instant> = None;
    let mut last_move = Instant::now();
    let mut switch_pending = a.switch;
    let mut switch_done = false;
    while start.elapsed() < Duration::from_secs(a.seconds) {
        if ended.lock().is_some() {
            break;
        }
        // 保持阶段：每 2 秒重发一次鼠标，用于观察解锁后注入是否恢复
        if ok && a.hold > 0 {
            if last_move.elapsed() >= Duration::from_secs(2) {
                last_move = Instant::now();
                let (hx, hy) = (a.mx2.unwrap_or(a.mx), a.my2.unwrap_or(a.my));
                let _ = input_tx.send(ControlMsg::MouseMove { x: hx, y: hy });
                println!("[e2e] 保持中：重发 MouseMove({hx}, {hy})");
            }
            if hold_until.is_some_and(|t| Instant::now() >= t) {
                break;
            }
        }
        let n = frames.load(Ordering::Relaxed);
        if n >= a.frames {
            if let Some(idx) = switch_pending.take() {
                // 切显示器：计数清零再收一轮，验证切换后仍有画面
                println!("[e2e] 请求切换到显示器 {idx}");
                let _ = input_tx.send(ControlMsg::SwitchMonitor { index: idx });
                frames.store(0, Ordering::Relaxed);
                switch_done = true;
                control_sent = false; // 切屏后再发一次鼠标，验证新显示器的坐标原点
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
            ok = true;
            if a.hold == 0 {
                break;
            }
            if hold_until.is_none() {
                hold_until = Some(Instant::now() + Duration::from_secs(a.hold));
                println!("[e2e] 收满帧，保持连接 {} 秒", a.hold);
            }
        }
        // 有帧之后验证控制通道：强制刷新（应立即出 IDR）+ 一次鼠标移动
        if n > 0 && !control_sent {
            control_sent = true;
            let _ = input_tx.send(ControlMsg::Refresh);
            let _ = input_tx.send(ControlMsg::MouseMove { x: a.mx, y: a.my });
            println!(
                "[e2e] 已发送控制消息：Refresh + MouseMove({}, {})",
                a.mx, a.my
            );
            if let Some(k) = &a.key {
                let _ = input_tx.send(ControlMsg::KeyDown {
                    k: k.clone(),
                    mods: Vec::new(),
                });
                let _ = input_tx.send(ControlMsg::KeyUp {
                    k: k.clone(),
                    mods: Vec::new(),
                });
                println!("[e2e] 已发送键盘按键：{k}");
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    let n = frames.load(Ordering::Relaxed);
    let k = key_frames.load(Ordering::Relaxed);
    let b = bytes.load(Ordering::Relaxed);
    let m = monitors.load(Ordering::Relaxed);
    println!(
        "[e2e] 汇总：帧={n}（关键帧={k}） 字节={b} 显示器={m} 控制消息已发={control_sent} 切屏已做={switch_done} 耗时={:?}",
        start.elapsed()
    );
    if let Some(r) = ended.lock().clone() {
        println!("[e2e] 结束原因：{r}");
    }
    task.abort();

    if ok && m > 0 {
        println!("[e2e] PASS：链路通畅，能收到实时画面");
        std::process::exit(0);
    }
    println!("[e2e] FAIL：未在超时内收到足够视频帧");
    for s in statuses.lock().iter() {
        println!("[e2e]   状态轨迹: {s}");
    }
    std::process::exit(1);
}
