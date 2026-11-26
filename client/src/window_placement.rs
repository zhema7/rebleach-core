//! 让被控端新创建的窗口默认出现在「主控端当前观看的显示器」上。
//!
//! 背景：Windows 默认把新进程的窗口放在主显示器。主控端切换虚拟屏后，
//! 新拉起的应用仍出现在 0 号屏，用户看不到（文档第 8 章待修项）。
//!
//! 实现：安装一个 `EVENT_OBJECT_SHOW` 全局 WinEvent 钩子。当有顶层窗口显示时，
//! 若它的中心不在目标显示器矩形内，就把它移动到目标显示器中央。
//! 钩子只在会话建立且采集到某块显示器时启用，会话结束后禁用。

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);
static TARGET_X: AtomicI32 = AtomicI32::new(0);
static TARGET_Y: AtomicI32 = AtomicI32::new(0);
static TARGET_W: AtomicI32 = AtomicI32::new(0);
static TARGET_H: AtomicI32 = AtomicI32::new(0);
static HOOK_STARTED: AtomicBool = AtomicBool::new(false);

/// 设置新窗口应出现的目标显示器（虚拟桌面物理坐标 + 尺寸）
pub fn set_target(x: i32, y: i32, w: i32, h: i32) {
    if w <= 0 || h <= 0 {
        return;
    }
    TARGET_X.store(x, Ordering::Relaxed);
    TARGET_Y.store(y, Ordering::Relaxed);
    TARGET_W.store(w, Ordering::Relaxed);
    TARGET_H.store(h, Ordering::Relaxed);
    ENABLED.store(true, Ordering::Relaxed);
    ensure_hook();
}

/// 会话结束：不再干预新窗口位置
pub fn disable() {
    ENABLED.store(false, Ordering::Relaxed);
}

fn ensure_hook() {
    if HOOK_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::Builder::new()
        .name("win-placement".into())
        .spawn(|| unsafe {
            use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent};
            use windows::Win32::UI::WindowsAndMessaging::{
                GetMessageW, EVENT_OBJECT_SHOW, MSG, WINEVENT_OUTOFCONTEXT,
                WINEVENT_SKIPOWNPROCESS,
            };

            unsafe extern "system" fn proc(
                _hook: windows::Win32::UI::Accessibility::HWINEVENTHOOK,
                event: u32,
                hwnd: windows::Win32::Foundation::HWND,
                id_object: i32,
                id_child: i32,
                _thread: u32,
                _time: u32,
            ) {
                if event != EVENT_OBJECT_SHOW || id_object != 0 || id_child != 0 {
                    return;
                }
                if !ENABLED.load(Ordering::Relaxed) {
                    return;
                }
                place_window(hwnd);
            }

            let hook = SetWinEventHook(
                EVENT_OBJECT_SHOW,
                EVENT_OBJECT_SHOW,
                None,
                Some(proc),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            );
            if hook.is_invalid() {
                HOOK_STARTED.store(false, Ordering::SeqCst);
                return;
            }
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
            let _ = UnhookWinEvent(hook);
        })
        .ok();
}

fn place_window(hwnd: windows::Win32::Foundation::HWND) {
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::*;

    unsafe {
        if hwnd.is_invalid() || hwnd == HWND::default() {
            return;
        }
        let mut pid = 0u32;
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        // 不移动本进程自己的窗口（托盘、日志窗等）
        if pid == GetCurrentProcessId() {
            return;
        }
        if !IsWindowVisible(hwnd).as_bool() {
            return;
        }
        let style = GetWindowLongW(hwnd, GWL_STYLE);
        // 只处理有标题栏的顶层窗口，跳过子窗口、工具提示、弹出菜单
        if style & (WS_CHILD.0 | WS_CAPTION.0) as i32 != WS_CAPTION.0 as i32 {
            return;
        }
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return;
        }
        let (tx, ty, tw, th) = (
            TARGET_X.load(Ordering::Relaxed),
            TARGET_Y.load(Ordering::Relaxed),
            TARGET_W.load(Ordering::Relaxed),
            TARGET_H.load(Ordering::Relaxed),
        );
        if tw <= 0 || th <= 0 {
            return;
        }
        // 窗口中心已在目标屏内则不动
        let cx = (rect.left + rect.right) / 2;
        let cy = (rect.top + rect.bottom) / 2;
        if cx >= tx && cx < tx + tw && cy >= ty && cy < ty + th {
            return;
        }
        let w = (rect.right - rect.left).min(tw).max(120);
        let h = (rect.bottom - rect.top).min(th).max(80);
        let nx = tx + (tw - w) / 2;
        let ny = ty + (th - h) / 2;
        let _ = SetWindowPos(
            hwnd,
            None,
            nx,
            ny,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS,
        );
    }
}
