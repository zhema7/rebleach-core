// 视频管线：常驻 WGC 采集（GPU 缩放到 1920 宽）→ BGRA→I420 → openh264 编码 → DataChannel 发送
//          + 主控端收帧组装 + I420→RGBA（渲染前转换）
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use tracing::{debug, error, info, warn};

use crate::capture;
use crate::control;
use crate::protocol::ControlMsg;

// ============================================================
// 采集监督线程：持有采集/编码线程，支持运行中切换显示器；
// 暴露共享统计（码率自适应 + 上报主控端）
// ============================================================
#[derive(Debug, Clone, Copy)]
pub enum CaptureCmd {
    SwitchMonitor(u32),
    /// 重建采集+编码（刷新画面 / 分辨率变更后，立即出 IDR）
    Restart,
}

enum CaptureOutcome {
    Quit,
    Switch(u32),
    Restart,
}

/// 采集线程的控制句柄（切显示器）
#[derive(Clone)]
pub struct CaptureHandle {
    cmd_tx: std::sync::mpsc::Sender<CaptureCmd>,
}

impl CaptureHandle {
    pub fn switch_monitor(&self, index: u32) {
        let _ = self.cmd_tx.send(CaptureCmd::SwitchMonitor(index));
    }

    /// 刷新画面：保持当前显示器，重建采集与编码器
    pub fn restart(&self) {
        let _ = self.cmd_tx.send(CaptureCmd::Restart);
    }
}

/// 进程启动以来的毫秒数（惰性初始化起点）
fn proc_ms() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// 采集/编码统计（多线程共享）
pub struct Stats {
    /// 累计已编码发送帧数
    pub frames: AtomicU32,
    /// 累计已发送字节数
    pub bytes: AtomicU64,
    /// 当前目标码率（bps）：发送侧按 bufferedAmount 自适应调整，编码线程每帧读取生效
    pub bitrate: AtomicU32,
    /// DataChannel 里尚未发出的字节数（send_loop 写入，编码线程据此在编码前丢帧）
    pub backlog: AtomicU64,
    /// 主控端手动指定画质后为 true：关闭自动码率自适应，避免被立刻改回去
    pub manual: AtomicBool,
    /// 基础目标帧率（会话建立时的 fps，不随降级变化）
    pub fps_base: AtomicU32,
    /// 当前目标帧率（降级时会下调，采集/编码线程读取）
    pub fps_target: AtomicU32,
    /// 当前降级档位：0=全画质 1=1280宽 2=960宽
    pub degrade: AtomicU32,
    /// 采集配置代号：降级档位变化时自增，采集线程据此重建采集与编码
    pub epoch: AtomicU64,
    /// 上一统计窗口的平均转换耗时（微秒）
    pub conv_us: AtomicU64,
    /// 上一统计窗口的平均编码耗时（微秒）
    pub enc_us: AtomicU64,
    /// 上次实际出帧时刻（进程启动起的毫秒数）：看门狗据此判断管线是否停摆
    pub last_emit_ms: AtomicU64,
    /// 主控端上报的丢失帧率（千分比，M1 应用层 AIMD 依据）
    pub remote_loss_permille: AtomicU32,
    /// 主控端上报的 ctrl 通道 RTT（ms）
    pub remote_rtt_ms: AtomicU32,
    /// 主控端上报的有效吞吐（kbps）
    pub remote_goodput_kbps: AtomicU32,
    /// 主控端上报的接收帧率 ×10（静止画面时避免盲目抬码率）
    pub remote_fps_x10: AtomicU32,
    /// 主控端请求关键帧的计数器：编码线程发现变化即 `force_intra_frame`
    pub keyframe_req: AtomicU64,
    /// 编码线程已消费到的 keyframe_req 值
    pub keyframe_done: AtomicU64,
}

/// 背压水位按"时间"折算：队列里最多允许积压 250ms 的数据。
/// 固定字节水位在慢链路（如 300kbps 中继）上会变成数秒延迟，必须跟着码率走。
const BACKLOG_WINDOW_MS: u64 = 250;

impl Stats {
    pub fn new(bitrate: u32) -> Self {
        Self {
            frames: AtomicU32::new(0),
            bytes: AtomicU64::new(0),
            bitrate: AtomicU32::new(bitrate),
            backlog: AtomicU64::new(0),
            manual: AtomicBool::new(false),
            fps_base: AtomicU32::new(30),
            fps_target: AtomicU32::new(30),
            degrade: AtomicU32::new(0),
            epoch: AtomicU64::new(0),
            conv_us: AtomicU64::new(0),
            enc_us: AtomicU64::new(0),
            last_emit_ms: AtomicU64::new(proc_ms()),
            remote_loss_permille: AtomicU32::new(0),
            remote_rtt_ms: AtomicU32::new(0),
            remote_goodput_kbps: AtomicU32::new(0),
            remote_fps_x10: AtomicU32::new(0),
            keyframe_req: AtomicU64::new(0),
            keyframe_done: AtomicU64::new(0),
        }
    }

    /// 记录一次实际出帧（真实帧或静止保活帧）
    pub fn touch_emit(&self) {
        self.last_emit_ms.store(proc_ms(), Ordering::Relaxed);
    }

    /// 距上次出帧的毫秒数：看门狗判断管线是否停摆
    pub fn ms_since_emit(&self) -> u64 {
        proc_ms().saturating_sub(self.last_emit_ms.load(Ordering::Relaxed))
    }

    /// 主控端画质档位：low/balanced/high/custom。
    /// 设置后关闭自动码率自适应，直到会话重新建立。
    pub fn set_quality(&self, level: &str, custom_kbps: u32) {
        let bps = match level {
            "low" => 600_000,
            "balanced" => 2_000_000,
            "high" => 4_000_000,
            "custom" => (custom_kbps.max(150) as u32 * 1000).clamp(150_000, 8_000_000),
            _ => return,
        };
        self.manual.store(true, Ordering::Relaxed);
        self.bitrate.store(bps, Ordering::Relaxed);
        info!("🎚️ 主控端指定画质 {level} → {} kbps", bps / 1000);
    }

    /// 主控端指定编码宽度上限（0=保持不变，1=原画不限制，>=640=像素上限）。
    /// 变更后自增 epoch，采集线程会按新宽度重建采集与编码器。
    pub fn set_encode_cap(&self, max_width: u32) {
        if max_width == 0 {
            return; // 旧主控端：不改编码宽度
        }
        USER_MAX_WIDTH.store(max_width, Ordering::Relaxed);
        let lvl = self.degrade.load(Ordering::Relaxed).min(2) as usize;
        set_active_width(level_width(lvl));
        self.epoch.fetch_add(1, Ordering::Relaxed);
        if max_width == 1 {
            info!("🖼️ 主控端画质：原画（不限制编码宽度）");
        } else {
            info!("🖼️ 主控端画质：编码宽度上限 {max_width}px");
        }
    }
}

/// 默认码率：1920x1200 屏幕内容 1080p 下 2Mbps 起步即可，靠自适应按需上升
pub const DEFAULT_BITRATE: u32 = 2_000_000;

/// 负载自适应：降级档位对应的编码宽度上限。
/// 档位 0 不使用此表（用主控端选择的画质上限），1/2 档固定为 1280/960。
const DEGRADE_WIDTHS: [u32; 3] = [MAX_ENCODE_WIDTH, 1280, 960];
const DEGRADE_FPS_CAPS: [u32; 3] = [0, 15, 10];

