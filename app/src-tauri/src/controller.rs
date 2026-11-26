// 主控端应用：WebView UI + WebRTC 会话 + 本地媒体 WebSocket 推流
// 前端：WebView2 + WebCodecs 硬解 H.264 + 键鼠回传
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::extract::State as AxumState;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use parking_lot::Mutex;
use rebleach_client::protocol::{self, ControlMsg, Role};
use rebleach_client::session::{self, EventSink, SessionConfig, SessionEvent};
use serde::Deserialize;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_updater::UpdaterExt;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, warn};

use crate::ctlcfg::{self, CtlConfig};
use crate::lan::{self, LanDevice};
use crate::{parse_internal_ca, updater_url_from_server};

// ============================================================
// 媒体中枢：H.264 帧 → 本地 WebSocket（二进制）→ 前端 WebCodecs
// 不走 Tauri IPC：2880x1800 单帧几十 KB，JSON 序列化开销不可接受
// ============================================================
struct MediaHub {
    tx: broadcast::Sender<Arc<Vec<u8>>>,
    /// 最近一个关键帧（新连接前端立即补发，无需等下一个 GOP）
    last_key: Mutex<Option<Arc<Vec<u8>>>>,
}

impl MediaHub {
    fn new() -> Self {
        let (tx, _rx) = broadcast::channel(120);
        Self {
            tx,
            last_key: Mutex::new(None),
        }
    }

    fn publish(&self, key: bool, data: Vec<u8>) {
        let bytes = Arc::new(data);
        if key {
            *self.last_key.lock() = Some(bytes.clone());
        }
        let _ = self.tx.send(bytes); // 无订阅者时忽略
    }

    fn reset(&self) {
        *self.last_key.lock() = None;
    }
}

struct SessionHandle {
    task: tokio::task::JoinHandle<()>,
    input_tx: mpsc::UnboundedSender<ControlMsg>,
    /// 本机剪贴板轮询任务（断开时 abort）
    clip_task: tokio::task::JoinHandle<()>,
}

struct AppState {
    hub: Arc<MediaHub>,
    media_port: u16,
    session: Mutex<Option<SessionHandle>>,
    /// 更新清单端点（connect 时从信令服务器地址派生）
    updater_url: Mutex<Option<String>>,
    /// 主控端配置（本机 ID + 中转节点列表）
    ctl_cfg: Mutex<CtlConfig>,
    /// 主控端剪贴板同步开关（工具栏一键关闭，避免敏感内容外泄）
    clip_enabled: Arc<AtomicBool>,
}

// ============================================================
// 媒体 WebSocket
// ============================================================
async fn media_handler(
    ws: WebSocketUpgrade,
    AxumState(hub): AxumState<Arc<MediaHub>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| media_socket(socket, hub))
}

async fn media_socket(mut socket: WebSocket, hub: Arc<MediaHub>) {
    let mut rx = hub.tx.subscribe();

    // 先补发最近关键帧，保证解码器从 IDR 开始
    let cached = hub.last_key.lock().clone();
    if let Some(k) = cached {
        if socket
            .send(WsMessage::Binary(k.as_ref().clone().into()))
            .await
            .is_err()
        {
            return;
        }
    }

    loop {
        tokio::select! {
            recv = rx.recv() => match recv {
                Ok(b) => {
                    if socket.send(WsMessage::Binary(b.as_ref().clone().into())).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("媒体通道丢帧 {n} 帧（前端处理慢）");
                }
                Err(_) => break,
            },
            incoming = socket.recv() => match incoming {
                None | Some(Err(_)) | Some(Ok(WsMessage::Close(_))) => break,
                _ => {}
            },
        }
    }
    info!("媒体连接关闭");
}

// ============================================================
// Tauri 命令
// ============================================================
#[tauri::command]
fn media_port(state: State<'_, AppState>) -> u16 {
    state.media_port
}

/// 前端日志转发到 Rust 日志（WebView console 在打包后不可见，排查必需）
#[tauri::command]
fn js_log(level: String, msg: String) {
    match level.as_str() {
        "error" => error!("[web] {msg}"),
        "warn" => warn!("[web] {msg}"),
        _ => info!("[web] {msg}"),
    }
}

