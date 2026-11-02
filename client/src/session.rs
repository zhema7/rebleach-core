// 信令 + WebRTC 会话核心（与 UI 无关）
// 对外只吐 SessionEvent：状态文本 / H.264 编码帧（Annex-B）
// 由调用方决定渲染方式：CLI 走 winit 软解码，Tauri 走 WebCodecs 硬解
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async_tls_with_config, tungstenite::Message};
use tracing::{error, info, warn};

use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::{API, APIBuilder};
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_credential_type::RTCIceCredentialType;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::stats::{StatsReport, StatsReportType};

use crate::audio;
use crate::capture;
use crate::control;
use crate::identity::{self, Identity, TofuResult};
use crate::protocol::{ControlMsg, DeviceInfo, MonitorDesc, Role, SdpKind, WsMsg};
use crate::video;

// ============================================================
// 会话配置 / 事件
// ============================================================
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub server: String,
    pub id: String,
    pub role: Role,
    /// controller 模式必填：对端（被控端）设备 ID
    pub peer: Option<String>,
    /// 被控端采集帧率
    pub fps: u64,
    /// STUN/TURN 服务器列表（每项可逗号分隔多个）。
    /// 格式：`stun:host:port` 或 `turn:user:pass@host:port`（无凭据时 `turn:host:port`）。
    /// 为空时使用内置默认 STUN + TURN（见 `DEFAULT_STUN` / `DEFAULT_TURN`）。
    pub ice_servers: Vec<String>,
    /// 设备身份密钥文件（ed25519，TOFU 身份校验用）；None 用默认路径
    /// （Windows: %APPDATA%/rebleach/identity.json）
    pub key_file: Option<std::path::PathBuf>,
    /// 局域网直连验证码（LAN 模式主控端填写，随 Connect 上报；被控端内嵌信令校验）
    pub auth: Option<String>,
}

/// 默认 STUN 列表：自建（香港，3 端口）+ 小米。
/// 同 IP 多端口（3478/3479/3480，服务器侧 iptables REDIRECT 到 coturn 3478）：
/// 对端点相关映射的对称 NAT，向不同目的端口发 STUN 会产生不同 NAT 映射，
/// 从而拿到多个 srflx 候选——对端向任一映射地址打洞，都可能命中（对称 NAT
/// 「映射相关、过滤无关」场景下的直连率提升手段）；顺带提供端口级冗余
/// （某些企业防火墙只拦知名端口）。
/// 曾加入 stun.qq.com 但实测在某些网络下不可达（5s gather 超时），宁缺毋滥。
/// trickle 化之后慢/死 STUN 节点不再阻塞 SDP 发出，但候选收集完成时间仍受拖累，
/// 故列表保持精简。
pub const DEFAULT_STUN_LIST: &[&str] = &[
    "stun:stun.l.google.com:19302",
    "stun:stun1.l.google.com:19302",
    "stun:stun.miwifi.com:3478",
];

/// 重新枚举显示器并推 Monitors 清单给主控端。
/// 在虚拟屏增删、分辨率变更后调用，让主控端下拉刷新。
async fn refresh_monitors(dc: &Arc<webrtc::data_channel::RTCDataChannel>) {
    let list: Vec<crate::protocol::MonitorDesc> = capture::enumerate()
        .into_iter()
        .enumerate()
        .map(|(i, m)| crate::protocol::MonitorDesc {
            index: i as u32,
            name: m.name,
            width: m.width,
            height: m.height,
            primary: m.primary,
            is_virtual: m.is_virtual,
        })
        .collect();
    info!("🖥️ 重新推送显示器清单: {} 台", list.len());
    let _ = dc
        .send_text(ControlMsg::Monitors { list }.to_json())
        .await;
}

/// 推一次驱动状态给主控端（增删屏成功/失败后都推，让 UI 实时反映）。
fn push_driver_status(dc: &Arc<webrtc::data_channel::RTCDataChannel>) {
    let (has_driver, driver_kind, hint) = control::detect_virtual_status();
    let active_count = capture::enumerate()
        .into_iter()
        .filter(|m| m.is_virtual)
        .count() as u32;
    let msg = ControlMsg::DriverStatus {
        has_driver,
        active_count,
        driver_kind,
        hint,
    }
    .to_json();
    let dc2 = dc.clone();
    tokio::spawn(async move {
        let _ = dc2.send_text(msg).await;
    });
}

/// 把用户输入的 ICE 服务器字符串列表解析成 webrtc RTCIceServer。
/// 支持 `stun:host:port` 与 `turn:user:pass@host:port`；每个元素内可用逗号分隔多个。
pub fn parse_ice_servers(raw: &[String]) -> Vec<RTCIceServer> {
    let mut out = Vec::new();
    for item in raw {
        for part in item.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if let Some(s) = part
                .strip_prefix("stun:")
                .or_else(|| part.strip_prefix("stuns:"))
            {
                out.push(RTCIceServer {
                    urls: vec![format!("stun:{s}")],
                    ..Default::default()
                });
            } else if let Some(s) = part
                .strip_prefix("turn:")
                .or_else(|| part.strip_prefix("turns:"))
            {
                let scheme = if part.starts_with("turns:") {
                    "turns"
                } else {
                    "turn"
                };
                if let Some(at) = s.rfind('@') {
                    let cred = &s[..at];
                    let host = &s[at + 1..];
                    let (username, credential) = cred.split_once(':').unwrap_or((cred, ""));
                    out.push(RTCIceServer {
                        urls: vec![format!("{scheme}:{host}")],
                        username: username.to_string(),
                        credential: credential.to_string(),
                        // 默认是 Unspecified，webrtc-rs 会直接报 invalid turn server credentials
                        credential_type: RTCIceCredentialType::Password,
                        ..Default::default()
                    });
                } else {
                    out.push(RTCIceServer {
                        urls: vec![format!("{scheme}:{s}")],
                        ..Default::default()
                    });
                }
            } else {
                warn!("忽略无法识别的 ICE 服务器地址: {part}");
            }
        }
    }
    out
}

/// 服务端注册成功后下发的 TURN 临时凭据（TURN REST，进程内单会话够用）
#[derive(Debug, Clone)]
struct TurnCreds {
    urls: Vec<String>,
    username: String,
    credential: String,
}

static TURN_CREDS: parking_lot::Mutex<Option<TurnCreds>> = parking_lot::Mutex::new(None);

