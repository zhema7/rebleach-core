// 被控端 Windows 服务：SYSTEM 权限常驻，锁屏/未登录时即可上线 + 解锁/UAC
// 架构（简化版 TeamViewer 模型）：
//   - 服务（Session 0, SYSTEM）：仅在「未登录」或「已登录但已锁定」时跑 WebRTC
//     · 此时用户态 agent 退避；WGC 无法采集安全桌面 → 主控端看到黑屏，但可解锁
//     · 解锁/UAC 通过 SendSAS + OpenInputDesktop 注入（SYSTEM 具备 SeTcbPrivilege）
//   - 用户态 agent（HKCU Run 启动）：用户解锁登录后接管 WebRTC，视频/剪贴板/文件恢复
// 局限（未来迭代）：服务跑 WebRTC 时视频为黑，主控端只能解锁；完整方案需
//   CreateProcessAsUser 在用户会话内派生采集子进程，经 named pipe 把 H.264 帧交给服务转发
#![cfg(windows)]

use std::ffi::OsString;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tracing::{error, info, warn};
use windows_service::{
    define_windows_service,
    service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
};

use crate::agent;

pub const SERVICE_NAME: &str = "RebleachAgent";
pub const SERVICE_DISPLAY: &str = "Rebleach 被控端服务";

define_windows_service!(ffi_service_main, service_main);

/// 服务入口：SCM 调用 StartServiceCtrlDispatcherW 阻塞到服务停止
pub fn run() -> Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .context("StartServiceCtrlDispatcher 失败（需由 SCM 启动，勿直接运行）")?;
    Ok(())
}

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = run_inner() {
        error!("服务运行失败: {e:#}");
    }
}

fn set_status(
    handle: &windows_service::service_control_handler::ServiceStatusHandle,
    state: ServiceState,
    controls: ServiceControlAccept,
) -> Result<()> {
    handle
        .set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: controls,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(10),
            process_id: None,
        })
        .context("SetServiceStatus 失败")
}

