use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use axum::body::Body;
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use clap::Parser;
use hmac::{Hmac, Mac};
use parking_lot::{Mutex, RwLock};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

// ============================================================
// CLI
// ============================================================
#[derive(Debug, Parser)]
#[command(name = "rebleach-server", about = "Rebleach 信令 + 策略三合一服务")]
struct Cli {
    #[arg(long, default_value = "0.0.0.0:8088")]
    addr: String,

    #[arg(long, default_value = "./rebleach.db")]
    db: String,

    /// 管理端 Basic Auth 用户名
    #[arg(long, default_value = "admin")]
    admin_user: String,
    #[arg(long, default_value = "admin")]
    admin_pass: String,

    /// 出包中心目录（安装包产物放这里，管理端可下载）
    #[arg(long, default_value = "./packages")]
    packages_dir: String,

    /// TURN 服务器地址（如 turn:turn.example.com:3478），留空则不下发 TURN 凭据
    #[arg(long, default_value = "")]
    turn_url: String,

    /// coturn 的 static-auth-secret，用于签发临时凭据（TURN REST）
    #[arg(long, default_value = "")]
    turn_secret: String,

    /// 临时凭据有效期（秒）。只覆盖单次建连，不覆盖整个会话，故默认压到 10 分钟
    #[arg(long, default_value_t = 600)]
    turn_ttl: u64,
}