/// 构造 webrtc API：注入 SettingEngine 调优建连策略，目标是「先尽力直连，不行才走中继」。
///
/// - `set_relay_acceptance_min_wait(8s)`：让 ICE 在 8 秒内优先尝试 host/srflx/prflx，
///   8 秒内直连打通就根本不用 TURN。这是降低中转带宽的关键一步——默认值下 ICE
///   几乎立刻接受 relay 候选，大量本来能直连的会话也走了中继。
/// - `set_prflx_acceptance_min_wait(1s)`：对称 NAT 下 prflx 是最后的直连救命稻草，
///   越早接受越能赶在 relay 之前抢到直连。
/// - IPv6：默认 `supported_network_types()` 已包含 Udp4+Udp6，无需额外配置。
///   两端都有 v6 时会完全绕过 NAT 和中转，是移动 4G/5G 下的最优路径。
fn build_api() -> API {
    let mut se = SettingEngine::default();
    se.set_relay_acceptance_min_wait(Some(Duration::from_secs(8)));
    se.set_prflx_acceptance_min_wait(Some(Duration::from_secs(1)));
    // 排除虚拟网卡：Tailscale/ZeroTier/Wintun/VPN 这类接口"连得通"，但走的是加密隧道或
    // DERP 中转（实测 Tailscale relay 只有 ~300kbps），而 ICE 会因 host 候选优先级最高
    // 优先选中它，导致明明有 37ms 的公网直连却跑在隧道上。排除后只能选真实网卡。
    se.set_interface_filter(Box::new(|name: &str| {
        let n = name.to_ascii_lowercase();
        ![
            "tailscale",
            "wintun",
            "zerotier",
            "vethernet",
            "vmware",
            "virtualbox",
            "hyper-v",
            "loopback",
            "bluetooth",
            "openvpn",
            "tap-",
            "wsl",
            "docker",
            "radmin",
            "hamachi",
        ]
        .iter()
        .any(|k| n.contains(k))
    }));
    // CGNAT 100.64.0.0/10：Tailscale 与部分运营商 NAT 都用这段，从中选出的"直连"多为假直连
    se.set_ip_filter(Box::new(|ip: std::net::IpAddr| {
        let std::net::IpAddr::V4(v4) = ip else {
            return true;
        };
        let o = v4.octets();
        !(o[0] == 100 && (64..=127).contains(&o[1]))
    }));
    APIBuilder::new().with_setting_engine(se).build()
}

/// 建连后打印 ICE 实际选中的候选对，用于识别"假直连"（隧道/中继伪装成 host 候选）
fn spawn_pair_report(pc: Arc<RTCPeerConnection>) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(4)).await;
        let report = pc.get_stats().await;
        let name = |id: &str| -> String {
            match report.reports.get(id) {
                Some(webrtc::stats::StatsReportType::LocalCandidate(c))
                | Some(webrtc::stats::StatsReportType::RemoteCandidate(c)) => {
                    format!("{:?} {}:{}", c.candidate_type, c.ip, c.port)
                }
                _ => id.to_string(),
            }
        };
        for s in report.reports.values() {
            if let webrtc::stats::StatsReportType::CandidatePair(p) = s {
                if p.nominated {
                    info!(
                        "🔗 选中候选对 {} → {}，RTT {:.0}ms，已发 {} 字节",
                        name(&p.local_candidate_id),
                        name(&p.remote_candidate_id),
                        p.current_round_trip_time * 1000.0,
                        p.bytes_sent
                    );
                }
            }
        }
    });
}

fn build_rtc_config(ice_servers: &[String]) -> RTCConfiguration {
    let mut servers = if ice_servers.is_empty() {
        parse_ice_servers(
            &DEFAULT_STUN_LIST
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
        )
    } else {
        parse_ice_servers(ice_servers)
    };
    // 优先用服务端下发的临时凭据；没下发就不带 TURN（避免使用已失效的固定密码）
    if let Some(tc) = TURN_CREDS.lock().clone() {
        servers.push(RTCIceServer {
            urls: tc.urls,
            username: tc.username,
            credential: tc.credential,
            credential_type: RTCIceCredentialType::Password,
            ..Default::default()
        });
    }
    if servers.is_empty() {
        warn!("未配置任何 STUN/TURN，只能在同一局域网内直连");
    }
    RTCConfiguration {
        ice_servers: servers,
        ..Default::default()
    }
}

#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// 状态文本（直接展示给用户）
    Status(String),
    /// 会话结束（主控端用：自动返回主页面，不再停留在黑屏会话页）
    Ended(String),
    /// 一帧 H.264 Annex-B（key=true 时已确保带 SPS/PPS）
    Video { key: bool, data: Vec<u8> },
    /// 被控端推送的显示器清单（主控端切换下拉用）
    Monitors(Vec<MonitorDesc>),
    /// 对端剪贴板文本变化（收到后写入本机剪贴板）
    Clipboard(String),
    /// 被控端链路上报：编码 fps / 发送 kbps / DataChannel 缓冲字节
    Stats { fps: f32, kbps: f32, buffered: u64 },
    /// 被控端回传的日志/状态（增删虚拟屏等异步操作的结果）
    Log { level: String, msg: String },
    /// 被控端推送的虚拟屏驱动状态
    DriverStatus {
        has_driver: bool,
        active_count: u32,
        driver_kind: String,
        hint: String,
    },
}

pub type EventSink = Arc<dyn Fn(SessionEvent) + Send + Sync>;

/// 设备详细信息（Agent 启动时写入，Register 时随注册上报）
static DEVICE_INFO: parking_lot::Mutex<Option<DeviceInfo>> = parking_lot::Mutex::new(None);

/// 由被控端 Agent 在连接前调用；控制器/CLI 不调用则注册时不带设备信息
pub fn set_device_info(info: DeviceInfo) {
    *DEVICE_INFO.lock() = Some(info);
}

fn device_info() -> Option<DeviceInfo> {
    DEVICE_INFO.lock().clone()
}

/// 心跳中的「是否有主控端连接」标志（被控端 Agent 会话期间按真实状态更新）
static BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn set_busy(v: bool) {
    BUSY.store(v, std::sync::atomic::Ordering::Relaxed);
}

fn heartbeat() -> WsMsg {
    WsMsg::Heartbeat {
        busy: BUSY.load(std::sync::atomic::Ordering::Relaxed),
    }
}

/// 紧急控制执行器：独立于任何会话/采集状态，在后台 tokio task 里执行。
/// 白名单动作，不接受任意 shell 命令。
pub fn run_emergency(action: &str, delay_secs: u32) {
    let delay = delay_secs.min(3600); // 上限 1 小时
    let label = action.to_string();
    tokio::spawn(async move {
        use crate::control;
        use std::process::Command;
        if delay > 0 {
            info!("⏳ 紧急控制 {label} 将在 {delay}s 后执行");
            tokio::time::sleep(Duration::from_secs(delay as u64)).await;
        }
        match label.as_str() {
            "restart" => control::power("restart", 0),
            "shutdown" => control::power("shutdown", 0),
            "unlock" => {
                // Windows 没有标准 API 解已锁定的桌面（锁定需要交互式凭据）。
                // 我们退而执行：重启 explorer + 发送模拟按键唤醒锁屏，
                // 真正的解锁仍需对方/主控端重新登录，但至少能唤醒屏幕。
                let _ = Command::new("rundll32.exe")
                    .args(["user32.dll,LockWorkStation"])
                    .spawn();
                // 二次调用相当于"解锁自己"（需要本地会话上下文）
            }
            "reload" => {
                // 被控端会在 run_session 返回后 5 秒自动重连，
                // 这里直接 return 让主循环清理后重连。
                info!("🔄 紧急 reload：触发重连");
                return;
            }
            "disconnect" => {
                // 主控端：断开当前会话。由 run_session 返回后重新进入监听/连接状态
                info!("⛔ 紧急 disconnect");
                return;
            }
            other => warn!("未知紧急动作: {other}"),
        }
    });
}