#[tauri::command]
fn send_input(state: State<'_, AppState>, msg: ControlMsg) -> Result<(), String> {
    if let Some(h) = state.session.lock().as_ref() {
        if matches!(msg, ControlMsg::KeyDown { .. } | ControlMsg::KeyUp { .. }) {
            debug!("⌨️ 输入回传: {msg:?}");
        }
        h.input_tx.send(msg).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct ConnectArgs {
    server: String,
    id: String,
    role: String,
    peer: Option<String>,
    fps: Option<u64>,
    /// STUN/TURN 服务器，逗号分隔：`stun:host:port,turn:user:pass@host:port`
    ice: Option<String>,
    /// 局域网直连验证码（LAN 模式必填）
    auth: Option<String>,
}

#[tauri::command]
async fn connect(
    app: AppHandle,
    state: State<'_, AppState>,
    args: ConnectArgs,
) -> Result<(), String> {
    if args.id.trim().is_empty() {
        return Err("设备 ID 不能为空".into());
    }
    let role = protocol::role_from_str(&args.role);

    // 结束旧会话
    if let Some(h) = state.session.lock().take() {
        h.task.abort();
        h.clip_task.abort();
    }
    #[cfg(windows)]
    crate::win_key_hook::stop();
    state.hub.reset();
    let _ = app.emit("status", "正在连接…".to_string());

    // 剪贴板防回环：写入本机剪贴板的对端内容不再回发
    let clip_last = Arc::new(Mutex::new(String::new()));

    let hub = state.hub.clone();
    let app_sink = app.clone();
    let clip_sink = clip_last.clone();
    let clip_on = state.clip_enabled.clone();
    let sink: EventSink = Arc::new(move |ev| match ev {
        SessionEvent::Status(s) => {
            let _ = app_sink.emit("status", s);
        }
        SessionEvent::Video { key, mut data } => {
            // 前端包格式 [1B key][Annex-B]：session 已剥掉 key 字节，这里补回
            let mut packet = Vec::with_capacity(data.len() + 1);
            packet.push(key as u8);
            packet.append(&mut data);
            hub.publish(key, packet);
        }
        SessionEvent::Monitors(list) => {
            let _ = app_sink.emit("monitors", list);
        }
        SessionEvent::Stats {
            fps,
            kbps,
            buffered,
        } => {
            let _ = app_sink.emit(
                "netstats",
                serde_json::json!({ "fps": fps, "kbps": kbps, "buffered": buffered }),
            );
        }
        SessionEvent::Clipboard(text) => {
            *clip_sink.lock() = text.clone();
            if !clip_on.load(Ordering::Relaxed) {
                return;
            }
            if let Ok(mut cb) = arboard::Clipboard::new() {
                let _ = cb.set_text(&text);
            }
        }
        SessionEvent::Log { level, msg } => {
            // 被控端的操作结果（增删虚拟屏等）透传到前端状态栏
            let _ = app_sink.emit(
                "peer-log",
                serde_json::json!({ "level": level, "msg": msg }),
            );
        }
        SessionEvent::DriverStatus {
            has_driver,
            active_count,
            driver_kind,
            hint,
        } => {
            let _ = app_sink.emit(
                "driver-status",
                serde_json::json!({
                    "has_driver": has_driver,
                    "active_count": active_count,
                    "driver_kind": driver_kind,
                    "hint": hint,
                }),
            );
        }
        SessionEvent::Link {
            kind,
            detail,
            rtt_ms,
        } => {
            let _ = app_sink.emit(
                "link-info",
                serde_json::json!({ "kind": kind, "detail": detail, "rtt_ms": rtt_ms }),
            );
        }
        SessionEvent::Ended(reason) => {
            // 主控端断线/被驱逐 → 前端自动回主页面（保留上次输入）
            let _ = app_sink.emit("session-ended", reason);
        }
        SessionEvent::TermReady => {
            let _ = app_sink.emit("term-ready", ());
        }
        SessionEvent::TermOpened { sid } => {
            let _ = app_sink.emit("term-opened", serde_json::json!({ "sid": sid }));
        }
        SessionEvent::TermData { sid, data } => {
            let _ = app_sink.emit("term-data", serde_json::json!({ "sid": sid, "data": data }));
        }
        SessionEvent::TermExit { sid, code } => {
            let _ = app_sink.emit("term-exit", serde_json::json!({ "sid": sid, "code": code }));
        }
        SessionEvent::TermErr { sid, msg } => {
            let _ = app_sink.emit("term-err", serde_json::json!({ "sid": sid, "msg": msg }));
        }
    });

    let cfg = SessionConfig {
        server: args.server.clone(),
        id: args.id,
        role,
        peer: args.peer,
        fps: args.fps.unwrap_or(15).clamp(1, 60),
        ice_servers: args.ice.map(|s| vec![s]).unwrap_or_default(),
        key_file: Some(ctlcfg::identity_path()), // %APPDATA%/rebleach-controller/identity.json
        auth: args.auth.filter(|a| !a.trim().is_empty()),
    };
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    // WebView 收不到系统 Win 键：装原生低层钩子，把 Win 按下/抬起投递到被控端
    #[cfg(windows)]
    crate::win_key_hook::start(input_tx.clone());

    // 更新检查：端点从信令服务器地址派生，失败静默忽略（不影响会话）
    // 局域网直连（ws://）不派生更新端点
    let upd_url = if args.server.starts_with("wss://") {
        updater_url_from_server(&args.server)
    } else {
        None
    };
    *state.updater_url.lock() = upd_url.clone();
    if let Some(url) = upd_url {
        let app_upd = app.clone();
        tokio::spawn(async move {
            let Ok(endpoint) = url.parse() else { return };
            let Ok(updater) = app_upd
                .updater_builder()
                .endpoints(vec![endpoint])
                .and_then(|b| {
                    b.configure_client(|cb| match parse_internal_ca() {
                        Some(ca) => cb.add_root_certificate(ca),
                        None => cb,
                    })
                    .build()
                })
            else {
                return;
            };
            match updater.check().await {
                Ok(Some(u)) => {
                    info!("⬆️ 发现新版本 {}", u.version);
                    let _ = app_upd.emit("update-available", u.version.clone());
                }
                Ok(None) => debug!("已是最新版本"),
                Err(e) => debug!("更新检查失败（忽略）: {e}"),
            }
        });
    }

    // 本机剪贴板轮询 → 同步到对端（1s 周期，内容变化才发，防回环靠 clip_last）
    let clip_poll = clip_last.clone();
    let clip_tx = input_tx.clone();
    let clip_switch = state.clip_enabled.clone();
    let clip_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tick.tick().await;
            if !clip_switch.load(Ordering::Relaxed) {
                continue;
            }
            let Ok(mut cb) = arboard::Clipboard::new() else {
                continue;
            };
            let Ok(text) = cb.get_text() else {
                continue;
            };
            if text.is_empty() {
                continue;
            }
            {
                let mut last = clip_poll.lock();
                if *last == text {
                    continue;
                }
                *last = text.clone();
            }
            if clip_tx.send(ControlMsg::Clipboard { text }).is_err() {
                break;
            }
        }
    });

    let app_task = app.clone();
    let is_controller = role == Role::Controller;
    let task = tokio::spawn(async move {
        if let Err(e) = session::run_session(cfg, input_rx, sink).await {
            error!("会话结束: {e:#}");
            let _ = app_task.emit("status", format!("会话结束: {e}"));
            if is_controller {
                // 连接失败/信令断开时也回到主页面，避免停在黑屏会话页
                let _ = app_task.emit("session-ended", format!("{e}"));
            }
        }
    });

    state.session.lock().replace(SessionHandle {
        task,
        input_tx,
        clip_task,
    });
    Ok(())
}