// ============================================================
// 信令协议（客户端 ↔ 服务器 WebSocket JSON）
// ============================================================
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WsMsg {
    /// 设备上线注册：client → server
    Register {
        id: String,
        role: Role,
        pubkey: String,
        /// 主机名（可选，管理端展示用）
        #[serde(default)]
        hostname: Option<String>,
        /// 设备详细信息（被控端 Agent 上报）
        #[serde(default)]
        info: Option<DeviceInfo>,
    },
    /// A 请求连接 B：controller → server
    Connect {
        target: String,
    },
    /// 注册成功后下发 TURN 临时凭据：server → client
    Turn {
        urls: Vec<String>,
        username: String,
        credential: String,
        /// 凭据有效期（秒）
        ttl: u64,
    },
    /// 服务器转发给被控端：server → controlled
    Incoming {
        from: String,
        peer_pubkey: String,
    },
    /// SDP offer/answer：两端互发，服务器只做 relay + policy 校验
    Sdp {
        to: String,
        #[serde(with = "serde_sdp")]
        data: Vec<u8>,
        kind: SdpKind,
        /// 发送方对 data 的 ed25519 签名，relay 时原样透传
        #[serde(default)]
        sig: Option<String>,
    },
    /// ICE 候选：两端互发
    Ice {
        to: String,
        candidates: Vec<String>,
    },
    /// 会话结束
    SessionEnd {
        peer: String,
        reason: String,
    },
    /// 心跳（旧客户端）/ 兼容保留
    Keepalive,
    /// 心跳：busy = 当前是否有主控端连接
    Heartbeat {
        #[serde(default)]
        busy: bool,
    },
    /// 服务器主动关闭（策略撤销 / 窗口到期）
    Evict {
        peer: String,
        reason: String,
    },
    /// 错误
    Error {
        code: String,
        message: String,
    },
    /// 管理端 → 被控端：紧急控制指令
    /// action: restart / shutdown / unlock / reload / disconnect
    Emergency {
        action: String,
        #[serde(default)]
        delay_secs: u32,
    },
    EmergencyAck {
        action: String,
        ok: bool,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DeviceInfo {
    #[serde(default)]
    hostname: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    os: Option<String>,
    #[serde(default)]
    cpu: Option<String>,
    #[serde(default)]
    memory_mb: Option<u64>,
    #[serde(default)]
    storage_gb: Option<u64>,
    #[serde(default)]
    ip: Option<String>,
    #[serde(default)]
    region: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Role {
    Controller,
    Controlled,
}

impl Role {
    fn from_str_lossy(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "controller" | "controlled" => serde_json::from_str::<Role>(&format!("\"{s}\"")).ok(),
            _ => None,
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Role::Controller => "controller",
            Role::Controlled => "controlled",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SdpKind {
    Offer,
    Answer,
}

mod serde_sdp {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S>(v: &Vec<u8>, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_str(&base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            v,
        ))
    }
    pub fn deserialize<'de, D>(d: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(d)?;
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &s)
            .map_err(serde::de::Error::custom)
    }
}

// ============================================================
// 设备在线表
// ============================================================
#[derive(Debug)]
struct ClientSlot {
    /// 反向通道：服务器→客户端
    tx: mpsc::UnboundedSender<WsMsg>,
    role: Role,
    pubkey: String,
    connected_at: u64,
}

type ClientMap = RwLock<HashMap<String, Arc<ClientSlot>>>;

#[derive(Debug, Clone)]
struct AppState {
    db: Arc<Mutex<Connection>>,
    clients: Arc<ClientMap>,
    admin: Arc<(String, String)>,
    packages_dir: Arc<String>,
    /// (TURN 地址, static-auth-secret, 有效期秒)；secret 为空表示不下发
    turn: Arc<(String, String, u64)>,
}

impl AppState {
    /// 按 TURN REST 规范签发临时凭据：username=过期时间戳，password=HMAC-SHA1(secret, username)
    fn turn_credentials(&self) -> Option<WsMsg> {
        let (url, secret, ttl) = self.turn.as_ref();
        if url.is_empty() || secret.is_empty() {
            return None;
        }
        let username = format!("{}", now_epoch() + ttl);
        let credential = turn_password(secret, &username);
        Some(WsMsg::Turn {
            urls: vec![url.clone()],
            username,
            credential,
            ttl: *ttl,
        })
    }
}

/// HMAC-SHA1 后 base64 —— coturn 的 use-auth-secret 校验方式
fn turn_password(secret: &str, username: &str) -> String {
    type HmacSha1 = Hmac<Sha1>;
    let mut mac = HmacSha1::new_from_slice(secret.as_bytes()).expect("HMAC 支持任意长度密钥");
    mac.update(username.as_bytes());
    base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        mac.finalize().into_bytes(),
    )
}

// ============================================================
// 策略引擎（SQLite）
// ============================================================
fn db_init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS devices (
            id TEXT PRIMARY KEY,
            role TEXT NOT NULL,
            pubkey TEXT,
            hostname TEXT,
            model TEXT,
            os TEXT,
            cpu TEXT,
            memory_mb INTEGER,
            storage_gb INTEGER,
            ip TEXT,
            region TEXT,
            busy INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER,
            last_seen INTEGER
        );
        CREATE TABLE IF NOT EXISTS grants (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            controller_id TEXT NOT NULL,
            controlled_id TEXT NOT NULL,
            start_at INTEGER NOT NULL,
            end_at INTEGER NOT NULL,
            revoked INTEGER NOT NULL DEFAULT 0,
            note TEXT,
            created_at INTEGER,
            CHECK (start_at < end_at)
        );
        CREATE INDEX IF NOT EXISTS idx_grants_pair
            ON grants(controller_id, controlled_id);
        CREATE TABLE IF NOT EXISTS audit (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ts INTEGER NOT NULL,
            event TEXT NOT NULL,
            actor TEXT,
            peer TEXT,
            detail TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit(ts);",
    )?;
    // 旧库升级：SQLite 的 ADD COLUMN 不支持 IF NOT EXISTS，按 PRAGMA 结果补齐
    let existing: std::collections::HashSet<String> = {
        let mut stmt = conn.prepare("PRAGMA table_info(devices)")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
        rows.filter_map(|r| r.ok()).collect()
    };
    for (col, ty) in [
        ("model", "TEXT"),
        ("os", "TEXT"),
        ("cpu", "TEXT"),
        ("memory_mb", "INTEGER"),
        ("storage_gb", "INTEGER"),
        ("ip", "TEXT"),
        ("region", "TEXT"),
        ("busy", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        if !existing.contains(col) {
            conn.execute(&format!("ALTER TABLE devices ADD COLUMN {col} {ty}"), [])?;
        }
    }
    Ok(())
}

/// 心跳落库：刷新 last_seen，按需更新「是否被主控连接」
fn touch_device(state: &AppState, id: &Option<String>, busy: Option<bool>) {
    let Some(id) = id.as_ref() else { return };
    let now = now_epoch();
    let conn = state.db.lock();
    let _ = match busy {
        Some(b) => conn.execute(
            "UPDATE devices SET last_seen=?1, busy=?2 WHERE id=?3",
            rusqlite::params![now, b as i64, id],
        ),
        None => conn.execute(
            "UPDATE devices SET last_seen=?1 WHERE id=?2",
            rusqlite::params![now, id],
        ),
    };
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// 审计日志：关键事件落 SQLite，管理端「审计日志」页可查
fn audit(state: &AppState, event: &str, actor: &str, peer: &str, detail: &str) {
    let conn = state.db.lock();
    let _ = conn.execute(
        "INSERT INTO audit(ts, event, actor, peer, detail) VALUES(?1,?2,?3,?4,?5)",
        rusqlite::params![now_epoch() as i64, event, actor, peer, detail],
    );
}

/// 检查此刻 controller→controlled 是否在有效授权窗口内（精确匹配、一对一）
fn grant_active(conn: &Connection, controller: &str, controlled: &str, now: u64) -> bool {
    let mut stmt = conn
        .prepare(
            "SELECT 1 FROM grants WHERE controller_id=?1 AND controlled_id=?2
         AND revoked=0 AND start_at<=?3 AND end_at>?3 LIMIT 1",
        )
        .unwrap();
    stmt.exists(rusqlite::params![controller, controlled, now])
        .unwrap_or(false)
}

/// 获取当前生效的某设备的对端列表
fn current_peers(conn: &Connection, id: &str, role: Role, now: u64) -> Vec<String> {
    let (self_col, peer_col) = match role {
        Role::Controller => ("controller_id", "controlled_id"),
        Role::Controlled => ("controlled_id", "controller_id"),
    };
    // 通配授权以 '*' 出现在结果里，调用方据 this 判定「任意主控端均可」
    let sql = format!(
        "SELECT {peer_col} FROM grants WHERE {self_col}=?1 AND revoked=0 AND start_at<=?2 AND end_at>?2"
    );
    let mut stmt = conn.prepare(&sql).unwrap();
    let rows = stmt
        .query_map(rusqlite::params![id, now], |r| r.get::<_, String>(0))
        .unwrap();
    rows.filter_map(|r| r.ok()).collect()
}

// ============================================================
// WebSocket handler（axum 0.8 原生 WebSocket）
// ============================================================
async fn ws_upgrade(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    // Caddy 在前面反代，真实公网 IP 在 X-Forwarded-For 首个地址
    let peer_ip = headers
        .get("x-forwarded-for")
        .or_else(|| headers.get("x-real-ip"))
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|v| v.trim().to_string())
        .unwrap_or_default();
    upgrade.on_upgrade(move |socket| async move {
        if let Err(e) = handle_socket(socket, state, peer_ip).await {
            error!("ws error: {e}");
        }
    })
}

async fn handle_socket(
    mut socket: axum::extract::ws::WebSocket,
    state: AppState,
    peer_ip: String,
) -> Result<()> {
    use axum::extract::ws::Message as WsFrame;

    let (tx_back, mut rx_back) = mpsc::unbounded_channel::<WsMsg>();
    let device_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let device_id_clone = device_id.clone();
    let clients = state.clients.clone();

    loop {
        tokio::select! {
            // —— 收客户端消息 ——
            frame = socket.recv() => {
                let frame = match frame {
                    Some(Ok(f)) => f,
                    None | Some(Err(_)) => break,
                };
                let bytes = match frame {
                    WsFrame::Binary(b) => b.to_vec(),
                    WsFrame::Text(t) => t.as_bytes().to_vec(),
                    _ => continue,
                };
                let msg: WsMsg = match serde_json::from_slice(&bytes) {
                    Ok(m) => m,
                    Err(_) => {
                        let _ = tx_back.send(WsMsg::Error {
                            code: "bad_json".into(),
                            message: "expected WsMsg JSON".into(),
                        });
                        continue;
                    }
                };
                handle_incoming_msg(msg, &tx_back, &device_id_clone, &clients, &state, &peer_ip).await;
            }
            // —— 服务器→客户端消息转发 ——
            msg = rx_back.recv() => {
                let Some(msg) = msg else { break };
                let id = match device_id.lock().clone() {
                    Some(i) => i,
                    None => continue,
                };
                let bytes = match serde_json::to_vec(&msg) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                if socket.send(WsFrame::Binary(bytes.into())).await.is_err() {
                    info!("{id} ws send failed, closing");
                    break;
                }
            }
        }
    }

    // 断线清理：仅当槽位仍属于本连接时移除（防止重连顶替后被旧连接误清）
    if let Some(id) = device_id_clone.lock().clone() {
        let mine = clients
            .read()
            .get(&id)
            .map(|s| s.tx.same_channel(&tx_back))
            .unwrap_or(false);
        if mine {
            clients.write().remove(&id);
            // 掉线即视为空闲，避免管理端一直显示「连接中」
            let conn = state.db.lock();
            let _ = conn.execute(
                "UPDATE devices SET busy=0, last_seen=?1 WHERE id=?2",
                rusqlite::params![now_epoch(), id],
            );
            info!("disconnected {id}");
        }
    }
    Ok(())
}

/// 处理客户端发来的 WsMsg（抽成独立 async fn 让 select! 里的代码不膨胀）
async fn handle_incoming_msg(
    msg: WsMsg,
    tx_back: &mpsc::UnboundedSender<WsMsg>,
    device_id: &Arc<Mutex<Option<String>>>,
    clients: &Arc<ClientMap>,
    state: &AppState,
    peer_ip: &str,
) {
    match msg {
        WsMsg::Register {
            id,
            role,
            pubkey,
            hostname,
            info,
        } => {
            info!("REGISTER id={id} role={role}");
            // TOFU 服务端侧：同 id 已在线且公钥不同 → 拒绝抢占会话槽
            if let Some(existing) = clients.read().get(&id) {
                if existing.pubkey != pubkey {
                    warn!("REGISTER REJECTED id={id}: 与在线设备公钥不一致");
                    audit(
                        state,
                        "register_conflict",
                        &id,
                        "",
                        "pubkey mismatch with online device",
                    );
                    let _ = tx_back.send(WsMsg::Error {
                        code: "id_conflict".into(),
                        message: "device id is online with a different key".into(),
                    });
                    return;
                }
            }
            let now = now_epoch();
            let d = info.unwrap_or_default();
            let hostname = hostname.or(d.hostname);
            // Agent 自己拿到的公网 IP 更准（Caddy 透传失败时兜底）
            let ip =
                d.ip.filter(|s| !s.is_empty())
                    .unwrap_or_else(|| peer_ip.to_string());
            {
                let conn = state.db.lock();
                let _ = conn.execute(
                    "INSERT INTO devices(id, role, pubkey, hostname, model, os, cpu, memory_mb,
                        storage_gb, ip, region, busy, created_at, last_seen)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,0,?12,?12)
                     ON CONFLICT(id) DO UPDATE SET role=excluded.role, pubkey=excluded.pubkey,
                     hostname=excluded.hostname, model=excluded.model, os=excluded.os,
                     cpu=excluded.cpu, memory_mb=excluded.memory_mb, storage_gb=excluded.storage_gb,
                     ip=excluded.ip, region=excluded.region, last_seen=excluded.last_seen",
                    rusqlite::params![
                        id,
                        role.to_string(),
                        pubkey,
                        hostname,
                        d.model,
                        d.os,
                        d.cpu,
                        d.memory_mb,
                        d.storage_gb,
                        ip,
                        d.region,
                        now
                    ],
                );
            }
            *device_id.lock() = Some(id.clone());
            clients.write().insert(
                id.clone(),
                Arc::new(ClientSlot {
                    tx: tx_back.clone(),
                    role,
                    pubkey,
                    connected_at: now,
                }),
            );
            audit(state, "register", &id, "", &format!("role={role}"));
            // 注意：TURN 凭据不在这里发。注册不代表允许开会话，提前发等于把中继带宽
            // 开放给任何能注册的客户端（绕过 grant 授权窗）。改为 Connect 校验通过后签发。
        }

        WsMsg::Connect { target } => {
            let from = match device_id.lock().clone() {
                Some(i) => i,
                None => {
                    let _ = tx_back.send(WsMsg::Error {
                        code: "not_registered".into(),
                        message: "call register first".into(),
                    });
                    return;
                }
            };
            let now = now_epoch();
            let allowed = {
                let conn = state.db.lock();
                grant_active(&conn, &from, &target, now)
            };
            if !allowed {
                info!("CONNECT BLOCKED {from}→{target} (no active grant)");
                audit(state, "connect_blocked", &from, &target, "no active grant");
                let _ = tx_back.send(WsMsg::Error {
                    code: "window_expired".into(),
                    message: "no active authorization window".into(),
                });
                return;
            }
            // 一次读锁内同时取被控端槽位和主控端公钥（下发给被控端的必须是对端主控端的公钥，
            // 否则被控端 TOFU 会固定成自己的公钥，后续 SDP 验签必然失败）
            let (slot, from_pubkey) = {
                let map = clients.read();
                let slot = match map.get(&target).cloned() {
                    Some(s) => s,
                    None => {
                        let _ = tx_back.send(WsMsg::Error {
                            code: "target_offline".into(),
                            message: format!("{target} is not online"),
                        });
                        return;
                    }
                };
                let from_pubkey = map.get(&from).map(|s| s.pubkey.clone()).unwrap_or_default();
                (slot, from_pubkey)
            };
            let _ = slot.tx.send(WsMsg::Incoming {
                from: from.clone(),
                peer_pubkey: from_pubkey,
            });
            let _ = tx_back.send(WsMsg::Incoming {
                from: target.clone(),
                peer_pubkey: slot.pubkey.clone(),
            });
            // 授权窗校验已通过，此刻才为本会话签发 TURN 临时凭据（两端都要，谁打洞失败谁用）
            if let Some(creds) = state.turn_credentials() {
                let _ = tx_back.send(creds.clone());
                let _ = slot.tx.send(creds);
            }
            audit(state, "connect", &from, &target, "incoming dispatched");
        }

        WsMsg::Sdp {
            to,
            data,
            kind,
            sig,
        } => {
            let from = device_id.lock().clone().unwrap_or_default();
            let active = {
                let conn = state.db.lock();
                let now = now_epoch();
                let peer_role = clients.read().get(&to).map(|s| s.role);
                match peer_role {
                    Some(Role::Controlled) => grant_active(&conn, &from, &to, now),
                    Some(Role::Controller) => grant_active(&conn, &to, &from, now),
                    _ => false,
                }
            };
            if !active {
                info!("SDP BLOCKED {from}→{to} (window gone)");
                audit(state, "sdp_blocked", &from, &to, "window gone");
                let _ = tx_back.send(WsMsg::Evict {
                    peer: to.clone(),
                    reason: "window_expired".into(),
                });
                if let Some(slot) = clients.read().get(&to).cloned() {
                    let _ = slot.tx.send(WsMsg::Evict {
                        peer: from,
                        reason: "window_expired".into(),
                    });
                }
                return;
            }
            if let Some(slot) = clients.read().get(&to).cloned() {
                let _ = slot.tx.send(WsMsg::Sdp {
                    to: from,
                    data,
                    kind,
                    sig,
                });
            }
        }

        WsMsg::Ice { to, candidates } => {
            let from = device_id.lock().clone().unwrap_or_default();
            let active = {
                let conn = state.db.lock();
                let now = now_epoch();
                grant_active(&conn, &from, &to, now) || grant_active(&conn, &to, &from, now)
            };
            if !active {
                return;
            }
            if let Some(slot) = clients.read().get(&to).cloned() {
                let _ = slot.tx.send(WsMsg::Ice {
                    to: from,
                    candidates,
                });
            }
        }

        WsMsg::Keepalive => {
            touch_device(state, &device_id.lock().clone(), None);
        }
        WsMsg::Heartbeat { busy } => {
            touch_device(state, &device_id.lock().clone(), Some(busy));
        }
        WsMsg::SessionEnd { peer, reason } => {
            let me = device_id.lock().clone().unwrap_or_default();
            audit(state, "session_end", &me, &peer, &reason);
            if let Some(slot) = clients.read().get(&peer).cloned() {
                let _ = slot.tx.send(WsMsg::SessionEnd { peer: me, reason });
            }
        }
        WsMsg::Evict { .. } | WsMsg::Error { .. } => {}
        other => {
            warn!("unhandled ws msg: {other:?}");
        }
    }
}

