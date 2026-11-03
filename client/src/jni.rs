//! Android JNI 桥：Kotlin 平台层调用 Rust 核心的入口。
//!
//! 架构：
//!   - jni.rs 持有 WebRTC DataChannel 槽位（setup 时注册，push_video_frame 时使用）
//!   - 信令循环 + WebRTC 会话完全复用 session.rs 的 controller_session / controlled_session
//!   - EventSink 闭包把 SessionEvent 序列化成 JSON 推进 EventQueue，Kotlin 轮询
//!   - 主控端收到的二进制帧 → FrameAssembler → 帧 channel → android_forward_loop → FrameQueue
//!   - Rust 核心 ↔ Kotlin 平台层通过 jni-rs 做正确的 JNI 类型转换
//!
//! JNI 符号命名（Kotlin `external fun` 对应）：
//!   Java_com_rebleach_jni_NativeBridge_start   ← NativeBridge.start()
//!   Java_com_rebleach_jni_NativeBridge_stop    ← NativeBridge.stop()
//!   Java_com_rebleach_jni_NativeBridge_pollEvent   ← NativeBridge.pollEvent()
//!   Java_com_rebleach_jni_NativeBridge_pollFrame   ← NativeBridge.pollFrame()
//!   Java_com_rebleach_jni_NativeBridge_pushVideoFrame  ← NativeBridge.pushVideoFrame()
//!   Java_com_rebleach_jni_NativeBridge_pushAudioFrame  ← NativeBridge.pushAudioFrame()
//!   Java_com_rebleach_jni_NativeBridge_approveConnection ← NativeBridge.approveConnection()
//!   Java_com_rebleach_jni_NativeBridge_setBitrate ← NativeBridge.setBitrate()

#![cfg(target_os = "android")]

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;

use jni::objects::{JClass, JByteArray};
use jni::sys::{jlong, jint, jbyte};
use jni::JNIEnv;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

pub type EventQueue = Arc<Mutex<VecDeque<String>>>;
/// (is_key, annex_b_bytes)
pub type FrameQueue = Arc<Mutex<VecDeque<(bool, Vec<u8>)>>>;

pub struct AndroidSessionState {
    pub handle: u64,
    pub events: EventQueue,
    pub frames: FrameQueue,
    /// 允许多个主控端并行接入：每个主控端一路 screen/audio DC，帧广播给所有在线主控端
    pub screen_dcs: parking_lot::Mutex<Vec<Arc<webrtc::data_channel::RTCDataChannel>>>,
    pub audio_dcs: parking_lot::Mutex<Vec<Arc<webrtc::data_channel::RTCDataChannel>>>,
    pub bitrate_kbps: parking_lot::Mutex<i32>,
}

static SESSIONS: OnceLock<Mutex<HashMap<u64, AndroidSessionState>>> = OnceLock::new();

/// 当前活跃会话 handle（session.rs Android 分支用它找 jni 槽位）。
/// 多会话 Android 端暂不支持，M2 只跑一个。
static CURRENT_HANDLE: OnceLock<AtomicU64> = OnceLock::new();

pub fn set_current_handle(h: u64) {
    CURRENT_HANDLE.get_or_init(|| AtomicU64::new(0)).store(h, std::sync::atomic::Ordering::Relaxed);
}
pub fn current_handle() -> Option<u64> {
    let h = CURRENT_HANDLE.get()?.load(std::sync::atomic::Ordering::Relaxed);
    if h == 0 { None } else { Some(h) }
}

fn sessions() -> &'static Mutex<HashMap<u64, AndroidSessionState>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

// —— session.rs 要调的公开函数 ——

/// setup_controlled / setup_controller 的 Android 分支调这个把创建好的 DC 注册进来。
pub fn register_dc(handle: u64, label: &str, dc: Arc<webrtc::data_channel::RTCDataChannel>) {
    let map = sessions().lock();
    let Some(state) = map.get(&handle) else {
        warn!("register_dc: handle={handle} 不存在");
        return;
    };
    match label {
        "screen" => {
            let mut list = state.screen_dcs.lock();
            list.push(dc);
            info!("register_dc: handle={handle} screen DC 已注册（共 {} 路）", list.len());
        }
        "audio" => {
            let mut list = state.audio_dcs.lock();
            list.push(dc);
            info!("register_dc: handle={handle} audio DC 已注册（共 {} 路）", list.len());
        }
        _ => {}
    }
}