/// 统一入口：根据角色跑主控端 / 被控端会话
pub async fn run_session(
    cfg: SessionConfig,
    input_rx: mpsc::UnboundedReceiver<ControlMsg>,
    sink: EventSink,
) -> Result<()> {
    // TOFU 身份：加载/生成设备密钥对
    let key_path = cfg
        .key_file
        .clone()
        .unwrap_or_else(identity::default_key_file);
    let id = Identity::load_or_generate(&key_path).context("加载/生成设备身份密钥失败")?;
    let fp = identity::fingerprint(&id.pubkey_b64());
    info!("🔑 设备身份 {} (公钥指纹 {fp})", key_path.display());
    status(&sink, format!("设备公钥指纹 {fp}"));
    let tofu_path = identity::default_tofu_file(&key_path);
    match cfg.role {
        Role::Controller => controller_session(cfg, input_rx, sink, id, tofu_path).await,
        Role::Controlled => controlled_session(cfg, sink, id, tofu_path).await,
    }
}

type TaskList = Arc<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>>;

/// 主控端当前活跃屏幕 DataChannel 槽位：会话建立时注册、断开时清空。
/// 输入转发是常驻任务（input_rx 只被消费一次），经此槽位找到当前会话的 DC。
/// 修复旧 bug：`input_rx.take()` 让首次 Incoming 消费了接收端，重连后输入失效。
type ActiveDcSlot = Arc<parking_lot::Mutex<Option<Arc<RTCDataChannel>>>>;

struct Session {
    pc: Arc<RTCPeerConnection>,
    /// 会话内的后台任务（采集发送 / 帧转发 / 输入转发）
    tasks: TaskList,
    /// 音频播放句柄（主控端）；drop 时自动停止流
    _audio_playback: Option<audio::AudioPlayback>,
    /// 音频采集句柄（被控端）；drop 时自动停止流
    _audio_capture: Option<audio::AudioCapture>,
}

impl Session {
    fn new(pc: Arc<RTCPeerConnection>, tasks: TaskList) -> Self {
        Self {
            pc,
            tasks,
            _audio_playback: None,
            _audio_capture: None,
        }
    }

    /// 非阻塞清理：abort 会话内后台任务，再把 PC 的关闭丢到后台。
    /// 两点都不能省：① 只关 PC 时 DataChannel 的 send 仍返回 Ok，采集线程会一直跑；
    /// ② pc.close().await 在 ICE 已失效时要拖 20s+，阻塞会话循环会让新的 Offer
    /// 一直排不上处理（被控端表现为「忽略新 Offer」）
    fn teardown(&self) {
        for h in self.tasks.lock().drain(..) {
            h.abort();
        }
        let pc = self.pc.clone();
        tokio::spawn(async move {
            // 关闭前先结算一次 ICE 统计（关掉之后就拿不到了）
            log_ice_summary(&pc).await;
            let _ = pc.close().await;
        });
    }
}

/// P0 埋点：会话结束时记录实际被选中的候选对类型与收发字节。
/// 直连 = host / srflx / prflx；中继 = relay。
/// 用于统计直连率、中继流量占比——这是"省中转带宽"所有决策的数据基础。
async fn log_ice_summary(pc: &Arc<RTCPeerConnection>) {
    let report = pc.get_stats().await;
    // 候选对的 bytes 字段在 webrtc-rs 0.11 里不会累加，真实流量取 DataChannel 统计
    let mut dc_sent = 0usize;
    let mut dc_recv = 0usize;
    let mut best: Option<&webrtc::stats::ICECandidatePairStats> = None;
    for stats in report.reports.values() {
        match stats {
            StatsReportType::DataChannel(dc) => {
                dc_sent += dc.bytes_sent;
                dc_recv += dc.bytes_received;
            }
            StatsReportType::CandidatePair(pair) => {
                // 优先提名的候选对
                if best.is_none() || (pair.nominated && !best.is_some_and(|b| b.nominated)) {
                    best = Some(pair);
                }
            }
            _ => {}
        }
    }
    let Some(pair) = best else {
        info!("📊 ICE 结算：无候选对（本次未建立有效连接）");
        return;
    };
    let local = candidate_kind(&report, &pair.local_candidate_id);
    let remote = candidate_kind(&report, &pair.remote_candidate_id);
    let path = if local == "relay" || remote == "relay" {
        "RELAY(中继)"
    } else {
        "DIRECT(直连)"
    };
    info!(
        "📊 ICE 结算：{path} local={local} remote={remote} rtt={:.0}ms dc_sent={:.1}KB dc_recv={:.1}KB",
        pair.current_round_trip_time * 1000.0,
        dc_sent as f64 / 1024.0,
        dc_recv as f64 / 1024.0,
    );
}

/// 从统计报告里取候选类型（host / srflx / prflx / relay）
fn candidate_kind(report: &StatsReport, id: &str) -> String {
    match report.reports.get(id) {
        Some(StatsReportType::LocalCandidate(c)) | Some(StatsReportType::RemoteCandidate(c)) => {
            c.candidate_type.to_string()
        }
        _ => "unknown".to_string(),
    }
}

/// 监听 PC 状态：对端断开（Disconnected/Failed/Closed）时向会话循环报一次，
/// 用于清掉本地 session —— 否则被控端会一直持着死会话，忽略新的 Offer
fn watch_peer_disconnect(
    pc: &Arc<RTCPeerConnection>,
    tx: mpsc::UnboundedSender<RTCPeerConnectionState>,
) {
    let tx = Arc::new(parking_lot::Mutex::new(Some(tx)));
    pc.on_peer_connection_state_change(Box::new(move |state| {
        info!("🔗 PC state: {state}");
        let dead = matches!(
            state,
            RTCPeerConnectionState::Disconnected
                | RTCPeerConnectionState::Failed
                | RTCPeerConnectionState::Closed
        );
        let mut guard = tx.lock();
        if dead {
            if let Some(t) = guard.take() {
                let _ = t.send(state);
            }
        }
        Box::pin(async {})
    }));
}

fn status(sink: &EventSink, s: impl Into<String>) {
    sink(SessionEvent::Status(s.into()));
}

