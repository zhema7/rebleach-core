// 被控端 Agent：自动注册 + 设备信息 + 心跳（心跳在 client 会话内）
// 目标：安装打开即进托盘、开机自启、无需手动输入任何参数
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rebleach_client::protocol::{DeviceInfo, Role};
use rebleach_client::session::{self, EventSink, SessionConfig, SessionEvent};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;
use tracing::{info, warn};
use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

/// 内置默认中转地址。部署时用环境变量 REBLEACH_DEFAULT_SERVER 指定自己的服务器，
/// 避免把真实域名写进仓库。
pub const DEFAULT_SERVER: &str = match option_env!("REBLEACH_DEFAULT_SERVER") {
    Some(v) => v,
    None => "wss://rebleach.example.com/ws",
};
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "RebleachAgent";
/// 0.1.x 双角色版本使用的旧 Run 键（拆分后被控端包接管，需清理避免重复自启）
const LEGACY_RUN_VALUE: &str = "Rebleach";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    /// 是否作为被控端常驻（默认开启 → 安装打开即注册）
    pub enabled: bool,
    pub server: String,
    /// 本机设备 ID（6~9 位数字，首次生成后固定）
    pub id: String,
    /// 开机自动启动（注册到 HKCU Run，带 --tray 参数静默进托盘）
    pub autostart: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            server: DEFAULT_SERVER.to_string(),
            id: String::new(),
            autostart: true,
        }
    }
}

/// 数据目录：%PROGRAMDATA%\Rebleach（机器级）。
/// 服务（Session 0）/ SYSTEM 托盘 / 用户态托盘共享同一份配置，设备 ID 稳定。
/// 之前用 %APPDATA% 按用户存储：SYSTEM 身份下 APPDATA 指向 systemprofile，
/// 读不到原配置会重新生成随机设备 ID，主控端按旧 ID 无法连接。
pub fn data_dir() -> PathBuf {
    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        // 服务派生的子进程环境块里可能没有 ProgramData，退回系统盘固定路径，
        // 否则会落到 %TEMP%，服务与托盘读到不同目录 → PID/配置握手全部失效
        .unwrap_or_else(|| PathBuf::from("C:\\ProgramData"))
        .join("Rebleach")
}

/// 旧版路径：%APPDATA%\rebleach（按用户存储）
fn legacy_data_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .map(|p| p.join("rebleach"))
}