/// 主控端选择的编码宽度上限：默认 1920；1 表示不限制（原画）；>=640 为具体像素
static USER_MAX_WIDTH: AtomicU32 = AtomicU32::new(MAX_ENCODE_WIDTH);
/// 当前实际生效的编码宽度上限（考虑降级档位后）
static ACTIVE_MAX_WIDTH: AtomicU32 = AtomicU32::new(MAX_ENCODE_WIDTH);

/// 当前生效的编码宽度上限（0 表示不限制）
fn max_encode_width() -> u32 {
    ACTIVE_MAX_WIDTH.load(Ordering::Relaxed)
}

fn set_active_width(w: u32) {
    ACTIVE_MAX_WIDTH.store(if w == 1 { 0 } else { w }, Ordering::Relaxed);
}

/// 某降级档位实际使用的编码宽度上限（0 = 不限制）
/// 档位 0 用主控端选择值；低档位取主控端选择值与固定档位宽度的较小者
fn level_width(lvl: usize) -> u32 {
    let user = USER_MAX_WIDTH.load(Ordering::Relaxed);
    let user_cap = if user == 1 { 0 } else { user.max(640) };
    let lvl = lvl.min(2);
    if lvl == 0 {
        user_cap
    } else if user_cap == 0 {
        DEGRADE_WIDTHS[lvl]
    } else {
        user_cap.min(DEGRADE_WIDTHS[lvl])
    }
}

/// 启动采集监督线程：内部循环拉起采集+编码线程；
/// 收到 SwitchMonitor 命令时平滑重建采集（编码器随之按新分辨率重建）
/// 编码线程 → 发送任务的队列容量。故意比 `MAX_SEND_QUEUE` 大：
/// 发送侧才能在积压时按「丢最旧」裁剪，而不是让编码线程丢新帧。
pub const SEND_QUEUE_CAP: usize = 8;

pub fn spawn_capture_supervisor(
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    fps: u64,
    stats: Arc<Stats>,
) -> CaptureHandle {
    // 远控在"整机被 CPU/GPU 任务打满"时最有价值，默认优先级会被饿死
    // （实测 python 视频生成占满 CPU 时转换耗时 1.6ms → 14249ms，fps 30 → 0.1）
    boost_process_priority();
    stats.fps_base.store(fps as u32, Ordering::Relaxed);
    stats.fps_target.store(fps as u32, Ordering::Relaxed);
    stats.degrade.store(0, Ordering::Relaxed);
    // 新会话从默认 1080p 档开始，避免上一次会话的画质/降级档位残留（文档 P1-C）
    USER_MAX_WIDTH.store(MAX_ENCODE_WIDTH, Ordering::Relaxed);
    set_active_width(MAX_ENCODE_WIDTH);
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<CaptureCmd>();
    // 自愈看门狗：管线连续 6s 没有任何出帧（含静止保活）→ 重建采集；
    // 60s 内连续 3 次自愈仍不恢复 → 重启 Agent 进程
    let alive = Arc::new(AtomicBool::new(true));
    {
        let stats = stats.clone();
        let cmd_tx = cmd_tx.clone();
        let alive = alive.clone();
        std::thread::spawn(move || {
            let mut last_heal = Instant::now();
            let mut heals: u32 = 0;
            while alive.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(500));
                if !alive.load(Ordering::Relaxed) {
                    break;
                }
                let idle_ms = stats.ms_since_emit();
                if idle_ms < 6_000 {
                    continue;
                }
                if last_heal.elapsed() > Duration::from_secs(60) {
                    heals = 0;
                }
                heals += 1;
                last_heal = Instant::now();
                warn!(
                    "🩺 出帧已中断 {:.1}s（第 {heals} 次自愈）：重建采集",
                    idle_ms as f64 / 1000.0
                );
                let _ = cmd_tx.send(CaptureCmd::Restart);
                stats.touch_emit();
                if heals >= 3 {
                    restart_agent_process("采集管线连续 3 次自愈无效");
                }
            }
        });
    }
    let alive_sup = alive.clone();
    std::thread::spawn(move || {
        let mut monitor: Option<usize> = None;
        let mut failures: u32 = 0;
        loop {
            match capture_thread(tx.clone(), monitor, &cmd_rx, &stats) {
                Ok(CaptureOutcome::Switch(i)) => {
                    monitor = Some(i as usize);
                    failures = 0;
                }
                Ok(CaptureOutcome::Restart) => failures = 0,
                Ok(CaptureOutcome::Quit) => break,
                Err(e) => {
                    failures += 1;
                    if failures <= 3 || failures % 10 == 0 {
                        error!("采集启动失败（第 {failures} 次）: {e:#}，2s 后重试");
                    }
                    // 不退出监督线程：显示器重编号/拓扑变化后能自动恢复，
                    // "刷新画面"指令也仍然有效（旧版这里直接 break，导致刷新救不回来）
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
        }
        alive_sup.store(false, Ordering::Relaxed);
        info!("采集监督线程结束");
    });
    CaptureHandle { cmd_tx }
}

/// 自愈最后手段：延迟 2s 拉起自身副本并退出当前进程。
/// 用 cmd 延迟启动是为了等单实例互斥、安装目录文件句柄释放干净
pub fn restart_agent_process(reason: &str) -> ! {
    warn!("♻️ Agent 自愈重启：{reason}");
    // 防止故障机器上无限重启：环境变量里记次数，超过 3 次放弃
    let n: u32 = std::env::var("REBLEACH_SELF_HEAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if n >= 3 {
        error!("自愈重启已达上限（{n} 次），放弃重启，等待人工处理");
        std::process::exit(1);
    }
    if let Ok(exe) = std::env::current_exe() {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            let cmd = format!(
                "/C timeout /t 2 /nobreak >nul & start \"\" \"{}\"",
                exe.display()
            );
            let _ = std::process::Command::new("cmd")
                .arg(cmd)
                .env("REBLEACH_SELF_HEAL", (n + 1).to_string())
                .creation_flags(CREATE_NO_WINDOW)
                .spawn();
        }
        #[cfg(not(windows))]
        {
            let _ = std::process::Command::new(exe).spawn();
        }
    }
    std::process::exit(0);
}

/// 转换完成的帧 + 采集侧耗时，交给编码线程（流水线）
struct Prepared {
    frame: I420Frame,
    conv_ms: f64,
    wait_ms: f64,
}