// ============================================================
// 主控端：信令 + 建连 + 收帧转发
// ============================================================
async fn controller_session(
    cfg: SessionConfig,
    input_rx: mpsc::UnboundedReceiver<ControlMsg>,
    sink: EventSink,
    identity: Identity,
    tofu_path: std::path::PathBuf,
) -> Result<()> {
    status(&sink, format!("连接 {} ...", cfg.server));
    let (ws, _resp) =
        connect_async_tls_with_config(&cfg.server, None, false, Some(crate::tls::connector()))
            .await
            .with_context(|| format!("连接 {} 失败", cfg.server))?;
    info!("✅ WebSocket 已连接");
    status(&sink, "信令已连接");

    let (mut ws_sink, mut ws_stream) = ws.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<WsMsg>();
    tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            match serde_json::to_string(&m) {
                Ok(j) => {
                    if ws_sink.send(Message::Text(j.into())).await.is_err() {
                        break;
                    }
                }
                Err(e) => warn!("序列化出站消息失败: {e}"),
            }
        }
    });

    out_tx.send(WsMsg::Register {
        id: cfg.id.clone(),
        role: Role::Controller,
        pubkey: identity.pubkey_b64(),
        hostname: identity::hostname(),
        info: device_info(),
    })?;

    if let Some(peer) = &cfg.peer {
        out_tx.send(WsMsg::Connect {
            target: peer.clone(),
            auth: cfg.auth.clone(),
        })?;
        info!("📤 已发送 Connect -> {peer}");
        status(&sink, format!("请求连接 {peer} ..."));
    }

    let mut session: Option<Session> = None;
    // trickle：远端描述设置前到达的候选先入队，answer 设置后回放
    let mut pending_ice: Vec<String> = Vec::new();
    let mut remote_set = false;
    // 实际对端 ID：以服务器/迷你信令下发的 Incoming.from 为准
    // （LAN 直连时 cfg.peer 是占位符，不能用于 TOFU 验签查询）
    let mut active_peer: Option<String> = cfg.peer.clone();
    let mut ka = tokio::time::interval(Duration::from_secs(15));
    let (closed_tx, mut closed_rx) = mpsc::unbounded_channel::<RTCPeerConnectionState>();

    // 活跃 DC 槽位 + 常驻输入转发任务：
    // input_rx 在这里被一次性 move 进任务，重连后新会话经槽位继续接收输入
    let active_dc: ActiveDcSlot = Default::default();
    {
        let slot = active_dc.clone();
        let mut rx = input_rx;
        tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                let json = m.to_json();
                if json.is_empty() {
                    continue;
                }
                // parking_lot Mutex 不跨 await：lock → clone Arc → 释放 → send
                let dc_opt = slot.lock().clone();
                let Some(dc) = dc_opt else { continue };
                // 控制消息必须走文本帧（SCTP PPID 文本），被控端按 is_string 过滤
                if dc.send_text(json).await.is_err() {
                    // 发送失败通常是 DC 已关闭，清槽位等下次会话重建
                    *slot.lock() = None;
                }
            }
            info!("输入转发任务退出");
        });
    }

    loop {
        tokio::select! {
            _ = ka.tick() => {
                let _ = out_tx.send(heartbeat());
            }
            Some(state) = closed_rx.recv() => {
                if let Some(s) = session.take() {
                    warn!("🔌 连接断开 ({state})，清理会话");
                    s.teardown();
                    status(&sink, "连接已断开");
                    sink(SessionEvent::Ended("连接已断开".into()));
                }
                *active_dc.lock() = None;
                remote_set = false;
                pending_ice.clear();
            }
            msg_res = ws_stream.next() => {
                let Some(Ok(msg)) = msg_res else {
                    info!("服务器断开连接");
                    status(&sink, "信令断开");
                    sink(SessionEvent::Ended("信令断开".into()));
                    break;
                };
                let text = match msg {
                    Message::Text(t) => t.to_string(),
                    Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
                    Message::Close(_) => { info!("服务器主动关闭"); break; }
                    _ => continue,
                };
                let ws_msg: WsMsg = match serde_json::from_str(&text) {
                    Ok(m) => m,
                    Err(e) => { warn!("解析服务器消息失败: {e}; raw={text}"); continue; }
                };

                match ws_msg {
                    WsMsg::Incoming { from, peer_pubkey } => {
                        info!("📨 Incoming 来自 {from}，建立主控端会话...");
                        active_peer = Some(from.clone());
                        // TOFU：首次固定被控端公钥，之后公钥变更即拒绝
                        match identity::tofu_check(&tofu_path, &from, &peer_pubkey) {
                            Ok(TofuResult::Pinned) => status(&sink, format!("已固定 {from} 公钥指纹 {}", identity::fingerprint(&peer_pubkey))),
                            Ok(TofuResult::Match) => {}
                            Ok(TofuResult::Mismatch) => {
                                error!("⚠️ {from} 公钥与已固定值不一致，拒绝连接");
                                status(&sink, format!("⚠️ {from} 身份校验失败（公钥变更），已拒绝"));
                                let _ = out_tx.send(WsMsg::SessionEnd { peer: from, reason: "pubkey_mismatch".into() });
                                continue;
                            }
                            Err(e) => warn!("TOFU 检查失败（继续）: {e}"),
                        }
                        // 新一轮会话：清掉上一轮的 trickle 残留
                        remote_set = false;
                        pending_ice.clear();
                        match setup_controller(&out_tx, sink.clone(), from.clone(), active_dc.clone(), closed_tx.clone(), cfg.ice_servers.as_slice(), &identity).await {
                            Ok(s) => {
                                session = Some(s);
                                status(&sink, format!("已连接 {from}，等待画面..."));
                            }
                            Err(e) => {
                                error!("建立主控端会话失败: {e:#}");
                                status(&sink, format!("建立失败: {e}"));
                            }
                        }
                    }
                    WsMsg::Sdp { kind: SdpKind::Answer, data, sig, .. } => {
                        if let Some(s) = &session {
                            // TOFU 验签：用已固定的被控端公钥校验 answer 签名
                            let peer_id = active_peer.clone().unwrap_or_default();
                            let pinned = identity::pinned_pubkey(&tofu_path, &peer_id).ok().flatten();
                            let ok = pinned
                                .map(|pk| sig.as_deref().map(|sg| identity::verify_detached(&pk, &data, sg)).unwrap_or(false))
                                .unwrap_or(false);
                            if !ok {
                                error!("⚠️ answer 验签失败，中断会话");
                                status(&sink, "⚠️ 对端身份验签失败，已中断");
                                let _ = out_tx.send(WsMsg::SessionEnd { peer: peer_id, reason: "bad_signature".into() });
                                if let Some(s) = session.take() { s.teardown(); }
                                *active_dc.lock() = None;
                                remote_set = false;
                                pending_ice.clear();
                                continue;
                            }
                            let sdp_str = String::from_utf8(data).context("answer SDP 非 UTF-8")?;
                            let answer = RTCSessionDescription::answer(sdp_str).context("解析 answer SDP")?;
                            s.pc.set_remote_description(answer).await?;
                            info!("✅ 已设置远端 answer（验签通过），等待 DataChannel 打开");
                            remote_set = true;
                            // 回放 answer 之前到达的 trickle 候选
                            add_remote_candidates(&s.pc, pending_ice.drain(..)).await;
                        } else {
                            warn!("收到 answer 但无会话");
                        }
                    }
                    WsMsg::Sdp { kind, .. } => warn!("不匹配角色的 SDP (kind={kind:?})"),
                    WsMsg::Ice { candidates, .. } => {
                        // trickle：远端描述就绪则直接加，否则入队等 answer
                        if remote_set {
                            if let Some(s) = &session {
                                add_remote_candidates(&s.pc, candidates).await;
                            }
                        } else {
                            pending_ice.extend(candidates);
                        }
                    }
                    WsMsg::Turn { urls, username, credential, ttl } => {
                        info!("🔐 收到 TURN 临时凭据（有效 {ttl}s）");
                        *TURN_CREDS.lock() = Some(TurnCreds { urls, username, credential });
                    }
                    WsMsg::Error { code, message } => {
                        error!("❌ 服务器错误 [{code}]: {message}");
                        status(&sink, format!("错误: {message}"));
                    }
                    WsMsg::Evict { peer, reason } => {
                        warn!("⚠️ 被驱逐 peer={peer} reason={reason}");
                        status(&sink, format!("会话结束: {reason}"));
                        if let Some(s) = session.take() {
                            s.teardown();
                        }
                        *active_dc.lock() = None;
                        remote_set = false;
                        pending_ice.clear();
                        sink(SessionEvent::Ended(reason));
                    }
                    WsMsg::SessionEnd { peer, reason } => {
                        info!("🔚 会话结束 peer={peer} reason={reason}");
                        status(&sink, format!("会话结束: {reason}"));
                        if let Some(s) = session.take() {
                            s.teardown();
                        }
                        *active_dc.lock() = None;
                        remote_set = false;
                        pending_ice.clear();
                        sink(SessionEvent::Ended(reason));
                    }
                    WsMsg::Emergency { action, delay_secs } => {
                        let tx = out_tx.clone();
                        info!("🚨 紧急控制: action={action} delay={delay_secs}s");
                        run_emergency(&action, delay_secs);
                        let _ = tx.send(WsMsg::EmergencyAck { action, ok: true });
                    }
                    WsMsg::EmergencyAck { .. } => {}
                    _ => {}
                }
            }
        }
    }

    if let Some(s) = session.take() {
        s.teardown();
    }
    Ok(())
}