#[tauri::command]
fn disconnect(app: AppHandle, state: State<'_, AppState>) {
    if let Some(h) = state.session.lock().take() {
        h.task.abort();
        h.clip_task.abort();
    }
    #[cfg(windows)]
    crate::win_key_hook::stop();
    state.hub.reset();
    let _ = app.emit("status", "已断开".to_string());
}

/// 主控端全屏（工具栏按钮 / Esc 退出）
#[tauri::command]
fn set_fullscreen(window: tauri::WebviewWindow, on: bool) -> Result<(), String> {
    window.set_fullscreen(on).map_err(|e| e.to_string())
}

/// 剪贴板同步开关：关闭后不再读写本机剪贴板
#[tauri::command]
fn set_clipboard_enabled(state: State<'_, AppState>, on: bool) {
    state.clip_enabled.store(on, Ordering::Relaxed);
}

/// 打开一个终端标签页（sid 由主控端分配，同一会话内复用 term 通道）
#[tauri::command]
fn term_open(sid: u16, cols: u16, rows: u16, shell: Option<String>, cwd: Option<String>) -> Result<(), String> {
    if !session::term_available() {
        return Err("终端通道未建立".into());
    }
    let ok = session::term_send(rebleach_client::term::TermMsg::Open {
        sid,
        shell: shell.unwrap_or_else(|| "cmd".into()),
        cols,
        rows,
        cwd,
    });
    if ok { Ok(()) } else { Err("发送终端请求失败".into()) }
}