// ============================================================
// HTTP API（管理端用）
// ============================================================
#[derive(Deserialize)]
struct GrantCreate {
    controller_id: String,
    controlled_id: String,
    start_at: Option<i64>,
    /// 时长单位：h（小时）/ d（天）/ m（30 天，月）/ nm（n 个 30 天）
    /// 和 duration_value 配合使用；不传则回退到旧字段 duration_secs
    duration_unit: Option<String>,
    duration_value: Option<u64>,
    #[serde(default)]
    duration_secs: Option<u64>,
    note: Option<String>,
}

fn bad(msg: impl Into<String>) -> Response {
    (StatusCode::BAD_REQUEST, msg.into()).into_response()
}

fn ok_json(v: serde_json::Value) -> Response {
    Json(v).into_response()
}

/// 校验设备 ID：必须是非空纯数字串（与被控端 Agent 生成的 9 位数字一致）
fn validate_device_id(id: &str, field: &str) -> Result<(), String> {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        return Err(format!("{field} 不能为空"));
    }
    if !trimmed.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!(
            "{field} 必须是纯数字（{} 不是合法设备 ID）",
            trimmed
        ));
    }
    Ok(())
}

fn compute_duration_secs(g: &GrantCreate) -> Result<i64, String> {
    // 新格式：duration_unit + duration_value
    if let (Some(unit), Some(val)) = (&g.duration_unit, g.duration_value) {
        let v = val.max(1);
        let secs: i64 = match unit.as_str() {
            "h" => v as i64 * 3600,
            "d" => v as i64 * 86400,
            "m" => v as i64 * 2592000,  // 30 天
            "nm" => v as i64 * 2592000, // n × 30 天（和 m 合并处理）
            _ => return Err(format!("未知时长单位 {unit}，支持 h/d/m/nm")),
        };
        Ok(secs)
    } else if let Some(s) = g.duration_secs {
        // 旧格式兼容
        Ok(s.max(60) as i64)
    } else {
        Err("必须提供 duration_secs 或 (duration_unit + duration_value)".into())
    }
}

