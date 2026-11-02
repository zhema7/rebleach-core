// 服务派生用户态子进程：CreateProcessAsUser 在活跃会话内启动 tray agent
// 价值：
//   - 用户登录后立即上线（不必等 HKCU Run 注册表自启，也不受其被禁用/清除影响）
//   - tray 崩溃后服务 5s 内自动拉起
//   - 通过 PID 文件握手：服务派生 tray 后立即写入 %PROGRAMDATA%\Rebleach\tray-system.pid，
//     用户自启的 tray 发现该文件存在 → 立即退出让位
// 令牌选择：
//   - 有用户登录：WTSQueryUserToken 取登录用户令牌派生 rebleach-ui.exe（uiAccess
//     清单 + 已签名的 shim），它再启动托盘本体，托盘继承令牌的 UIAccess 标志，
//     锁屏（Winlogon 安全桌面）下也能 SendInput
//   - 登录界面（无用户登录）：拿不到用户令牌，回退复制服务自身 SYSTEM 令牌，
//     保证"开机未登录即可上线/采集"，但锁屏注入不可用
// 权限：CreateProcessAsUserW / WTSQueryUserToken 需要 SeTcbPrivilege，仅 SYSTEM 服务可调
#![cfg(windows)]

use anyhow::{Context, Result};
use std::path::PathBuf;
use tracing::{info, warn};

fn pid_file() -> PathBuf {
    crate::agent::data_dir().join("tray-system.pid")
}

/// SYSTEM tray 启动时调用，登记自己的 PID
pub fn write_self_pid_file() {
    if let Some(parent) = pid_file().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(pid_file(), std::process::id().to_string());
}

/// 清理 PID 文件（进程退出时调用）
pub fn clear_pid_file() {
    let _ = std::fs::remove_file(pid_file());
}

/// RAII guard：构造时写 PID 文件，drop 时清理。
/// SYSTEM tray 持有它，保证进程异常/正常退出后 PID 文件都会被清。
pub struct TrayPidGuard;
impl TrayPidGuard {
    pub fn new() -> Self {
        write_self_pid_file();
        TrayPidGuard
    }
}
impl Default for TrayPidGuard {
    fn default() -> Self {
        Self::new()
    }
}
impl Drop for TrayPidGuard {
    fn drop(&mut self) {
        // 只删自己写的 PID 文件：服务可能已经派生了新 tray 并覆写了该文件，
        // 旧进程退出时若无条件删除，会把新 tray 的登记抹掉 → 服务每 5s 误判"没有 tray"
        // 反复 kill/重生，形成抖动
        if let Ok(content) = std::fs::read_to_string(pid_file()) {
            if content.trim() == std::process::id().to_string() {
                clear_pid_file();
            }
        }
    }
}

/// PID 是否是一个仍存活的、且镜像名就是本程序的进程。
/// 只看 OpenProcess 会误判：PID 文件残留时该 PID 可能早已被别的进程复用，
/// 服务会误以为托盘活着而永远不再拉起。开机/重启后这个误判概率并不低。
fn is_alive_rebleach(pid: u32) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::core::PWSTR;
    let self_name = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_lowercase()))
        .unwrap_or_else(|| "rebleach.exe".into());
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let ok =
            QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len)
                .is_ok();
        let _ = CloseHandle(h);
        if !ok {
            return false;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]).to_lowercase();
        let name = std::path::Path::new(&path)
            .file_name()
            .map(|s| s.to_string_lossy().to_string());
        name.as_deref() == Some(self_name.as_str())
    }
}

/// 用户态 tray 启动时调用：若 SYSTEM tray 已在跑 → 立即退出
/// 返回 true 表示应该退出
pub fn should_user_tray_quit() -> bool {
    let Ok(content) = std::fs::read_to_string(pid_file()) else {
        return false;
    };
    let Ok(pid) = content.trim().parse::<u32>() else {
        return false;
    };
    if pid == std::process::id() {
        return false; // 自己写的（不可能，不会同时触发）
    }
    if is_alive_rebleach(pid) {
        warn!("服务派生 tray (PID {pid}) 已在跑，本进程让位退出");
        return true;
    }
    // PID 文件残留但进程已死（或 PID 已被别的程序复用），清掉
    clear_pid_file();
    false
}

