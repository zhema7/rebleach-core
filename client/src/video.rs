// 视频管线：常驻 WGC 采集（GPU 缩放到 1920 宽）→ BGRA→I420 → openh264 编码 → DataChannel 发送
//          + 主控端收帧组装 + I420→RGBA（渲染前转换）
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use tracing::{error, info, warn};

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
        }
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
}

/// 默认码率：1920x1200 屏幕内容 1080p 下 2Mbps 起步即可，靠自适应按需上升
pub const DEFAULT_BITRATE: u32 = 2_000_000;

/// 启动采集监督线程：内部循环拉起采集+编码线程；
/// 收到 SwitchMonitor 命令时平滑重建采集（编码器随之按新分辨率重建）
pub fn spawn_capture_supervisor(
    tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    fps: u64,
    stats: Arc<Stats>,
) -> CaptureHandle {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<CaptureCmd>();
    std::thread::spawn(move || {
        let mut monitor: Option<usize> = None;
        loop {
            match capture_thread(tx.clone(), fps, monitor, &cmd_rx, &stats) {
                Ok(CaptureOutcome::Switch(i)) => {
                    monitor = Some(i as usize);
                }
                Ok(CaptureOutcome::Restart) => {}
                Ok(CaptureOutcome::Quit) => break,
                Err(e) => {
                    error!("采集线程退出: {e:#}");
                    break;
                }
            }
        }
        info!("采集监督线程结束");
    });
    CaptureHandle { cmd_tx }
}

/// 转换完成的帧 + 采集侧耗时，交给编码线程（流水线）
struct Prepared {
    frame: I420Frame,
    conv_ms: f64,
    wait_ms: f64,
}

