// 局域网直连：
// - 被控端内嵌迷你信令（0.0.0.0:7681 /ws）：复用 WsMsg 协议，单被控 + 单主控，
//   Connect.auth 校验 6 位验证码，绕过管理端授权
// - UDP 发现（7682）：主控端广播 REBLEACH_DISCOVER，被控端回 REBLEACH_HERE|id|hostname
// 主控端只用到 discover()；被控端用到 run_relay() / run_discovery_responder() / gen_pin()
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::mpsc;
use tracing::{info, warn};

pub const LAN_PORT: u16 = 7681;
pub const DISCOVER_PORT: u16 = 7682;
const DISCOVER_MAGIC: &str = "REBLEACH_DISCOVER";
const REPLY_MAGIC: &str = "REBLEACH_HERE";

/// 生成 6 位数字验证码（进程级，每次启动更换）
pub fn gen_pin() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 + d.as_secs())
        .unwrap_or(0);
    let v = nanos
        .wrapping_mul(6364136223846793005)
        .wrapping_add(std::process::id() as u64)
        % 900_000
        + 100_000;
    format!("{v:06}")
}

/// 发现的局域网设备
#[derive(Debug, Clone, Serialize)]
pub struct LanDevice {
    pub id: String,
    pub addr: String,
    pub hostname: String,
}

/// 主控端：UDP 广播发现局域网内的被控端（等待 ~1.5s 收集回复）
#[cfg(feature = "controller")]
pub async fn discover() -> Vec<LanDevice> {
    use tokio::net::UdpSocket;
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0").await else {
        return Vec::new();
    };
    if sock.set_broadcast(true).is_err() {
        return Vec::new();
    }
    if sock
        .send_to(
            DISCOVER_MAGIC.as_bytes(),
            format!("255.255.255.255:{DISCOVER_PORT}"),
        )
        .await
        .is_err()
    {
        return Vec::new();
    }
    let mut out: Vec<LanDevice> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
    let mut buf = [0u8; 512];
    loop {
        let recv = tokio::time::timeout_at(deadline, sock.recv_from(&mut buf)).await;
        let Ok(Ok((n, from))) = recv else { break };
        let text = String::from_utf8_lossy(&buf[..n]).to_string();
        let mut parts = text.split('|');
        if parts.next() != Some(REPLY_MAGIC) {
            continue;
        }
        let id = parts.next().unwrap_or_default().to_string();
        let hostname = parts.next().unwrap_or_default().to_string();
        let addr = from.ip().to_string();
        if !id.is_empty() && !out.iter().any(|d| d.id == id && d.addr == addr) {
            out.push(LanDevice { id, addr, hostname });
        }
    }
    out
}

// ============================================================
// 被控端：UDP 发现应答 + 迷你信令
// ============================================================
#[cfg(feature = "agent")]
pub fn spawn_discovery_responder(device_id: String) {
    // setup 回调运行在 Tauri 主线程（无 tokio 上下文），必须走 Tauri 运行时
    tauri::async_runtime::spawn(async move {
        use tokio::net::UdpSocket;
        let Ok(sock) = UdpSocket::bind(("0.0.0.0", DISCOVER_PORT)).await else {
            warn!("局域网发现端口 {DISCOVER_PORT} 绑定失败（可能被其他实例占用）");
            return;
        };
        info!("📡 局域网发现应答 udp/{DISCOVER_PORT}");
        let mut buf = [0u8; 512];
        loop {
            let Ok((n, from)) = sock.recv_from(&mut buf).await else {
                continue;
            };
            if &buf[..n] != DISCOVER_MAGIC.as_bytes() {
                continue;
            }
            let hostname = std::env::var("COMPUTERNAME").unwrap_or_default();
            let reply = format!("{REPLY_MAGIC}|{device_id}|{hostname}");
            let _ = sock.send_to(reply.as_bytes(), from).await;
        }
    });
}

/// 迷你信令状态：一个被控端（本机 Agent 自连）+ 最多一个主控端
#[cfg(feature = "agent")]
struct RelayState {
    controlled: Mutex<Option<Peer>>,
    controller: Mutex<Option<Peer>>,
    pin: String,
}

#[cfg(feature = "agent")]
#[derive(Clone)]
struct Peer {
    id: String,
    pubkey: String,
    tx: mpsc::UnboundedSender<String>,
}