/// 解析被控端身份文件路径（必要时把旧版用户级密钥迁移到机器级目录）。
///
/// 铁律：只要旧密钥存在就绝不生成新密钥——身份一变，主控端的 TOFU 记录立刻失效，
/// 表现为 `pubkey_mismatch` 直接拒绝连接。因此迁移失败时宁可继续用旧路径，
/// 也不能让 Identity 重新生成。
pub fn identity_path() -> PathBuf {
    let dst = data_dir().join("identity.json");
    if dst.exists() {
        return dst;
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(d) = legacy_data_dir() {
        candidates.push(d.join("identity.json"));
    }
    if let Some(pd) = std::env::var_os("ProgramData") {
        if let Some(users) = Path::new(&pd).parent().map(|p| p.join("Users")) {
            if let Ok(rd) = std::fs::read_dir(&users) {
                for e in rd.flatten() {
                    candidates.push(
                        e.path()
                            .join("AppData")
                            .join("Roaming")
                            .join("rebleach")
                            .join("identity.json"),
                    );
                }
            }
        }
    }
    for src in candidates {
        if !src.exists() {
            continue;
        }
        let _ = std::fs::create_dir_all(data_dir());
        // 旧密钥可能正被运行中的托盘占用（os error 32 共享冲突），重试若干次
        for attempt in 0..10 {
            match std::fs::copy(&src, &dst) {
                Ok(_) => {
                    info!("🔑 已迁移身份密钥: {} → {}", src.display(), dst.display());
                    return dst;
                }
                Err(e) => {
                    if attempt == 9 {
                        warn!(
                            "🔑 迁移身份密钥失败（{e}），沿用旧路径避免重新生成: {}",
                            src.display()
                        );
                        return src;
                    }
                    std::thread::sleep(Duration::from_millis(300));
                }
            }
        }
    }
    info!("🔑 未找到旧身份密钥，将生成新的密钥对");
    dst
}

fn ensure_dir() -> PathBuf {
    let dir = data_dir();
    let fresh = !dir.exists();
    let _ = std::fs::create_dir_all(&dir);
    if fresh {
        // SYSTEM 创建的目录默认不允许普通用户写入；授权 Users 组，
        // 保证用户态托盘也能保存配置（Users SID = S-1-5-32-545）
        let _ = std::process::Command::new("icacls")
            .args([
                dir.to_string_lossy().as_ref(),
                "/grant",
                "*S-1-5-32-545:(OI)(CI)F",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    dir
}

fn config_path() -> PathBuf {
    data_dir().join("agent.json")
}

/// 配置文件是否能安全写回。读取失败（非"文件不存在"）时置 false：
/// 此时 load_config 手里只有默认值，写回会把真实配置（含设备 ID）覆盖掉，
/// 主控端按旧 ID 就永久连不上。这是 0.2.0 现场事故的直接机制之一。
static CONFIG_SAFE_TO_WRITE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

pub fn load_config() -> AgentConfig {
    let path = config_path();
    let mut cfg = match std::fs::read_to_string(&path) {
        Ok(s) => match serde_json::from_str::<AgentConfig>(&s) {
            Ok(c) => c,
            Err(e) => {
                // 解析失败：备份现场 + 尽量沿用可读字段（尤其是设备 ID），绝不静默重建
                let bak = backup_broken_config();
                let c = salvage_config(&s).unwrap_or_default();
                warn!(
                    "⚠️ 配置解析失败（{e}），原文件已备份到 {}，沿用其中可读字段（id={}）",
                    bak.map(|p| p.display().to_string())
                        .unwrap_or_else(|| "（备份失败）".into()),
                    if c.id.is_empty() {
                        "（缺失，将重新生成）"
                    } else {
                        c.id.as_str()
                    }
                );
                c
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => match migrate_legacy_config() {
            Some(c) => c,
            None => AgentConfig::default(),
        },
        Err(e) => {
            // 读不到就别写：多半是权限/占用，磁盘上的配置仍可能是好的
            CONFIG_SAFE_TO_WRITE.store(false, std::sync::atomic::Ordering::Relaxed);
            warn!(
                "⚠️ 读取配置 {} 失败（{e}），本次按默认值运行，且不回写配置文件",
                path.display()
            );
            AgentConfig::default()
        }
    };
    if cfg.server.trim().is_empty() {
        cfg.server = DEFAULT_SERVER.to_string();
    }
    if cfg.id.trim().is_empty() {
        cfg.id = gen_id();
    }
    cfg
}

/// 旧版配置按用户目录存储 → 迁移到机器级目录
fn migrate_legacy_config() -> Option<AgentConfig> {
    let legacy = legacy_data_dir()?.join("agent.json");
    let s = std::fs::read_to_string(legacy).ok()?;
    let c: AgentConfig = serde_json::from_str(&s).ok()?;
    let _ = std::fs::write(ensure_dir().join("agent.json"), &s);
    info!("📦 已迁移旧版配置到 {}", config_path().display());
    Some(c)
}

/// 备份损坏的配置（优先移走，避免下次启动重复备份），返回备份路径（供 WARN 里定位）
fn backup_broken_config() -> Option<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let bak = data_dir().join(format!("agent.json.broken-{stamp}"));
    if std::fs::rename(config_path(), &bak).is_ok() {
        return Some(bak);
    }
    std::fs::copy(config_path(), &bak).ok().map(|_| bak)
}

/// JSON 本身合法但结构对不上（字段缺失/改名/版本差异）时，尽量抢救已知字段
fn salvage_config(raw: &str) -> Option<AgentConfig> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let d = AgentConfig::default();
    Some(AgentConfig {
        enabled: v
            .get("enabled")
            .and_then(|x| x.as_bool())
            .unwrap_or(d.enabled),
        server: v
            .get("server")
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(d.server),
        id: v
            .get("id")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string(),
        autostart: v
            .get("autostart")
            .and_then(|x| x.as_bool())
            .unwrap_or(d.autostart),
    })
}

pub fn save_config(cfg: &AgentConfig) -> Result<(), String> {
    if !CONFIG_SAFE_TO_WRITE.load(std::sync::atomic::Ordering::Relaxed) {
        warn!("跳过配置写回：本次启动没能读到原配置，避免覆盖真实内容");
        return Ok(());
    }
    let dir = ensure_dir();
    let json = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("agent.json"), json).map_err(|e| e.to_string())
}

/// 生成 9 位数字设备 ID（与 RustDesk 类似，便于口述）
fn gen_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 + d.as_secs())
        .unwrap_or(0);
    // 混合进程号与时间，避免同秒启动碰撞
    let v = nanos
        .wrapping_mul(6364136223846793005)
        .wrapping_add(std::process::id() as u64)
        % 900_000_000
        + 100_000_000;
    v.to_string()
}

/// 采集硬件信息（内存/存储/系统/CPU）；IP 与地区异步补充
pub fn collect_info() -> DeviceInfo {
    let mut sys = sysinfo::System::new_all();
    sys.refresh_memory();

    let mut memory_mb = None;
    let total = sys.total_memory();
    if total > 0 {
        memory_mb = Some(total / 1024 / 1024);
    }

    let storage_gb = {
        let disks = sysinfo::Disks::new_with_refreshed_list();
        let total: u64 = disks.iter().map(|d| d.total_space()).sum();
        if total > 0 {
            Some(total / 1024 / 1024 / 1024)
        } else {
            None
        }
    };

    let cpu = sys
        .cpus()
        .first()
        .map(|c| c.brand().trim().to_string())
        .filter(|s| !s.is_empty());

    DeviceInfo {
        hostname: sysinfo::System::host_name(),
        model: device_model(),
        os: sysinfo::System::long_os_version(),
        cpu,
        memory_mb,
        storage_gb,
        ip: None,
        region: None,
    }
}

/// 设备型号：BIOS 厂商 + 机型（如 "Dell Inc. XPS 13 9310"）
fn device_model() -> Option<String> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let bios = hklm.open_subkey(r"HARDWARE\DESCRIPTION\System\BIOS").ok()?;
    let manufacturer = bios
        .get_value::<String, _>("SystemManufacturer")
        .unwrap_or_default();
    let product = bios
        .get_value::<String, _>("SystemProductName")
        .unwrap_or_default();
    let model = format!("{} {}", manufacturer.trim(), product.trim())
        .trim()
        .to_string();
    if model.is_empty() || model.eq_ignore_ascii_case("System Product Name") {
        None
    } else {
        Some(model)
    }
}