// ============================================================
// 主控端 WebRTC：创建 DataChannel，收帧组装 → 原始 H.264 → sink
// ============================================================
async fn setup_controller(
    out_tx: &mpsc::UnboundedSender<WsMsg>,
    sink: EventSink,
    peer: String,
    active_dc: ActiveDcSlot,
    closed_tx: mpsc::UnboundedSender<RTCPeerConnectionState>,
    ice_servers: &[String],
    identity: &Identity,
) -> Result<Session> {
    let api = build_api();
    let pc = Arc::new(
        api.new_peer_connection(build_rtc_config(ice_servers))
            .await?,
    );
    info!("🧩 主控端 PeerConnection 创建");

    watch_peer_disconnect(&pc, closed_tx);
    spawn_pair_report(pc.clone());

    // 创建 DataChannel（必须在 create_offer 之前）
    // create_data_channel 已返回 Arc<RTCDataChannel>，外层无需再 Arc::new
    let dc = pc.create_data_channel("screen", None).await?;
    info!("📡 DataChannel 'screen' 已创建");

    // 音频 DC：被控端推送 16kHz mono i16 PCM，主控端本地播放
    let dc_audio = pc.create_data_channel("audio", None).await?;
    info!("🎙️ DataChannel 'audio' 已创建");
    let playback = match audio::AudioPlayback::start() {
        Ok(p) => Some(p),
        Err(e) => {
            warn!("音频播放初始化失败，继续无音频会话: {e:#}");
            None
        }
    };
    if let Some(pb) = &playback {
        let sink_audio = pb.sink().clone();
        // AudioSink 内部是 Arc<Mutex<...>>，clone 共享同一缓冲
        let sink2 = sink_audio.clone();
        dc_audio.on_message(Box::new(move |msg: DataChannelMessage| {
            let s = sink2.clone();
            Box::pin(async move {
                if !msg.is_string {
                    audio::feed_playback(&s, &msg.data);
                }
            })
        }));
    }

    // 二进制帧 → 组装器 → 帧 channel → sink
    // 文本帧 → 被控端回传的控制消息（显示器清单/剪贴板/链路统计）→ SessionEvent
    let (frame_tx, frame_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let assembler = Arc::new(parking_lot::Mutex::new(video::FrameAssembler::new(
        frame_tx,
    )));
    let asm = assembler.clone();
    let sink_msg = sink.clone();
    dc.on_message(Box::new(move |msg: DataChannelMessage| {
        let asm = asm.clone();
        let sink2 = sink_msg.clone();
        Box::pin(async move {
            if msg.is_string {
                let Ok(s) = std::str::from_utf8(&msg.data) else {
                    return;
                };
                let Some(m) = ControlMsg::from_json(s) else {
                    return;
                };
                match m {
                    ControlMsg::Monitors { list } => sink2(SessionEvent::Monitors(list)),
                    ControlMsg::Clipboard { text } => sink2(SessionEvent::Clipboard(text)),
                    ControlMsg::Stats {
                        fps,
                        kbps,
                        buffered,
                    } => sink2(SessionEvent::Stats {
                        fps,
                        kbps,
                        buffered,
                    }),
                    ControlMsg::Log { level, msg } => sink2(SessionEvent::Log { level, msg }),
                    ControlMsg::DriverStatus {
                        has_driver,
                        active_count,
                        driver_kind,
                        hint,
                    } => sink2(SessionEvent::DriverStatus {
                        has_driver,
                        active_count,
                        driver_kind,
                        hint,
                    }),
                    _ => {}
                }
            } else {
                asm.lock().feed(&msg.data);
            }
        })
    }));

    let tasks: TaskList = Default::default();
    tasks
        .lock()
        .push(tokio::spawn(forward_loop(frame_rx, sink.clone())));

    // DC open → 把 dc 注册到活跃槽位（外层常驻输入转发任务经此槽位发送）
    // DC close → 清空槽位；外层输入转发会因槽位空而静默丢弃消息（不阻塞 UI）
    let dc_fwd = dc.clone();
    let slot_open = active_dc.clone();
    let slot_close = active_dc.clone();
    dc.on_open(Box::new(move || {
        *slot_open.lock() = Some(dc_fwd.clone());
        Box::pin(async {})
    }));
    dc.on_close(Box::new(move || {
        *slot_close.lock() = None;
        Box::pin(async {})
    }));

    // offer + trickle：SDP 立即发出（不含候选），候选由 start_trickle 逐个转发。
    // 回调必须先于 set_local_description 注册——gathering 从那一刻开始，
    // host 候选可能同步触发，注册晚了会丢。
    let offer = pc.create_offer(None).await?;
    start_trickle(&pc, out_tx.clone(), peer.clone());
    pc.set_local_description(offer).await?;
    let local = pc
        .local_description()
        .await
        .ok_or_else(|| anyhow!("local_description 缺失"))?;

    out_tx.send(WsMsg::Sdp {
        to: peer.clone(),
        data: local.sdp.clone().into_bytes(),
        kind: SdpKind::Offer,
        sig: Some(identity.sign(local.sdp.as_bytes())),
    })?;
    info!(
        "📤 已发送 OFFER ({} bytes, 已签名) -> {peer}",
        local.sdp.len()
    );

    let mut session = Session::new(pc, tasks);
    session._audio_playback = playback;
    Ok(session)
}

/// 组装好的帧（[1B key][Annex-B]）→ SessionEvent::Video
async fn forward_loop(mut rx: mpsc::UnboundedReceiver<Vec<u8>>, sink: EventSink) {
    while let Some(mut payload) = rx.recv().await {
        if payload.is_empty() {
            continue;
        }
        let key = payload[0] != 0;
        let data = payload.split_off(1);
        sink(SessionEvent::Video { key, data });
    }
    info!("帧转发结束");
}

// ============================================================
// 被控端：信令主循环
// ============================================================
async fn controlled_session(
    cfg: SessionConfig,
    sink: EventSink,
    identity: Identity,
    tofu_path: std::path::PathBuf,
) -> Result<()> {
    status(&sink, format!("连接 {} ...", cfg.server));
    let (ws, _resp) =
        connect_async_tls_with_config(&cfg.server, None, false, Some(crate::tls::connector()))
            .await
            .with_context(|| format!("连接 {} 失败", cfg.server))?;
    info!("✅ WebSocket 已连接");
    status(&sink, "信令已连接，等待主控端接入");

    let (mut ws_sink, mut ws_stream) = ws.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<WsMsg>();
    tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            match serde_json::to_string(&m) {
                Ok(j) => {
                    if ws_sink.send(Message::Text(j.into())).await.is_err() {
                        break;
                    }
                }
                Err(e) => warn!("序列化出站消息失败: {e}"),
            }
        }
    });

    out_tx.send(WsMsg::Register {
        id: cfg.id.clone(),
        role: Role::Controlled,
        pubkey: identity.pubkey_b64(),
        hostname: identity::hostname(),
        info: device_info(),
    })?;
    info!("📤 已发送 Register");

    let mut session: Option<Session> = None;
    // trickle：候选先于 Offer 到达（极端时序）时入队，会话建立后回放
    let mut pending_ice: Vec<String> = Vec::new();
    set_busy(false);
    // TOFU：验签/公钥检查失败的主控端拉黑名单（本会话有效）
    let mut blocked: std::collections::HashSet<String> = Default::default();
    let mut ka = tokio::time::interval(Duration::from_secs(15));
    let (closed_tx, mut closed_rx) = mpsc::unbounded_channel::<RTCPeerConnectionState>();

    loop {
        tokio::select! {
            _ = ka.tick() => {
                // 心跳同时上报「是否有主控端连接」，管理端据此展示占用状态
                let _ = out_tx.send(heartbeat());
            }
            // 主控端断开：清掉死会话，否则会一直「已有会话，忽略新 Offer」
            Some(state) = closed_rx.recv() => {
                if let Some(s) = session.take() {
                    warn!("🔌 主控端连接断开 ({state})，清理会话并等待重新接入");
                    s.teardown();
                    status(&sink, "主控端已断开，等待重新接入");
                    set_busy(false);
                }
                pending_ice.clear();
            }
            msg_res = ws_stream.next() => {
                let Some(Ok(msg)) = msg_res else {
                    info!("服务器断开连接");
                    status(&sink, "信令断开");
                    sink(SessionEvent::Ended("信令断开".into()));
                    break;
                };
                let text = match msg {
                    Message::Text(t) => t.to_string(),
                    Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
                    Message::Close(_) => { info!("服务器主动关闭"); break; }
                    _ => continue,
                };
                let ws_msg: WsMsg = match serde_json::from_str(&text) {
                    Ok(m) => m,
                    Err(e) => { warn!("解析服务器消息失败: {e}; raw={text}"); continue; }
                };

                match ws_msg {
                    WsMsg::Incoming { from, peer_pubkey } => {
                        // TOFU：首次接入固定主控端公钥；公钥变更 → 拒绝并拉黑
                        match identity::tofu_check(&tofu_path, &from, &peer_pubkey) {
                            Ok(TofuResult::Pinned) => {
                                info!("📌 TOFU 固定主控端 {from} 公钥指纹 {}", identity::fingerprint(&peer_pubkey));
                                status(&sink, format!("已固定主控端 {from} 公钥指纹 {}", identity::fingerprint(&peer_pubkey)));
                            }
                            Ok(TofuResult::Match) => {}
                            Ok(TofuResult::Mismatch) => {
                                error!("⚠️ 主控端 {from} 公钥与已固定值不一致，拒绝接入");
                                status(&sink, format!("⚠️ 主控端 {from} 身份校验失败（公钥变更），已拒绝"));
                                blocked.insert(from.clone());
                                let _ = out_tx.send(WsMsg::SessionEnd { peer: from, reason: "pubkey_mismatch".into() });
                            }
                            Err(e) => warn!("TOFU 检查失败（继续）: {e}"),
                        }
                    }
                    WsMsg::Turn { urls, username, credential, ttl } => {
                        info!("🔐 收到 TURN 临时凭据（有效 {ttl}s）");
                        *TURN_CREDS.lock() = Some(TurnCreds { urls, username, credential });
                    }
                    WsMsg::Sdp { kind: SdpKind::Offer, to, data, sig } => {
                        // 服务器转发时已把 to 改写为发送方（主控端）ID
                        if blocked.contains(&to) {
                            warn!("忽略已拉黑主控端 {to} 的 Offer");
                            continue;
                        }
                        // TOFU 验签：必须用已固定的主控端公钥校验 offer 签名
                        let ok = identity::pinned_pubkey(&tofu_path, &to).ok().flatten()
                            .map(|pk| sig.as_deref().map(|sg| identity::verify_detached(&pk, &data, sg)).unwrap_or(false))
                            .unwrap_or(false);
                        if !ok {
                            error!("⚠️ 来自 {to} 的 Offer 验签失败/未固定，拒绝");
                            status(&sink, format!("⚠️ {to} 验签失败，已拒绝"));
                            let _ = out_tx.send(WsMsg::SessionEnd { peer: to, reason: "bad_signature".into() });
                            continue;
                        }
                        // 服务器只会在同一对设备间转发 Offer：又收到 Offer 说明主控端重建了连接，
                        // 旧会话（可能还没被 ICE 超时判死）必须让位，否则会一直拒绝新连接
                        if let Some(old) = session.take() {
                            warn!("收到新 Offer，关闭旧会话后重建");
                            old.teardown();
                        }
                        // 旧会话的候选对新 PC 无意义（ufrag 不同）
                        pending_ice.clear();
                        info!("📨 收到 OFFER（验签通过），建立被控端会话...");
                        status(&sink, "主控端接入，正在建立会话...");
                        let sdp_str = String::from_utf8(data).context("offer SDP 非 UTF-8")?;
                        match setup_controlled(&out_tx, to, sdp_str, cfg.fps, closed_tx.clone(), cfg.ice_servers.as_slice(), &identity).await {
                            Ok(s) => {
                                info!("✅ 被控端会话建立，answer 已发出");
                                // 回放先于会话到达的 trickle 候选（远端描述已在 setup 内设好）
                                add_remote_candidates(&s.pc, pending_ice.drain(..)).await;
                                session = Some(s);
                                set_busy(true);
                                status(&sink, "已共享屏幕，等待连接建立");
                            }
                            Err(e) => {
                                error!("建立被控端会话失败: {e:#}");
                                status(&sink, format!("建立失败: {e}"));
                            }
                        }
                    }
                    WsMsg::Ice { candidates, .. } => {
                        // trickle：setup_controlled 已设远端描述，直接加；无会话则入队
                        if let Some(s) = &session {
                            add_remote_candidates(&s.pc, candidates).await;
                        } else {
                            pending_ice.extend(candidates);
                        }
                    }
                    WsMsg::Evict { peer, reason } => {
                        warn!("⚠️ 被驱逐 peer={peer} reason={reason}");
                        status(&sink, format!("会话结束: {reason}"));
                        if let Some(s) = session.take() {
                            s.teardown();
                        }
                        set_busy(false);
                        pending_ice.clear();
                    }
                    WsMsg::SessionEnd { peer, reason } => {
                        info!("🔚 会话结束 peer={peer} reason={reason}");
                        status(&sink, format!("会话结束: {reason}"));
                        if let Some(s) = session.take() {
                            s.teardown();
                        }
                        set_busy(false);
                        pending_ice.clear();
                    }
                    WsMsg::Error { code, message } => {
                        error!("❌ 服务器错误 [{code}]: {message}");
                        status(&sink, format!("错误: {message}"));
                    }
                    WsMsg::Emergency { action, delay_secs } => {
                        let tx = out_tx.clone();
                        info!("🚨 紧急控制: action={action} delay={delay_secs}s");
                        run_emergency(&action, delay_secs);
                        let _ = tx.send(WsMsg::EmergencyAck { action, ok: true });
                    }
                    WsMsg::EmergencyAck { .. } => {}
                    _ => {}
                }
            }
        }
    }

    if let Some(s) = session.take() {
        s.teardown();
    }
    info!("被控端退出");
    Ok(())
}