fn capture_thread(
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    monitor: Option<usize>,
    cmd_rx: &std::sync::mpsc::Receiver<CaptureCmd>,
    stats: &Arc<Stats>,
) -> Result<CaptureOutcome> {
    // 采集线程是出帧入口：与编码线程同档，整机繁忙时不被外部任务饿死
    unsafe {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
        };
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
    // 降级档位在采集监督层读取：降级后重建采集 + 编码，分辨率/帧率随之下调
    let gen_at_start = stats.epoch.load(Ordering::Relaxed);
    let fps = (stats.fps_target.load(Ordering::Relaxed) as u64).max(1);
    // 常驻 WGC 会话 + GPU 下采样：帧到达时在 GPU 上直接缩到编码尺寸再回读（9.2MB），
    // 省掉 xcap 每帧全尺寸回读 + 两次 20MB 克隆 + 通道交换
    // DDA 采集线程按同一目标帧率限速，先写入全局再建采集
    capture::set_target_fps(fps);
    let lvl = stats.degrade.load(Ordering::Relaxed).min(2) as usize;
    let active_w = level_width(lvl);
    set_active_width(active_w);
    let cap = capture::Capture::start_for(monitor).context("启动屏幕采集失败")?;
    let (w, h) = (cap.src_w as i32, cap.src_h as i32);
    let (ew, eh) = (cap.dst_w, cap.dst_h);
    let backend = cap.backend;
    // 鼠标映射：主控端发来的是编码帧坐标，换算到当前显示器的物理坐标
    control::set_mapping(
        w as f64 / ew as f64,
        h as f64 / eh as f64,
        cap.origin_x,
        cap.origin_y,
    );
    // 记录当前观看的显示器：主控端改分辨率时作用于它；新窗口默认落在这块屏
    control::set_active_monitor(&cap.name, cap.origin_x, cap.origin_y, cap.src_w, cap.src_h);
    info!(
        "🖥️ 采集启动: 显示器 {}x{} → 编码 {}x{}（backend={} + GPU 下采样 + 转换/编码流水）",
        w, h, ew, eh, backend
    );

    // WGC 只在画面变化时出帧，桌面静止时可能长时间等不到首帧，
    // 用一次性 GDI 截屏兜底，否则主控端会一直停在「等待画面」
    let mut first_pending = cap.first_frame(Duration::from_millis(700));

    // 转换（多线程）与编码（单线程）分居两线程：串行时单帧 = 转换 + 编码 ≈ 30 + 17ms，
    // 顶在 19fps；流水后单帧上限只取两者较大值。队列容量 2，编码跟不上时丢新帧保低延迟。
    let (ftx, frx) = std::sync::mpsc::sync_channel::<Prepared>(2);
    // I420 缓冲池：编码完的缓冲环回采集线程复用，避免每帧重新分配 ~3.5MB
    let (pool_tx, pool_rx) = std::sync::mpsc::sync_channel::<I420Frame>(2);
    let enc_stats = stats.clone();
    let enc = std::thread::spawn(move || {
        encode_loop(
            frx, pool_tx, tx, fps, w, h, ew, eh, backend, enc_stats,
        )
    });

    let interval = Duration::from_millis(1000 / fps.max(1));
    let mut dropped: u64 = 0;
    let outcome = loop {
        // 负载自适应调整了档位：重建采集与编码（分辨率/帧率随档位变化）
        if stats.epoch.load(Ordering::Relaxed) != gen_at_start {
            let lvl_now = stats.degrade.load(Ordering::Relaxed).min(2) as usize;
            let w_now = level_width(lvl_now);
            let fps_now = stats.fps_target.load(Ordering::Relaxed);
            info!(
                "🔁 负载档位变化 → 重建采集（档位 {lvl_now} / 目标宽 {w_now}px / {fps_now}fps）"
            );
            break CaptureOutcome::Restart;
        }
        // 画面静止时 WGC 不出帧，recv_timeout 醒来看切换命令，避免命令饿死
        match cmd_rx.try_recv() {
            Ok(CaptureCmd::SwitchMonitor(i)) => {
                info!("🖥️ 切换采集到显示器 #{i}");
                break CaptureOutcome::Switch(i);
            }
            Ok(CaptureCmd::Restart) => {
                info!("🔄 刷新画面：重建采集与编码");
                break CaptureOutcome::Restart;
            }
            Err(_) => {}
        }
        let t0 = Instant::now();

        let (buf, wait) = match first_pending.take() {
            Some(f) => (f, 0.0),
            None => {
                let tw = Instant::now();
                match cap.recv_timeout(Duration::from_millis(200)) {
                    Some(b) => (b, tw.elapsed().as_secs_f64() * 1000.0),
                    None => {
                        // 采集线程已退出时 recv 会立刻返回（不等超时），据此识别后端死亡，
                        // 交给监督层重建；否则会在这里空转到天荒地老，主控端画面永久停住
                        if tw.elapsed() < Duration::from_millis(5) {
                            error!("采集后端已退出（通道断开），重建采集");
                            break CaptureOutcome::Restart;
                        }
                        continue; // 超时：回循环头看命令
                    }
                }
            }
        };
        let mut frame = pool_rx
            .try_recv()
            .unwrap_or_else(|_| I420Frame::new(ew, eh));
        let tc = Instant::now();
        bgra_to_i420(ew, eh, &buf, &mut frame);
        let conv_ms = tc.elapsed().as_secs_f64() * 1000.0;
        // BGRA 回读缓冲环回采集回调复用，省掉每帧 9.2MB 的重新分配
        cap.recycle(buf);
        match ftx.try_send(Prepared {
            frame,
            conv_ms,
            wait_ms: wait,
        }) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => dropped += 1,
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                info!("编码线程已退出");
                break CaptureOutcome::Quit;
            }
        }
        // 限速：WGC 在高刷屏上最多 60fps 出帧，按目标帧率留出空档，避免占满 CPU
        if let Some(rest) = interval.checked_sub(t0.elapsed()) {
            std::thread::sleep(rest);
        }
    };
    cap.stop();
    drop(ftx);
    info!("采集循环结束（编码跟不上丢弃 {dropped} 帧）");
    let _ = enc.join();
    Ok(outcome)
}