/// setup_controller 的 Android 分支（android_forward_loop）调这个把完整帧推进 FrameQueue。
/// Kotlin 侧用 pollFrame 拉帧，零 base64 膨胀。
pub fn push_frame_to_kotlin(handle: u64, bytes: &[u8], is_key: bool) {
    let map = sessions().lock();
    let Some(state) = map.get(&handle) else { return };
    state.frames.lock().push_back((is_key, bytes.to_vec()));
    // 丢帧保护：队列积压超过 30 帧丢最老的
    let mut q = state.frames.lock();
    if q.len() > 30 {
        let _ = q.pop_front();
    }
}

/// SessionEvent → JSON 推 EventQueue 的闭包。
pub fn make_event_sink(handle: u64) -> crate::session::EventSink {
    let events = {
        let map = sessions().lock();
        let state = map.get(&handle).expect("state 必须已创建");
        state.events.clone()
    };
    Arc::new(move |ev| {
        let json = serde_json::json!(&ev).to_string();
        events.lock().push_back(json);
    })
}

// ============================================================
// JNI 导出（正确 JNI 符号名 + jni-rs 类型）
// ============================================================

/// 启动会话。返回 handle（非 0 成功），0 = 失败。
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rebleach_jni_NativeBridge_start<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    config_json: JByteArray<'local>,
) -> jlong {
    let handle = handle as u64;
    if handle == 0 {
        error!("rebleach_start: handle=0 无效");
        return 0;
    }

    // Kotlin ByteArray → Rust String
    let config_str = match env.convert_byte_array(&config_json) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
        Err(e) => {
            error!("rebleach_start: config_json 读取失败: {e}");
            return 0;
        }
    };

    info!("rebleach_start: handle={handle}, config_json={}", &config_str[..config_str.len().min(200)]);

    // 创建 SessionState
    {
        let mut map = sessions().lock();
        if map.contains_key(&handle) {
            warn!("rebleach_start: handle={handle} 已有会话，覆盖");
        }
        map.insert(handle, AndroidSessionState {
            handle,
            events: Arc::new(Mutex::new(VecDeque::new())),
            frames: Arc::new(Mutex::new(VecDeque::new())),
            screen_dcs: parking_lot::Mutex::new(Vec::new()),
            audio_dcs: parking_lot::Mutex::new(Vec::new()),
            bitrate_kbps: parking_lot::Mutex::new(4000),
        });
    }

    let (events, config_parsed) = match parse_config(&config_str) {
        Ok(cfg) => {
            let events = {
                let map = sessions().lock();
                map.get(&handle).unwrap().events.clone()
            };
            (events, cfg)
        }
        Err(e) => {
            let map = sessions().lock();
            let state = map.get(&handle).unwrap();
            state.events.lock().push_back(
                serde_json::json!({"type":"error","msg":format!("config 解析失败: {e}")}).to_string()
            );
            sessions().lock().remove(&handle);
            return 0;
        }
    };

    info!("rebleach_start: handle={handle}, role={:?}, server={}", config_parsed.role, config_parsed.server);

    // 把 handle 设到全局槽位，让 session.rs Android 分支能拿到并注册 DC
    set_current_handle(handle);

    // 启动 tokio runtime + 会话循环（新 native 线程，需要 AttachCurrentThread...）
    // 但 tokio tasks 都不直接调 JVM（push 到本地队列），只有启动/停/轮询由 Kotlin 线程调 JNI
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(3)
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                error!("rebleach_start: runtime 创建失败: {e:#}");
                events.lock().push_back(
                    serde_json::json!({"type":"error","msg":format!("runtime 创建失败: {e}")}).to_string()
                );
                return;
            }
        };
        rt.block_on(async move {
            let sink = make_event_sink(handle);
            let (_input_tx, input_rx) = mpsc::unbounded_channel();
            // Android 没有桌面键鼠输入，input_rx 永远收不到消息也没关系

            // 通知 Kotlin 会话已启动，信令连接中
            events.lock().push_back(
                serde_json::json!({"type":"connecting","server":config_parsed.server}).to_string()
            );

            // run_session 内部会根据 config.role 走 controller_session / controlled_session
            let result = crate::session::run_session(config_parsed, input_rx, sink).await;
            match result {
                Ok(()) => {
                    info!("rebleach_start: 会话正常退出 handle={handle}");
                    events.lock().push_back(
                        serde_json::json!({"type":"closed","reason":"正常退出"}).to_string()
                    );
                }
                Err(e) => {
                    error!("rebleach_start: 会话异常退出 handle={handle}: {e:#}");
                    events.lock().push_back(
                        serde_json::json!({"type":"error","msg":format!("会话退出: {e}")}).to_string()
                    );
                }
            }
        });
    });

    handle as jlong
}