/// 用户会话内是否已有 SYSTEM 身份的 tray（看 PID 文件）
pub fn tray_system_running() -> Result<bool> {
    let Ok(content) = std::fs::read_to_string(pid_file()) else {
        return Ok(false);
    };
    let Ok(pid) = content.trim().parse::<u32>() else {
        return Ok(false);
    };
    Ok(is_alive_rebleach(pid))
}

/// 进程是否以 LocalSystem（S-1-5-18）身份运行。
/// 用于区分"服务自己派生的 SYSTEM tray"与"用户登录时自启的普通 token tray"，
/// 前者绝不能被清理，否则服务会把刚派生的子进程杀掉，陷入 5s 重生循环。
unsafe fn is_system_process(pid: u32) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let proc = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(h) => h,
        Err(_) => return false,
    };
    let mut token = windows::Win32::Foundation::HANDLE::default();
    let ok = unsafe { OpenProcessToken(proc, TOKEN_QUERY, &mut token) }.is_ok();
    let _ = unsafe { CloseHandle(proc) };
    if !ok {
        return false;
    }
    let mut buf = [0u8; 512];
    let mut ret = 0u32;
    let got = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr() as _),
            buf.len() as u32,
            &mut ret,
        )
    };
    let _ = unsafe { CloseHandle(token) };
    if got.is_err() {
        return false;
    }
    unsafe {
        let tu = &*(buf.as_ptr() as *const TOKEN_USER);
        let s = tu.User.Sid.0 as *const windows::Win32::Security::SID;
        if s.is_null() {
            return false;
        }
        // S-1-5-18：Authority = 5，第一个 sub-authority = 18
        (*s).IdentifierAuthority.Value[5] == 5
            && (*s).SubAuthorityCount >= 1
            && *(*s).SubAuthority.as_ptr() == 18
    }
}

/// 杀掉会话内用户态的老 rebleach.exe（派生前清理）
fn kill_user_tray() {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::*;
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let self_pid = std::process::id();
        let self_name = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_lowercase()))
            .unwrap_or_else(|| "rebleach.exe".into());
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                if entry.th32ProcessID != self_pid {
                    let len = entry
                        .szExeFile
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(entry.szExeFile.len());
                    let name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase();
                    if name == self_name {
                        let mut sid: u32 = 0;
                        if ProcessIdToSessionId(entry.th32ProcessID, &mut sid).is_ok()
                            && sid != 0
                            && !is_system_process(entry.th32ProcessID)
                        {
                            if let Ok(handle) =
                                OpenProcess(PROCESS_TERMINATE, false, entry.th32ProcessID)
                            {
                                let _ = TerminateProcess(handle, 1);
                                let _ = CloseHandle(handle);
                                warn!(
                                    "🛑 已清理会话内老 tray (PID {})，让位给服务派生 tray",
                                    entry.th32ProcessID
                                );
                            }
                        }
                    }
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
}

/// 取同一用户的"关联提升令牌"（过滤令牌 → 完整令牌）。
/// 服务以 LocalSystem 运行，持有 SeTcbPrivilege，可查询其他令牌的 TokenLinkedToken。
/// 返回 None 表示：令牌本就是完整令牌、用户非管理员（无提升令牌）或查询失败。
unsafe fn elevated_linked_token(
    token: windows::Win32::Foundation::HANDLE,
) -> Option<windows::Win32::Foundation::HANDLE> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION_TYPE, TOKEN_LINKED_TOKEN, TokenElevationType,
        TokenElevationTypeLimited, TokenLinkedToken,
    };
    unsafe {
        let mut et = TOKEN_ELEVATION_TYPE::default();
        let mut ret = 0u32;
        if let Err(e) = GetTokenInformation(
            token,
            TokenElevationType,
            Some(&mut et as *mut _ as *mut core::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION_TYPE>() as u32,
            &mut ret,
        ) {
            warn!("TokenElevationType 查询失败: {e}");
            return None;
        }
        warn!("user token ElevationType={} (1=Default 2=Full 3=Limited)", et.0);
        if et != TokenElevationTypeLimited {
            return None;
        }
        let mut linked = TOKEN_LINKED_TOKEN::default();
        if let Err(e) = GetTokenInformation(
            token,
            TokenLinkedToken,
            Some(&mut linked as *mut _ as *mut core::ffi::c_void),
            std::mem::size_of::<TOKEN_LINKED_TOKEN>() as u32,
            &mut ret,
        ) {
            warn!("TokenLinkedToken 查询失败: {e}");
            return None;
        }
        let h = linked.LinkedToken;
        if h.0.is_null() {
            warn!("TokenLinkedToken 返回空句柄（用户可能无提升令牌）");
            None
        } else {
            info!("已取得用户提升令牌（关联令牌），将用于派生 UIAccess 托盘");
            // 诊断：关联令牌的完整性级别（S-1-16-12288=High）与类型
            log_token_integrity(h, "linked(elevated)");
            // 把关联令牌复制为主令牌再交给 CreateProcessAsUser
            use windows::Win32::Security::{
                DuplicateTokenEx, SecurityImpersonation, TOKEN_ALL_ACCESS, TokenPrimary,
            };
            let mut primary = windows::Win32::Foundation::HANDLE::default();
            match DuplicateTokenEx(
                h,
                TOKEN_ALL_ACCESS,
                None,
                SecurityImpersonation,
                TokenPrimary,
                &mut primary,
            ) {
                Ok(()) => {
                    let _ = CloseHandle(h);
                    Some(primary)
                }
                Err(e) => {
                    warn!("DuplicateTokenEx(关联令牌) 失败: {e}");
                    let _ = CloseHandle(h);
                    None
                }
            }
        }
    }
}