fn capture_thread(
    tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    fps: u64,
    monitor: Option<usize>,
    cmd_rx: &std::sync::mpsc::Receiver<CaptureCmd>,
    stats: &Arc<Stats>,
) -> Result<CaptureOutcome> {
    // 常驻 WGC 会话 + GPU 下采样：帧到达时在 GPU 上直接缩到编码尺寸再回读（9.2MB），
    // 省掉 xcap 每帧全尺寸回读 + 两次 20MB 克隆 + 通道交换
    // DDA 采集线程按同一目标帧率限速，先写入全局再建采集
    capture::set_target_fps(fps);
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
                    None => continue, // 超时：回循环头看命令
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
    tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
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

    while let Ok(p) = rx.recv() {
        // 动态码率：openh264 0.8 的 Encoder 不支持运行中改码率，
        // 目标偏差超过 8% 时重建编码器并强制下一帧 IDR（新 SPS/PPS 随 IDR 下发）
        let target = stats.bitrate.load(Ordering::Relaxed);
        if target != cur_bps {
            let drift = (target as f64 - cur_bps as f64).abs() / cur_bps.max(1) as f64;
            // 阈值 8%：自适应每次升降幅度在 10%~50%，太小则改了目标却不重建编码器、实际码率不生效
            if drift > 0.08 {
                match Encoder::with_api_config(
                    OpenH264API::from_source(),
                    encoder_config(target, fps),
                ) {
                    Ok(mut e2) => {
                        e2.force_intra_frame();
                        encoder = e2;
                        sps = None;
                        pps = None;
                        cur_bps = target;
                        info!(
                            "🎚️ 码率调整 → {} kbps（编码器重建 + 强制 IDR）",
                            target / 1000
                        );
                    }
                    Err(e) => warn!("编码器重建失败（保持 {cur_bps} bps）: {e}"),
                }
            } else {
                cur_bps = target; // 小偏差不同步，避免频繁重建
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
            if tx.send(payload).is_err() {
                info!("接收端关闭，停止采集");
                break;
            }
            frames_sent += 1;
            bytes_sent += len;
            last_emit = Instant::now();
            if key {
                last_idr = Instant::now();
            }
            stats.frames.fetch_add(1, Ordering::Relaxed);
            stats.bytes.fetch_add(len, Ordering::Relaxed);
        }

        if last_log.elapsed() >= Duration::from_secs(5) {
            let secs = last_log.elapsed().as_secs_f64();
            let n = frames_in.max(1) as f64;
            info!(
                "📊 采集 {}x{} (backend={}) → {}x{}: {:.1} fps, {:.0} KB/s, 转换 {:.1} + 编码 {:.1} + 等帧 {:.1} ms/帧, 跳过 {}",
                w,
                h,
                backend,
                ew,
                eh,
                frames_sent as f64 / secs,
                bytes_sent as f64 / 1024.0 / secs,
                conv_ms / n,
                enc_ms / n,
                wait_ms / n,
                skipped
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
// async 发送任务：从采集线程收帧，[u32 seq][u32 len][payload...16KB] 发出
// 每 5s 按 DataChannel 缓冲水位做码率自适应，并向主控端上报链路统计
// ============================================================
pub async fn send_loop(
    dc: Arc<webrtc::data_channel::RTCDataChannel>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    stats: Arc<Stats>,
) -> Result<()> {
    let seq = Arc::new(AtomicU32::new(0));
    // 5s 一次：2s 周期在自适应上升期会每 2 秒重建一次编码器 + 强制 IDR（实测 6 连跳）
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_frames = 0u32;
    let mut last_bytes = 0u64;
    let mut last_tick = Instant::now();
    // 上一个周期的队列深度（估算链路吞吐用）
    let mut prev_buffered = 0u64;
    loop {
        tokio::select! {
            recv = rx.recv() => {
                let Some(payload) = recv else { break };
                let seq_v = seq.fetch_add(1, Ordering::Relaxed);
                let mut header = Vec::with_capacity(8);
                header.extend_from_slice(&seq_v.to_le_bytes());
                header.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                dc.send(&Bytes::from(header)).await.context("DC send header")?;
                for chunk in payload.chunks(16_000) {
                    dc.send(&Bytes::copy_from_slice(chunk)).await.context("DC send chunk")?;
                }
                // 把发送侧积压反馈给编码线程：队列涨起来就在编码前丢帧，压住延迟
                stats.backlog.store(dc.buffered_amount().await as u64, Ordering::Relaxed);
            }
            _ = tick.tick() => {
                // interval 首次立即触发，按实际间隔算速率（不要假设固定 5s）
                let dt = last_tick.elapsed().as_secs_f32();
                if dt < 0.5 {
                    continue;
                }
                last_tick = Instant::now();
                let f = stats.frames.load(Ordering::Relaxed);
                let b = stats.bytes.load(Ordering::Relaxed);
                let fps = (f - last_frames) as f32 / dt;
                let produced = b - last_bytes;
                let kbps = produced as f32 * 8.0 / (dt * 1000.0);
                last_frames = f;
                last_bytes = b;
                let buffered = dc.buffered_amount().await as u64;
                stats.backlog.store(buffered, Ordering::Relaxed);
                // 实际发出量 = 本周期产出 + 队列下降量，队列积压时它就等于链路真实吞吐
                let drained = produced + prev_buffered.saturating_sub(buffered);
                prev_buffered = buffered;
                adapt_bitrate(&stats, buffered, drained as f32 * 8.0 / (dt * 1000.0));
                let msg = ControlMsg::Stats { fps, kbps, buffered };
                let _ = dc.send_text(msg.to_json()).await;
            }
        }
    }
    info!("发送循环结束（采集线程已关闭）");
    Ok(())
}

/// 码率自适应：
/// - 队列延迟 >260ms 说明链路已饱和 → 目标码率对齐实测吞吐（90%），一个周期收敛
/// - 队列延迟 <80ms 说明还有余量 → 按 1.5 倍缓升（5s 一档，2~3 档到顶），上限 4Mbps
/// - 中间区间保持不动（滞回，避免频繁重建编码器）
/// 全部按时间而非字节判断，慢链路（中继 ~300kbps）上才不会把"几秒延迟"当成"正常水位"
fn adapt_bitrate(stats: &Stats, buffered: u64, capacity_kbps: f32) {
    // 主控端手动指定画质时不做自动调整
    if stats.manual.load(Ordering::Relaxed) {
        return;
    }
    let cur = stats.bitrate.load(Ordering::Relaxed);
    // 同样按"队列延迟"判断：同样的 20KB，在 4Mbps 上是 40ms，在 300kbps 上是 533ms
    let queued_ms = buffered as f64 * 8000.0 / cur.max(150_000) as f64;
    let next = if queued_ms > 260.0 {
        // 下限 150kbps：慢链路（中继/弱网）上 400kbps 的地板会把队列顶满
        ((capacity_kbps * 900.0) as u32).clamp(150_000, 4_000_000)
    } else if queued_ms < 80.0 {
        // 1.5 倍一档：10% 的档位要 6 次才能从 2M 爬到 4M，每次都重建编码器
        (cur * 3 / 2).min(4_000_000)
    } else {
        cur
    };
    if next != cur {
        stats.bitrate.store(next, Ordering::Relaxed);
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

// ============================================================
// 主控端：收帧组装器（8 字节头 + 16KB 分块重组），整帧经 mpsc 交解码
// ============================================================
pub struct FrameAssembler {
    hdr: Vec<u8>,
    payload: Vec<u8>,
    expected: u32,
    seq: u32,
    out: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    frames: u64,
    total: u64,
}

impl FrameAssembler {
    pub fn new(out: tokio::sync::mpsc::UnboundedSender<Vec<u8>>) -> Self {
        Self {
            hdr: Vec::with_capacity(8),
            payload: Vec::new(),
            expected: 0,
            seq: 0,
            out,
            frames: 0,
            total: 0,
        }
    }

    pub fn feed(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            if self.hdr.len() < 8 {
                let need = 8 - self.hdr.len();
                let take = need.min(data.len());
                self.hdr.extend_from_slice(&data[..take]);
                data = &data[take..];
                if self.hdr.len() < 8 {
                    return;
                }
                self.seq = u32::from_le_bytes(self.hdr[0..4].try_into().unwrap());
                self.expected = u32::from_le_bytes(self.hdr[4..8].try_into().unwrap());
                self.payload.clear();
                if self.expected == 0 {
                    self.finish_frame();
                    self.hdr.clear();
                    continue;
                }
                if self.expected > 32 * 1024 * 1024 {
                    error!("帧长度异常 {}，丢弃", self.expected);
                    self.hdr.clear();
                    continue;
                }
            }
            let need = self.expected as usize - self.payload.len();
            let take = need.min(data.len());
            self.payload.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.payload.len() == self.expected as usize {
                self.finish_frame();
                self.hdr.clear();
            }
        }
    }

    fn finish_frame(&mut self) {
        let len = self.payload.len();
        if self.out.send(std::mem::take(&mut self.payload)).is_err() {
            return;
        }
        self.frames += 1;
        self.total += len as u64;
        if self.frames % 60 == 1 {
            info!(
                "📥 收到 {f} 帧, 共 {t} bytes (seq={s}, len={l})",
                f = self.frames,
                t = self.total,
                s = self.seq,
                l = len
            );
        }
    }
}

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
    if sw <= MAX_ENCODE_WIDTH {
        return ((sw & !1).max(2), (sh & !1).max(2));
    }
    let w = MAX_ENCODE_WIDTH & !1;
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

    // ---------- FrameAssembler ----------
    #[test]
    fn frame_assembler_single_chunk() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let mut asm = FrameAssembler::new(tx);
        let payload = b"hello world";
        // 头：[u32 seq][u32 len]
        let mut pkt = Vec::new();
        pkt.extend_from_slice(&1u32.to_le_bytes());
        pkt.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        pkt.extend_from_slice(payload);
        asm.feed(&pkt);

        let got = rx.try_recv().expect("应收到组装好的帧");
        assert_eq!(got, payload);
    }

    #[test]
    fn frame_assembler_split_across_chunks() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let mut asm = FrameAssembler::new(tx);
        let payload = b"abcdefghij";
        let mut pkt = Vec::new();
        pkt.extend_from_slice(&7u32.to_le_bytes());
        pkt.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        pkt.extend_from_slice(payload);

        // 切成两段喂入
        asm.feed(&pkt[..6]);
        assert!(rx.try_recv().is_err(), "头未收完不应出帧");
        asm.feed(&pkt[6..]);

        let got = rx.try_recv().expect("应收到组装好的帧");
        assert_eq!(got, payload);
    }
}