/// 编码线程：收转换好的帧 → openh264 编码 → 打包 [1B key][Annex-B] 交发送任务
/// 码率自适应：每帧读取 stats.bitrate，变化时调用 set_bitrate 生效
fn encode_loop(
    rx: std::sync::mpsc::Receiver<Prepared>,
    pool_tx: std::sync::mpsc::SyncSender<I420Frame>,
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    fps: u64,
    w: i32,
    h: i32,
    ew: u32,
    eh: u32,
    backend: &'static str,
    stats: Arc<Stats>,
) -> Result<()> {
    use openh264::OpenH264API;
    use openh264::encoder::Encoder;

    // 编码线程是出帧关键路径：提一档优先级，整机繁忙时不被转换线程/UI 抢占
    // （实测争用时单帧 9ms 会被拖到 16–18ms）
    unsafe {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
        };
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
    // 屏幕内容 + 码率控制：1920x1200 下 6Mbps 足够
    // 注意 UsageType：ScreenContentRealTime 会启用屏幕内容专用工具，在本机实测
    // 1920x1200 单帧 39ms；CameraVideoRealTime 同样码率下单帧只需 10ms，故用后者
    let mut cur_bps = stats.bitrate.load(Ordering::Relaxed);
    let mut encoder =
        Encoder::with_api_config(OpenH264API::from_source(), encoder_config(cur_bps, fps))
            .context("openh264 编码器初始化失败")?;

    let mut last_log = Instant::now();
    // 上次发出 IDR 的时刻（周期关键帧兜底，见下）
    let mut last_idr = Instant::now();
    // 上次实际出帧的时刻（背压限频用）
    let mut last_emit = Instant::now();
    // 上一次因码率变化重建编码器的时刻：限制最小间隔，避免 5s 内连续两次重建（文档 P1-A）
    let mut last_enc_rebuild: Option<Instant> = None;
    let mut last_keepalive_log = Instant::now();
    // 背压滞回状态：高水位进入限频，低水位退出
    let mut throttled = false;
    let mut frames_in: u64 = 0;
    let mut frames_sent: u64 = 0;
    let mut skipped: u64 = 0;
    let mut bytes_sent: u64 = 0;
    let mut conv_ms: f64 = 0.0;
    let mut enc_ms: f64 = 0.0;
    let mut wait_ms: f64 = 0.0;
    // 缓存的 SPS/PPS：关键帧若不自带参数集，则补在最前面（前端/解码器首次即可 configure）
    let mut sps: Option<Vec<u8>> = None;
    let mut pps: Option<Vec<u8>> = None;
    // 负载自适应滞回计数（5s 一个统计窗口）
    let mut bad_windows: u32 = 0;
    let mut good_windows: u32 = 0;

    // 最后一帧的编码结果：桌面静止（WGC/DDA 不出帧）时按 2fps 重发，
    // 否则主控端会停在最后一帧上，看起来像"死机"
    let mut last_payload: Option<Vec<u8>> = None;

    loop {
        let p = match rx.recv_timeout(Duration::from_millis(300)) {
            Ok(p) => p,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Some(pl) = last_payload.as_ref() {
                    if last_emit.elapsed() >= Duration::from_millis(500) {
                        let _ = tx.try_send(pl.clone());
                        last_emit = Instant::now();
                        stats.touch_emit();
                        frames_sent += 1;
                        bytes_sent += pl.len() as u64;
                        stats.frames.fetch_add(1, Ordering::Relaxed);
                        // 显式日志：证明静止保活确实生效（文档 P1-B 要求可观测）
                        if last_keepalive_log.elapsed() >= Duration::from_secs(10) {
                            info!("💤 静止保活生效：桌面无新帧，每 500ms 重发最后一帧（约 2fps）");
                            last_keepalive_log = Instant::now();
                        }
                        stats.bytes.fetch_add(pl.len() as u64, Ordering::Relaxed);
                    }
                }
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        // 动态码率：openh264 0.8 的 Encoder 不支持运行中改码率，
        // 目标偏差超过 8% 时重建编码器并强制下一帧 IDR（新 SPS/PPS 随 IDR 下发）
        let target = stats.bitrate.load(Ordering::Relaxed);
        if target != cur_bps {
            let drift = (target as f64 - cur_bps as f64).abs() / cur_bps.max(1) as f64;
            // 阈值 8%：自适应每次升降幅度在 10%~50%，太小则改了目标却不重建编码器、实际码率不生效。
            // 上升方向限制最小间隔 10s（文档 P1-A：5s 内连续两次重建会明显掉帧）；
            // 下降方向（拥塞）2s 即可生效，否则 AIMD 的快速回退会被编码器拖住。
            let downshift = target < cur_bps;
            let can_rebuild = last_enc_rebuild.map_or(true, |t| {
                t.elapsed()
                    >= if downshift {
                        Duration::from_secs(2)
                    } else {
                        Duration::from_secs(10)
                    }
            }) || (stats.manual.load(Ordering::Relaxed) && last_enc_rebuild.is_none());
            if drift > 0.08 && can_rebuild {
                match Encoder::with_api_config(
                    OpenH264API::from_source(),
                    encoder_config(
                        target,
                        stats.fps_target.load(Ordering::Relaxed).max(1) as u64,
                    ),
                ) {
                    Ok(mut e2) => {
                        e2.force_intra_frame();
                        encoder = e2;
                        sps = None;
                        pps = None;
                        cur_bps = target;
                        last_enc_rebuild = Some(Instant::now());
                        last_idr = Instant::now();
                        info!(
                            "🎚️ 码率调整 → {} kbps（编码器重建 + 强制 IDR）",
                            target / 1000
                        );
                    }
                    Err(e) => warn!("编码器重建失败（保持 {cur_bps} bps）: {e}"),
                }
            } else if drift <= 0.08 {
                cur_bps = target; // 小偏差不同步，避免频繁重建
            } else {
                debug!(
                    "码率目标 {} kbps 已变化，但距上次重建不足 10s，延后生效",
                    target / 1000
                );
            }
        }

        frames_in += 1;
        conv_ms += p.conv_ms;
        wait_ms += p.wait_ms;

        // 网络侧背压：DataChannel 里还压着大量未发出的数据时，本帧直接丢弃、不进编码器。
        // 必须在编码前丢：编码后再丢 P 帧会切断参考链，解码端花屏直到下一个 IDR。
        // 水位按当前码率折成时间（250ms）：慢链路上 96KB 是 3~4 秒，也就成了"卡"的主因。
        let backlog = stats.backlog.load(Ordering::Relaxed);
        let high = ((cur_bps as u64 / 8 * BACKLOG_WINDOW_MS) / 1000).clamp(6_000, 200_000);
        let low = (high / 3).max(2_000);
        if backlog > high {
            throttled = true;
        } else if backlog < low {
            throttled = false;
        }
        if throttled && last_emit.elapsed() < Duration::from_millis(350) {
            let _ = pool_tx.try_send(p.frame);
            skipped += 1;
            continue;
        }

        // 主控端丢帧后请求关键帧（M1）：与周期 IDR 互补，弱网下 2s 内自愈
        let req = stats.keyframe_req.load(Ordering::Relaxed);
        if req != stats.keyframe_done.load(Ordering::Relaxed) {
            stats.keyframe_done.store(req, Ordering::Relaxed);
            encoder.force_intra_frame();
            info!("🖼️ 收到关键帧请求（累计 {req}），强制 IDR");
        }

        // 周期关键帧兜底：openh264 的 IntraFramePeriod 在本版本实测不生效
        // （60s 内只有首个 IDR），解码端一旦错过首帧就会永久黑屏。
        // 慢链路（<1Mbps）上 IDR 本身就要吃掉一两秒的带宽，改成 5 秒一次减少卡顿尖峰。
        let idr_period = if cur_bps < 1_000_000 { 5 } else { 2 };
        if last_idr.elapsed() >= Duration::from_secs(idr_period) {
            encoder.force_intra_frame();
        }

        let te = Instant::now();
        let bitstream = encoder.encode(&p.frame).context("H.264 编码失败")?;
        enc_ms += te.elapsed().as_secs_f64() * 1000.0;
        // 缓冲用完立刻还给采集线程（队列满就丢弃，下一帧重新分配）
        let _ = pool_tx.try_send(p.frame);

        let key = matches!(
            bitstream.frame_type(),
            openh264::encoder::FrameType::IDR | openh264::encoder::FrameType::I
        );
        let mut raw = Vec::with_capacity(64 * 1024);
        bitstream.write_vec(&mut raw);
        let nals: Vec<&[u8]> = split_annexb(&raw);

        let mut has_sps = false;
        let mut has_pps = false;
        for nal in &nals {
            let Some(&hdr) = nal.first() else { continue };
            match hdr & 0x1f {
                7 => {
                    has_sps = true;
                    sps = Some(nal.to_vec());
                }
                8 => {
                    has_pps = true;
                    pps = Some(nal.to_vec());
                }
                _ => {}
            }
        }

        if raw.is_empty() {
            // 编码器按帧率/码率主动跳过（画面无变化或超出预算）
            skipped += 1;
        } else {
            // [1B key][Annex-B]：key 供前端选 EncodedVideoChunk.type
            let mut payload = Vec::with_capacity(raw.len() + 64);
            payload.push(key as u8);
            if key && !(has_sps && has_pps) {
                for ps in [sps.as_deref(), pps.as_deref()].into_iter().flatten() {
                    payload.extend_from_slice(&[0, 0, 0, 1]);
                    payload.extend_from_slice(ps);
                }
            }
            payload.extend_from_slice(&raw);
            let len = payload.len() as u64;
            let keepalive_copy = payload.clone();
            match tx.try_send(payload) {
                Ok(()) => {
                    last_payload = Some(keepalive_copy);
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(p)) => {
                    // 发送队列满：send_loop 被 DataChannel 背压卡住。
                    // 这帧（通常是 P 帧）过了也不影响画面，丢了保低延迟。
                    // 如果丢的是 I 帧，下一帧会被编码器自动判定为 P 帧——
                    // 没关系，我们 2s 周期强制 IDR，最多 2 秒就出关键帧了。
                    skipped += 1;
                    // p 被丢弃，payload 随 p 一起 drop
                    let _ = p;
                    continue;
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    info!("接收端关闭，停止采集");
                    break;
                }
            }
            frames_sent += 1;
            bytes_sent += len;
            last_emit = Instant::now();
            stats.touch_emit();
            if key {
                last_idr = Instant::now();
            }
            stats.frames.fetch_add(1, Ordering::Relaxed);
            stats.bytes.fetch_add(len, Ordering::Relaxed);
        }

        if last_log.elapsed() >= Duration::from_secs(5) {
            let secs = last_log.elapsed().as_secs_f64();
            let n = frames_in.max(1) as f64;
            let avg_conv = conv_ms / n;
            let avg_enc = enc_ms / n;
            let fps_sent = frames_sent as f64 / secs;
            info!(
                "📊 采集 {}x{} (backend={}) → {}x{}: {:.1} fps, {:.0} KB/s, 转换 {:.1} + 编码 {:.1} + 等帧 {:.1} ms/帧, 跳过 {}",
                w,
                h,
                backend,
                ew,
                eh,
                fps_sent,
                bytes_sent as f64 / 1024.0 / secs,
                avg_conv,
                avg_enc,
                wait_ms / n,
                skipped
            );
            // 供主控端与降级判定使用的窗口均值
            stats.conv_us.store((avg_conv * 1000.0) as u64, Ordering::Relaxed);
            stats.enc_us.store((avg_enc * 1000.0) as u64, Ordering::Relaxed);
            adapt_load(
                &stats,
                avg_conv,
                avg_enc,
                fps_sent,
                frames_in,
                &mut bad_windows,
                &mut good_windows,
            );
            last_log = Instant::now();
            frames_in = 0;
            frames_sent = 0;
            skipped = 0;
            bytes_sent = 0;
            conv_ms = 0.0;
            enc_ms = 0.0;
            wait_ms = 0.0;
        }
    }
    info!("编码线程结束");
    Ok(())
}

/// openh264 编码参数（初始创建与码率调整重建共用）
fn encoder_config(bps: u32, fps: u64) -> openh264::encoder::EncoderConfig {
    use openh264::encoder::{
        BitRate, Complexity, EncoderConfig, FrameRate, IntraFramePeriod, Profile, RateControlMode,
        UsageType,
    };
    EncoderConfig::new()
        .usage_type(UsageType::CameraVideoRealTime)
        .rate_control_mode(RateControlMode::Bitrate)
        .bitrate(BitRate::from_bps(bps))
        .max_frame_rate(FrameRate::from_hz(fps.max(1) as f32))
        .intra_frame_period(IntraFramePeriod::from_num_frames((fps.max(1) * 2) as u32))
        .complexity(Complexity::Low)
        .profile(Profile::Baseline)
        .skip_frames(true)
}

// ============================================================
// 负载自适应：系统被外部任务（CUDA/CPU 密集）打满时降分辨率与帧率，
// 保证远控"能看能操作"，负载回落后自动升回
// ============================================================
/// 某档位的实际目标帧率（帧率上限与基础帧率取小）
fn level_fps(stats: &Stats, lvl: usize) -> u32 {
    let base = stats.fps_base.load(Ordering::Relaxed).max(1);
    let cap = DEGRADE_FPS_CAPS[lvl.min(2)];
    if cap == 0 {
        base
    } else {
        cap.min(base)
    }
}

/// 切换降级档位：更新目标帧率/编码宽度，并自增 gen 让采集线程重建采集+编码
fn apply_load_level(stats: &Stats, lvl: usize) {
    let lvl = lvl.min(2);
    stats.degrade.store(lvl as u32, Ordering::Relaxed);
    stats.fps_target.store(level_fps(stats, lvl), Ordering::Relaxed);
    set_active_width(level_width(lvl));
    stats.epoch.fetch_add(1, Ordering::Relaxed);
}

/// 每 5s 统计窗口评估一次管线压力：
/// - 转换 > 200ms 或 编码 > 30ms 或 fps < 目标 30% → 判为过载
/// - 连续 2 个窗口过载 → 降一档
/// - 连续 3 个窗口健康（转换 <50ms、编码 <20ms、fps > 目标 80%）→ 升一档
fn adapt_load(
    stats: &Stats,
    conv_ms: f64,
    enc_ms: f64,
    fps: f64,
    frames_in: u64,
    bad: &mut u32,
    good: &mut u32,
) {
    // 画面静止时 WGC 根本不出帧（这不是过载），窗口内没有输入帧就跳过判定
    if frames_in == 0 {
        *bad = 0;
        *good = 0;
        return;
    }
    let lvl = stats.degrade.load(Ordering::Relaxed).min(2) as usize;
    let target = stats.fps_target.load(Ordering::Relaxed).max(1) as f64;
    // 判据只看"真实处理耗时"：桌面静止时等帧长但转换/编码都很快，
    // 那是"没帧可拿"（视频层已在做 2fps 保活），绝不能当成负载过高去降级重建
    let overloaded = conv_ms > 50.0 || enc_ms > 30.0;
    let healthy =
        conv_ms < 20.0 && enc_ms < 15.0 && frames_in >= 5 && fps > target * 0.8;

    if overloaded {
        *bad += 1;
        *good = 0;
    } else if healthy {
        *good += 1;
        *bad = 0;
    } else {
        *bad = 0;
        *good = 0;
    }

    if overloaded && lvl == 2 {
        warn!(
            "⚠️ 系统负载过高（转换 {conv_ms:.0}ms / 编码 {enc_ms:.0}ms / {fps:.1}fps），已是最低档，请结束抢占 CPU/GPU 的任务"
        );
    }

    if *bad >= 2 && lvl < 2 {
        let next = lvl + 1;
        apply_load_level(stats, next);
        warn!(
            "⚠️ 系统负载过高（转换 {conv_ms:.0}ms / 编码 {enc_ms:.0}ms / {fps:.1}fps），已降级到 {}x{}@{}fps",
            level_width(next),
            level_width(next) * 9 / 16,
            level_fps(stats, next)
        );
        *bad = 0;
    } else if *good >= 3 && lvl > 0 {
        let next = lvl - 1;
        apply_load_level(stats, next);
        info!(
            "✅ 系统负载恢复，画质升回 {}x{}@{}fps",
            level_width(next),
            level_width(next) * 9 / 16,
            level_fps(stats, next)
        );
        *good = 0;
    }
}

/// 整机繁忙时默认优先级会被饿死（实测 python 占满 CPU 后转换 1.6ms → 14249ms），
/// 远控恰恰是"机器卡了要远程处理"时最有价值的工具，进程提到 ABOVE_NORMAL
fn boost_process_priority() {
    unsafe {
        use windows::Win32::System::Threading::{
            GetCurrentProcess, SetPriorityClass, ABOVE_NORMAL_PRIORITY_CLASS,
        };
        let _ = SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS);
    }
    info!("⚡ 进程优先级已提升为 ABOVE_NORMAL");
}