/// 诊断：打印令牌的完整性级别 SID（如 S-1-16-8192 Medium / S-1-16-12288 High）。
fn log_token_integrity(token: windows::Win32::Foundation::HANDLE, tag: &str) {
    use windows::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_MANDATORY_LABEL,
        TokenIntegrityLevel,
    };
    unsafe {
        let mut buf = [0u8; 256];
        let mut ret = 0u32;
        if let Ok(()) = GetTokenInformation(
            token,
            TokenIntegrityLevel,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            buf.len() as u32,
            &mut ret,
        ) {
            let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
            let psid = label.Label.Sid;
            let count = *GetSidSubAuthorityCount(psid);
            let rid = *GetSidSubAuthority(psid, (count - 1) as u32);
            let il_str = match rid {
                0x0000 => "Untrusted",
                0x1000 => "Low",
                0x2000 => "Medium",
                0x2100 => "Medium-High",
                0x3000 => "High",
                0x4000 => "System",
                _ => "Unknown",
            };
            warn!("令牌[{tag}] 完整性级别 = S-1-16-{rid:#X} ({il_str})");
        }
    }
}

/// SYSTEM 直派生路径下要派生的 exe：当前 exe 本体（rebleach.exe）。
/// 直接用 SYSTEM 令牌 CreateProcessAsUserW 启动，不走 ShellExecute、不要
/// uiAccess shim——SYSTEM 身份本身就能 OpenDesktop Winlogon，UIAccess 是多余的。
fn tray_exe_path() -> Option<PathBuf> {
    std::env::current_exe().ok()
}

/// 用户令牌路径下要派生的 exe：优先 rebleach-ui.exe（uiAccess 清单 shim，
/// 由它再启动托盘本体，托盘继承 UIAccess 令牌）；缺失时（开发环境直跑）回退本体。
fn ui_shim_exe() -> Result<PathBuf> {
    let cur = std::env::current_exe().context("读取当前 exe 失败")?;
    if let Some(dir) = cur.parent() {
        let shim = dir.join("rebleach-ui.exe");
        if shim.exists() {
            return Ok(shim);
        }
    }
    warn!("rebleach-ui.exe 不存在，回退直接派生 rebleach.exe（无 UIAccess）");
    Ok(cur)
}

