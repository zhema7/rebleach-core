//! 轻量主控链路（无界面、无视频）：为 `rebleach exec / tunnel / proxy` 提供
//! 「信令 → WebRTC → term/tunnel DataChannel」的最小闭环。
//!
//! 与交互式 `controller_session` 的区别：
//! - 不创建 screen/media/input/audio 通道 → 被控端不会启动采集管线，
//!   纯终端/隧道会话在被控端近乎零开销；
//! - 不需要 winit / Tauri，事件走 mpsc 给 CLI 消费；
//! - 复用同一套 TOFU 身份、SDP 签名、trickle ICE 与 TURN 凭据逻辑。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

use crate::identity::{self, Identity, TofuResult};
use crate::protocol::{Role, SdpKind, WsMsg};
use crate::session::{build_api, build_rtc_config};

/// 无界面链路配置
#[derive(Debug, Clone)]
pub struct HeadlessConfig {
    /// 信令服务器 WebSocket 地址
    pub server: String,
    /// 主控端（本机）设备 ID
    pub id: String,
    /// 目标被控端设备 ID
    pub peer: String,
    /// STUN/TURN 列表（同 SessionConfig.ice_servers）
    pub ice_servers: Vec<String>,
    /// 身份密钥文件；None = 默认路径
    pub key_file: Option<std::path::PathBuf>,
    /// 局域网直连验证码
    pub auth: Option<String>,
}

/// 链路事件：term/tunnel 解码后的消息 + 状态通知
#[derive(Debug)]
pub enum LinkEvent {
    Term(crate::term::TermMsg),
    Tunnel(crate::tunnel::TunnelMsg),
    /// 连接状态（错误/断开）——CLI 收到后链路不再可用，应尽快退出
    Status(String),
}

/// 一条已建立的无界面链路（事件流由 connect 单独返回：
/// Receiver 需要 &mut，放进共享结构会让 Arc 包装不可行）
pub struct HeadlessLink {
    pc: Arc<RTCPeerConnection>,
    term_dc: Arc<RTCDataChannel>,
    tunnel_dc: Option<Arc<RTCDataChannel>>,
    session_id: String,
    /// 对端设备 ID（以信令下发的实际 ID 为准）
    pub peer: String,
}

impl HeadlessLink {
    /// 发送 term 帧（exec：Data/Resize）
    pub async fn send_term(&self, m: crate::term::TermMsg) -> Result<()> {
        let bytes = bytes::Bytes::from(m.encode());
        self.term_dc
            .send(&bytes)
            .await
            .map(|_| ())
            .context("term 通道发送失败（连接已断开）")
    }

    /// 发送 tunnel 帧（Open/Data/Close）
    pub async fn send_tunnel(&self, m: crate::tunnel::TunnelMsg) -> Result<()> {
        let Some(dc) = &self.tunnel_dc else {
            bail!("隧道通道未建立");
        };
        let bytes = bytes::Bytes::from(m.encode());
        dc.send(&bytes)
            .await
            .map(|_| ())
            .context("tunnel 通道发送失败（连接已断开）")
    }

    /// 本次会话审计 ID（录制文件目录名；与图形侧录制同一命名规则）
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// 关闭链路
    pub async fn close(&self) {
        let _ = self.pc.close().await;
    }
}

