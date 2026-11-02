// 被控端应用：纯托盘（无窗口）
// 安装打开即：注册到管理端 + 局域网迷你信令 + UDP 发现应答 + 开机自启
// 设置仅通过托盘菜单（复制设备 ID / 复制局域网验证码 / 退出），状态看托盘提示
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager};
use tauri_plugin_updater::UpdaterExt;
use tracing::{error, info, warn};

use crate::agent::{self, AgentConfig};
use crate::lan;
use crate::parse_internal_ca;
#[cfg(all(feature = "agent", windows))]
use crate::virtual_display;

/// 启动后自动检查更新：发现新版本即静默下载安装并重启
/// （此刻刚启动、几乎不可能在会话中；安装失败静默忽略，下轮启动再试）
fn spawn_auto_update(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let updater = match app
            .updater_builder()
            .configure_client(|cb| match parse_internal_ca() {
                Some(ca) => cb.add_root_certificate(ca),
                None => cb,
            })
            .build()
        {
            Ok(u) => u,
            Err(e) => {
                warn!("更新器构建失败（忽略）: {e}");
                return;
            }
        };
        match updater.check().await {
            Ok(Some(u)) => {
                info!("⬆️ 发现新版本 {}，开始静默更新", u.version);
                match u.download_and_install(|_, _| {}, || {}).await {
                    Ok(()) => app.restart(),
                    Err(e) => warn!("更新安装失败（下轮启动重试）: {e}"),
                }
            }
            Ok(None) => info!("已是最新版本"),
            Err(e) => warn!("更新检查失败（忽略）: {e}"),
        }
    });
}

/// 同会话单实例互斥：true = 本进程持有。
///
/// 必须早于 `child::TrayPidGuard::new()` 调用：重复实例若先写了 PID 文件，
/// 退出时（guard Drop）会把它删掉，服务随即误判"没有 tray"，每 5 秒再派生一轮，
/// 形成「派生 → 秒退 → 删 PID → 再派生」的抖动（0.2.0 现场 10 分钟 698 次）。
pub fn claim_single_instance() -> bool {
    use std::sync::atomic::{AtomicBool, Ordering};
    static CLAIMED: AtomicBool = AtomicBool::new(false);
    if CLAIMED.load(Ordering::Relaxed) {
        return true;
    }
    // Local\ 命名空间是会话级的：同一会话内只允许一个 tray
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
        use windows::Win32::System::Threading::CreateMutexW;
        let name: Vec<u16> = "Local\\RebleachTrayAgent\0".encode_utf16().collect();
        unsafe {
            // 句柄故意不关闭：内核在进程退出时回收，互斥生命周期 = 进程生命周期
            let _ = CreateMutexW(None, false, windows::core::PCWSTR(name.as_ptr()));
            if GetLastError() == ERROR_ALREADY_EXISTS {
                return false;
            }
        }
    }
    CLAIMED.store(true, Ordering::Relaxed);
    true
}