// ============================================================
// 被控端 WebRTC：应答方，DC 打开后采集发送；文本帧 → 控制注入
// ============================================================
async fn setup_controlled(
    out_tx: &mpsc::UnboundedSender<WsMsg>,
    peer: String,
    offer_sdp: String,
    fps: u64,
    closed_tx: mpsc::UnboundedSender<RTCPeerConnectionState>,
    ice_servers: &[String],
    identity: &Identity,
) -> Result<Session> {
    let api = build_api();
    let pc = Arc::new(
        api.new_peer_connection(build_rtc_config(ice_servers))
            .await?,
    );
    info!("🧩 被控端 PeerConnection 创建");

    watch_peer_disconnect(&pc, closed_tx);
    spawn_pair_report(pc.clone());

    // 对端（主控端）创建的 DC 到达后：open 即启动采集 + 挂控制消息处理
    let tasks: TaskList = Default::default();
    let ts = tasks.clone();
    pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
        let ts_open = ts.clone();
        Box::pin(async move {
            let label = dc.label().to_string();
            info!("📡 收到 DataChannel '{label}'");

            if label == "screen" {
                // 采集句柄槽位：on_open 里创建，on_message 里响应 SwitchMonitor
                let cap_slot: Arc<parking_lot::Mutex<Option<video::CaptureHandle>>> =
                    Default::default();
                // 编码统计槽位：主控端画质设置需要直接改目标码率
                let stats_slot: Arc<parking_lot::Mutex<Option<Arc<video::Stats>>>> =
                    Default::default();

                // open → 采集监督线程 + 发送任务 + 显示器清单推送 + 剪贴板轮询
                let dc_for_open = dc.clone();
                // 同时给 on_message 闭包一份（虚拟屏增删后推新 Monitors 用）
                let dc_for_cmd = dc_for_open.clone();
                let cap_slot_open = cap_slot.clone();
                let stats_slot_open = stats_slot.clone();
                dc.on_open(Box::new(move || {
                    let dc2 = dc_for_open.clone();
                    let ts2 = ts_open.clone();
                    let cap_slot2 = cap_slot_open.clone();
                    Box::pin(async move {
                        info!("🟢 DataChannel open，启动屏幕采集 (fps={fps})");
                        let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
                        let stats = Arc::new(video::Stats::new(video::DEFAULT_BITRATE));
                        let handle = video::spawn_capture_supervisor(tx, fps, stats.clone());
                        *cap_slot2.lock() = Some(handle);
                        *stats_slot_open.lock() = Some(stats.clone());

                        // 推送显示器清单（主控端下拉切换用）
                        let list: Vec<MonitorDesc> = capture::enumerate()
                            .into_iter()
                            .enumerate()
                            .map(|(i, m)| MonitorDesc {
                                index: i as u32,
                                name: m.name,
                                width: m.width,
                                height: m.height,
                                primary: m.primary,
                                is_virtual: m.is_virtual,
                            })
                            .collect();
                        info!("🖥️ 显示器清单: {} 台", list.len());
                        let _ = dc2.send_text(ControlMsg::Monitors { list }.to_json()).await;

                        // 帧发送 + 码率自适应 + 链路上报（teardown abort 后 rx 断开，采集随之退出）
                        let dc_send = dc2.clone();
                        let h = tokio::spawn(async move {
                            if let Err(e) = video::send_loop(dc_send, rx, stats).await {
                                warn!("发送循环退出: {e:#}");
                            }
                        });
                        ts2.lock().push(h);

                        // 新会话开始：锁屏期间可能留下附着过 Winlogon 的旧注入线程，
                        // 它的 SendInput 已永久失效，这里直接重建，保证本会话第一次注入就生效。
                        #[cfg(windows)]
                        control::reset_input_thread();

                        // 剪贴板轮询：本机复制 → 同步给主控端（1s 粒度，回环按内容抑制）
                        // 顺带 250ms 检测锁屏状态：解锁瞬间主动重建注入线程，
                        // 否则旧线程附过 Winlogon 后 SendInput 会一直失败。
                        let dc_clip = dc2.clone();
                        let hc = tokio::spawn(async move {
                            let mut last_seq = control::clipboard_seq();
                            #[cfg(windows)]
                            let mut was_secure = crate::desktop::is_secure_desktop();
                            let mut tick = tokio::time::interval(Duration::from_millis(250));
                            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                            loop {
                                tick.tick().await;
                                #[cfg(windows)]
                                {
                                    let secure = crate::desktop::is_secure_desktop();
                                    if was_secure && !secure {
                                        info!("🖥️ 检测到解锁：主动重建注入线程以恢复键鼠");
                                        control::reset_input_thread();
                                    }
                                    was_secure = secure;
                                }
                                let seq = control::clipboard_seq();
                                if seq == last_seq {
                                    continue;
                                }
                                last_seq = seq;
                                let Some(text) = control::get_clipboard() else {
                                    continue;
                                };
                                if text.is_empty() || text == control::last_clip() {
                                    continue;
                                }
                                control::note_clip(&text);
                                if dc_clip
                                    .send_text(ControlMsg::Clipboard { text }.to_json())
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            info!("剪贴板同步结束");
                        });
                        ts2.lock().push(hc);
                    })
                }));

                // 控制消息（文本帧 JSON）：SwitchMonitor 拦截给采集监督线程，其余走控制注入
                // dc_for_cmd 已在上面 clone 好，用于虚拟屏增删后推新 Monitors 清单
                dc.on_message(Box::new(move |msg: DataChannelMessage| {
                    let cap_slot3 = cap_slot.clone();
                    let stats_slot3 = stats_slot.clone();
                    let dc_cmd = dc_for_cmd.clone();
                    Box::pin(async move {
                        if !msg.is_string {
                            return;
                        }
                        let Ok(s) = std::str::from_utf8(&msg.data) else {
                            return;
                        };
                        let Some(m) = ControlMsg::from_json(s) else {
                            return;
                        };
                        match m {
                            ControlMsg::SwitchMonitor { index } => {
                                info!("🖥️ 主控端请求切换到显示器 #{index}");
                                if let Some(h) = cap_slot3.lock().as_ref() {
                                    h.switch_monitor(index);
                                }
                            }
                            ControlMsg::Refresh => {
                                info!("🔄 主控端请求刷新画面");
                                if let Some(h) = cap_slot3.lock().as_ref() {
                                    h.restart();
                                }
                            }
                            ControlMsg::SetQuality {
                                level,
                                bitrate_kbps,
                            } => {
                                info!("🎚️ 主控端设置画质: {level} ({bitrate_kbps} kbps)");
                                if let Some(st) = stats_slot3.lock().as_ref() {
                                    st.set_quality(&level, bitrate_kbps);
                                }
                            }
                            ControlMsg::SetResolution { width, height } => {
                                control::handle_msg(ControlMsg::SetResolution { width, height });
                                // 等系统应用新模式后再重建采集，否则仍是旧尺寸
                                tokio::time::sleep(Duration::from_millis(900)).await;
                                if let Some(h) = cap_slot3.lock().as_ref() {
                                    h.restart();
                                }
                            }
                            ControlMsg::AddVirtualDisplay {
                                count,
                                width,
                                height,
                            } => {
                                info!("🖥️ 主控端请求新增 {count} 块虚拟屏（{width}x{height}）");
                                match control::add_virtual(count, width, height) {
                                    Ok(_) => {
                                        // 驱动注册新屏到 DWM 需要几百毫秒
                                        tokio::time::sleep(Duration::from_millis(900)).await;
                                        // 推新 Monitors 清单给主控端，让下拉刷新
                                        refresh_monitors(&dc_cmd).await;
                                        // 推驱动状态
                                        push_driver_status(&dc_cmd);
                                        // 如果当前在采集那块虚拟屏上或者显示器列表变了，重建采集
                                        if let Some(h) = cap_slot3.lock().as_ref() {
                                            h.restart();
                                        }
                                        let _ = dc_cmd
                                            .send_text(
                                                ControlMsg::Log {
                                                    level: "info".into(),
                                                    msg: format!("✅ 已新增 {count} 块虚拟屏，驱动注册完成"),
                                                }
                                                .to_json()
                                                .as_str(),
                                            )
                                            .await;
                                    }
                                    Err(e) => {
                                        warn!("🖥️ 新增虚拟屏失败: {e:#}");
                                        push_driver_status(&dc_cmd);
                                        let _ = dc_cmd
                                            .send_text(
                                                ControlMsg::Log {
                                                    level: "warn".into(),
                                                    msg: format!("⚠️ 新增虚拟屏失败：{e}"),
                                                }
                                                .to_json()
                                                .as_str(),
                                            )
                                            .await;
                                    }
                                }
                            }
                            ControlMsg::RemoveVirtualDisplay { index } => {
                                info!("🖥️ 主控端请求删除虚拟屏 #{index}");
                                match control::remove_virtual(index) {
                                    Ok(_) => {
                                        tokio::time::sleep(Duration::from_millis(900)).await;
                                        refresh_monitors(&dc_cmd).await;
                                        push_driver_status(&dc_cmd);
                                        if let Some(h) = cap_slot3.lock().as_ref() {
                                            h.restart();
                                        }
                                        let _ = dc_cmd
                                            .send_text(
                                                ControlMsg::Log {
                                                    level: "info".into(),
                                                    msg: format!("✅ 已删除虚拟屏 #{index}"),
                                                }
                                                .to_json()
                                                .as_str(),
                                            )
                                            .await;
                                    }
                                    Err(e) => {
                                        warn!("🖥️ 删除虚拟屏失败: {e:#}");
                                        push_driver_status(&dc_cmd);
                                        let _ = dc_cmd
                                            .send_text(
                                                ControlMsg::Log {
                                                    level: "warn".into(),
                                                    msg: format!("⚠️ 删除虚拟屏失败：{e}"),
                                                }
                                                .to_json()
                                                .as_str(),
                                            )
                                            .await;
                                    }
                                }
                            }
                            ControlMsg::QueryDriverStatus => {
                                push_driver_status(&dc_cmd);
                            }
                            other => control::handle_msg(other),
                        }
                    })
                }));
            } else if label == "audio" {
                // 音频 DC：open 后启动麦克风采集，PCM 推送至该 DC
                let audio_slot: Arc<parking_lot::Mutex<Option<audio::AudioCapture>>> =
                    Default::default();
                let dc_audio = dc.clone();
                dc.on_open(Box::new(move || {
                    let dc_capture = dc_audio.clone();
                    let slot = audio_slot.clone();
                    Box::pin(async move {
                        match audio::AudioCapture::start(dc_capture) {
                            Ok(cap) => {
                                info!("🎙️ 音频采集已启动");
                                *slot.lock() = Some(cap);
                            }
                            Err(e) => warn!("音频采集启动失败（会话继续无音频）: {e:#}"),
                        }
                    })
                }));
            }
        })
    }));

    let offer = RTCSessionDescription::offer(offer_sdp).context("解析 offer SDP")?;
    pc.set_remote_description(offer).await?;
    let answer = pc.create_answer(None).await?;
    start_trickle(&pc, out_tx.clone(), peer.clone());
    pc.set_local_description(answer).await?;
    let local = pc
        .local_description()
        .await
        .ok_or_else(|| anyhow!("local_description 缺失"))?;

    out_tx.send(WsMsg::Sdp {
        to: peer.clone(),
        data: local.sdp.clone().into_bytes(),
        kind: SdpKind::Answer,
        sig: Some(identity.sign(local.sdp.as_bytes())),
    })?;
    info!(
        "📤 已发送 ANSWER ({} bytes, 已签名) -> {peer}",
        local.sdp.len()
    );

    Ok(Session::new(pc, tasks))
}