/// 通过 IP 定位服务获取公网 IP 与地区（http，无 TLS 依赖；失败静默）
pub async fn enrich_network(info: &mut DeviceInfo) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(4))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    let url = "http://ip-api.com/json/?fields=status,country,regionName,city,query&lang=zh-CN";
    let Ok(resp) = client.get(url).send().await else {
        return;
    };
    let Ok(body) = resp.text().await else { return };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
        return;
    };
    if v.get("status").and_then(|s| s.as_str()) != Some("success") {
        return;
    }
    let s = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let ip = s("query");
    if !ip.is_empty() {
        info.ip = Some(ip);
    }
    let region = format!("{}{}{}", s("country"), s("regionName"), s("city")).replace("  ", " ");
    if !region.trim().is_empty() {
        info.region = Some(region.trim().to_string());
    }
}

/// 开机自启：HKCU Run 写入 `"<exe>" --tray`；同时清掉旧版双角色包留下的自启项
pub fn set_autostart(on: bool) -> Result<(), String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(RUN_KEY).map_err(|e| e.to_string())?;
    // 旧版（0.1.x）自启项若指向其他路径的旧 exe，删除避免开机双开
    if let Ok(old) = key.get_value::<String, _>(LEGACY_RUN_VALUE) {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        if !old.contains(&exe.display().to_string()) {
            let _ = key.delete_value(LEGACY_RUN_VALUE);
            info!("🧹 已清理旧版自启项 {LEGACY_RUN_VALUE}");
        }
    }
    if on {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let cmd = format!("\"{}\" --tray", exe.display());
        key.set_value(RUN_VALUE, &cmd).map_err(|e| e.to_string())
    } else {
        let _ = key.delete_value(RUN_VALUE);
        Ok(())
    }
}