// ============================================================
// async 发送任务（M1）：媒体通道不可靠无序 + 1100B 分片 + 令牌桶 pacing
// 队列深度 ≤2 帧，超出丢最旧；每 500ms 按主控端反馈做 AIMD 码率决策
// ============================================================
/// 发送队列深度上限（帧）。超出丢最旧的非关键帧
pub const MAX_SEND_QUEUE: usize = 2;

/// 入队并按 drop-oldest 裁剪：优先保留关键帧
fn push_frame(queue: &mut VecDeque<Vec<u8>>, frame: Vec<u8>) {
    queue.push_back(frame);
    while queue.len() > MAX_SEND_QUEUE {
        // 从最旧的开始找第一个非关键帧；全是关键帧时丢最旧的
        let idx = queue
            .iter()
            .position(|f| f.first() != Some(&1))
            .unwrap_or(0);
        queue.remove(idx);
    }
}

/// 令牌桶等待：按目标码率匀速放行 `len` 字节，避免瞬时打满 SCTP 缓冲
async fn pace(tokens: &mut f64, last: &mut Instant, bps: u32, len: usize) {
    let rate = bps.max(150_000) as f64 / 8.0; // B/s
    let burst = (rate * 0.2).max(2.0 * crate::media::FRAG_BYTES as f64);
    loop {
        let now = Instant::now();
        *tokens = (*tokens + now.duration_since(*last).as_secs_f64() * rate).min(burst);
        *last = now;
        if *tokens >= len as f64 {
            *tokens -= len as f64;
            return;
        }
        let wait = ((len as f64 - *tokens) / rate).min(0.05);
        tokio::time::sleep(Duration::from_secs_f64(wait)).await;
    }
}