#[cfg(feature = "agent")]
pub fn spawn_relay(pin: String) {
    use axum::Router;
    use axum::extract::State as AxumState;
    use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
    use axum::response::IntoResponse;
    use axum::routing::get;

    let state = Arc::new(RelayState {
        controlled: Mutex::new(None),
        controller: Mutex::new(None),
        pin,
    });

    async fn handler(
        ws: WebSocketUpgrade,
        AxumState(st): AxumState<Arc<RelayState>>,
    ) -> impl IntoResponse {
        ws.on_upgrade(move |sock| serve(sock, st))
    }

    async fn serve(socket: WebSocket, st: Arc<RelayState>) {
        use futures_util::{SinkExt, StreamExt};
        use rebleach_client::protocol::{Role, WsMsg};

        let (mut ws_sink, mut ws_stream) = socket.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let writer = tokio::spawn(async move {
            while let Some(text) = rx.recv().await {
                if ws_sink.send(WsMessage::Text(text.into())).await.is_err() {
                    break;
                }
            }
        });

        let mut my_role: Option<Role> = None;
        let mut my_id = String::new();
        // 主控端 Register 后暂存，验证码通过才正式登记（此前收不到被控端任何消息）
        let mut pending: Option<Peer> = None;

        while let Some(Ok(msg)) = ws_stream.next().await {
            let text = match msg {
                WsMessage::Text(t) => t.to_string(),
                WsMessage::Binary(b) => String::from_utf8_lossy(&b).to_string(),
                WsMessage::Close(_) => break,
                _ => continue,
            };
            let Ok(m) = serde_json::from_str::<WsMsg>(&text) else {
                continue;
            };
            match m {
                WsMsg::Register {
                    id, role, pubkey, ..
                } => {
                    my_id = id.clone();
                    my_role = Some(role);
                    let peer = Peer {
                        id: id.clone(),
                        pubkey,
                        tx: tx.clone(),
                    };
                    match role {
                        Role::Controlled => {
                            info!("🏠 迷你信令：被控端 {id} 已登记");
                            *st.controlled.lock() = Some(peer);
                        }
                        Role::Controller => {
                            info!("🏠 迷你信令：主控端 {id} 待验证码");
                            pending = Some(peer);
                        }
                    }
                }
                WsMsg::Connect { auth, .. } => {
                    if my_role != Some(Role::Controller) {
                        continue;
                    }
                    if auth.as_deref() != Some(st.pin.as_str()) {
                        warn!("局域网连接验证码错误，已拒绝");
                        let _ = tx.send(
                            serde_json::to_string(&WsMsg::Error {
                                code: "auth_failed".into(),
                                message: "局域网验证码错误".into(),
                            })
                            .unwrap_or_default(),
                        );
                        continue;
                    }
                    let Some(me) = pending.take() else {
                        continue;
                    };
                    let Some(ctl) = st.controlled.lock().clone() else {
                        // 静默拒绝会让现场无法定位：必须说清是"没有被控会话登记"
                        warn!(
                            "🚫 局域网连接被拒：迷你信令上没有任何 Role::Controlled 登记（主控端 {}）",
                            me.id
                        );
                        let _ = tx.send(
                            serde_json::to_string(&WsMsg::Error {
                                code: "offline".into(),
                                message: "被控端未就绪（本机被控会话未登记到迷你信令）".into(),
                            })
                            .unwrap_or_default(),
                        );
                        continue;
                    };
                    // 与正式服务器一致：双向都发 Incoming
                    let _ = ctl.tx.send(
                        serde_json::to_string(&WsMsg::Incoming {
                            from: me.id.clone(),
                            peer_pubkey: me.pubkey.clone(),
                        })
                        .unwrap_or_default(),
                    );
                    let _ = tx.send(
                        serde_json::to_string(&WsMsg::Incoming {
                            from: ctl.id.clone(),
                            peer_pubkey: ctl.pubkey.clone(),
                        })
                        .unwrap_or_default(),
                    );
                    *st.controller.lock() = Some(me.clone());
                    info!("🏠 局域网会话建立：{} → {}", me.id, ctl.id);
                }
                WsMsg::Sdp { .. } | WsMsg::Ice { .. } | WsMsg::SessionEnd { .. } => {
                    // 两个端点之间互转（主控端必须通过验证码登记后才会出现在表中）
                    let target = match my_role {
                        Some(Role::Controller) => {
                            if st.controller.lock().as_ref().map(|p| &p.id) == Some(&my_id) {
                                st.controlled.lock().clone()
                            } else {
                                None
                            }
                        }
                        Some(Role::Controlled) => st.controller.lock().clone(),
                        None => None,
                    };
                    if let Some(p) = target {
                        // 与正式服务器一致：转发时把 to/peer 改写为发送方 ID，
                        // 对端用它做 TOFU 验签与回包路由
                        let out = match m {
                            WsMsg::Sdp {
                                data, kind, sig, ..
                            } => serde_json::to_string(&WsMsg::Sdp {
                                to: my_id.clone(),
                                data,
                                kind,
                                sig,
                            }),
                            WsMsg::Ice { candidates, .. } => serde_json::to_string(&WsMsg::Ice {
                                to: my_id.clone(),
                                candidates,
                            }),
                            WsMsg::SessionEnd { reason, .. } => {
                                serde_json::to_string(&WsMsg::SessionEnd {
                                    peer: my_id.clone(),
                                    reason,
                                })
                            }
                            _ => continue,
                        };
                        if let Ok(j) = out {
                            let _ = p.tx.send(j);
                        }
                    }
                }
                WsMsg::Heartbeat { .. } | WsMsg::Keepalive => {}
                _ => {}
            }
        }

        // 连接断开：清理登记，并通知对端结束会话
        match my_role {
            Some(Role::Controlled) => {
                let mut g = st.controlled.lock();
                if g.as_ref().map(|p| &p.id) == Some(&my_id) {
                    *g = None;
                }
            }
            Some(Role::Controller) => {
                let mut g = st.controller.lock();
                if g.as_ref().map(|p| &p.id) == Some(&my_id) {
                    *g = None;
                }
                if let Some(ctl) = st.controlled.lock().clone() {
                    let _ = ctl.tx.send(
                        serde_json::to_string(&rebleach_client::protocol::WsMsg::SessionEnd {
                            peer: my_id.clone(),
                            reason: "主控端已断开".into(),
                        })
                        .unwrap_or_default(),
                    );
                }
            }
            None => {}
        }
        writer.abort();
        info!("🏠 迷你信令连接关闭: {my_id}");
    }

    tauri::async_runtime::spawn(async move {
        let router = Router::new().route("/ws", get(handler)).with_state(state);
        let listener = match tokio::net::TcpListener::bind(("0.0.0.0", LAN_PORT)).await {
            Ok(l) => l,
            Err(e) => {
                warn!("局域网信令端口 {LAN_PORT} 绑定失败: {e}（局域网直连不可用）");
                return;
            }
        };
        info!("🏠 局域网迷你信令 ws://0.0.0.0:{LAN_PORT}/ws");
        if let Err(e) = axum::serve(listener, router).await {
            warn!("局域网迷你信令退出: {e}");
        }
    });
}