#[tauri::command]
fn term_input(sid: u16, data: String) {
    let _ = session::term_send(rebleach_client::term::TermMsg::Data {
        sid,
        data: data.into_bytes(),
    });
}

#[tauri::command]
fn term_resize(sid: u16, cols: u16, rows: u16) {
    let _ = session::term_send(rebleach_client::term::TermMsg::Resize { sid, cols, rows });
}

#[tauri::command]
fn term_close(sid: u16) {
    let _ = session::term_send(rebleach_client::term::TermMsg::Exit { sid, code: 0 });
}

/// 远端画面截图 → 图片文件夹（主控端本地保存）
#[tauri::command]
fn save_screenshot(data: String) -> Result<String, String> {
    use base64::Engine;
    let b64 = data
        .split_once(',')
        .map(|(_, b)| b)
        .unwrap_or(data.as_str());
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|e| format!("截图数据无效: {e}"))?;
    let dir = std::env::var_os("USERPROFILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Pictures")
        .join("Rebleach");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("rebleach-{ts}.png"));
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    Ok(path.display().to_string())
}

// ============================================================
// 节点管理 / 局域网发现
// ============================================================
#[tauri::command]
fn ctl_config(state: State<'_, AppState>) -> CtlConfig {
    state.ctl_cfg.lock().clone()
}

/// 保存节点列表与当前选用节点（内置节点不可删由前端保证，这里兜底）
#[tauri::command]
fn ctl_save(state: State<'_, AppState>, cfg: CtlConfig) -> Result<CtlConfig, String> {
    let mut next = cfg;
    next.nodes.retain(|n| !n.url.trim().is_empty());
    if !next
        .nodes
        .iter()
        .any(|n| n.builtin && n.url == ctlcfg::DEFAULT_NODE_URL)
    {
        next.nodes.insert(
            0,
            ctlcfg::Node {
                name: ctlcfg::DEFAULT_NODE_NAME.to_string(),
                url: ctlcfg::DEFAULT_NODE_URL.to_string(),
                builtin: true,
            },
        );
    }
    if !next.nodes.iter().any(|n| n.url == next.active) {
        next.active = ctlcfg::DEFAULT_NODE_URL.to_string();
    }
    ctlcfg::save(&next)?;
    *state.ctl_cfg.lock() = next.clone();
    Ok(next)
}

/// 局域网发现：UDP 广播收集在线被控端（~1.5s）
#[tauri::command]
async fn lan_discover() -> Vec<LanDevice> {
    lan::discover().await
}

/// 下载并安装更新，完成后重启（端点用 connect 时派生的那一个）
#[tauri::command]
async fn install_update(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let url = state
        .updater_url
        .lock()
        .clone()
        .ok_or("更新端点未知（尚未连接过服务器）")?;
    let endpoint = url.parse().map_err(|_| "更新端点无效")?;
    let updater = app
        .updater_builder()
        .endpoints(vec![endpoint])
        .and_then(|b| {
            b.configure_client(|cb| match parse_internal_ca() {
                Some(ca) => cb.add_root_certificate(ca),
                None => cb,
            })
            .build()
        })
        .map_err(|e| e.to_string())?;
    let Some(update) = updater.check().await.map_err(|e| e.to_string())? else {
        return Err("已是最新版本".into());
    };
    let _ = app.emit("status", format!("正在下载更新 v{}…", update.version));
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| e.to_string())?;
    app.restart();
}