/// 启动被控端常驻任务：注册 → 等待主控端 → 掉线自动重连
/// 锁屏/未登录不再退出会话：SYSTEM 托盘（服务派生）能附着 Winlogon 桌面，
/// 继续采集锁屏/登录画面并接受注入；普通用户态托盘附着失败时画面变黑但不掉线。
pub fn spawn(app: AppHandle, cfg: AgentConfig) -> tauri::async_runtime::JoinHandle<()> {
    // setup 回调运行在 Tauri 主线程（无 tokio 上下文），必须走 Tauri 运行时
    tauri::async_runtime::spawn(async move {
        let mut info = collect_info();
        let mut last_geo: Option<Instant> = None;
        // 指数退避重连计数器：0=刚连上，每次失败+1，稳定 60s 以上清零
        let mut retry_backoff: u8 = 0;
        loop {
            let need_geo = last_geo
                .map(|t| t.elapsed() > Duration::from_secs(600))
                .unwrap_or(true);
            if need_geo {
                enrich_network(&mut info).await;
                last_geo = Some(Instant::now());
                info!(
                    "🌍 设备信息：{} / {} / 内存 {:?}MB / 存储 {:?}GB / IP {:?} / {}",
                    info.model.as_deref().unwrap_or("?"),
                    info.os.as_deref().unwrap_or("?"),
                    info.memory_mb,
                    info.storage_gb,
                    info.ip,
                    info.region.as_deref().unwrap_or("?")
                );
            }
            session::set_device_info(info.clone());

            let (input_tx, input_rx) = mpsc::unbounded_channel();
            let app_s = app.clone();
            let id_for_ev = cfg.id.clone();
            let sink: EventSink = Arc::new(move |ev| match ev {
                SessionEvent::Status(s) => {
                    let _ = app_s.emit(
                        "agent-status",
                        serde_json::json!({ "status": s, "id": id_for_ev.clone() }),
                    );
                }
                SessionEvent::Ended(r) => {
                    let _ = app_s.emit(
                        "agent-status",
                        serde_json::json!({ "status": r, "id": id_for_ev.clone(), "ended": true }),
                    );
                }
                SessionEvent::Video { .. } => {}
                SessionEvent::Monitors(_) => {}
                SessionEvent::Clipboard(_) => {}
                SessionEvent::Stats { .. } => {}
                SessionEvent::Log { .. } => {}
                SessionEvent::DriverStatus { .. } => {}
            });

            // 保证用户会话里有可采集的桌面（无物理显示器时提升虚拟屏分辨率）
            crate::virtual_display::ensure();
            let cfg_run = SessionConfig {
                server: cfg.server.clone(),
                id: cfg.id.clone(),
                role: Role::Controlled,
                peer: None,
                fps: 30,
                ice_servers: Vec::new(),
                key_file: Some(identity_path()),
                auth: None,
            };
            info!("🛰️ 被控端上线（ID {} → {}）", cfg.id, cfg.server);
            let _ = app.emit(
                "agent-status",
                serde_json::json!({ "status": "正在注册到管理端…", "id": cfg.id }),
            );
            // 指数退避重连：2→4→8→…→60s 封顶；会话稳定 60s 以上后计数器清零
            let session_start = Instant::now();
            let outcome = Some(session::run_session(cfg_run, input_rx, sink).await);
            let stable = session_start.elapsed() > Duration::from_secs(60);
            let delay_secs = {
                let mut bk = retry_backoff;
                if stable { bk = 0; } else { bk = (bk + 1).min(5); }
                retry_backoff = bk;
                let base = 2u64.saturating_pow(bk as u32).min(60);
                // 加一点随机抖动（±20%）避免雪崩：用纳秒时间戳做快速伪随机
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_nanos() as f64 / 1_000_000_000.0 - 0.5)
                    .unwrap_or(0.0);
                let jitter = (base as f64 * 0.2 * nanos) as i64;
                (base as i64 + jitter).clamp(1, 60) as u64
            };
            match &outcome {
                Some(Ok(())) => warn!("被控端会话退出，{delay_secs}s 后重连（退避 {retry_backoff}）"),
                Some(Err(e)) => warn!("被控端会话失败: {e:#}，{delay_secs}s 后重连（退避 {retry_backoff}）"),
                None => {}
            }
            drop(input_tx);
            tokio::time::sleep(Duration::from_secs(delay_secs)).await;
        }
    })
}