async fn api_grants_create(State(state): State<AppState>, Json(g): Json<GrantCreate>) -> Response {
    if let Err(e) = validate_device_id(&g.controller_id, "主控端 ID") {
        return bad(e);
    }
    if let Err(e) = validate_device_id(&g.controlled_id, "被控端 ID") {
        return bad(e);
    }
    if g.controller_id == g.controlled_id {
        return bad("主控端和被控端 ID 不能相同");
    }
    let secs = match compute_duration_secs(&g) {
        Ok(s) => s,
        Err(e) => return bad(e),
    };
    let now = now_epoch() as i64;
    let start = g.start_at.unwrap_or(now);
    if start < now - 60 {
        return bad("start_at 不能早于当前时间");
    }
    let end = start + secs;
    // 被控端设备 ID 必须在库中存在（防止给不存在的设备开授权）
    {
        let conn = state.db.lock();
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM devices WHERE id=?1 AND role='controlled' LIMIT 1",
                rusqlite::params![g.controlled_id],
                |_| Ok::<_, rusqlite::Error>(true),
            )
            .unwrap_or(false);
        if !exists {
            return bad(format!("被控端 {} 不存在于设备列表", g.controlled_id));
        }
    }
    let id = {
        let conn = state.db.lock();
        match conn.execute(
            "INSERT INTO grants(controller_id, controlled_id, start_at, end_at, note, created_at)
             VALUES(?1,?2,?3,?4,?5,?6)",
            rusqlite::params![g.controller_id, g.controlled_id, start, end, g.note, now],
        ) {
            Ok(_) => conn.last_insert_rowid(),
            Err(e) => return bad(e.to_string()),
        }
    };
    audit(
        &state,
        "grant_create",
        &g.controller_id,
        &g.controlled_id,
        &format!("grant #{id} window [{start},{end})"),
    );
    ok_json(serde_json::json!({
        "ok": true,
        "id": id,
        "controller_id": g.controller_id,
        "controlled_id": g.controlled_id,
        "start_at": start,
        "end_at": end,
        "duration_secs": secs,
    }))
}