/// 通过 cmd.exe /c start 间接启动 exe（走 ShellExecute 路径，授予 UIAccess）。
/// 服务直接 CreateProcess* 启动 uiAccess 清单 exe 会被 appinfo 以 740 拒绝；
/// 但 cmd.exe 无 uiAccess 清单，CreateProcessAsUserW 可正常启动，cmd 的 start
/// 内部调 ShellExecuteEx，由 shell 走 appinfo 授予 UIAccess。
///
/// 由于 cmd.exe 启动 exe 后立即退出，CreateProcessAsUserW 返回的是 cmd.exe 的 PID，
/// 不是托盘本体的 PID。因此启动后轮询 shim 写入的 tray-system.pid 文件，
/// 拿到真实托盘 PID 后返回。
unsafe fn spawn_via_shell_start(
    primary: windows::Win32::Foundation::HANDLE,
    exe_str: &str,
    env: *mut core::ffi::c_void,
) -> Result<u32> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, PROCESS_INFORMATION,
        STARTUPINFOW,
    };
    use windows::core::{PCWSTR, PWSTR};

    // cmd.exe /c start "" "<exe>" --tray --tray-system
    // 第一对 "" 是窗口标题（start 把第一个带引号参数当标题），后面才是命令与参数。
    let cmd = format!("cmd.exe /c start \"\" \"{exe_str}\" --tray --tray-system");
    let mut cmd_wide: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
    let si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut pi = PROCESS_INFORMATION::default();

    let res = CreateProcessAsUserW(
        Some(primary),
        PCWSTR::null(),
        Some(PWSTR(cmd_wide.as_mut_ptr())),
        None,
        None,
        false,
        CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
        Some(env),
        None,
        &si,
        &mut pi,
    );
    match res {
        Ok(()) => {
            let cmd_pid = pi.dwProcessId;
            let _ = CloseHandle(pi.hProcess);
            let _ = CloseHandle(pi.hThread);
            info!("cmd start 已启动（cmd PID {cmd_pid}），等待 shim 写入托盘 PID 文件");

            // 轮询 tray-system.pid：shim 启动 rebleach.exe 后会立即写入其 PID。
            // 最多等 5s，每 100ms 查一次。
            for _ in 0..50 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                if let Ok(content) = std::fs::read_to_string(pid_file()) {
                    if let Ok(pid) = content.trim().parse::<u32>() {
                        if pid != cmd_pid && is_alive_rebleach(pid) {
                            info!("shim 已写入托盘 PID {pid}，进程存活");
                            return Ok(pid);
                        }
                    }
                }
            }
            warn!("等待 shim 写入托盘 PID 超时（cmd PID {cmd_pid}）");
            Err(anyhow::anyhow!("ShellExecute 启动后未在超时内拿到托盘 PID"))
        }
        Err(e) => {
            warn!("CreateProcessAsUserW(cmd start) 失败: {e}");
            Err(e.into())
        }
    }
}