/// `legacy=true` 时兼容 0.2.x 主控端：走旧 `[u32 seq][u32 len]` + 16KB 分片（无帧头、无 pacing）
pub async fn send_loop(
    dc: Arc<webrtc::data_channel::RTCDataChannel>,
    ctrl: Arc<webrtc::data_channel::RTCDataChannel>,
    mut rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    stats: Arc<Stats>,
    legacy: bool,
) -> Result<()> {
    let seq = AtomicU32::new(0);
    let mut tokens = 0f64;
    let mut last_refill = Instant::now();
    let mut queue: VecDeque<Vec<u8>> = VecDeque::with_capacity(MAX_SEND_QUEUE + 1);

    // 500ms 一个决策周期（方案 4.4：反馈 200ms、决策 500ms）
    let mut last_frames = 0u32;
    let mut last_bytes = 0u64;
    let mut last_tick = Instant::now();
    let mut prev_buffered = 0u64;
    let mut ticks: u32 = 0;

    loop {
        // 定时决策：不放在 select 里，避免持续有帧时永远轮不到
        if last_tick.elapsed() >= Duration::from_millis(500) {
            let dt = last_tick.elapsed().as_secs_f32();
            last_tick = Instant::now();
            ticks += 1;
            let f = stats.frames.load(Ordering::Relaxed);
            let b = stats.bytes.load(Ordering::Relaxed);
            let produced = b - last_bytes;
            let fps = (f - last_frames) as f32 / dt;
            let kbps = produced as f32 * 8.0 / (dt * 1000.0);
            last_frames = f;
            last_bytes = b;
            let buffered = dc.buffered_amount().await as u64;
            stats.backlog.store(buffered, Ordering::Relaxed);
            // 队列下降量 = 链路真实吞吐（积压时比产出更能反映可用带宽）
            let drained = produced + prev_buffered.saturating_sub(buffered);
            prev_buffered = buffered;
            adapt_bitrate(&stats, buffered, drained as f32 * 8.0 / (dt * 1000.0));
            // 统计每 1s 上报一次（走 ctrl 可靠通道，不与媒体抢占）
            if ticks % 2 == 0 {
                let msg = ControlMsg::Stats { fps, kbps, buffered };
                let _ = ctrl.send_text(msg.to_json()).await;
            }
        }

        // 有积压时先发一帧：发帧期间到达的新帧在下一轮被 try_recv 收进来裁剪
        if let Some(payload) = queue.pop_front() {
            while let Ok(f) = rx.try_recv() {
                push_frame(&mut queue, f);
            }
            let buffered = dc.buffered_amount().await as u64;
            stats.backlog.store(buffered, Ordering::Relaxed);
            send_frame(&dc, &mut tokens, &mut last_refill, &stats, &seq, &payload, legacy).await?;
            continue;
        }
        tokio::select! {
            recv = rx.recv() => {
                let Some(frame) = recv else { break };
                push_frame(&mut queue, frame);
            }
            // 无帧时也能定期醒来做码率决策（静止保活 2fps 场景）
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
    }
    info!("发送循环结束（采集线程已关闭）");
    Ok(())
}

/// 发送一整帧。
/// - 新协议：1100B 分片 + 18B 帧头，逐片令牌桶 pacing；
/// - legacy：0.2.x 的 `[u32 seq][u32 len]` 头 + 16KB 串行分片。
async fn send_frame(
    dc: &Arc<webrtc::data_channel::RTCDataChannel>,
    tokens: &mut f64,
    last_refill: &mut Instant,
    stats: &Stats,
    seq: &AtomicU32,
    payload: &[u8],
    legacy: bool,
) -> Result<()> {
    let key = payload.first() == Some(&1);
    let seq_v = seq.fetch_add(1, Ordering::Relaxed);
    if legacy {
        let mut header = [0u8; 8];
        header[..4].copy_from_slice(&seq_v.to_le_bytes());
        header[4..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        dc.send(&Bytes::copy_from_slice(&header))
            .await
            .context("DC send header")?;
        for chunk in payload.chunks(16_000) {
            dc.send(&Bytes::copy_from_slice(chunk))
                .await
                .context("DC send chunk")?;
        }
        return Ok(());
    }
    let bps = stats.bitrate.load(Ordering::Relaxed);
    let ts = proc_ms() as u32;
    for msg in crate::media::fragment_frame(seq_v, ts, key, 0, 0, payload) {
        pace(tokens, last_refill, bps, msg.len()).await;
        dc.send(&Bytes::from(msg)).await.context("媒体分片发送失败")?;
    }
    Ok(())
}

/// 应用层 AIMD（方案 4.4）：
/// - 主控端反馈丢失 >3% 或队列延迟 >260ms → 码率 ×0.75（并参考实测吞吐）
/// - 丢失 <1% 且队列健康 → 码率 ×1.15
/// - 上限 8Mbps、下限 150kbps；主控端手动指定画质时不自动调整
fn adapt_bitrate(stats: &Stats, buffered: u64, capacity_kbps: f32) {
    if stats.manual.load(Ordering::Relaxed) {
        return;
    }
    let cur = stats.bitrate.load(Ordering::Relaxed);
    let loss = stats.remote_loss_permille.load(Ordering::Relaxed) as f32 / 1000.0;
    let recv_fps = stats.remote_fps_x10.load(Ordering::Relaxed) as f32 / 10.0;
    // 队列延迟按当前码率折算成毫秒
    let queued_ms = buffered as f64 * 8000.0 / cur.max(150_000) as f64;
    let next = if loss > 0.03 || queued_ms > 260.0 {
        let cut = (cur as f32 * 0.75) as u32;
        if capacity_kbps > 100.0 {
            cut.min((capacity_kbps * 900.0) as u32)
        } else {
            cut
        }
    } else if loss < 0.01 && queued_ms < 80.0 && recv_fps >= 10.0 {
        // 静止画面（接收帧率很低）时不抬码率，避免恢复活动时突发 IDR 撑爆队列
        (cur as f32 * 1.15) as u32
    } else {
        cur
    };
    let next = next.clamp(150_000, 8_000_000);
    if next != cur {
        stats.bitrate.store(next, Ordering::Relaxed);
        info!(
            "🎛️ AIMD 码率 {} → {} kbps（丢帧 {:.1}%，队列 {:.0}ms）",
            cur / 1000,
            next / 1000,
            loss * 100.0,
            queued_ms
        );
    }
}

// ============================================================
// Annex-B 工具：按起始码切分 NAL 单元
// ============================================================
/// 切分 Annex-B 码流（去掉 3/4 字节起始码与 NAL 尾部补零）
pub fn split_annexb(buf: &[u8]) -> Vec<&[u8]> {
    let n = buf.len();
    let mut out = Vec::new();
    let mut i = 0;
    let mut cur: Option<usize> = None;
    while i + 2 < n {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            if let Some(s) = cur {
                push_nz(&mut out, &buf[s..i]);
            }
            i += 3;
            cur = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = cur {
        push_nz(&mut out, &buf[s..]);
    }
    out
}

fn push_nz<'a>(out: &mut Vec<&'a [u8]>, mut nal: &'a [u8]) {
    while let Some((&0, rest)) = nal.split_last() {
        nal = rest;
    }
    if !nal.is_empty() {
        out.push(nal);
    }
}

// FrameAssembler 已提取到 crate::frame_assembler（无 cfg gate，Android/桌面通用）
pub use crate::frame_assembler::FrameAssembler;

// ============================================================
// 分辨率与颜色空间转换
// ============================================================
/// 编码目标宽度上限：2880x1800 下 openh264 全尺寸单帧 ~73ms（顶到 14fps），
/// 缩到 1920x1200 后 ~29ms；大致等于 2x DPI 屏幕的 1.33 倍逻辑分辨率，文字仍清晰
pub const MAX_ENCODE_WIDTH: u32 = 1920;

/// 源尺寸 → 编码尺寸（等比缩放到不超过 MAX_ENCODE_WIDTH，宽高取偶数便于 I420 2x2 采样）
/// 为什么不强制对齐 16：1920x1080 本身不是 16 的倍数，硬对齐会把 1080 压成 1072；
/// 而 16 对齐只省下不到一行宏块（约 1.5% 像素），收益远小于分辨率损失。
pub fn target_size(sw: u32, sh: u32) -> (u32, u32) {
    let cap = max_encode_width();
    if cap == 0 || sw <= cap {
        return ((sw & !1).max(2), (sh & !1).max(2));
    }
    let w = cap & !1;
    let h = ((sh as u64 * w as u64 / sw as u64) as u32) & !1;
    (w, h.max(2))
}

/// BGRA → I420（BT.601 full range）。尺寸已是编码尺寸（GPU 侧缩放过），只做颜色空间转换。
/// 写入调用方提供的 `frame`（尺寸不符时就地调整），避免每帧重新分配 ~3.5MB。
/// 按色度行分带并行：带边界对齐 2 行，色度不跨带
pub fn bgra_to_i420(w: u32, h: u32, bgra: &[u8], frame: &mut I420Frame) {
    let (w, h) = (w as usize, h as usize);
    let cw = (w + 1) / 2;
    let ch = (h + 1) / 2;
    frame.fit(w, h);

    // 转换线程只占剩余算力：留 2 核给编码线程与系统，且 worker 降优先级，
    // 整机繁忙时先把 CPU 让给编码线程（编码单帧耗时直接决定 fps 上限）
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .saturating_sub(2)
        .clamp(1, 4)
        .min(ch);
    let band = (ch + threads - 1) / threads;

    let I420Frame { y, u, v, .. } = frame;
    let mut ychunks = y.chunks_mut(band * 2 * w);
    let mut uchunks = u.chunks_mut(band * cw);
    let mut vchunks = v.chunks_mut(band * cw);

    std::thread::scope(|scope| {
        let mut c0 = 0usize;
        // 最多保留一带不派发，最后这一带由调用线程自己跑，少建一条线程
        let mut pending: Option<(usize, usize, &mut [u8], &mut [u8], &mut [u8])> = None;
        while let (Some(yb), Some(ub), Some(vb)) = (ychunks.next(), uchunks.next(), vchunks.next())
        {
            let c1 = (c0 + band).min(ch);
            if let Some((p0, p1, py, pu, pv)) = pending.take() {
                scope.spawn(move || {
                    low_priority();
                    bgra_band(bgra, w, h, cw, p0, p1, py, pu, pv);
                });
            }
            pending = Some((c0, c1, yb, ub, vb));
            c0 = c1;
        }
        if let Some((p0, p1, py, pu, pv)) = pending {
            bgra_band(bgra, w, h, cw, p0, p1, py, pu, pv);
        }
    });
}

/// 转换 worker：降到低于正常优先级，整机繁忙时给编码线程让路
fn low_priority() {
    unsafe {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
        };
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

/// 处理色度行 [c0, c1) 覆盖的源行，写入本带的 Y / U / V 切片
fn bgra_band(
    bgra: &[u8],
    w: usize,
    h: usize,
    cw: usize,
    c0: usize,
    c1: usize,
    y: &mut [u8],
    u: &mut [u8],
    v: &mut [u8],
) {
    let ty_lo = c0 * 2;
    let ty_hi = (c1 * 2).min(h);

    for (i, sy) in (ty_lo..ty_hi).enumerate() {
        let src = &bgra[sy * w * 4..(sy + 1) * w * 4];
        let dst = &mut y[i * w..(i + 1) * w];
        for (px, d) in src.chunks_exact(4).zip(dst.iter_mut()) {
            let b = px[0] as i32;
            let g = px[1] as i32;
            let r = px[2] as i32;
            *d = ((((66 * r + 129 * g + 25 * b) >> 8) + 16).min(255)) as u8;
        }
    }

    // 每个色度块覆盖 <=2 行 × 2 列源像素
    for cy in 0..(c1 - c0) {
        let y0 = (c0 + cy) * 2;
        let y1 = (y0 + 1).min(h);
        let yn = (y1 - y0) as f32;
        for cx in 0..cw {
            let x0 = cx * 2;
            let x1 = (x0 + 2).min(w);
            let xn = (x1 - x0) as f32;
            let mut us = 0i32;
            let mut vs = 0i32;
            for sy in y0..y1 {
                let row = &bgra[sy * w * 4..(sy + 1) * w * 4];
                for px in row[x0 * 4..x1 * 4].chunks_exact(4) {
                    let b = px[0] as i32;
                    let g = px[1] as i32;
                    let r = px[2] as i32;
                    us += -38 * r - 74 * g + 112 * b;
                    vs += 112 * r - 94 * g - 18 * b;
                }
            }
            let n = xn * yn;
            let ci = cy * cw + cx;
            u[ci] = ((us as f32 / n / 256.0 + 128.0).clamp(0.0, 255.0)) as u8;
            v[ci] = ((vs as f32 / n / 256.0 + 128.0).clamp(0.0, 255.0)) as u8;
        }
    }
}

/// I420 → RGBA（主控端解码后渲染用，BT.601 full range）
pub fn i420_to_rgba(width: u32, height: u32, y: &[u8], u: &[u8], v: &[u8]) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let cw = (w + 1) / 2;
    let mut rgba = vec![0u8; w * h * 4];

    for py in 0..h {
        for px in 0..w {
            let yv = y[py * w + px] as i32 - 16;
            let uv = u[(py / 2) * cw + px / 2] as i32 - 128;
            let vv = v[(py / 2) * cw + px / 2] as i32 - 128;

            let r = (298 * yv + 409 * vv + 128) >> 8;
            let g = (298 * yv - 100 * uv - 208 * vv + 128) >> 8;
            let b = (298 * yv + 516 * uv + 128) >> 8;

            let idx = (py * w + px) * 4;
            rgba[idx] = r.clamp(0, 255) as u8;
            rgba[idx + 1] = g.clamp(0, 255) as u8;
            rgba[idx + 2] = b.clamp(0, 255) as u8;
            rgba[idx + 3] = 255;
        }
    }
    rgba
}

// ============================================================
// I420 帧容器（openh264 YUVSource 实现）
// ============================================================
pub struct I420Frame {
    pub width: i32,
    pub height: i32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl I420Frame {
    /// 按尺寸分配零缓冲（首次/缓冲池为空时用）
    pub fn new(w: u32, h: u32) -> Self {
        let (w, h) = (w as i32, h as i32);
        let cw = (w as usize + 1) / 2;
        let ch = (h as usize + 1) / 2;
        Self {
            width: w,
            height: h,
            y: vec![0u8; w as usize * h as usize],
            u: vec![0u8; cw * ch],
            v: vec![0u8; cw * ch],
        }
    }

    /// 尺寸不变时完全复用已有缓冲；变化时按需调整长度
    fn fit(&mut self, w: usize, h: usize) {
        if self.width as usize == w && self.height as usize == h {
            return;
        }
        self.width = w as i32;
        self.height = h as i32;
        let cw = (w + 1) / 2;
        let ch = (h + 1) / 2;
        self.y.resize(w * h, 0);
        self.u.resize(cw * ch, 0);
        self.v.resize(cw * ch, 0);
    }
}

impl openh264::formats::YUVSource for I420Frame {
    fn dimensions(&self) -> (usize, usize) {
        (self.width as usize, self.height as usize)
    }
    fn strides(&self) -> (usize, usize, usize) {
        let w = self.width as usize;
        let cw = (w + 1) / 2;
        (w, cw, cw)
    }
    fn y(&self) -> &[u8] {
        &self.y
    }
    fn u(&self) -> &[u8] {
        &self.u
    }
    fn v(&self) -> &[u8] {
        &self.v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- target_size ----------
    #[test]
    fn target_size_below_max_keeps_even() {
        // 1920x1080 未超上限，宽高取偶后不变
        assert_eq!(target_size(1920, 1080), (1920, 1080));
        assert_eq!(target_size(1280, 720), (1280, 720));
    }

    #[test]
    fn target_size_odd_dimensions_become_even() {
        // 奇数 → 向下取偶；结果至少 2（I420 要求）
        assert_eq!(target_size(101, 99), (100, 98));
        assert_eq!(target_size(3, 3), (2, 2));
        assert_eq!(target_size(1, 1), (2, 2));
    }

    #[test]
    fn target_size_above_max_scales_down() {
        // 4K → 等比缩到 1920 宽
        let (w, h) = target_size(3840, 2160);
        assert_eq!(w, 1920);
        assert_eq!(h, 1080);
    }

    #[test]
    fn target_size_preserves_aspect_ratio() {
        // 2560x1440 → 1920x1080（16:9）
        let (w, h) = target_size(2560, 1440);
        assert_eq!((w, h), (1920, 1080));
    }

    // ---------- split_annexb ----------
    #[test]
    fn split_annexb_three_byte_start_code() {
        // 00 00 01 <nal1> 00 00 01 <nal2>
        let buf = [0, 0, 1, 0x65, 0xAB, 0, 0, 1, 0x67];
        let nals = split_annexb(&buf);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0], &[0x65, 0xAB]);
        assert_eq!(nals[1], &[0x67]);
    }

    #[test]
    fn split_annexb_four_byte_start_code() {
        // 00 00 00 01 <nal>
        let buf = [0, 0, 0, 1, 0x65, 0x88];
        let nals = split_annexb(&buf);
        assert_eq!(nals.len(), 1);
        assert_eq!(nals[0], &[0x65, 0x88]);
    }

    #[test]
    fn split_annexb_trailing_zeros_trimmed() {
        // NAL 尾部的补零应被去掉
        let buf = [0, 0, 1, 0x65, 0xAB, 0, 0, 0];
        let nals = split_annexb(&buf);
        assert_eq!(nals.len(), 1);
        assert_eq!(nals[0], &[0x65, 0xAB]);
    }

    #[test]
    fn split_annexb_empty_input() {
        assert!(split_annexb(&[]).is_empty());
        assert!(split_annexb(&[0, 0]).is_empty());
    }

    // ---------- bgra_to_i420 ----------
    #[test]
    fn bgra_to_i420_white_produces_full_y() {
        // 2x2 纯白 BGRA：Y 应为 235（BT.601 full range），U/V 应为 128
        let w = 2u32;
        let h = 2u32;
        let bgra = vec![255u8; (w * h * 4) as usize];
        let mut frame = I420Frame::new(w, h);
        bgra_to_i420(w, h, &bgra, &mut frame);

        assert_eq!(frame.y.len(), 4);
        assert!(
            frame.y.iter().all(|&v| v == 235),
            "Y 应全为 235, got {:?}",
            frame.y
        );
        assert_eq!(frame.u.len(), 1);
        assert_eq!(frame.v.len(), 1);
        assert_eq!(frame.u[0], 128);
        assert_eq!(frame.v[0], 128);
    }

    #[test]
    fn bgra_to_i420_round_trip_approx() {
        // 4x4 图像，每个 2x2 块为纯色 → 色度 2x2 降采样无损，
        // BGRA → I420 → RGBA 应高度近似还原（仅亮度整数舍入误差）
        let w = 4u32;
        let h = 4u32;
        // 4 个 2x2 块的纯色（B, G, R）
        let block_colors = [
            (30u8, 60, 90),
            (200, 100, 50),
            (10, 200, 150),
            (180, 30, 200),
        ];
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        for by in 0u32..2 {
            for bx in 0u32..2 {
                let (b, g, r) = block_colors[(by * 2 + bx) as usize];
                for py in 0u32..2 {
                    for px in 0u32..2 {
                        let idx = ((by * 2 + py) * w + (bx * 2 + px)) as usize * 4;
                        bgra[idx] = b;
                        bgra[idx + 1] = g;
                        bgra[idx + 2] = r;
                        bgra[idx + 3] = 255;
                    }
                }
            }
        }
        let mut frame = I420Frame::new(w, h);
        bgra_to_i420(w, h, &bgra, &mut frame);
        let rgba = i420_to_rgba(w, h, &frame.y, &frame.u, &frame.v);

        // 逐像素比较，允许 ±8 整数舍入误差
        for i in 0..(w * h) as usize {
            let b_in = bgra[i * 4] as i32;
            let g_in = bgra[i * 4 + 1] as i32;
            let r_in = bgra[i * 4 + 2] as i32;
            let r_out = rgba[i * 4] as i32;
            let g_out = rgba[i * 4 + 1] as i32;
            let b_out = rgba[i * 4 + 2] as i32;
            assert!(
                (r_in - r_out).abs() <= 8,
                "R 误差过大: in={r_in} out={r_out}"
            );
            assert!(
                (g_in - g_out).abs() <= 8,
                "G 误差过大: in={g_in} out={g_out}"
            );
            assert!(
                (b_in - b_out).abs() <= 8,
                "B 误差过大: in={b_in} out={b_out}"
            );
        }
    }

    // ---------- FrameAssembler 测试已移至 crate::frame_assembler ----------
}