async fn api_grants_revoke(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    // 查出 controller/controlled 以便主动驱逐
    let pair: Option<(String, String)> = {
        let conn = state.db.lock();
        let _ = conn.execute(
            "UPDATE grants SET revoked=1 WHERE id=?",
            rusqlite::params![id],
        );
        conn.query_row(
            "SELECT controller_id, controlled_id FROM grants WHERE id=?1",
            rusqlite::params![id],
            |r| Ok::<_, rusqlite::Error>((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .ok()
    };
    if let Some((cid, tid)) = &pair {
        audit(&state, "grant_revoke", cid, tid, &format!("grant #{id}"));
        let clients = state.clients.read();
        if let Some(s) = clients.get(cid) {
            let _ = s.tx.send(WsMsg::Evict {
                peer: tid.clone(),
                reason: "revoked".into(),
            });
        }
        if let Some(s) = clients.get(tid) {
            let _ = s.tx.send(WsMsg::Evict {
                peer: cid.clone(),
                reason: "revoked".into(),
            });
        }
    }
    ok_json(serde_json::json!({"ok": true}))
}

#[derive(Deserialize)]
struct PolicyQuery {
    device: String,
    role: String,
}

async fn api_policy(State(state): State<AppState>, Query(q): Query<PolicyQuery>) -> Response {
    let role = match Role::from_str_lossy(&q.role) {
        Some(r) => r,
        None => return (StatusCode::BAD_REQUEST, "bad role").into_response(),
    };
    let now = now_epoch();
    let peers = {
        let conn = state.db.lock();
        current_peers(&conn, &q.device, role, now)
    };
    ok_json(serde_json::json!({
        "ok": true,
        "data": {
            "device": q.device,
            "role": role.to_string(),
            "server_time": now,
            "allow_target_ids": peers,
            "poll_seconds": 1,
        }
    }))
}

async fn health() -> &'static str {
    "ok"
}

// ---------- 设备列表（含在线状态） ----------
async fn api_devices(State(state): State<AppState>) -> Response {
    let rows: Vec<serde_json::Value> = {
        let conn = state.db.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, role, COALESCE(pubkey,''), COALESCE(hostname,''), created_at, last_seen,
                        COALESCE(model,''), COALESCE(os,''), COALESCE(cpu,''),
                        COALESCE(memory_mb,0), COALESCE(storage_gb,0), COALESCE(ip,''),
                        COALESCE(region,''), COALESCE(busy,0)
                 FROM devices ORDER BY last_seen DESC",
            )
            .unwrap();
        let clients = state.clients.read();
        stmt.query_map([], |r| {
            let id: String = r.get(0)?;
            Ok(serde_json::json!({
                "id": id,
                "role": r.get::<_, String>(1)?,
                "pubkey": r.get::<_, String>(2)?,
                "hostname": r.get::<_, String>(3)?,
                "created_at": r.get::<_, i64>(4)?,
                "last_seen": r.get::<_, i64>(5)?,
                "online": clients.contains_key(&id),
                "model": r.get::<_, String>(6)?,
                "os": r.get::<_, String>(7)?,
                "cpu": r.get::<_, String>(8)?,
                "memory_mb": r.get::<_, i64>(9)?,
                "storage_gb": r.get::<_, i64>(10)?,
                "ip": r.get::<_, String>(11)?,
                "region": r.get::<_, String>(12)?,
                "busy": r.get::<_, i64>(13)? != 0,
            }))
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
    };
    ok_json(serde_json::json!({"ok": true, "data": rows}))
}

// ---------- 紧急控制（管理端直接对在线被控端下发，绕过授权窗口） ----------
#[derive(Deserialize)]
struct EmergencyReq {
    action: String,
    #[serde(default)]
    delay_secs: Option<u32>,
}

/// 合法的紧急动作集合（白名单避免误触发危险命令）
const EMERGENCY_ACTIONS: &[&str] = &["restart", "shutdown", "unlock", "reload", "disconnect"];

async fn api_devices_emergency(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<EmergencyReq>,
) -> Response {
    if validate_device_id(&id, "被控端 ID").is_err() {
        return bad("无效的被控端设备 ID");
    }
    if !EMERGENCY_ACTIONS.contains(&req.action.as_str()) {
        return bad(format!(
            "未知紧急动作 {}；支持 {}",
            req.action,
            EMERGENCY_ACTIONS.join(", ")
        ));
    }
    // 被控端必须在线（有活跃 WS）
    let online = state.clients.read().contains_key(&id);
    if !online {
        return bad(format!("被控端 {} 当前离线", id));
    }
    let delay = req.delay_secs.unwrap_or(match req.action.as_str() {
        "restart" | "shutdown" => 5,
        _ => 0,
    });
    let msg = WsMsg::Emergency {
        action: req.action.clone(),
        delay_secs: delay,
    };
    let sent = state
        .clients
        .read()
        .get(&id)
        .and_then(|s| s.tx.send(msg).ok())
        .is_some();
    if !sent {
        return bad("下发失败：被控端信令通道写入失败");
    }
    audit(
        &state,
        "emergency",
        "admin",
        &id,
        &format!("action={} delay={delay}", req.action),
    );
    ok_json(serde_json::json!({
        "ok": true,
        "id": id,
        "action": req.action,
        "delay_secs": delay,
    }))
}

// ---------- 授权列表 ----------
async fn api_grants_list(State(state): State<AppState>) -> Response {
    let now = now_epoch() as i64;
    let rows: Vec<serde_json::Value> = {
        let conn = state.db.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, controller_id, controlled_id, start_at, end_at, revoked,
                        COALESCE(note,''), created_at
                 FROM grants ORDER BY id DESC LIMIT 500",
            )
            .unwrap();
        stmt.query_map([], |r| {
            let start: i64 = r.get(3)?;
            let end: i64 = r.get(4)?;
            let revoked: i64 = r.get(5)?;
            let status = if revoked != 0 {
                "revoked"
            } else if end <= now {
                "expired"
            } else if start > now {
                "pending"
            } else {
                "active"
            };
            Ok(serde_json::json!({
                "id": r.get::<_, i64>(0)?,
                "controller_id": r.get::<_, String>(1)?,
                "controlled_id": r.get::<_, String>(2)?,
                "start_at": start,
                "end_at": end,
                "revoked": revoked != 0,
                "status": status,
                "note": r.get::<_, String>(6)?,
                "created_at": r.get::<_, i64>(7)?,
            }))
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
    };
    ok_json(serde_json::json!({"ok": true, "data": rows}))
}

// ---------- 审计日志 ----------
#[derive(Deserialize)]
struct AuditQuery {
    limit: Option<u32>,
}

async fn api_audit(State(state): State<AppState>, Query(q): Query<AuditQuery>) -> Response {
    let limit = q.limit.unwrap_or(200).min(1000);
    let rows: Vec<serde_json::Value> = {
        let conn = state.db.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, ts, event, COALESCE(actor,''), COALESCE(peer,''), COALESCE(detail,'')
                 FROM audit ORDER BY id DESC LIMIT ?1",
            )
            .unwrap();
        stmt.query_map(rusqlite::params![limit], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, i64>(0)?,
                "ts": r.get::<_, i64>(1)?,
                "event": r.get::<_, String>(2)?,
                "actor": r.get::<_, String>(3)?,
                "peer": r.get::<_, String>(4)?,
                "detail": r.get::<_, String>(5)?,
            }))
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
    };
    ok_json(serde_json::json!({"ok": true, "data": rows}))
}

// ---------- 出包中心 ----------
async fn api_packages_list(State(state): State<AppState>) -> Response {
    let mut items = Vec::new();
    if let Ok(mut rd) = tokio::fs::read_dir(state.packages_dir.as_str()).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            let name = e.file_name().to_string_lossy().to_string();
            if let Ok(md) = e.metadata().await {
                if md.is_file() {
                    items.push(serde_json::json!({
                        "name": name,
                        "size": md.len(),
                        "mtime": md.modified().ok()
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0),
                    }));
                }
            }
        }
    }
    items.sort_by_key(|x| std::cmp::Reverse(x["mtime"].as_i64().unwrap_or(0)));
    ok_json(serde_json::json!({"ok": true, "data": items}))
}