/// 建立无界面链路：信令注册 → Connect → TOFU → Offer/Answer → 等 term DC 打开。
/// `want_tunnel` 为 true 时同时创建 tunnel 通道（被控端需 0.4.0+）。
pub async fn connect(
    cfg: HeadlessConfig,
    want_tunnel: bool,
) -> Result<(HeadlessLink, mpsc::UnboundedReceiver<LinkEvent>)> {
    // TOFU 身份：与交互式主控端同一套密钥与固定记录
    let key_path = cfg
        .key_file
        .clone()
        .unwrap_or_else(identity::default_key_file);
    let id = Identity::load_or_generate(&key_path).context("加载/生成设备身份密钥失败")?;
    let tofu_path = identity::default_tofu_file(&key_path);

    info!("🔗 无界面链路：连接 {} -> {peer}", cfg.server, peer = cfg.peer);
    let (ws, _resp) = tokio_tungstenite::connect_async_tls_with_config(
        &cfg.server,
        None,
        false,
        Some(crate::tls::connector()),
    )
    .await
    .with_context(|| format!("连接 {} 失败", cfg.server))?;
    let (mut ws_sink, mut ws_stream) = ws.split();

    // 信令发送任务：内部通道 → WebSocket（读写解耦，避免生命周期纠缠）
    let (sig_tx, mut sig_rx) = mpsc::unbounded_channel::<WsMsg>();
    tokio::spawn(async move {
        while let Some(m) = sig_rx.recv().await {
            match serde_json::to_string(&m) {
                Ok(j) => {
                    if ws_sink.send(Message::Text(j.into())).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    async fn sig_send(tx: &mpsc::UnboundedSender<WsMsg>, m: WsMsg) -> Result<()> {
        tx.send(m).context("信令发送通道已关闭")
    }

    sig_send(&sig_tx, WsMsg::Register {
        id: cfg.id.clone(),
        role: Role::Controller,
        pubkey: id.pubkey_b64(),
        hostname: identity::hostname(),
        info: None,
    })
    .await?;
    sig_send(&sig_tx, WsMsg::Connect {
        target: cfg.peer.clone(),
        auth: cfg.auth.clone(),
    })
    .await?;
    info!("📤 已发送 Connect -> {}", cfg.peer);

    // 等 Incoming（服务器把被控端公钥带回来，完成 TOFU 固定/校验）
    let from = tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(Ok(msg)) = ws_stream.next().await {
            let text = match msg {
                Message::Text(t) => t.to_string(),
                Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
                Message::Close(_) => bail!("信令连接被服务器关闭"),
                _ => continue,
            };
            let Ok(m) = serde_json::from_str::<WsMsg>(&text) else { continue };
            match m {
                WsMsg::Incoming { from, peer_pubkey } => {
                    match identity::tofu_check(&tofu_path, &from, &peer_pubkey) {
                        Ok(TofuResult::Pinned) => {
                            info!(
                                "📌 已固定被控端 {from} 公钥指纹 {}",
                                identity::fingerprint(&peer_pubkey)
                            );
                        }
                        Ok(TofuResult::Match) => {}
                        Ok(TofuResult::Mismatch) => {
                            bail!("被控端 {from} 公钥与已固定值不一致，拒绝连接");
                        }
                        Err(e) => warn!("TOFU 检查失败（继续）: {e}"),
                    }
                    return Ok(from);
                }
                WsMsg::Error { code, message } => {
                    bail!("信令错误 [{code}]: {message}");
                }
                _ => continue,
            }
        }
        bail!("信令连接中断")
    })
    .await
    .map_err(|_| anyhow!("等待被控端响应超时（30s），设备可能离线"))??;
    info!("📨 Incoming 来自 {from}，建立 WebRTC...");

    // PeerConnection：复用主控端的 ICE 调优（虚拟网卡过滤 / 直连优先）
    let api = build_api();
    let pc = Arc::new(
        api.new_peer_connection(build_rtc_config(&cfg.ice_servers))
            .await?,
    );

    let (event_tx, event_rx) = mpsc::unbounded_channel::<LinkEvent>();

    // PC 状态监控：断开/失败 → Status 事件，CLI 据此退出而不是挂死
    {
        let tx = event_tx.clone();
        pc.on_peer_connection_state_change(Box::new(move |state| {
            let tx = tx.clone();
            Box::pin(async move {
                if matches!(
                    state,
                    RTCPeerConnectionState::Disconnected
                        | RTCPeerConnectionState::Failed
                        | RTCPeerConnectionState::Closed
                ) {
                    let _ = tx.send(LinkEvent::Status(format!("连接断开 ({state})")));
                }
            })
        }));
    }

    // DC 公共初始化：ordered + reliable（与交互式主控端一致）
    let dc_init = Some(RTCDataChannelInit {
        ordered: Some(true),
        ..Default::default()
    });

    // term 通道
    let dc_term = pc.create_data_channel("term", dc_init.clone()).await?;
    let (term_open_tx, term_open_rx) = tokio::sync::oneshot::channel::<Result<()>>();
    {
        let tx = event_tx.clone();
        dc_term.on_message(Box::new(move |msg| {
            let tx = tx.clone();
            Box::pin(async move {
                if msg.is_string {
                    return;
                }
                if let Some(m) = crate::term::TermMsg::decode(&msg.data) {
                    let _ = tx.send(LinkEvent::Term(m));
                }
            })
        }));
        let tx = event_tx.clone();
        dc_term.on_open(Box::new(move || {
            let tx = tx.clone();
            Box::pin(async move {
                info!("🖥️ term DataChannel 已打开");
                let _ = tx.send(LinkEvent::Status("term-open".into()));
                let _ = term_open_tx.send(Ok(()));
            })
        }));
        let tx = event_tx.clone();
        dc_term.on_close(Box::new(move || {
            let tx = tx.clone();
            Box::pin(async move {
                let _ = tx.send(LinkEvent::Status("term 通道已关闭".into()));
            })
        }));
    }

    // tunnel 通道（可选）
    let (tunnel_open_tx, tunnel_open_rx) = tokio::sync::oneshot::channel::<Result<()>>();
    let tunnel_dc: Option<Arc<RTCDataChannel>> = if want_tunnel {
        let dc_tunnel = pc.create_data_channel("tunnel", dc_init).await?;
        let tx = event_tx.clone();
        dc_tunnel.on_message(Box::new(move |msg| {
            let tx = tx.clone();
            Box::pin(async move {
                if msg.is_string {
                    return;
                }
                if let Some(m) = crate::tunnel::TunnelMsg::decode(&msg.data) {
                    let _ = tx.send(LinkEvent::Tunnel(m));
                }
            })
        }));
        dc_tunnel.on_open(Box::new(move || {
            Box::pin(async move {
                info!("🔌 tunnel DataChannel 已打开");
                let _ = tunnel_open_tx.send(Ok(()));
            })
        }));
        Some(dc_tunnel)
    } else {
        None
    };

    // offer + trickle：SDP 先发，候选边收集边发（与交互式一致）
    let offer = pc.create_offer(None).await?;
    {
        let sig = sig_tx.clone();
        let peer_ws = from.clone();
        pc.on_ice_candidate(Box::new(move |cand| {
            let sig = sig.clone();
            let peer_ws = peer_ws.clone();
            Box::pin(async move {
                let Some(c) = cand else { return };
                if let Ok(init) = c.to_json()
                    && let Ok(j) = serde_json::to_string(&init)
                {
                    let _ = sig.send(WsMsg::Ice {
                        to: peer_ws.clone(),
                        candidates: vec![j],
                    });
                }
            })
        }));
    }
    pc.set_local_description(offer).await?;
    let local = pc
        .local_description()
        .await
        .ok_or_else(|| anyhow!("local_description 缺失"))?;

    sig_send(&sig_tx, WsMsg::Sdp {
        to: from.clone(),
        data: local.sdp.clone().into_bytes(),
        kind: SdpKind::Offer,
        sig: Some(id.sign(local.sdp.as_bytes())),
    })
    .await?;
    info!(
        "📤 已发送 OFFER ({} bytes, 已签名) -> {from}",
        local.sdp.len()
    );

    // 信令读取任务：answer → 设置远端描述；Ice → trickle 回放；Error/SessionEnd → Status
    let (answer_tx, answer_rx) = tokio::sync::oneshot::channel::<Result<()>>();
    {
        let pc2 = pc.clone();
        let tofu2 = tofu_path.clone();
        let from2 = from.clone();
        let tx = event_tx.clone();
        let sig2 = sig_tx.clone();
        let pending: Arc<parking_lot::Mutex<Vec<String>>> = Default::default();
        let answered: Arc<std::sync::atomic::AtomicBool> = Default::default();
        tokio::spawn(async move {
            // oneshot Sender 只能发一次：用 Option 收敛多次发送点（先到先得）
            let mut answer_tx = Some(answer_tx);
            while let Some(Ok(msg)) = ws_stream.next().await {
                let text = match msg {
                    Message::Text(t) => t.to_string(),
                    Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
                    Message::Close(_) => break,
                    _ => continue,
                };
                let Ok(m) = serde_json::from_str::<WsMsg>(&text) else { continue };
                match m {
                    WsMsg::Sdp { kind: SdpKind::Answer, data, sig, .. } => {
                        // TOFU 验签：被控端 answer 必须能被已固定公钥验证
                        let ok = identity::pinned_pubkey(&tofu2, &from2)
                            .ok()
                            .flatten()
                            .map(|pk| {
                                sig.as_deref()
                                    .map(|sg| identity::verify_detached(&pk, &data, sg))
                                    .unwrap_or(false)
                            })
                            .unwrap_or(false);
                        if !ok {
                            let _ = tx.send(LinkEvent::Status("对端 answer 验签失败".into()));
                            if let Some(a) = answer_tx.take() {
                                let _ = a.send(Err(anyhow!("对端 answer 验签失败")));
                            }
                            break;
                        }
                        match String::from_utf8(data)
                            .context("answer SDP 非 UTF-8")
                            .and_then(|s| {
                                RTCSessionDescription::answer(s).context("解析 answer SDP")
                            }) {
                            Ok(answer) => {
                                if let Err(e) = pc2.set_remote_description(answer).await {
                                    let _ = tx
                                        .send(LinkEvent::Status(format!("设置 answer 失败: {e}")));
                                    if let Some(a) = answer_tx.take() {
                                        let _ = a.send(Err(anyhow!("设置 answer 失败: {e}")));
                                    }
                                    break;
                                }
                                answered.store(true, std::sync::atomic::Ordering::Relaxed);
                                let backlog: Vec<String> = pending.lock().drain(..).collect();
                                for c in backlog {
                                    add_candidate(&pc2, c).await;
                                }
                                if let Some(a) = answer_tx.take() {
                                    let _ = a.send(Ok(()));
                                }
                            }
                            Err(e) => {
                                let _ = tx.send(LinkEvent::Status(format!("{e:#}")));
                                if let Some(a) = answer_tx.take() {
                                    let _ = a.send(Err(anyhow!("{e:#}")));
                                }
                                break;
                            }
                        }
                    }
                    WsMsg::Ice { candidates, .. } => {
                        if answered.load(std::sync::atomic::Ordering::Relaxed) {
                            for c in candidates {
                                add_candidate(&pc2, c).await;
                            }
                        } else {
                            pending.lock().extend(candidates);
                        }
                    }
                    WsMsg::Turn { urls, username, credential, ttl } => {
                        info!("🔐 收到 TURN 临时凭据（有效 {ttl}s）");
                        crate::session::store_turn_creds(urls, username, credential);
                    }
                    WsMsg::Error { code, message } => {
                        let _ = tx.send(LinkEvent::Status(format!("信令错误 [{code}]: {message}")));
                        if let Some(a) = answer_tx.take() {
                            let _ = a.send(Err(anyhow!("信令错误 [{code}]: {message}")));
                        }
                        break;
                    }
                    WsMsg::SessionEnd { reason, .. } => {
                        let _ = tx.send(LinkEvent::Status(format!("会话结束: {reason}")));
                        break;
                    }
                    _ => {}
                }
            }
            // 读取循环退出（服务器断开）：若还没拿到 answer，唤醒等待方报错
            if let Some(a) = answer_tx.take() {
                let _ = a.send(Err(anyhow!("信令连接中断")));
            }
            let _ = sig2;
        });
    }

    // 等 answer + DC 打开（整体 30s 超时）
    tokio::time::timeout(Duration::from_secs(30), async {
        answer_rx
            .await
            .map_err(|_| anyhow!("信令断开，未收到 answer"))??;
        term_open_rx
            .await
            .map_err(|_| anyhow!("term 通道异常关闭"))?
            .map_err(|e| anyhow!("term 通道打开失败: {e}"))?;
        if want_tunnel {
            tunnel_open_rx
                .await
                .map_err(|_| anyhow!("tunnel 通道异常关闭"))?
                .map_err(|e| anyhow!("tunnel 通道打开失败: {e}"))?;
        }
        Ok::<(), anyhow::Error>(())
    })
    .await
    .map_err(|_| anyhow!("建连超时（30s）：被控端未响应"))??;

    if want_tunnel && tunnel_dc.is_none() {
        bail!("tunnel 通道创建失败");
    }

    let session_id = new_session_id();
    info!("✅ 无界面链路已建立 (peer={from}, session={session_id})");
    Ok((
        HeadlessLink {
            pc,
            term_dc: dc_term,
            tunnel_dc,
            session_id,
            peer: from,
        },
        event_rx,
    ))
}

async fn add_candidate(pc: &Arc<RTCPeerConnection>, json: String) {
    match serde_json::from_str::<webrtc::ice_transport::ice_candidate::RTCIceCandidateInit>(&json) {
        Ok(init) => {
            if let Err(e) = pc.add_ice_candidate(init).await {
                warn!("添加候选失败: {e}");
            }
        }
        Err(e) => warn!("候选 JSON 解析失败: {e}"),
    }
}

/// 会话审计 ID：8 字节随机 hex（与图形侧录制同一命名风格）
fn new_session_id() -> String {
    use rand::RngCore;
    let mut b = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}