// ============================================================
// 接入配置：服务端管理凭据 + SSH 公钥 / config / 安装令牌
// ============================================================
#[derive(serde::Serialize)]
struct AccessInfo {
    /// 当前节点信令地址（ws/wss）
    server: String,
    /// 对应 HTTP 管理地址
    server_http: String,
    id: String,
    pubkey: String,
    private_path: String,
    public_path: String,
    admin_user: String,
    admin_configured: bool,
}

#[derive(serde::Serialize)]
struct EnrollTokenInfo {
    token: String,
    expires_at: i64,
    command: String,
}

/// 取当前节点的服务端地址与主控端身份/管理员凭据
fn access_context(state: &AppState) -> (String, String, String, String) {
    let cfg = state.ctl_cfg.lock().clone();
    (
        cfg.active.clone(),
        cfg.id.clone(),
        cfg.admin_user.clone(),
        cfg.admin_pass.clone(),
    )
}

fn require_admin(user: &str, pass: &str) -> Result<(), String> {
    if user.trim().is_empty() || pass.is_empty() {
        return Err("请先填写管理员账号与密码".into());
    }
    Ok(())
}

/// 接入信息：服务器地址、主控端 ID、SSH 公钥（缺密钥时自动生成）
#[tauri::command]
fn access_info(state: State<'_, AppState>) -> Result<AccessInfo, String> {
    let (server, id, admin_user, admin_pass) = access_context(&state);
    let key = rebleach_client::cli::ensure_operator_key(&id).map_err(|e| format!("{e:#}"))?;
    Ok(AccessInfo {
        server_http: rebleach_client::cli::signaling_to_http(&server),
        server,
        id,
        pubkey: key.pubkey,
        private_path: key.private_path.display().to_string(),
        public_path: key.public_path.display().to_string(),
        admin_configured: !admin_user.trim().is_empty() && !admin_pass.is_empty(),
        admin_user,
    })
}