async fn api_package_download(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    // 防目录穿越
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return bad("bad file name");
    }
    let path = std::path::Path::new(state.packages_dir.as_str()).join(&name);
    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(_) => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };
    let body = Body::from_stream(tokio_util::io::ReaderStream::new(file));
    Response::builder()
        .header("content-type", "application/octet-stream")
        .header(
            "content-disposition",
            format!("attachment; filename=\"{name}\""),
        )
        .body(body)
        .unwrap()
}

// ---------- 自动更新 ----------
/// GET /api/v1/updater/latest.json：Tauri updater 清单
/// 管理员将 packages_dir/updater-manifest.json 编辑好后即可对全量客户端发版
async fn api_updater_manifest(State(state): State<AppState>) -> Response {
    let path = std::path::Path::new(state.packages_dir.as_str()).join("updater-manifest.json");
    match tokio::fs::read_to_string(&path).await {
        Ok(s) => Response::builder()
            .header("content-type", "application/json")
            .header("cache-control", "no-store")
            .body(Body::from(s))
            .unwrap(),
        Err(_) => (StatusCode::NOT_FOUND, "no updater manifest").into_response(),
    }
}

// ============================================================
// 管理端网页（内嵌静态页，复用 Basic Auth）
// ============================================================
async fn admin_root() -> Response {
    axum::response::Redirect::to("/admin/devices").into_response()
}