/// 停止会话（清理 handle）
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rebleach_jni_NativeBridge_stop(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    let handle = handle as u64;
    info!("rebleach_stop: handle={handle}");
    sessions().lock().remove(&handle);
}

/// 轮询事件队列。Kotlin 提供 ByteArray 作为输出 buffer。
/// 返回事件字节数；0 = 队空；-1 = handle 不存在或 buffer 太小
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rebleach_jni_NativeBridge_pollEvent<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    out_buf: JByteArray<'local>,
    out_cap: jint,
) -> jint {
    let handle = handle as u64;
    if handle == 0 || out_cap <= 0 { return -1; }

    // 从事件队列取一条
    let bytes: Vec<u8> = {
        let map = sessions().lock();
        let Some(state) = map.get(&handle) else { return -1; };
        match state.events.lock().pop_front() {
            Some(json) => json.into_bytes(),
            None => return 0,
        }
    };

    if bytes.len() > out_cap as usize { return -1; }

    // 写入 Kotlin 提供的 ByteArray
    let bytes_i8: Vec<i8> = bytes.iter().map(|&b| b as i8).collect();
    if env.set_byte_array_region(&out_buf, 0, &bytes_i8).is_err() { return -1; }
    bytes.len() as jint
}

/// 拉一帧 H.264 Annex-B。
/// out_buf: Kotlin 提供的 ByteArray 接收帧数据
/// out_is_key: Kotlin 提供的 1-byte ByteArray 回写 is_key (0/1)
/// 返回: >0 帧字节数；0 队空；-1 错误
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rebleach_jni_NativeBridge_pollFrame<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    out_buf: JByteArray<'local>,
    out_cap: jint,
    out_is_key: JByteArray<'local>,
) -> jint {
    let handle = handle as u64;
    if handle == 0 || out_cap <= 0 { return -1; }

    let (is_key, data) = {
        let map = sessions().lock();
        let Some(state) = map.get(&handle) else { return -1; };
        match state.frames.lock().pop_front() {
            Some((k, d)) => (k, d),
            None => return 0,
        }
    };

    if data.len() > out_cap as usize { return -1; }

    // 写入帧数据到 Kotlin ByteArray
    let data_i8: Vec<i8> = data.iter().map(|&b| b as i8).collect();
    if env.set_byte_array_region(&out_buf, 0, &data_i8).is_err() { return -1; }

    // 回写 is_key（1 byte）
    let kb_i8: [i8; 1] = [if is_key { 1 } else { 0 }];
    if env.set_byte_array_region(&out_is_key, 0, &kb_i8).is_err() { return -1; }

    data.len() as jint
}

/// 被控端：Kotlin MediaCodec H.264 Annex-B 帧 → Rust → DataChannel.send
/// is_key: 0=非关键帧，1=关键帧
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rebleach_jni_NativeBridge_pushVideoFrame<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    is_key: jbyte,
    annex_b: JByteArray<'local>,
) {
    let handle = handle as u64;
    let is_key = is_key != 0;

    let data = match env.convert_byte_array(&annex_b) {
        Ok(b) => b,
        Err(e) => {
            error!("pushVideoFrame: annex_b 读取失败: {e}");
            return;
        }
    };

    if data.is_empty() { return; }

    let dcs: Vec<_> = {
        let map = sessions().lock();
        let Some(state) = map.get(&handle) else { return; };
        state.screen_dcs.lock().clone()
    };
    if dcs.is_empty() { return; }

    let is_key_ = is_key;
    _spawn_rt(async move {
        let bytes = bytes::Bytes::from(data);
        // 广播给所有在线主控端（多主控端并行接入）
        for dc in dcs {
            if dc.ready_state()
                != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open
            {
                continue;
            }
            if let Err(e) = dc.send(&bytes).await {
                if is_key_ { warn!("pushVideoFrame(key) send 失败: {e}"); }
            }
        }
    });
}

