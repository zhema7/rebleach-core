//! 安全桌面（Winlogon / UAC）访问辅助。
//!
//! 锁屏、UAC 提示、Ctrl+Alt+Del 都发生在安全桌面上：
//! - 采集：线程必须先 SetThreadDesktop 到当前输入桌面，`GetDC(NULL)` 拿到的才是
//!   安全桌面的屏幕 DC，BitBlt/StretchBlt 才能截到锁屏/登录画面
//! - 注入：SendInput 只作用于调用线程所在桌面，否则被静默丢弃
//!
//! 这些 API 需要 SYSTEM 权限（服务派生的 SYSTEM 托盘），普通用户调用会失败。

#![cfg(windows)]

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing::{info, warn};

use windows::Win32::Foundation::{HANDLE, HWND, LPARAM};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, DESKTOP_ENUMERATE,
    DESKTOP_READOBJECTS, DESKTOP_SWITCHDESKTOP, DESKTOP_WRITEOBJECTS, EnumDesktopWindows,
    GetProcessWindowStation, GetThreadDesktop, GetUserObjectInformationW, HDESK, OpenDesktopW,
    OpenInputDesktop, SetThreadDesktop, UOI_NAME,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
use windows::core::BOOL;

/// 打开桌面所需的访问权限。
///
/// **重要**：DESKTOP_JOURNALPLAYBACK (0x0020) 是 SendInput 在安全桌面生效的必要条件。
/// 症状：OpenDesktop 成功、SetThreadDesktop 成功、SendInput 返回成功，但输入被系统
/// 静默丢弃——这是因为线程在 Winlogon 上没有 journal playback 权限，系统拒绝将
/// 模拟输入转发给桌面窗口。
/// 参考：https://www.sysadmins.lv/retired-msft-blogs/alejacma/sendinput-doesnt-work-but-it-doesnt-return-an-error.aspx
///
/// DESKTOP_ENUMERATE (0x0200) 不申请：它在安全桌面上会因为权限不足被整体拒掉，
/// 反而导致本应成功的 DESKTOP_READOBJECTS|DESKTOP_WRITEOBJECTS 也拿不到句柄。
const DESKTOP_JOURNALPLAYBACK: u32 = 0x0020;
const DESKTOP_ACCESS: DESKTOP_ACCESS_FLAGS = DESKTOP_ACCESS_FLAGS(
    DESKTOP_READOBJECTS.0
        | DESKTOP_WRITEOBJECTS.0
        | DESKTOP_SWITCHDESKTOP.0
        | DESKTOP_JOURNALPLAYBACK,
);

/// 锁屏 / 登录界面所在的桌面
const SECURE_UI_DESKTOP: &str = "Winlogon";

/// 最近一次成功附着的线程与桌面名（采集和输入线程分别记录）
static LAST_ATTACHED: Mutex<Option<(u32, String)>> = Mutex::new(None);
static LAST_ATTACH_WARNING: Mutex<Option<Instant>> = Mutex::new(None);

fn warn_attach_limited(message: String) {
    let mut last = LAST_ATTACH_WARNING
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if last.map_or(true, |at| at.elapsed() >= Duration::from_secs(1)) {
        warn!("{message}");
        *last = Some(Instant::now());
    }
}

fn name_of(h: HANDLE) -> Option<String> {
    let mut buf = [0u16; 128];
    let mut needed = 0u32;
    unsafe {
        GetUserObjectInformationW(
            h,
            UOI_NAME,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            std::mem::size_of::<[u16; 128]>() as u32,
            Some(&mut needed),
        )
        .ok()?;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// 当前输入桌面名称（"Default" / "Winlogon" / "Screen-saver" 等）
pub fn input_desktop_name() -> Option<String> {
    unsafe {
        let hdesk = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS).ok()?;
        let name = name_of(HANDLE(hdesk.0));
        let _ = CloseDesktop(hdesk);
        name
    }
}

/// 打开 Winlogon 桌面，且只有在 LogonUI / LockApp 的窗口确实在上面时才返回句柄。
///
/// 部分机器锁屏时 `OpenInputDesktop` 会返回空的 "Screen-saver" 桌面：
/// 按那个名字附着只能截到全黑画面，注入也会被系统丢弃（os error 5）。
/// 真正的锁屏/登录界面一直在 Winlogon 上，所以以证据（窗口类名）为准。
fn open_secure_ui_desktop() -> Option<HDESK> {
    unsafe {
        let path: Vec<u16> = format!("{SECURE_UI_DESKTOP}\0").encode_utf16().collect();
        let hdesk = match OpenDesktopW(
            windows::core::PCWSTR(path.as_ptr()),
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS.0,
        ) {
            Ok(h) => h,
            Err(e) => {
                // 输入桌面已是 Winlogon 却打不开：这是权限问题，必须留证据
                if input_desktop_name().is_some_and(|n| n.eq_ignore_ascii_case(SECURE_UI_DESKTOP)) {
                    let tid = GetCurrentThreadId();
                    warn_attach_limited(format!(
                        "🖥️ 线程 {tid} OpenDesktop(Winlogon) 失败: {e}（{}）",
                        describe()
                    ));
                }
                return None;
            }
        };
        if has_logon_ui(hdesk) {
            Some(hdesk)
        } else {
            let _ = CloseDesktop(hdesk);
            None
        }
    }
}

/// 桌面上是否存在登录/锁屏界面窗口（LogonUI / LockApp / UWP 锁屏宿主）
fn has_logon_ui(hdesk: HDESK) -> bool {
    unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        unsafe {
            let found = &mut *(lparam.0 as *mut bool);
            let mut buf = [0u16; 256];
            let len = GetClassNameW(hwnd, &mut buf);
            if len > 0 {
                let class = String::from_utf16_lossy(&buf[..len as usize]).to_ascii_lowercase();
                if class.starts_with("windows.ui.")
                    || class.contains("logon")
                    || class.contains("lock")
                    || class.contains("applicationframe")
                {
                    *found = true;
                    return BOOL(0);
                }
            }
        }
        BOOL(1)
    }

    let mut found = false;
    unsafe {
        let _ = EnumDesktopWindows(
            Some(hdesk),
            Some(callback),
            LPARAM(&mut found as *mut bool as isize),
        );
    }
    found
}