// ============================================================
// ICE trickle：候选逐个经服务器转发给对端
// ============================================================
// 非 trickle 模式要等全部候选收集完才发 SDP——一个慢 STUN 节点就拖累整体建连，
// 且 relay_acceptance_min_wait 调优的候选也无法在 SDP 之后补充。trickle 后 SDP
// 立即发出（只含 ufrag/pwd），候选边收集边发，建连延迟显著降低。
//
// 候选序列化为 RTCIceCandidateInit 的 JSON（保留 sdp_mid / mline_index，
// 只发 candidate 字符串会导致对端 add_ice_candidate 定位失败）。
fn start_trickle(pc: &Arc<RTCPeerConnection>, out_tx: mpsc::UnboundedSender<WsMsg>, peer: String) {
    pc.on_ice_candidate(Box::new(move |cand| {
        let Some(c) = cand else {
            return Box::pin(async {});
        };
        match c
            .to_json()
            .map_err(|e| e.to_string())
            .and_then(|init| serde_json::to_string(&init).map_err(|e| e.to_string()))
        {
            Ok(j) => {
                // 只摘 candidate 行的关键部分（typ host/srflx/relay + 地址），全行太长
                let brief = j
                    .split("candidate:")
                    .nth(1)
                    .map(|s| s.split("\\r").next().unwrap_or(s))
                    .unwrap_or("?");
                info!("🧊 trickle 发出候选: {brief}");
                let _ = out_tx.send(WsMsg::Ice {
                    to: peer.clone(),
                    candidates: vec![j],
                });
            }
            Err(e) => warn!("🧊 候选序列化失败: {e}"),
        }
        Box::pin(async {})
    }));
}

