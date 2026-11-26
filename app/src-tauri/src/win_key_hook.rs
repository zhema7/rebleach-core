//! 主控端原生 Win 键钩子。
//!
//! WebView2 / Chromium 在 Windows 上收不到系统 Win 键（被 Shell 截获用于开始菜单），
//! 导致远控时按 Win 键没有反应。这里用 WH_KEYBOARD_LL 低层钩子：
//! - 只有当主控端窗口在前台时，才拦截 LWIN/RWIN；
//! - 把 Win 按下/抬起转换成 `ControlMsg::KeyDown/KeyUp` 发到被控端；
//! - 返回 1 阻止本机弹出开始菜单。
//!
//! 普通按键完全放行，仍由前端 JS 处理，避免重复注入。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use rebleach_client::protocol::ControlMsg;
use tokio::sync::mpsc::UnboundedSender;

static HOOK_TX: Mutex<Option<UnboundedSender<ControlMsg>>> = Mutex::new(None);
static HOOK_THREAD: AtomicU32 = AtomicU32::new(0);

/// 会话建立时调用：安装钩子并把 Win 键事件投递到当前会话的输入队列
pub fn start(tx: UnboundedSender<ControlMsg>) {
    stop();
    *HOOK_TX.lock().unwrap() = Some(tx);
    std::thread::Builder::new()
        .name("win-key-hook".into())
        .spawn(|| unsafe {
            use windows::Win32::UI::WindowsAndMessaging::{
                CallNextHookEx, GetMessageW, SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION,
                MSG, WH_KEYBOARD_LL,
            };

            unsafe extern "system" fn proc(
                code: i32,
                wparam: windows::Win32::Foundation::WPARAM,
                lparam: windows::Win32::Foundation::LPARAM,
            ) -> windows::Win32::Foundation::LRESULT {
                if code == HC_ACTION as i32 {
                    if let Some(vk) = win_key_vk(lparam) {
                        if foreground_is_ours() {
                            send_win_key(vk, wparam.0);
                            // 吞掉事件：本机不弹开始菜单，同时避免再次触发
                            return windows::Win32::Foundation::LRESULT(1);
                        }
                    }
                }
                CallNextHookEx(None, code, wparam, lparam)
            }

            let hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(proc), None, 0) {
                Ok(h) => h,
                Err(_) => {
                    HOOK_THREAD.store(0, Ordering::Relaxed);
                    return;
                }
            };
            let tid = windows::Win32::System::Threading::GetCurrentThreadId();
            HOOK_THREAD.store(tid, Ordering::Relaxed);
            let mut msg = MSG::default();
            // 低层键盘钩子必须在带消息循环的线程里安装，否则回调不会被调用
            loop {
                let r = GetMessageW(&mut msg, None, 0, 0);
                if !r.as_bool() {
                    break;
                }
            }
            let _ = UnhookWindowsHookEx(hook);
            HOOK_THREAD.store(0, Ordering::Relaxed);
        })
        .ok();
}

/// 会话结束：卸载钩子，放行 Win 键
pub fn stop() {
    let tid = HOOK_THREAD.swap(0, Ordering::Relaxed);
    if tid != 0 {
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_QUIT};
            let _ = PostThreadMessageW(tid, WM_QUIT, Default::default(), Default::default());
        }
    }
    *HOOK_TX.lock().unwrap() = None;
}

unsafe fn win_key_vk(
    lparam: windows::Win32::Foundation::LPARAM,
) -> Option<u32> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_LWIN, VK_RWIN};
    use windows::Win32::UI::WindowsAndMessaging::KBDLLHOOKSTRUCT;
    let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
    if kb.vkCode == VK_LWIN.0 as u32 {
        Some(VK_LWIN.0 as u32)
    } else if kb.vkCode == VK_RWIN.0 as u32 {
        Some(VK_RWIN.0 as u32)
    } else {
        None
    }
}

unsafe fn foreground_is_ours() -> bool {
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    let fg = unsafe { GetForegroundWindow() };
    if fg.is_invalid() {
        return false;
    }
    let mut pid = 0u32;
    let _ = unsafe { GetWindowThreadProcessId(fg, Some(&mut pid)) };
    pid == unsafe { GetCurrentProcessId() }
}

fn send_win_key(vk: u32, wparam: usize) {
    use windows::Win32::UI::Input::KeyboardAndMouse::VK_RWIN;
    use windows::Win32::UI::WindowsAndMessaging::{WM_KEYDOWN, WM_SYSKEYDOWN};
    let key = if vk == VK_RWIN.0 as u32 { "RWin" } else { "LWin" };
    let is_down = wparam == WM_KEYDOWN as usize || wparam == WM_SYSKEYDOWN as usize;
    if let Some(tx) = HOOK_TX.lock().unwrap().as_ref() {
        let msg = if is_down {
            ControlMsg::KeyDown {
                k: key.into(),
                mods: Vec::new(),
            }
        } else {
            ControlMsg::KeyUp {
                k: key.into(),
                mods: Vec::new(),
            }
        };
        let _ = tx.send(msg);
    }
}