/// 用指定令牌在会话内启动 tray（`--tray --tray-system`）。
/// 四条路径，按优先级尝试：
///   0) cmd.exe /c start "" "<exe>" --tray --tray-system —— 走 ShellExecute 路径，
///      这是服务派生 UIAccess 进程的唯一可行方式。CreateProcess* 系列（含模拟
///      +CreateProcessW）都会被 appinfo 以 740 拒绝，因为调用方是服务（SYSTEM）。
///      cmd.exe 本身无 uiAccess 清单，CreateProcessAsUserW 可成功启动它；
///      cmd 的 start 内部调 ShellExecuteEx，由 shell 走 appinfo 授予 UIAccess。
///   1) 模拟用户令牌 + 普通 CreateProcessW（兜底，已知服务场景会 740）
///   2) CreateProcessWithTokenW（需 SeImpersonatePrivilege）
///   3) CreateProcessAsUserW
/// 成功返回子进程 PID；所有路径都失败则返回最后一个错误。
unsafe fn spawn_with_token(
    primary: windows::Win32::Foundation::HANDLE,
    sid: u32,
    desks: &[&str],
    tag: &str,
    exe: &std::path::Path,
) -> Result<u32> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Security::{
        DuplicateTokenEx, ImpersonateLoggedOnUser, RevertToSelf, SecurityImpersonation,
        TOKEN_ALL_ACCESS, TokenImpersonation,
    };
    use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
    use windows::Win32::System::Threading::{
        CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, CreateProcessW,
        CreateProcessWithTokenW, LOGON_WITH_PROFILE, PROCESS_INFORMATION, STARTUPINFOW,
    };
    use windows::core::{PCWSTR, PWSTR};
    unsafe {
        let mut env: *mut core::ffi::c_void = std::ptr::null_mut();
        CreateEnvironmentBlock(&mut env, Some(primary), false)
            .context("CreateEnvironmentBlock 失败")?;

        let exe_str = exe.display().to_string();
        let exe_wide: Vec<u16> = exe_str.encode_utf16().chain(std::iter::once(0)).collect();

        // 路径 0：cmd.exe /c start —— 走 ShellExecute，授予 UIAccess。
        // cmd.exe 无 uiAccess 清单，CreateProcessAsUserW 可直接启动它；
        // start 内部 ShellExecuteEx 由 shell 走 appinfo 授予 UIAccess。
        // SYSTEM 直派生不走这里：SYSTEM 身份本身能 OpenDesktop Winlogon，
        // UIAccess 是多余的（且 SYSTEM 令牌走 ShellExecute 会被 appinfo 以 740 拒绝）。
        if !tag.starts_with("SYSTEM(直派生")
            && let Ok(pid) = spawn_via_shell_start(primary, &exe_str, env)
        {
            info!("👶 已通过 ShellExecute(cmd start) 派生 UIAccess 托盘（PID {pid}, {tag}）");
            let _ = DestroyEnvironmentBlock(env);
            return Ok(pid);
        }

        // 复制一份模拟令牌（impersonation），用于路径 1 的 ImpersonateLoggedOnUser。
        let mut imp_token = windows::Win32::Foundation::HANDLE::default();
        let dup_imp = DuplicateTokenEx(
            primary,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenImpersonation,
            &mut imp_token,
        );
        if let Err(e) = &dup_imp {
            warn!("DuplicateTokenEx(模拟令牌) 失败，跳过模拟+CreateProcessW 路径: {e}");
        } else {
            // 显式把模拟令牌的会话设为目标会话，避免 CreateProcessW 把进程建在
            // 服务所在的会话 0（appinfo 会因会话不匹配拒绝 UIAccess 进程创建）。
            use windows::Win32::Security::{SetTokenInformation, TokenSessionId};
            if let Err(e) = SetTokenInformation(
                imp_token,
                TokenSessionId,
                &sid as *const u32 as *const core::ffi::c_void,
                std::mem::size_of::<u32>() as u32,
            ) {
                warn!("SetTokenInformation(TokenSessionId, 模拟令牌) 失败: {e}");
            }
        }

        let mut last_err: Option<anyhow::Error> = None;
        let mut spawned_pid: Option<u32> = None;
        for desk in desks {
            let cmd = format!("\"{exe_str}\" --tray --tray-system");
            let mut cmd_wide: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
            let mut desktop: Vec<u16> = desk.encode_utf16().chain(std::iter::once(0)).collect();
            let si = STARTUPINFOW {
                cb: std::mem::size_of::<STARTUPINFOW>() as u32,
                lpDesktop: PWSTR(desktop.as_mut_ptr()),
                ..Default::default()
            };
            let mut pi = PROCESS_INFORMATION::default();

            // 路径 1：模拟用户 + 普通 CreateProcessW（授予 UIAccess 的唯一路径）
            if dup_imp.is_ok() && !imp_token.0.is_null() {
                match ImpersonateLoggedOnUser(imp_token) {
                    Ok(()) => {
                        let res = CreateProcessW(
                            PCWSTR(exe_wide.as_ptr()),
                            Some(PWSTR(cmd_wide.as_mut_ptr())),
                            None,
                            None,
                            false,
                            CREATE_UNICODE_ENVIRONMENT,
                            Some(env as *const core::ffi::c_void),
                            None,
                            &si,
                            &mut pi,
                        );
                        let _ = RevertToSelf();
                        match res {
                            Ok(()) => {
                                info!(
                                    "👶 已在会话 {sid} 派生 tray agent（PID {}, {tag}, 桌面 {desk}, 模拟+CreateProcessW）",
                                    pi.dwProcessId
                                );
                                spawned_pid = Some(pi.dwProcessId);
                                let _ = CloseHandle(pi.hProcess);
                                let _ = CloseHandle(pi.hThread);
                                break;
                            }
                            Err(e) => {
                                warn!("模拟+CreateProcessW 失败({tag}): {e}，回退 CreateProcessWithTokenW");
                                last_err = Some(e.into());
                            }
                        }
                    }
                    Err(e) => {
                        warn!("ImpersonateLoggedOnUser 失败({tag}): {e}");
                        last_err = Some(e.into());
                    }
                }
            }
            if spawned_pid.is_some() {
                break;
            }

            // 路径 2：CreateProcessWithTokenW（需 SeImpersonatePrivilege）
            match CreateProcessWithTokenW(
                primary,
                LOGON_WITH_PROFILE,
                PCWSTR(exe_wide.as_ptr()),
                Some(PWSTR(cmd_wide.as_mut_ptr())),
                CREATE_UNICODE_ENVIRONMENT,
                Some(env as *const core::ffi::c_void),
                None,
                &si,
                &mut pi,
            ) {
                Ok(()) => {
                    info!(
                        "👶 已在会话 {sid} 派生 tray agent（PID {}, {tag}, 桌面 {desk}）",
                        pi.dwProcessId
                    );
                    spawned_pid = Some(pi.dwProcessId);
                    let _ = CloseHandle(pi.hProcess);
                    let _ = CloseHandle(pi.hThread);
                    break;
                }
                Err(e) => {
                    warn!("CreateProcessWithTokenW 失败({tag}): {e}，回退 CreateProcessAsUserW");
                    // 路径 3：CreateProcessAsUserW
                    match CreateProcessAsUserW(
                        Some(primary),
                        PCWSTR(exe_wide.as_ptr()),
                        Some(PWSTR(cmd_wide.as_mut_ptr())),
                        None,
                        None,
                        false,
                        CREATE_UNICODE_ENVIRONMENT,
                        Some(env),
                        None,
                        &si,
                        &mut pi,
                    ) {
                        Ok(()) => {
                            info!(
                                "👶 已在会话 {sid} 派生 tray agent（PID {}, {tag}, 桌面 {desk}）",
                                pi.dwProcessId
                            );
                            spawned_pid = Some(pi.dwProcessId);
                            let _ = CloseHandle(pi.hProcess);
                            let _ = CloseHandle(pi.hThread);
                            break;
                        }
                        Err(e) => {
                            warn!("👶 在会话 {sid} 桌面 {desk} 派生失败({tag}): {e}");
                            last_err = Some(e.into());
                        }
                    }
                }
            }
        }
        if dup_imp.is_ok() && !imp_token.0.is_null() {
            let _ = CloseHandle(imp_token);
        }
        let _ = DestroyEnvironmentBlock(env);
        match spawned_pid {
            Some(pid) => Ok(pid),
            None => Err(last_err
                .unwrap_or_else(|| anyhow::anyhow!("无可用桌面"))
                .context("派生失败（所有桌面候选均不可用）")),
        }
    }
}