/// 被控端：16kHz mono i16 PCM → Rust → audio DataChannel
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rebleach_jni_NativeBridge_pushAudioFrame<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    pcm: JByteArray<'local>,
) {
    let handle = handle as u64;

    let data = match env.convert_byte_array(&pcm) {
        Ok(b) => b,
        Err(e) => {
            error!("pushAudioFrame: pcm 读取失败: {e}");
            return;
        }
    };

    if data.is_empty() { return; }

    let dcs: Vec<_> = {
        let map = sessions().lock();
        let Some(state) = map.get(&handle) else { return; };
        state.audio_dcs.lock().clone()
    };
    if dcs.is_empty() { return; }

    _spawn_rt(async move {
        let bytes = bytes::Bytes::from(data);
        for dc in dcs {
            if dc.ready_state()
                != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open
            {
                continue;
            }
            if let Err(e) = dc.send(&bytes).await {
                warn!("pushAudioFrame: send 失败: {e}");
            }
        }
    });
}

/// 批准/拒绝主控端连接（stub）
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rebleach_jni_NativeBridge_approveConnection(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    _handle: jlong,
    _allow: jbyte,
) {
    // M3：通知信令层允许/拒绝主控制入
}

/// 设置码率（kbps）
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_rebleach_jni_NativeBridge_setBitrate(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    kbps: jint,
) {
    let handle = handle as u64;
    let map = sessions().lock();
    if let Some(state) = map.get(&handle) {
        *state.bitrate_kbps.lock() = kbps;
    }
}

// —— 辅助 ——

fn parse_config(json: &str) -> Result<crate::session::SessionConfig, String> {
    let j: serde_json::Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let role_str = j.get("role").and_then(|v| v.as_str()).ok_or("role 缺失")?;
    let role = match role_str {
        "agent" | "controlled" => crate::protocol::Role::Controlled,
        "controller" | "master" => crate::protocol::Role::Controller,
        _ => return Err(format!("未知 role: {role_str}")),
    };
    let server = j.get("server").and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| option_env!("REBLEACH_DEFAULT_SERVER")
            .unwrap_or("wss://rebleach.example.com:443")
            .to_string());
    let peer = j.get("peer").and_then(|v| v.as_str()).map(|s| s.to_string());
    let fps = j.get("fps").and_then(|v| v.as_u64()).unwrap_or(30);
    let id = j.get("id").and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    // 设备 ID 是服务器端鉴权/授权的唯一标识，必须显式提供
    if id.is_empty() {
        return Err("id 缺失：请在 config 中传入数字设备 ID".into());
    }
    // Android 没有 APPDATA/HOME，默认路径不可写；Kotlin 传 filesDir 下的可写路径
    let key_file = j.get("key_file").and_then(|v| v.as_str()).map(std::path::PathBuf::from);
    if let Some(p) = &key_file {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
    }
    // STUN/TURN（跨 NAT 必需）：Kotlin 侧传 stun:host:port / turn:host:port
    let ice_servers: Vec<String> = j
        .get("ice_servers")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let auth = j.get("auth").and_then(|v| v.as_str()).map(|s| s.to_string());

    Ok(crate::session::SessionConfig {
        server,
        id,
        role,
        peer,
        fps,
        ice_servers,
        key_file,
        auth,
    })
}

/// 全局 tokio runtime（所有 Android 会话的 DataChannel.send 都在这上面 spawn）
static GLOBAL_RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
fn _rt_ensure() -> &'static tokio::runtime::Runtime {
    GLOBAL_RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("全局 runtime 创建失败")
    })
}

fn _spawn_rt<F>(fut: F) -> tokio::task::JoinHandle<()>
where F: std::future::Future<Output = ()> + Send + 'static
{
    _rt_ensure().spawn(fut)
}