fn run_inner() -> Result<()> {
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let handle = service_control_handler::register(SERVICE_NAME, move |event| match event {
        ServiceControl::Stop => {
            let _ = stop_tx.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })
    .context("RegisterServiceCtrlHandler 失败")?;

    set_status(
        &handle,
        ServiceState::StartPending,
        ServiceControlAccept::empty(),
    )?;

    // SCM stop（mpsc，同步）→ tokio watch（异步）
    let (watch_tx, watch_rx) = tokio::sync::watch::channel(false);
    std::thread::spawn(move || {
        if stop_rx.recv().is_ok() {
            let _ = watch_tx.send(true);
        }
    });

    set_status(&handle, ServiceState::Running, ServiceControlAccept::STOP)?;
    info!("🛠️ {SERVICE_DISPLAY} 已启动");

    let rt = tokio::runtime::Runtime::new().context("创建 tokio runtime 失败")?;
    rt.block_on(async {
        let cfg = agent::load_config();
        if let Err(e) = agent::save_config(&cfg) {
            warn!("写入被控端配置失败: {e}");
        }
        // 并行：headless agent（锁屏/未登录时上线）+ 会话监控（登录后派生 tray 子进程）
        let (mon_tx, mon_rx) = tokio::sync::watch::channel(false);
        let monitor = tokio::spawn(child_monitor(mon_rx));
        agent::run_headless(cfg, watch_rx).await;
        let _ = mon_tx.send(true);
        let _ = monitor.await;
    });

    set_status(
        &handle,
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
    )?;
    info!("🛠️ {SERVICE_DISPLAY} 已停止");
    Ok(())
}

// ============================================================
// 会话监控：用户登录后立即在其会话内派生 tray agent（CreateProcessAsUser）
// 替代/兜底 HKCU Run 自启；tray 崩溃后 5s 内自动拉起
// ============================================================
async fn child_monitor(mut stop: tokio::sync::watch::Receiver<bool>) {
    // 派生退避：托盘在跑就恢复 5s 巡检；每次派生后翻倍（上限 30s），避免秒退循环变成 5s 风暴。
    let mut backoff = Duration::from_secs(5);
    loop {
        if *stop.borrow() {
            break;
        }
        match ensure_tray_running() {
            Ok(true) => backoff = Duration::from_secs(5),
            Ok(false) => backoff = (backoff * 2).min(Duration::from_secs(30)),
            Err(e) => {
                warn!("子进程监控: {e:#}");
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
        tokio::select! {
            _ = stop.changed() => break,
            _ = tokio::time::sleep(backoff) => {}
        }
    }
}

/// 返回 true = 会话内托盘已在跑（本轮未派生）；false = 刚派生了一个新托盘
fn ensure_tray_running() -> Result<bool> {
    let Some(sid) = active_session_id() else {
        return Ok(true); // 还拿不到可用会话（开机极早期），下轮再试，别退避
    };
    match crate::child::tray_system_running() {
        Ok(true) => Ok(true),
        Ok(false) => {
            crate::child::spawn_tray_in_session(sid)?;
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

/// 取当前控制台会话 ID。
/// 开机停在登录界面时（用户还没登录）控制台会话已经存在，可以直接在
/// session 1 派生 SYSTEM 托盘 —— 这就是「开机未登录即可上线」的关键。
/// 若 WTSGetActiveConsoleSessionId 暂时拿不到，退化枚举 WTS 会话里
/// Active/Connected 且 ID 最小的那个（物理控制台）。
fn active_session_id() -> Option<u32> {
    use windows::Win32::System::RemoteDesktop::*;
    unsafe {
        let sid = WTSGetActiveConsoleSessionId();
        if sid != 0xFFFFFFFF {
            return Some(sid);
        }
        let mut p: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
        let mut count = 0u32;
        if WTSEnumerateSessionsW(None, 0, 1, &mut p, &mut count).is_err() || p.is_null() {
            return None;
        }
        let mut best: Option<u32> = None;
        for i in 0..count as usize {
            let s = &*p.add(i);
            if s.SessionId == 0 {
                continue;
            }
            if s.State == WTSActive || s.State == WTSConnected {
                best = Some(best.map_or(s.SessionId, |b| b.min(s.SessionId)));
            }
        }
        WTSFreeMemory(p as *mut _);
        best
    }
}

// ============================================================
// 安装 / 卸载（通过 sc.exe，无需引入 SCM API）
// 需要管理员权限；服务以 LocalSystem 运行 + auto 启动
// ============================================================

/// 安装服务：binPath 指向当前 exe 加 --service 参数
/// 注意：Command::args 在 Windows 上会对含空格的参数自动加引号，
/// 所以 bin 字符串不要再手动拼 \"...\"，否则 sc.exe 收到的是双重引用。
pub fn install() -> Result<String> {
    let exe = std::env::current_exe().context("读取当前 exe 路径失败")?;
    let bin = format!("{} --service", exe.display());

    let out = std::process::Command::new("sc.exe")
        .arg("create")
        .arg(SERVICE_NAME)
        .arg("binPath=")
        .arg(&bin)
        .arg("start=")
        .arg("auto")
        .output()
        .context("调用 sc.exe 失败")?;
    if !out.status.success() {
        return Err(anyhow!(
            "sc create 失败: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    // 描述 + 失败自动重启（10s/30s/1min）
    let _ = std::process::Command::new("sc.exe")
        .args([
            "description",
            SERVICE_NAME,
            "Rebleach 远程桌面被控端常驻服务",
        ])
        .output();
    let _ = std::process::Command::new("sc.exe")
        .args([
            "failure",
            SERVICE_NAME,
            "reset=",
            "86400",
            "actions=",
            "restart/10000/restart/30000/restart/60000",
        ])
        .output();
    // 立即启动
    let start = std::process::Command::new("sc.exe")
        .args(["start", SERVICE_NAME])
        .output()
        .context("启动服务失败")?;
    if !start.status.success() {
        warn!(
            "服务已创建但启动失败: {}",
            String::from_utf8_lossy(&start.stderr)
        );
    }
    Ok(format!(
        "服务 {SERVICE_NAME} 已安装并启动（LocalSystem / auto）"
    ))
}

/// 卸载服务：先停止再删除
pub fn uninstall() -> Result<String> {
    let _ = std::process::Command::new("sc.exe")
        .args(["stop", SERVICE_NAME])
        .output();
    // 等停止完成（最多 5s）
    for _ in 0..10 {
        std::thread::sleep(Duration::from_millis(500));
        let q = std::process::Command::new("sc.exe")
            .args(["query", SERVICE_NAME])
            .output();
        if let Ok(o) = q {
            let s = String::from_utf8_lossy(&o.stdout);
            if s.contains("STOPPED") || s.contains("does not exist") {
                break;
            }
        }
    }
    let out = std::process::Command::new("sc.exe")
        .args(["delete", SERVICE_NAME])
        .output()
        .context("调用 sc.exe 失败")?;
    if !out.status.success() {
        return Err(anyhow!(
            "sc delete 失败: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(format!("服务 {SERVICE_NAME} 已卸载"))
}

/// 查询服务是否已安装
pub fn is_installed() -> bool {
    std::process::Command::new("sc.exe")
        .args(["query", SERVICE_NAME])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