/// 启用当前进程令牌中的指定特权。
/// LocalSystem 服务持有 SeTcbPrivilege/SeDebugPrivilege，但 SCM 可能未启用它们
/// （服务 PRIVILEGES 字段为空时默认不启用）。显式启用可保证 WTSQueryUserToken
/// 与跨会话 OpenProcess 不被拒。
fn enable_privilege(name: &str) {
    use windows::Win32::Foundation::{CloseHandle, LUID};
    use windows::Win32::Security::{
        AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = windows::Win32::Foundation::HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES, &mut token).is_err() {
            return;
        }
        let name_wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let mut luid = LUID::default();
        if LookupPrivilegeValueW(
            None,
            windows::core::PCWSTR(name_wide.as_ptr()),
            &mut luid,
        )
        .is_err()
        {
            let _ = CloseHandle(token);
            return;
        }
        let mut tp = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        let _ = AdjustTokenPrivileges(token, false, Some(&mut tp), 0, None, None);
        let _ = CloseHandle(token);
    }
}

/// 兜底：当 WTSQueryUserToken 返回 ERROR_NO_TOKEN 时，从会话内的用户进程
/// （优先 explorer.exe）取令牌并复制为主令牌。
/// 用户态进程的令牌就是登录用户的令牌（管理员开 UAC 时为过滤令牌，
/// 后续 elevated_linked_token 仍可取到关联提升令牌）。
unsafe fn user_token_from_session_process(
    sid: u32,
) -> Option<windows::Win32::Foundation::HANDLE> {
    winlogon_token_from_session(sid, "explorer.exe")
}

/// 从 winlogon.exe 复制主令牌：winlogon.exe 进程拥有 Winlogon 安全桌面，
/// 其令牌天然带有对 Winlogon 桌面的完整访问权限（包括 journal playback），
/// 派生的托盘进程用此令牌运行，SendInput 到 Winlogon 桌面无需额外权限申请。
/// 这是 RustDesk 处理锁屏输入的路径（as_user=false → 从 winlogon.exe 取令牌）。
///
/// 注意：winlogon.exe 只在锁屏/登录/注销期间存在。解锁后会被替换为 LogonUI.exe，
/// 此时退回 explorer.exe（用户令牌）路径。
unsafe fn winlogon_token_from_session(
    sid: u32,
    target_exe: &str,
) -> Option<windows::Win32::Foundation::HANDLE> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Security::{
        DuplicateTokenEx, SecurityImpersonation, TOKEN_ALL_ACCESS, TokenPrimary,
    };
    use windows::Win32::System::Diagnostics::ToolHelp::*;
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return None;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found_pid: Option<u32> = None;
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase();
                if name == target_exe.to_lowercase() {
                    let mut psid: u32 = 0;
                    if ProcessIdToSessionId(entry.th32ProcessID, &mut psid).is_ok() && psid == sid {
                        found_pid = Some(entry.th32ProcessID);
                        break;
                    }
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        let pid = found_pid?;
        let Ok(proc) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            warn!("OpenProcess({pid}, {target_exe}) 失败");
            return None;
        };
        let mut token = windows::Win32::Foundation::HANDLE::default();
        let ok = OpenProcessToken(proc, TOKEN_ALL_ACCESS, &mut token).is_ok();
        let _ = CloseHandle(proc);
        if !ok {
            warn!("OpenProcessToken({pid}, {target_exe}) 失败");
            return None;
        }
        let mut primary = windows::Win32::Foundation::HANDLE::default();
        let dup = DuplicateTokenEx(
            token,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        );
        let _ = CloseHandle(token);
        match dup {
            Ok(()) => {
                info!("从会话 {sid} 的 {target_exe}(PID {pid}) 取得主令牌");
                Some(primary)
            }
            Err(e) => {
                warn!("DuplicateTokenEx({target_exe} {pid}) 失败: {e}");
                None
            }
        }
    }
}