macro_rules! admin_page {
    ($name:ident, $file:literal) => {
        async fn $name() -> axum::response::Html<&'static str> {
            axum::response::Html(include_str!($file))
        }
    };
}
admin_page!(page_devices, "../web/devices.html");
admin_page!(page_grants, "../web/grants.html");
admin_page!(page_packages, "../web/packages.html");
admin_page!(page_audit, "../web/audit.html");

// ============================================================
// Basic Auth 中间件（仅保护 /api/v1/*）
// ============================================================
use axum::middleware;

async fn admin_middleware(
    State(state): State<AppState>,
    headers: HeaderMap,
    req: axum::http::Request<Body>,
    next: middleware::Next,
) -> Response {
    let (u, p) = (state.admin.0.clone(), state.admin.1.clone());
    if let Some(v) = headers.get("authorization") {
        if let Ok(s) = v.to_str() {
            if let Some(rest) = s.strip_prefix("Basic ") {
                if let Ok(decoded) =
                    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, rest)
                {
                    let s = String::from_utf8_lossy(&decoded);
                    if let Some((a, b)) = s.split_once(':') {
                        if a == u && b == p {
                            return next.run(req).await;
                        }
                    }
                }
            }
        }
    }
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("www-authenticate", "Basic realm=\"rebleach-admin\"")
        .body(Body::from("auth required"))
        .unwrap()
}

// ============================================================
// Main
// ============================================================
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();

    // DB
    let conn = Connection::open(&cli.db)?;
    db_init(&conn)?;
    std::fs::create_dir_all(&cli.packages_dir).ok();
    let state = AppState {
        db: Arc::new(Mutex::new(conn)),
        clients: Arc::new(RwLock::new(HashMap::new())),
        admin: Arc::new((cli.admin_user, cli.admin_pass)),
        packages_dir: Arc::new(cli.packages_dir),
        turn: Arc::new((cli.turn_url, cli.turn_secret, cli.turn_ttl)),
    };

    // 窗口到期扫描任务：每 1s 找出过期 grant，主动驱逐两端
    {
        let state = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let now = now_epoch() as i64;
                let expired: Vec<(i64, String, String)> = {
                    let conn = state.db.lock();
                    let Ok(mut stmt) = conn.prepare(
                        "SELECT id, controller_id, controlled_id FROM grants
                         WHERE revoked=0 AND end_at <= ?1",
                    ) else {
                        continue;
                    };
                    let Ok(rows) = stmt.query_map([now], |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    }) else {
                        continue;
                    };
                    rows.filter_map(|r| r.ok()).collect()
                };
                for (id, c, t) in expired {
                    {
                        let conn = state.db.lock();
                        let _ = conn.execute("UPDATE grants SET revoked=1 WHERE id=?", [id]);
                    }
                    audit(&state, "grant_expire", &c, &t, &format!("grant #{id}"));
                    {
                        let clients = state.clients.read();
                        if let Some(s) = clients.get(&c) {
                            let _ = s.tx.send(WsMsg::Evict {
                                peer: t.clone(),
                                reason: "window_expired".into(),
                            });
                        }
                        if let Some(s) = clients.get(&t) {
                            let _ = s.tx.send(WsMsg::Evict {
                                peer: c.clone(),
                                reason: "window_expired".into(),
                            });
                        }
                    }
                    info!("grant {id} ({c}→{t}) 已过期，驱逐两端");
                }
            }
        });
    }

    // API 子路由（带 Basic Auth 保护）
    let app = build_app(state);

    let listener = tokio::net::TcpListener::bind(&cli.addr).await?;
    info!(
        "Rebleach 信令服务启动 -> ws://{}/ws   http://{}/api/v1   管理端 http://{}/admin/",
        cli.addr, cli.addr, cli.addr
    );
    axum::serve(listener, app).await?;
    Ok(())
}