/// 把本机主控端公钥登记到服务端（被控端按有效授权拉取）
#[tauri::command]
async fn access_register_key(state: State<'_, AppState>) -> Result<String, String> {
    let (server, id, user, pass) = access_context(&state);
    require_admin(&user, &pass)?;
    let pubkey = rebleach_client::cli::ensure_operator_key(&id)
        .map_err(|e| format!("{e:#}"))?
        .pubkey;
    let id_out = id.clone();
    tauri::async_runtime::spawn_blocking(move || {
        rebleach_client::cli::register_ssh_key(&server, &user, &pass, &id, &pubkey)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{e:#}"))?;
    Ok(format!("已登记主控端 {id_out} 的公钥"))
}

/// 按当前有效授权生成 ~/.ssh/config.d/rebleach.conf
#[tauri::command]
async fn access_write_ssh_config(
    state: State<'_, AppState>,
    backend: Option<String>,
    ssh_user: Option<String>,
) -> Result<rebleach_client::cli::SshConfigOutcome, String> {
    let (server, id, user, pass) = access_context(&state);
    require_admin(&user, &pass)?;
    let backend = match backend.as_deref().unwrap_or("builtin") {
        "sshd" | "tcp" => rebleach_client::cli::SshBackend::Sshd,
        _ => rebleach_client::cli::SshBackend::BuiltIn,
    };
    tauri::async_runtime::spawn_blocking(move || {
        rebleach_client::cli::write_grants_ssh_config(
            &server,
            &user,
            &pass,
            &id,
            backend,
            ssh_user,
            "rebleach-proxy",
            None,
        )
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{e:#}"))
}

/// 生成安装令牌，返回一键安装命令
#[tauri::command]
async fn access_new_enroll_token(
    state: State<'_, AppState>,
    note: String,
    ttl_secs: u64,
) -> Result<EnrollTokenInfo, String> {
    let (server, _id, user, pass) = access_context(&state);
    require_admin(&user, &pass)?;
    let http = rebleach_client::cli::signaling_to_http(&server);
    let (token, expires_at) = tauri::async_runtime::spawn_blocking(move || {
        rebleach_client::cli::create_enroll_token(&server, &user, &pass, &note, ttl_secs)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{e:#}"))?;
    let command = format!("irm \"{http}/bootstrap/install.ps1?token={token}\" | iex");
    Ok(EnrollTokenInfo {
        token,
        expires_at,
        command,
    })
}

// ============================================================
// 入口
// ============================================================
fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

pub fn run() {
    // 媒体端口同步绑定，确保前端首次 invoke 前端口已确定
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定媒体端口失败");
    let port = listener.local_addr().expect("读取媒体端口失败").port();
    listener.set_nonblocking(true).expect("设置非阻塞失败");

    let hub = Arc::new(MediaHub::new());
    let hub_srv = hub.clone();
    tauri::async_runtime::spawn(async move {
        let Ok(l) = tokio::net::TcpListener::from_std(listener) else {
            error!("媒体端口转换失败");
            return;
        };
        let router = Router::new()
            .route("/stream", get(media_handler))
            .with_state(hub_srv);
        info!("🎞️ 媒体服务 ws://127.0.0.1:{port}/stream");
        if let Err(e) = axum::serve(l, router).await {
            error!("媒体服务退出: {e}");
        }
    });

    // 主控端配置：首次启动生成主控 ID + 写入默认节点
    let cfg = ctlcfg::load();
    if let Err(e) = ctlcfg::save(&cfg) {
        warn!("写入主控端配置失败: {e}");
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(AppState {
            hub,
            media_port: port,
            session: Mutex::new(None),
            updater_url: Mutex::new(None),
            ctl_cfg: Mutex::new(cfg),
            clip_enabled: Arc::new(AtomicBool::new(true)),
        })
        .setup(|app| {
            // 托盘：双击进主界面；菜单可打开/退出
            let show = MenuItem::with_id(app, "show", "打开主界面", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;
            let mut tray = TrayIconBuilder::with_id("main")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .tooltip("Rebleach 主控端")
                .on_menu_event(|app, ev| match ev.id().as_ref() {
                    "show" => show_main(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, ev| {
                    if matches!(ev, TrayIconEvent::DoubleClick { .. }) {
                        show_main(tray.app_handle());
                    }
                });
            if let Some(icon) = app.default_window_icon().cloned() {
                tray = tray.icon(icon);
            }
            tray.build(app)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            media_port,
            js_log,
            send_input,
            connect,
            disconnect,
            install_update,
            set_fullscreen,
            set_clipboard_enabled,
            save_screenshot,
            ctl_config,
            ctl_save,
            access_info,
            access_register_key,
            access_write_ssh_config,
            access_new_enroll_token,
            lan_discover,
            term_open,
            term_input,
            term_resize,
            term_close
        ])
        .run(tauri::generate_context!())
        .expect("Tauri 启动失败");
}

/// 联网验证：内置 CA 能让 reqwest 通过自签证书校验并拉到更新清单。
/// 默认 ignore（依赖公网环境），需要时跑 `cargo test -p rebleach-app -- --ignored`
#[cfg(test)]
mod updater_tls_tests {
    use crate::{init_tls_provider, parse_internal_ca};

    #[test]
    #[ignore]
    fn pinned_ca_can_fetch_manifest() {
        init_tls_provider();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let ca = parse_internal_ca().expect("内置 CA 解析失败");
            let client = reqwest::Client::builder()
                .add_root_certificate(ca)
                .build()
                .unwrap();
            let resp = client
                .get("https://updates.example.com/api/v1/updater/latest.json")
                .send()
                .await
                .unwrap_or_else(|e| panic!("请求失败（自签证书未被信任？）: {e}"));
            assert!(resp.status().is_success(), "状态码 {}", resp.status());
            let body: serde_json::Value = resp.json().await.unwrap();
            assert!(body["version"].is_string(), "清单格式异常: {body}");
        });
    }
}