// ============================================================
// 服务化：无 Tauri 依赖的 headless agent（供 Windows 服务调用）
// 配合用户态 agent 的「锁屏让位」协议：
//   - 服务：仅在「未登录」或「已登录但已锁定」时上线（此时用户态 agent 已退出会话）
//   - 用户态 agent：仅在「已登录且未锁定」时上线（WGC 可用、剪贴板/文件传输可用）
// ============================================================

/// 当前控制台会话是否处于锁定状态（未登录视为锁定）
/// 使用 WTSQuerySessionInformationW(WTSSessionInfoEx) 读 SessionFlags
#[cfg(windows)]
pub fn session_locked() -> bool {
    use windows::Win32::System::RemoteDesktop::*;
    unsafe {
        let sid = WTSGetActiveConsoleSessionId();
        if sid == 0xFFFFFFFF {
            return true; // 无活跃用户：服务应上线
        }
        let mut buf: windows::core::PWSTR = windows::core::PWSTR::null();
        let mut bytes: u32 = 0;
        if WTSQuerySessionInformationW(None, sid, WTSSessionInfoEx, &mut buf, &mut bytes).is_err()
            || buf.is_null()
        {
            return false; // 查询失败：保守当作未锁定（避免与用户态 agent 抢会话）
        }
        let info = &*(buf.0 as *const WTSINFOEXW);
        let flags = info.Data.WTSInfoExLevel1.SessionFlags;
        let locked = flags == WTS_SESSIONSTATE_LOCK as i32;
        let _ = WTSFreeMemory(buf.0 as *mut _);
        locked
    }
}

#[cfg(not(windows))]
pub fn session_locked() -> bool {
    false
}

/// 用户会话内是否已有存活的 SYSTEM 托盘（由服务 CreateProcessAsUser 派生）。
/// 服务据此决定是否退避：托盘在跑就让它占着，托盘崩了就自己兜底上线。
#[cfg(windows)]
pub fn tray_alive() -> bool {
    crate::child::tray_system_running().unwrap_or(false)
}

#[cfg(not(windows))]
pub fn tray_alive() -> bool {
    false
}

/// 服务/独立进程调用的 agent 主循环：
/// - 会话内 SYSTEM 托盘在跑（正常情况）→ 服务退避，由托盘负责采集/注入
/// - 托盘没起来 → 服务**不**接管会话（session 0 无桌面可采集），只由 child_monitor 反复派生托盘
/// - stop 收到 true 时整体退出
#[cfg(windows)]
pub async fn run_headless(_cfg: AgentConfig, mut stop: tokio::sync::watch::Receiver<bool>) {
    let mut info = collect_info();
    let mut last_geo: Option<Instant> = None;
    loop {
        if *stop.borrow() {
            info!("🛑 服务收到停止信号");
            break;
        }
        // 会话内已有存活托盘 → 让托盘跑，服务退避。
        // 若托盘没起来/崩了 → 服务兜底上线，保证管理端始终能看到设备在线。
        if tray_alive() {
            tokio::select! {
                _ = stop.changed() => break,
                _ = tokio::time::sleep(Duration::from_secs(5)) => continue,
            }
        }

        let need_geo = last_geo
            .map(|t| t.elapsed() > Duration::from_secs(600))
            .unwrap_or(true);
        if need_geo {
            enrich_network(&mut info).await;
            last_geo = Some(Instant::now());
            info!(
                "🌍 [svc] 设备信息：{} / IP {:?} / {}",
                info.model.as_deref().unwrap_or("?"),
                info.ip,
                info.region.as_deref().unwrap_or("?")
            );
        }
        session::set_device_info(info.clone());

        // 到这里说明会话内没有存活托盘。
        // 服务跑在 session 0、窗口站是 Service-0x0-3e7$，GDI 截屏必然失败
        // （实测 GetDC(NULL) 能拿到 DC，但 StretchBlt 每次 err=6 失败）——
        // 用它顶包只会让主控端"连得上、0 帧、全黑"，比设备离线更难排查。
        // 所以服务不自己跑会话，只负责把会话内托盘拉起来（child_monitor 每 5s 试一次）。
        warn!("⚠️ [svc] 未发现会话内托盘：session 0 无桌面可采集，本轮不接管会话（等待托盘上线）");
        tokio::select! {
            _ = stop.changed() => break,
            _ = tokio::time::sleep(Duration::from_secs(3)) => {}
        }
    }
}