/// 按名字打开桌面
fn open_desktop_by_name(name: &str) -> Option<HDESK> {
    unsafe {
        let path: Vec<u16> = format!("{name}\0").encode_utf16().collect();
        OpenDesktopW(
            windows::core::PCWSTR(path.as_ptr()),
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS.0,
        )
        .ok()
    }
}

/// 当前系统认定的输入桌面名（以 OpenInputDesktop 返回值为准），
/// "Screen-saver" 视为 Default 处理。
pub fn target_desktop_name() -> Option<String> {
    let name = input_desktop_name().unwrap_or_else(|| "Default".into());
    if name.eq_ignore_ascii_case("Screen-saver") {
        Some("Default".to_string())
    } else {
        Some(name)
    }
}

/// 是否处于 Winlogon 安全桌面（锁屏 / 登录界面）。
/// 按桌面名判断，而不是 OpenDesktop(Winlogon) 是否成功——后者会因为权限
/// 不足返回失败，把本来"确实在安全桌面上"的情况误判成"不在"。
pub fn is_secure_desktop() -> bool {
    input_desktop_name().is_some_and(|n| n.eq_ignore_ascii_case(SECURE_UI_DESKTOP))
}

/// 采集目标桌面：直接附着到 OpenInputDesktop 返回的活动桌面。
/// 部分机器解锁状态下 OpenInputDesktop 会返回 "Screen-saver"，GDI
/// 截屏附着它会拿到全黑画面，此时回退到 Default。
pub fn attach_input_desktop() -> bool {
    match unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS) } {
        Ok(hdesk) => {
            let name = name_of(HANDLE(hdesk.0)).unwrap_or_else(|| "<未知>".into());
            if name.eq_ignore_ascii_case("Screen-saver") {
                let _ = unsafe { CloseDesktop(hdesk) };
                if let Some(d) = open_desktop_by_name("Default") {
                    return attach_handle(d, "Default".to_string()).is_some();
                }
                return false;
            }
            attach_handle(hdesk, name).is_some()
        }
        Err(e) => {
            let tid = unsafe { GetCurrentThreadId() };
            warn_attach_limited(format!("🖥️ 线程 {tid} OpenInputDesktop 失败: {e}"));
            false
        }
    }
}

/// 注入目标桌面：锁屏用 Winlogon，否则用 Default。
///
/// 与采集不同：SendInput 必须发到真正接收键盘鼠标的桌面。在 OpenInputDesktop
/// 返回 "Screen-saver" 的机器后者是个空桌面，注入会被系统拒绝（os error 5），
/// 而实际显示/接收输入的是 Default——所以注入固定走 Default。
///
/// 返回实际附着的桌面名，调用方可据此判断线程是否跨过安全桌面。
/// 注入目标桌面：优先 OpenDesktop("Winlogon")（锁屏凭据页所在安全桌面），
/// 只有 SYSTEM/winlogon 令牌能拿到句柄；失败时退回 OpenInputDesktop / Default。
///
/// 注入线程必须附着到**系统真正接收键盘鼠标**的桌面才能让 SendInput 生效。
/// 在 Winlogon 上附着 → SendInput → 保持 Winlogon 直到批次结束。
/// 批次后自动回 Default 会导致下一次注入必须重附着，但 SYSTEM 身份可反复切换。
pub fn attach_for_injection() -> Option<String> {
    // 1) 优先：OpenDesktop("Winlogon")——SYSTEM 身份能拿到句柄，用户 High/UIAccess 拿不到。
    //    能打开 = 确实在锁屏态；打不开则看 OpenInputDesktop 的名字判断。
    if let Ok(hdesk) = unsafe {
        let path: Vec<u16> = format!("{SECURE_UI_DESKTOP}\0").encode_utf16().collect();
        OpenDesktopW(
            windows::core::PCWSTR(path.as_ptr()),
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS.0,
        )
    } {
        // 有 LogonUI/LockApp 窗口才附着（避免附着空桌面）
        if has_logon_ui(hdesk) {
            return attach_handle(hdesk, SECURE_UI_DESKTOP.to_string());
        }
        let _ = unsafe { CloseDesktop(hdesk) };
    }

    // 2) OpenInputDesktop 返回 "Winlogon"——凭据页刚切过来，LogonUI 窗口枚举不到但
    //    输入确实在 Winlogon，直接按名字打开附着
    if let Some(name) = input_desktop_name() {
        if name.eq_ignore_ascii_case(SECURE_UI_DESKTOP) {
            if let Some(hdesk) = open_desktop_by_name(SECURE_UI_DESKTOP) {
                return attach_handle(hdesk, SECURE_UI_DESKTOP.to_string());
            }
        }
        // OpenInputDesktop 返回 Default / Screen-saver：解锁态，按 Default 处理
        if !name.eq_ignore_ascii_case("Screen-saver") {
            if let Some(hdesk) = open_desktop_by_name("Default") {
                return attach_handle(hdesk, "Default".to_string());
            }
        }
    }

    // 3) 兜底：直接开 Default
    if let Some(hdesk) = open_desktop_by_name("Default") {
        return attach_handle(hdesk, "Default".to_string());
    }
    let tid = unsafe { GetCurrentThreadId() };
    warn_attach_limited(format!("🖥️ 线程 {tid} attach_for_injection 所有路径均失败"));
    None
}