/// 把对端 trickle 过来的候选逐个加进 PC（远端描述未设时会由底层排队/报错，调用方负责时序）
async fn add_remote_candidates(
    pc: &RTCPeerConnection,
    candidates: impl IntoIterator<Item = String>,
) {
    for c in candidates {
        match serde_json::from_str::<RTCIceCandidateInit>(&c) {
            Ok(init) => {
                if let Err(e) = pc.add_ice_candidate(init).await {
                    warn!("🧊 add_ice_candidate 失败: {e}");
                }
            }
            Err(e) => warn!("🧊 候选解析失败: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_stun_and_turn() {
        let servers = parse_ice_servers(&[
            "stun:stun.l.google.com:19302".to_string(),
            "turn:alice:secret@turn.example.com:3478".to_string(),
        ]);
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].urls[0], "stun:stun.l.google.com:19302");
        assert!(servers[0].username.is_empty());
        assert_eq!(servers[1].urls[0], "turn:turn.example.com:3478");
        assert_eq!(servers[1].username, "alice");
        assert_eq!(servers[1].credential, "secret");
        // webrtc-rs 对 Unspecified 会报 invalid turn server credentials
        assert_eq!(servers[1].credential_type, RTCIceCredentialType::Password);
    }

    #[test]
    fn parse_comma_separated_and_no_cred() {
        let servers = parse_ice_servers(&["stun:a:19302,turn:b:3478".to_string()]);
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].urls[0], "stun:a:19302");
        assert_eq!(servers[1].urls[0], "turn:b:3478");
        assert!(servers[1].username.is_empty());
    }
}
