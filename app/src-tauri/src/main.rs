// Rebleach 桌面端（Tauri 2）—— 双安装包拆分
// feature controller（默认）：主控端，WebView UI + WebRTC 会话 + 节点管理 + 局域网直连
// feature agent：被控端，纯托盘（无窗口）+ 自动注册 + 局域网迷你信令
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(feature = "agent")]
mod agent;
#[cfg(feature = "agent")]
mod agent_app;
#[cfg(all(feature = "agent", windows))]
mod child;
#[cfg(feature = "controller")]
mod controller;
#[cfg(feature = "controller")]
mod ctlcfg;
mod lan;
#[cfg(all(feature = "agent", windows))]
mod service;
#[cfg(all(feature = "agent", windows))]
mod virtual_display;

// ============================================================
// 共享：内置 CA / TLS provider / 更新端点派生
// ============================================================
/// 服务端用的是 Caddy 自签 CA，更新请求必须信任内置证书，否则 TLS 校验直接失败。
const ROOT_CA_PEM: &[u8] = include_bytes!("../certs/rebleach-root.crt");

/// 内置 CA 解析结果（失败时更新会因证书校验不通过而报错）
fn parse_internal_ca() -> Option<reqwest::Certificate> {
    match reqwest::Certificate::from_pem(ROOT_CA_PEM) {
        Ok(ca) => Some(ca),
        Err(e) => {
            tracing::warn!("内置 CA 解析失败，更新请求可能校验不过: {e}");
            None
        }
    }
}

/// reqwest 用的是 rustls-no-provider，必须显式装一个 provider，否则 HTTPS 请求会 panic
fn init_tls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// ws(s)://host/ws → http(s)://host/api/v1/updater/download/updater-manifest-controller.json
/// （复用包下载公开端点直接服务清单文件，服务器无需为双产品新增路由）
#[cfg(feature = "controller")]
fn updater_url_from_server(server: &str) -> Option<String> {
    let base = server.trim().trim_end_matches('/');
    let base = base.strip_suffix("/ws").unwrap_or(base);
    let http = base
        .replacen("wss://", "https://", 1)
        .replacen("ws://", "http://", 1);
    if http.starts_with("http://") || http.starts_with("https://") {
        Some(format!(
            "{http}/api/v1/updater/download/updater-manifest-controller.json"
        ))
    } else {
        None
    }
}

/// 日志初始化。
/// 被控端构建（GUI 子系统、无控制台、服务/SYSTEM 托盘）把日志落到
/// %ProgramData%\Rebleach\rebleach.log，否则出问题完全没有可观测性。
fn init_tracing() {
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());

    #[cfg(all(feature = "agent", windows))]
    if let Some(file) = open_log_file() {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(file)
            .with_ansi(false)
            .init();
        return;
    }

    // 主控端同为 GUI 子系统，stdout 丢失 → 落到 %APPDATA%/rebleach-controller/controller.log
    #[cfg(all(feature = "controller", windows))]
    {
        if let Some(file) = open_controller_log_file() {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(file)
                .with_ansi(false)
                .init();
            return;
        }
    }

    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// 打开 %APPDATA%/rebleach-controller/controller.log（超过 4MB 时截断）
#[cfg(all(feature = "controller", windows))]
fn open_controller_log_file() -> Option<std::sync::Mutex<std::fs::File>> {
    use std::fs::OpenOptions;
    let path = ctlcfg::log_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok()?;
    }
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > 4 * 1024 * 1024 {
            let _ = std::fs::remove_file(&path);
        }
    }
    let f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    Some(std::sync::Mutex::new(f))
}

/// 打开 %ProgramData%\Rebleach\rebleach.log（超过 4MB 时截断）
#[cfg(all(feature = "agent", windows))]
fn open_log_file() -> Option<std::sync::Mutex<std::fs::File>> {
    use std::fs::OpenOptions;
    let dir = std::path::PathBuf::from(std::env::var_os("ProgramData")?).join("Rebleach");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("rebleach.log");
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > 4 * 1024 * 1024 {
            let _ = std::fs::remove_file(&path);
        }
    }
    let f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    Some(std::sync::Mutex::new(f))
}

fn main() {
    init_tracing();
    init_tls_provider();

    // 服务模式分发（仅 agent 构建可用）：
    //   rebleach --service           由 SCM 启动，跑 SYSTEM 常驻 agent
    //   rebleach --install-service   注册服务（需管理员）
    //   rebleach --uninstall-service 卸载服务（需管理员）
    #[cfg(all(feature = "agent", windows))]
    let _tray_guard: Option<child::TrayPidGuard> = {
        let args: Vec<String> = std::env::args().collect();
        if args.iter().any(|a| a == "--service") {
            if let Err(e) = service::run() {
                eprintln!("服务运行失败: {e:#}");
                std::process::exit(1);
            }
            return;
        }
        if args.iter().any(|a| a == "--install-service") {
            match service::install() {
                Ok(msg) => {
                    println!("{msg}");
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("安装服务失败: {e:#}");
                    std::process::exit(1);
                }
            }
        }
        if args.iter().any(|a| a == "--uninstall-service") {
            match service::uninstall() {
                Ok(msg) => {
                    println!("{msg}");
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("卸载服务失败: {e:#}");
                    std::process::exit(1);
                }
            }
        }
        if args.iter().any(|a| a == "--migrate-identity") {
            // 安装包（管理员）在装完、before 注册服务时调用：把用户级旧密钥迁到机器级，
            // 此时托盘通常还没启动，不会出现 os error 32 的占用问题
            let path = agent::identity_path();
            println!("身份文件: {}", path.display());
            std::process::exit(0);
        }
        if args.iter().any(|a| a == "--tray-system") {
            // 同会话已有存活 tray → 先退出：否则会覆盖 PID 文件，再在退出时把它删掉，
            // 服务会误判"没有托盘"每 5 秒不断派生 doomed 子进程
            if child::should_user_tray_quit() {
                std::process::exit(0);
            }
            // 单实例判定必须早于写 PID 文件：重复实例退出时不得留下"删 PID"的副作用
            if !agent_app::claim_single_instance() {
                tracing::info!("已有 tray 实例在跑，退出重复实例（不触碰 PID 文件）");
                std::process::exit(0);
            }
            // 服务派的 SYSTEM tray：guard 登记 PID，进程结束自动清理。
            // 注意必须绑定到 main 作用域（不能写在内部 if 块里，否则出块即 drop 删掉 PID 文件）
            Some(child::TrayPidGuard::new())
        } else {
            if args.iter().any(|a| a == "--tray") {
                // 用户态 tray：若 SYSTEM tray 已在跑 → 直接退出让位
                if child::should_user_tray_quit() {
                    std::process::exit(0);
                }
                if !agent_app::claim_single_instance() {
                    tracing::info!("已有 tray 实例在跑，退出重复实例");
                    std::process::exit(0);
                }
            }
            None
        }
    };

    #[cfg(feature = "controller")]
    controller::run();
    #[cfg(all(feature = "agent", not(feature = "controller")))]
    agent_app::run();
}