/// 注入失败后的回退：切到 OpenInputDesktop 返回的桌面再试一次。
/// 仅在线程桌面确实变化时返回 true。
pub fn switch_to_raw_input_desktop() -> bool {
    let Some(hdesk) =
        (unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS) }).ok()
    else {
        return false;
    };
    let name = name_of(HANDLE(hdesk.0)).unwrap_or_else(|| "<未知>".into());
    if current_desktop_name().is_some_and(|cur| cur.eq_ignore_ascii_case(&name)) {
        let _ = unsafe { CloseDesktop(hdesk) };
        return false;
    }
    attach_handle(hdesk, name).is_some()
}

fn attach_handle(hdesk: HDESK, name: String) -> Option<String> {
    let tid = unsafe { GetCurrentThreadId() };

    // 已经是目标桌面就不折腾（SetThreadDesktop 对已有窗口的线程会失败）
    if current_desktop_name().is_some_and(|cur| cur.eq_ignore_ascii_case(&name)) {
        let _ = unsafe { CloseDesktop(hdesk) };
        return Some(name);
    }

    match unsafe { SetThreadDesktop(hdesk) } {
        Ok(()) => {
            // 成功后句柄随线程生命周期保留，不能 CloseDesktop
            let mut last = LAST_ATTACHED.lock().unwrap_or_else(|e| e.into_inner());
            let changed = last.as_ref().map_or(true, |(last_tid, last_name)| {
                *last_tid != tid || *last_name != name
            });
            if changed {
                let input = input_desktop_name().unwrap_or_else(|| "?".into());
                if name.eq_ignore_ascii_case(&input) {
                    info!("🖥️ 线程 {tid} 已附着到桌面 {name}");
                } else {
                    info!(
                        "🖥️ 线程 {tid} 已附着到桌面 {name}（锁屏界面在此，输入桌面实为 {input}）"
                    );
                }
                *last = Some((tid, name.clone()));
            }
            Some(name)
        }
        Err(e) => {
            let _ = unsafe { CloseDesktop(hdesk) };
            warn_attach_limited(format!(
                "🖥️ 线程 {tid} 附着桌面 {name} 失败: {e}（{}）",
                describe()
            ));
            None
        }
    }
}

/// 当前线程所在桌面名（线程桌面句柄由系统管理，不能关闭）
pub fn current_desktop_name() -> Option<String> {
    unsafe { name_of(HANDLE(GetThreadDesktop(GetCurrentThreadId()).ok()?.0)) }
}

/// 进程窗口站名（"WinSta0" / "Service-0x0-3e7$" 等）
///
/// GDI 截屏的前提是进程在 WinSta0 上：服务进程（LocalSystem，session 0）
/// 默认挂在 Service-0x0-3e7$ 上，GetDC(NULL) 能拿到 DC，但 StretchBlt 必失败。
pub fn window_station_name() -> Option<String> {
    let hwinsta = unsafe { GetProcessWindowStation().ok()? };
    name_of(HANDLE(hwinsta.0))
}

/// 当前进程所在会话号：0 = 服务（无交互桌面，截屏必失败），1 = 用户会话
pub fn session_id() -> Option<u32> {
    unsafe {
        let mut sid = 0u32;
        windows::Win32::System::RemoteDesktop::ProcessIdToSessionId(std::process::id(), &mut sid)
            .ok()
            .map(|_| sid)
    }
}

/// 一句话描述采集线程所处的位置，用于采集失败时定位（会话/窗口站错＝永远采不到画面）
pub fn describe() -> String {
    format!(
        "会话={} 窗口站={} 线程桌面={}",
        session_id()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "?".into()),
        window_station_name().unwrap_or_else(|| "?".into()),
        current_desktop_name().unwrap_or_else(|| "?".into())
    )
}