/// 用一个用户令牌尝试派生 UIAccess 托盘：先关联提升令牌，失败再退回原令牌。
/// 成功返回子 PID；失败返回错误（调用方再决定是否回退 SYSTEM）。
unsafe fn try_spawn_with_user_token(
    user_token: windows::Win32::Foundation::HANDLE,
    sid: u32,
) -> Result<u32> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Security::{
        DuplicateTokenEx, SecurityImpersonation, TOKEN_ALL_ACCESS, TokenPrimary,
    };
    unsafe {
        log_token_integrity(user_token, "user(limited)");
        let elevated = elevated_linked_token(user_token);
        let mut primary = windows::Win32::Foundation::HANDLE::default();
        let dup = DuplicateTokenEx(
            user_token,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        );
        match dup {
            Ok(()) => {
                // 先试提升令牌（UIAccess 必需），失败再退回过滤令牌（至少能上线）
                let mut r = Err(anyhow::anyhow!("无可用用户令牌"));
                if let Some(linked) = elevated {
                    if let Ok(exe) = ui_shim_exe() {
                        r = spawn_with_token(
                            linked,
                            sid,
                            &["winsta0\\default"],
                            "用户身份(UIAccess·提升)",
                            &exe,
                        );
                    }
                    let _ = CloseHandle(linked);
                }
                if r.is_err() {
                    if let Ok(exe) = ui_shim_exe() {
                        let r2 = spawn_with_token(
                            primary,
                            sid,
                            &["winsta0\\default"],
                            "用户身份(UIAccess)",
                            &exe,
                        );
                        if let Err(e) = r2 {
                            warn!("用户令牌(过滤)派生失败: {e:#}");
                        } else {
                            r = r2;
                        }
                    }
                }
                let _ = CloseHandle(primary);
                r
            }
            Err(e) => {
                warn!("DuplicateTokenEx(用户令牌) 失败: {e}");
                Err(e.into())
            }
        }
    }
}