pub fn run() {
    // 诊断：panic 钩子把崩溃信息写到日志文件（GUI 子系统无控制台，stderr 丢失）
    install_panic_hook();

    if !claim_single_instance() {
        info!("已有 tray 实例在跑，退出重复实例");
        return;
    }

    // 首次启动自动生成设备 ID 并写入配置（随后自动注册到管理端）
    let cfg = agent::load_config();
    info!(
        "⚙️ 被控端配置：id={} enabled={} server={} 数据目录={}",
        cfg.id,
        cfg.enabled,
        cfg.server,
        agent::data_dir().display()
    );
    if let Err(e) = agent::save_config(&cfg) {
        warn!("写入被控端配置失败: {e}");
    }

    // 局域网直连验证码：每次启动随机生成，托盘提示/菜单可见
    let pin = lan::gen_pin();
    info!("🔢 局域网验证码: {pin}");
    // 同步写到机器级数据目录（与配置同处），便于无控制台环境下排查
    let dir = agent::data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join("lan-pin.txt"), &pin);

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(move |app| {
            // 注册虚拟屏增删 handlers：client 库纯协议，真实实现由 app bin 注入
            // 必须在用户会话里的托盘进程里注册（session 0 的服务调驱动不影响用户桌面）
            #[cfg(all(feature = "agent", windows))]
            {
                rebleach_client::control::register_virtual_handlers(
                    virtual_display::add_virtual_displays,
                    virtual_display::remove_virtual_display,
                    virtual_display::detect_status,
                );
                info!("🖥️ 虚拟屏 handlers 已注册（驱动: {}）", virtual_display::detect_driver().label());
            }

            let cfg: AgentConfig = agent::load_config();
            let tooltip = format!("Rebleach 被控端 · ID {} · 验证码 {}", cfg.id, pin);

            let copy_id = MenuItem::with_id(app, "copyid", "复制设备 ID", true, None::<&str>)?;
            let copy_pin =
                MenuItem::with_id(app, "copypin", "复制局域网验证码", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&copy_id, &copy_pin, &quit])?;

            let pin_menu = pin.clone();
            let mut tray = TrayIconBuilder::with_id("main")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .tooltip(tooltip)
                .on_menu_event(move |app, ev| match ev.id().as_ref() {
                    "copyid" => {
                        let id = agent::load_config().id;
                        if let Ok(mut cb) = arboard::Clipboard::new() {
                            let _ = cb.set_text(&id);
                        }
                        info!("📋 设备 ID 已复制: {id}");
                    }
                    "copypin" => {
                        if let Ok(mut cb) = arboard::Clipboard::new() {
                            let _ = cb.set_text(&pin_menu);
                        }
                        info!("📋 局域网验证码已复制");
                    }
                    "quit" => app.exit(0),
                    _ => {}
                });
            if let Some(icon) = app.default_window_icon().cloned() {
                tray = tray.icon(icon);
            }
            // 开机停在登录界面时 shell（explorer）还没起来，托盘图标可能创建失败；
            // 这里不能让它中断 setup —— 否则进程退出、服务反复重生，永远上不了线
            if let Err(e) = tray.build(app) {
                warn!("托盘图标创建失败（登录界面/无 shell 时可忽略）: {e}");
            }

            // 开机自启（同时清理 0.1.x 旧版自启项）。
            let is_system = std::env::var("USERNAME")
                .map(|u| u.eq_ignore_ascii_case("SYSTEM"))
                .unwrap_or(false);
            #[cfg(windows)]
            let svc_installed = crate::service::is_installed();
            #[cfg(not(windows))]
            let svc_installed = false;
            if !is_system {
                if svc_installed {
                    if let Err(e) = agent::set_autostart(false) {
                        warn!("清理 HKCU 自启项失败: {e}");
                    }
                } else if let Err(e) = agent::set_autostart(cfg.autostart) {
                    warn!("写入开机自启失败: {e}");
                }
            }

            // 局域网：迷你信令（7681）+ UDP 发现应答（7682）
            lan::spawn_relay(pin.clone());
            lan::spawn_discovery_responder(cfg.id.clone());

            // 局域网直连：本机迷你信令上的被控会话。
            {
                let mut lan_cfg = cfg.clone();
                lan_cfg.server = format!("ws://127.0.0.1:{}/ws", lan::LAN_PORT);
                agent::spawn(app.handle().clone(), lan_cfg);
            }

            // 被控端常驻：注册到管理端（中转模式）
            if cfg.enabled {
                agent::spawn(app.handle().clone(), cfg.clone());
            }

            spawn_auto_update(app.handle().clone());
            Ok(())
        });

    let app = match builder.build(tauri::generate_context!()) {
        Ok(a) => a,
        Err(e) => {
            error!("🧨 .build() 失败: {e:#}");
            return;
        }
    };

    // 纯托盘应用无窗口：事件循环会因「无窗口」请求退出（code=None），必须阻止；
    // 托盘菜单「退出」走 app.exit(0)、自动更新走 restart()，code 均为 Some，不受影响
    app.run(|_app, event| {
        if let tauri::RunEvent::ExitRequested {
            code: None, api, ..
        } = event
        {
            api.prevent_exit();
        }
    });
}

/// 把 panic 信息写入日志文件（GUI 子系统 stderr 不可见）。
fn install_panic_hook() {
    use std::io::Write;
    let log_path = agent::data_dir().join("panic.log");
    std::panic::set_hook(Box::new(move |info| {
        let msg = format!(
            "PANIC: {}\n{}",
            info,
            std::backtrace::Backtrace::force_capture()
        );
        eprintln!("{msg}");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            let _ = writeln!(f, "{}", msg);
        }
    }));
}
