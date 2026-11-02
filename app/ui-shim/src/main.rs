//! rebleach-ui.exe —— UIAccess 启动器（被控端托盘的外壳进程）
//!
//! 为什么存在：
//! - 锁屏（Winlogon 安全桌面）下注入键鼠需要进程令牌带 UIAccess 标志；
//!   该标志只在进程创建时按"exe 清单 uiAccess=true + Authenticode 签名 +
//!   Program Files 安全路径 + 用户令牌"授予。
//! - 但 SCM 拒绝启动带 uiAccess 清单的服务 exe（错误 740），
//!   所以清单只能打在这个 shim 上，不能打在 rebleach.exe 上。
//! - UIPI 校验看的是令牌上的 UIAccess 标志，子进程默认继承父进程令牌，
//!   因此本 shim 拿到 UIAccess 后拉起 rebleach.exe 托盘本体，托盘即可注入。
//!
//! 行为：以原样参数启动同目录 rebleach.exe，立即把子进程 PID 写入
//! %ProgramData%\Rebleach\tray-system.pid（服务登记的是 shim 的 PID，
//! 这里尽早改写为托盘本体的 PID，缩小服务巡检的误判窗口），
//! 然后等待托盘退出并以相同退出码退出（托盘死 → shim 死 → 服务 5s 内重拉）。
#![windows_subsystem = "windows"]

use std::process::Command;

fn log_line(msg: &str) {
    let Some(pd) = std::env::var_os("ProgramData") else { return };
    let dir = std::path::PathBuf::from(pd).join("Rebleach");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("rebleach.log"))
    {
        use std::io::Write;
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "{epoch} [ui-shim] {msg}");
    }
}

fn main() {
    let shim = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            log_line(&format!("读取 current_exe 失败: {e}"));
            std::process::exit(1);
        }
    };
    let dir = shim.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let target = dir.join("rebleach.exe");
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();

    match Command::new(&target).args(&args).current_dir(&dir).spawn() {
        Ok(mut child) => {
            if let Some(pd) = std::env::var_os("ProgramData") {
                let data = std::path::PathBuf::from(pd).join("Rebleach");
                let _ = std::fs::create_dir_all(&data);
                let _ = std::fs::write(data.join("tray-system.pid"), child.id().to_string());
            }
            log_line(&format!("已启动托盘本体 PID {}（{}）", child.id(), target.display()));
            let code = match child.wait() {
                Ok(s) => s.code().unwrap_or(0),
                Err(e) => {
                    log_line(&format!("等待托盘本体失败: {e}"));
                    1
                }
            };
            std::process::exit(code);
        }
        Err(e) => {
            log_line(&format!("派生 {} 失败: {e}", target.display()));
            std::process::exit(1);
        }
    }
}