/// 在指定会话内派生 `rebleach.exe --tray --tray-system`。
///
/// 派生顺序（按 Winlogon 安全桌面访问权限从强到弱）：
///   1. winlogon.exe 令牌：winlogon.exe 进程**拥有** Winlogon 桌面，
///      其令牌天然带有完整访问（含 journal playback），是唯一能让 SendInput
///      在锁屏凭据页生效的路径（RustDesk as_user=false 做法）。
///      解锁后 winlogon.exe 被 LogonUI.exe 替换，自动跳过本条。
///   2. SYSTEM 直派生：SYSTEM 身份能 OpenDesktop Winlogon，但 SendInput 需要
///      DESKTOP_JOURNALPLAYBACK 权限（已在 DESKTOP_ACCESS 里加上 0x0020）。
///   3. 登录用户令牌（WTSQueryUserToken / explorer.exe）：兜底，解锁态正常，
///      锁屏态因用户令牌拿不到 Winlogon 句柄而不可用。
pub fn spawn_tray_in_session(sid: u32) -> Result<()> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TOKEN_ALL_ACCESS,
        TOKEN_DUPLICATE, TOKEN_QUERY, TokenPrimary, TokenSessionId,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        // 0) 会话内已有存活的派生 tray → 什么都不做。
        if let Ok(true) = tray_system_running() {
            return Ok(());
        }
        // 1) 清理会话内可能存在的用户自启老 tray
        kill_user_tray();
        // 2) 只清死进程残留的登记
        clear_pid_file();

        // 启用特权（SeTcbPrivilege 等）
        enable_privilege("SeTcbPrivilege");
        enable_privilege("SeDebugPrivilege");
        enable_privilege("SeAssignPrimaryTokenPrivilege");
        enable_privilege("SeIncreaseQuotaPrivilege");
        enable_privilege("SeImpersonatePrivilege");

        // —— 路径 1：winlogon.exe 令牌（锁屏期间存在，解锁后自动跳过）——
        if let Some(wl_token) = winlogon_token_from_session(sid, "winlogon.exe") {
            let tray_exe = tray_exe_path();
            if let Some(exe) = &tray_exe {
                match spawn_with_token(
                    wl_token,
                    sid,
                    &["winsta0\\default"],
                    "winlogon(桌面所有者)",
                    exe,
                ) {
                    Ok(pid) => {
                        register_spawned_pid(pid);
                        return Ok(());
                    }
                    Err(e) => warn!("winlogon 令牌派生失败: {e:#}"),
                }
            }
        }

        // —— 路径 2：SYSTEM 令牌 + DESKTOP_JOURNALPLAYBACK (0x0020) ——
        //    Microsoft 2010 年博客确认：SYSTEM 打开 Winlogon 句柄后，缺少
        //    journal playback 权限会导致 SendInput 静默丢弃（返回成功但无效）。
        //    DESKTOP_ACCESS 已加上 0x0020，这条路径在 SYSTEM 身份下应能生效。
        let mut svc_token = HANDLE::default();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_QUERY,
            &mut svc_token,
        )
        .is_ok()
        {
            let mut primary = HANDLE::default();
            if DuplicateTokenEx(
                svc_token,
                TOKEN_ALL_ACCESS,
                None,
                SecurityImpersonation,
                TokenPrimary,
                &mut primary,
            )
            .is_ok()
            {
                let _ = CloseHandle(svc_token);
                if SetTokenInformation(
                    primary,
                    TokenSessionId,
                    &sid as *const u32 as *const core::ffi::c_void,
                    std::mem::size_of::<u32>() as u32,
                )
                .is_ok()
                {
                    let tray_exe = tray_exe_path();
                    if let Some(exe) = &tray_exe {
                        match spawn_with_token(
                            primary,
                            sid,
                            &["winsta0\\default"],
                            "SYSTEM(JOURNALPLAYBACK)",
                            exe,
                        ) {
                            Ok(pid) => {
                                register_spawned_pid(pid);
                                return Ok(());
                            }
                            Err(e) => warn!("SYSTEM 直派生失败: {e:#}"),
                        }
                    }
                } else {
                    warn!("SetTokenInformation(TokenSessionId) for SYSTEM 失败");
                }
                let _ = CloseHandle(primary);
            } else {
                let _ = CloseHandle(svc_token);
                warn!("DuplicateTokenEx(SYSTEM) 失败");
            }
        } else {
            warn!("OpenProcessToken(SYSTEM) 失败");
        }

        // —— 路径 3：登录用户令牌（WTSQueryUserToken / explorer.exe）——
        //    兜底，解锁态正常，锁屏态因用户令牌拿不到 Winlogon 句柄不可用。
        let mut user_token = HANDLE::default();
        let got_user_token = match windows::Win32::System::RemoteDesktop::WTSQueryUserToken(
            sid,
            &mut user_token,
        ) {
            Ok(()) => true,
            Err(_) => match winlogon_token_from_session(sid, "explorer.exe") {
                Some(t) => {
                    user_token = t;
                    true
                }
                None => false,
            },
        };
        if got_user_token {
            match try_spawn_with_user_token(user_token, sid) {
                Ok(pid) => {
                    let _ = CloseHandle(user_token);
                    register_spawned_pid(pid);
                    return Ok(());
                }
                Err(e) => warn!("用户提升令牌派生也失败: {e:#}"),
            }
            let _ = CloseHandle(user_token);
        }

        Err(anyhow::anyhow!("所有派生路径均失败"))
    }
}

/// 服务在派生成功后立即登记 PID 文件。
/// 竞态：子进程启动→自己写 PID 有窗口期，期间巡检会误判"没有 tray"再次派生；
/// 且用户令牌下子进程可能无权写 %PROGRAMDATA%。由服务（SYSTEM）写最稳。
fn register_spawned_pid(pid: u32) {
    if let Some(parent) = pid_file().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(pid_file(), pid.to_string());
}