/// 构建 HTTP 路由（信令 WS + 管理 REST + 自动更新）。
/// 提取为独立函数以便集成测试直接构造 AppState 后调用。
fn build_app(state: AppState) -> Router {
    // API 子路由（带 Basic Auth 保护）
    let api = Router::new()
        .route("/grants", post(api_grants_create).get(api_grants_list))
        .route("/grants/{id}", any(api_grants_revoke))
        .route("/devices", get(api_devices))
        .route("/devices/{id}/emergency", post(api_devices_emergency))
        .route("/policy", get(api_policy))
        .route("/audit", get(api_audit))
        .route("/packages", get(api_packages_list))
        .route("/packages/{name}", get(api_package_download))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            admin_middleware,
        ));

    // 管理端网页（同样 Basic Auth 保护；浏览器会弹登录框）
    let admin = Router::new()
        .route("/", get(admin_root))
        .route("/devices", get(page_devices))
        .route("/grants", get(page_grants))
        .route("/packages", get(page_packages))
        .route("/audit", get(page_audit))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            admin_middleware,
        ));

    Router::new()
        .route("/ws", get(ws_upgrade))
        .route("/health", get(health))
        // 自动更新：公开访问（包完整性由 minisign 签名校验，无需 Basic Auth）
        .route("/api/v1/updater/latest.json", get(api_updater_manifest))
        .route("/api/v1/updater/download/{name}", get(api_package_download))
        .nest("/api/v1", api)
        .nest("/admin", admin)
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    /// 构造测试用 AppState：内存 SQLite + 独立临时 packages 目录 + 固定管理员凭据
    fn test_state() -> AppState {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let conn = Connection::open_in_memory().unwrap();
        db_init(&conn).unwrap();
        let tmp = std::env::temp_dir().join(format!("rebleach-test-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&tmp).ok();
        AppState {
            db: Arc::new(Mutex::new(conn)),
            clients: Arc::new(RwLock::new(HashMap::new())),
            admin: Arc::new(("admin".to_string(), "secret".to_string())),
            packages_dir: Arc::new(tmp.to_string_lossy().to_string()),
            turn: Arc::new((String::new(), String::new(), 0)),
        }
    }

    fn basic_auth_header() -> String {
        let raw = format!("admin:secret");
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, raw);
        format!("Basic {b64}")
    }

    /// 创建授权的前置条件：被控端必须已在设备库中，且 ID 为纯数字
    fn seed_controlled(state: &AppState, id: &str) {
        let conn = state.db.lock();
        conn.execute(
            "INSERT INTO devices(id, role, created_at, last_seen) VALUES(?1,'controlled',0,0)",
            rusqlite::params![id],
        )
        .unwrap();
    }

    async fn get(state: AppState, path: &str, auth: bool) -> (StatusCode, Vec<u8>) {
        let app = build_app(state);
        let mut req = Request::builder().uri(path).method("GET");
        if auth {
            req = req.header("authorization", basic_auth_header());
        }
        let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let status = resp.status();
        let bytes = resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, bytes)
    }

    async fn post_json(
        state: AppState,
        path: &str,
        body: &str,
        auth: bool,
    ) -> (StatusCode, Vec<u8>) {
        let app = build_app(state);
        let mut req = Request::builder()
            .uri(path)
            .method("POST")
            .header("content-type", "application/json");
        if auth {
            req = req.header("authorization", basic_auth_header());
        }
        let resp = app
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, bytes)
    }

    #[tokio::test]
    async fn health_endpoint() {
        let (status, body) = get(test_state(), "/health", false).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"ok");
    }

    #[tokio::test]
    async fn devices_requires_auth() {
        let (status, _) = get(test_state(), "/api/v1/devices", false).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn devices_returns_empty_list() {
        let (status, body) = get(test_state(), "/api/v1/devices", true).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["ok"], true);
        assert!(v["data"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn grants_create_and_list() {
        let state = test_state();
        seed_controlled(&state, "200000002");
        let body = serde_json::json!({
            "controller_id": "100000001",
            "controlled_id": "200000002",
            "duration_secs": 3600,
            "note": "test grant"
        })
        .to_string();
        let (status, resp) = post_json(state.clone(), "/api/v1/grants", &body, true).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&resp).unwrap();
        assert_eq!(v["ok"], true);
        assert!(v["id"].as_i64().unwrap() > 0);

        // 列表应包含刚创建的 grant
        let (status, resp) = get(state, "/api/v1/grants", true).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&resp).unwrap();
        let list = v["data"].as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["controller_id"], "100000001");
        assert_eq!(list[0]["controlled_id"], "200000002");
    }

    #[tokio::test]
    async fn grants_create_without_auth_rejected() {
        let body = serde_json::json!({
            "controller_id": "a", "controlled_id": "b", "duration_secs": 60
        })
        .to_string();
        let (status, _) = post_json(test_state(), "/api/v1/grants", &body, false).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn grants_revoke_then_status_revoked() {
        let state = test_state();
        seed_controlled(&state, "200000002");
        let body = serde_json::json!({
            "controller_id": "100000001", "controlled_id": "200000002", "duration_secs": 60
        })
        .to_string();
        let (_, resp) = post_json(state.clone(), "/api/v1/grants", &body, true).await;
        let v: serde_json::Value = serde_json::from_slice(&resp).unwrap();
        let id = v["id"].as_i64().unwrap();

        let (status, _) = get(state.clone(), &format!("/api/v1/grants/{id}"), true).await;
        // revoke 路由是 any()，GET 也会触发撤销
        assert_eq!(status, StatusCode::OK);

        let (_, resp) = get(state, "/api/v1/grants", true).await;
        let v: serde_json::Value = serde_json::from_slice(&resp).unwrap();
        assert_eq!(v["data"][0]["revoked"], true);
        assert_eq!(v["data"][0]["status"], "revoked");
    }

    #[tokio::test]
    async fn audit_logs_grant_creation() {
        let state = test_state();
        seed_controlled(&state, "200000002");
        let body = serde_json::json!({
            "controller_id": "100000001", "controlled_id": "200000002", "duration_secs": 60
        })
        .to_string();
        post_json(state.clone(), "/api/v1/grants", &body, true).await;

        let (status, resp) = get(state, "/api/v1/audit", true).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&resp).unwrap();
        let list = v["data"].as_array().unwrap();
        assert!(!list.is_empty());
        assert_eq!(list[0]["event"], "grant_create");
    }

    #[tokio::test]
    async fn packages_list_empty_dir() {
        let state = test_state();
        let (status, resp) = get(state, "/api/v1/packages", true).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&resp).unwrap();
        assert_eq!(v["ok"], true);
        assert!(v["data"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn package_download_blocks_path_traversal() {
        let state = test_state();
        let (status, _) = get(state, "/api/v1/packages/../etc/passwd", true).await;
        // 防目录穿越：返回 400（bad）或被路由规范化，总之不能泄露
        assert_ne!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn updater_manifest_missing_returns_404() {
        let (status, _) = get(test_state(), "/api/v1/updater/latest.json", false).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn updater_manifest_public_no_auth() {
        // 创建 manifest 文件，验证公开可访问
        let state = test_state();
        let path = std::path::Path::new(state.packages_dir.as_str()).join("updater-manifest.json");
        std::fs::write(&path, r#"{"version":"1.0.0"}"#).unwrap();

        let (status, body) = get(state, "/api/v1/updater/latest.json", false).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["version"], "1.0.0");
    }
}
