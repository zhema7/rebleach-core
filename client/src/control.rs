// 被控端：控制消息 → enigo 键鼠注入 / 剪贴板 / 文件接收
// 控制消息来自 DataChannel 文本帧（JSON），由 session.rs 的 on_message 分发到这里
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use parking_lot::Mutex;
use tracing::{debug, info, warn};

use crate::protocol::ControlMsg;

/// 键鼠和剪贴板共用一个无窗口线程，避免 Tokio worker 持有窗口或钩子后无法切换桌面。
#[derive(Clone)]
enum InputCommand {
    Message(ControlMsg),
    Unlock(String),
    SetClipboard {
        text: String,
        done: mpsc::Sender<Result<()>>,
    },
    ReadClipboard(mpsc::Sender<Option<String>>),
}

/// 输入线程的发送端。线程可能因锁屏→解锁而自毁重建，所以不能是 OnceLock。
static INPUT_TX: StdMutex<Option<mpsc::Sender<InputCommand>>> = StdMutex::new(None);
/// 当前是否有存活的输入线程（线程自毁后置 false，下次提交命令时重建）
static INPUT_ALIVE: AtomicBool = AtomicBool::new(false);
/// 输入线代际：旧线程退出时只清自己代际的标记，避免覆盖新线程
static INPUT_GEN: AtomicU64 = AtomicU64::new(0);
static LAST_INPUT_WARNING: Mutex<Option<Instant>> = Mutex::new(None);

/// 取得可用的输入线程发送端；线程不存在或已退出时拉起新线程。
///
/// 必须重建而不是复用：注入线程一旦附着过 Winlogon（安全桌面），再切回 Default
/// 后 SendInput 会被系统永久拒绝（os error 5），只有新线程才能恢复注入。
fn input_sender() -> mpsc::Sender<InputCommand> {
    let mut guard = INPUT_TX.lock().unwrap_or_else(|e| e.into_inner());
    if INPUT_ALIVE.load(Ordering::SeqCst) {
        if let Some(tx) = guard.as_ref() {
            return tx.clone();
        }
    }
    let (tx, rx) = mpsc::channel();
    let generation = INPUT_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    std::thread::Builder::new()
        .name("rebleach-input".into())
        .spawn(move || input_loop(rx, generation))
        .expect("输入线程启动失败");
    INPUT_ALIVE.store(true, Ordering::SeqCst);
    *guard = Some(tx.clone());
    tx
}

/// 提交命令到输入线程；线程刚自毁时自动拉起新线程重试一次。
fn submit(cmd: InputCommand) -> std::result::Result<(), String> {
    for attempt in 0..2 {
        let tx = input_sender();
        match tx.send(cmd.clone()) {
            Ok(()) => return Ok(()),
            Err(e) => {
                // 线程已退出（多为锁屏→解锁后主动自毁）：标记死亡并重建
                INPUT_ALIVE.store(false, Ordering::SeqCst);
                if attempt == 1 {
                    return Err(e.to_string());
                }
                info!("🖥️ 注入线程已退出，重建后重试");
            }
        }
    }
    Err("提交输入命令失败".into())
}

/// 主动让输入线程在下一次提交时重建。
///
/// 用于锁屏→解锁的瞬间：旧线程附着过 Winlogon，SendInput 已失效，
/// 提前重建可避免解锁后的第一条键鼠命令被吞掉。
pub fn reset_input_thread() {
    INPUT_ALIVE.store(false, Ordering::SeqCst);
}

fn input_loop(rx: mpsc::Receiver<InputCommand>, generation: u64) {
    // 本线程是否附着过安全桌面（Winlogon）。附过之后再回到普通桌面，
    // SendInput 会被系统永久拒绝，只能让线程自毁由外部重建。
    #[cfg(windows)]
    let mut touched_secure = crate::desktop::attach_for_injection()
        .is_some_and(|d| d.eq_ignore_ascii_case("Winlogon"));
    let mut en = match Enigo::new(&Settings::default()) {
        Ok(en) => en,
        Err(e) => {
            warn!("enigo 初始化失败: {e}");
            if INPUT_GEN.load(Ordering::SeqCst) == generation {
                INPUT_ALIVE.store(false, Ordering::SeqCst);
            }
            return;
        }
    };

    while let Ok(command) = rx.recv() {
        match command {
            InputCommand::Message(msg) => {
                let Some(desk) = attach_input("键鼠注入") else {
                    continue;
                };
                #[cfg(windows)]
                if desk.eq_ignore_ascii_case("Winlogon") {
                    touched_secure = true;
                }
                #[cfg(not(windows))]
                let _ = desk;
                let desktop = target_desktop();
                if let Err(first) = apply(&mut en, msg.clone()) {
                    // 注入被拒后只重试一次：桌面若已切换就重挂新桌面，
                    // 否则退回系统真正认的输入桌面（锁屏时可能是 Screen-saver）。
                    let retried = if target_desktop() != desktop {
                        reattach_desktop()
                    } else {
                        switch_raw_desktop()
                    };
                    if retried {
                        match apply(&mut en, msg) {
                            Ok(()) => info!("⌨️ 重挂桌面后注入成功"),
                            Err(e) => {
                                warn_input_limited(format!("键鼠注入失败: {e}（首次: {first}）"))
                            }
                        }
                    } else {
                        warn_input_limited(format!("键鼠注入失败: {first}（{}）", desktop_hint()));
                    }
                }
                // 注入线程保持在当前附着的桌面（Winlogon/Default），
                // 直到下一条消息再来时重新 attach_input；不在批次间切回 Default
                // 以避免锁屏态来回附着导致窗口枚举/句柄时序问题。
            }
            InputCommand::Unlock(password) => {
                // unlock_screen 会先附到 Winlogon 再注入密码，本线程同样被"污染"
                #[cfg(windows)]
                {
                    touched_secure = true;
                }
                if let Err(e) = unlock_screen(&password) {
                    warn!("解锁失败: {e:#}");
                }
                // 解锁成功后桌面自动切回 Default，touched_secure 标记让线程自毁重建
            }
            InputCommand::SetClipboard { text, done } => {
                let result = if attach_input("写入剪贴板").is_some() {
                    set_clipboard_now(&text)
                } else {
                    Err(anyhow!("无法附着到当前输入桌面"))
                };
                let _ = done.send(result);
            }
            InputCommand::ReadClipboard(done) => {
                let text = if attach_input("读取剪贴板").is_some() {
                    get_clipboard_now()
                } else {
                    None
                };
                let _ = done.send(text);
            }
        }

        // 附过 Winlogon 且现在已回到普通桌面：本线程 SendInput 已失效，退出重建
        #[cfg(windows)]
        if touched_secure && !crate::desktop::is_secure_desktop() {
            info!("🖥️ 已从锁屏桌面回到普通桌面，注入线程自毁以便重建并恢复键鼠");
            break;
        }
    }

    if INPUT_GEN.load(Ordering::SeqCst) == generation {
        INPUT_ALIVE.store(false, Ordering::SeqCst);
    }
}

#[cfg(windows)]
fn attach_input(operation: &str) -> Option<String> {
    match crate::desktop::attach_for_injection() {
        Some(name) => Some(name),
        None => {
            warn_input_limited(format!(
                "{operation}已跳过：附着输入桌面失败（{}）",
                crate::desktop::describe()
            ));
            None
        }
    }
}

#[cfg(not(windows))]
fn attach_input(_operation: &str) -> Option<String> {
    Some("Default".into())
}

#[cfg(windows)]
fn target_desktop() -> Option<String> {
    crate::desktop::target_desktop_name()
}

#[cfg(not(windows))]
fn target_desktop() -> Option<String> {
    None
}

#[cfg(windows)]
fn reattach_desktop() -> bool {
    crate::desktop::attach_for_injection().is_some()
}

#[cfg(not(windows))]
fn reattach_desktop() -> bool {
    true
}

#[cfg(windows)]
fn switch_raw_desktop() -> bool {
    crate::desktop::switch_to_raw_input_desktop()
}

#[cfg(not(windows))]
fn switch_raw_desktop() -> bool {
    false
}

#[cfg(windows)]
fn desktop_hint() -> String {
    crate::desktop::describe()
}

#[cfg(not(windows))]
fn desktop_hint() -> String {
    String::new()
}

fn warn_input_limited(message: String) {
    let mut last = LAST_INPUT_WARNING.lock();
    if last.map_or(true, |at| at.elapsed() >= Duration::from_secs(1)) {
        warn!("{message}");
        *last = Some(Instant::now());
    }
}

// ============================================================
// 鼠标坐标映射：主控端发来的是「编码帧坐标」（≤1920 宽），
// 需要按采集缩放比放大，再叠加当前显示器在虚拟桌面中的原点
// ============================================================
/// (scale_x, scale_y, origin_x, origin_y)
static MAPPING: Mutex<(f64, f64, i32, i32)> = Mutex::new((1.0, 1.0, 0, 0));

/// 采集端切换显示器/分辨率时更新映射（video 采集线程调用）
pub fn set_mapping(scale_x: f64, scale_y: f64, origin_x: i32, origin_y: i32) {
    *MAPPING.lock() = (scale_x, scale_y, origin_x, origin_y);
}

fn map_point(x: u32, y: u32) -> (i32, i32) {
    let (sx, sy, ox, oy) = *MAPPING.lock();
    ((x as f64 * sx) as i32 + ox, (y as f64 * sy) as i32 + oy)
}

// ============================================================
// 剪贴板回环抑制：记录最近一次「本端设置/发送」的文本，
// 对端应用我们发的内容会引起本端剪贴板变化，不应再回发
// ============================================================
static LAST_CLIP: Mutex<String> = Mutex::new(String::new());

pub fn last_clip() -> String {
    LAST_CLIP.lock().clone()
}

/// 记录「本端设置/发送」的剪贴板文本（session 剪贴板轮询任务也调用，防回环）
pub fn note_clip(text: &str) {
    *LAST_CLIP.lock() = text.to_string();
}

/// 处理一条控制消息 JSON（解析失败静默忽略）
pub fn handle_control(json: &str) {
    let Some(msg) = ControlMsg::from_json(json) else {
        warn!("无法解析控制消息: {json}");
        return;
    };
    handle_msg(msg);
}

/// 处理已解析的控制消息（session 侧需要拦截 SwitchMonitor，故拆开）
pub fn handle_msg(msg: ControlMsg) {
    debug!("🖱️ 控制消息: {msg:?}");
    match msg {
        ControlMsg::Clipboard { text } => {
            note_clip(&text);
            if let Err(e) = set_clipboard(&text) {
                warn!("写入剪贴板失败: {e:#}");
            }
        }
        ControlMsg::FileOffer { name, size, chunks } => file_offer(name, size, chunks),
        ControlMsg::FileChunk { seq, data } => file_chunk(seq, &data),
        ControlMsg::FileDone => file_done(),
        ControlMsg::FileCancel => {
            *FILE_RX.lock() = None;
            info!("📁 文件传输被取消");
        }
        ControlMsg::Lock => {
            info!("🔒 主控端请求锁定桌面");
            lock_screen();
        }
        ControlMsg::Unlock { password } => {
            info!("🔓 主控端请求解锁桌面（{} 字符）", password.chars().count());
            if let Err(e) = submit(InputCommand::Unlock(password)) {
                warn!("提交解锁请求失败: {e}");
            }
        }
        ControlMsg::Power { action, delay_secs } => {
            info!("⚡ 主控端请求 {action}（{delay_secs}s）");
            power(&action, delay_secs);
        }
        ControlMsg::SetResolution { width, height } => match set_resolution(width, height) {
            Ok(msg) => info!("🖥️ {msg}"),
            Err(e) => warn!("修改分辨率失败: {e:#}"),
        },
        // 虚拟屏增删：session 侧拦截以在成功后重建采集并推新 Monitors 清单；
        // 这里作为 fallback（某些非采集路径的 session 也可能收到）
        ControlMsg::AddVirtualDisplay {
            count,
            width,
            height,
        } => match add_virtual(count, width, height) {
            Ok(_) => info!("🖥️ 已请求新增 {count} 块虚拟屏"),
            Err(e) => warn!("新增虚拟屏失败: {e:#}"),
        },
        ControlMsg::RemoveVirtualDisplay { index } => match remove_virtual(index) {
            Ok(_) => info!("🖥️ 已请求删除虚拟屏 #{index}"),
            Err(e) => warn!("删除虚拟屏失败: {e:#}"),
        },
        // Refresh / SetQuality 由 session 侧处理（需要采集句柄与统计）
        ControlMsg::Refresh | ControlMsg::SetQuality { .. } => {}
        other => {
            if let Err(e) = submit(InputCommand::Message(other)) {
                warn!("提交键鼠输入失败: {e}");
            }
        }
    }
}

fn apply(en: &mut Enigo, msg: ControlMsg) -> Result<()> {
    match msg {
        ControlMsg::MouseMove { x, y } => {
            let (px, py) = map_point(x, y);
            en.move_mouse(px, py, Coordinate::Abs)?;
        }
        ControlMsg::MouseMoveAbs { x, y } => {
            // 已经是虚拟桌面绝对物理坐标，直接注入（跨屏拖拽窗口用）
            en.move_mouse(x, y, Coordinate::Abs)?;
        }
        ControlMsg::MouseDown { b } => {
            en.button(to_button(b), Direction::Press)?;
        }
        ControlMsg::MouseUp { b } => {
            en.button(to_button(b), Direction::Release)?;
        }
        ControlMsg::Wheel { dy } => {
            en.scroll(dy.clamp(-10000, 10000), Axis::Vertical)?;
        }
        // 组合键：先按下全部修饰键，再按目标键；松开时反向
        ControlMsg::KeyDown { k, mods } => {
            for m in &mods {
                let _ = en.key(map_key(m), Direction::Press);
            }
            en.key(map_key(&k), Direction::Press)?;
        }
        ControlMsg::KeyUp { k, mods: _mods } => {
            // 只松开按键本身；修饰键等它的 key_up 再松。
            // 之前这里连 mods 一起松开 —— 按住 Shift 打字时，第一个字母的 keyup 就把 Shift
            // 放掉了，导致 Shift+点击 / Shift+拖拽 / Shift+方向键选区全部失效。
            en.key(map_key(&k), Direction::Release)?;
        }
        _ => {}
    }
    Ok(())
}

/// 鼠标按键编号 → enigo Button（0=左 1=右 2=中）
fn to_button(b: u8) -> Button {
    match b {
        1 => Button::Right,
        2 => Button::Middle,
        _ => Button::Left,
    }
}

/// 单字符 → Windows 虚拟键码。
///
/// **为什么不能直接用 `Key::Unicode`**：enigo 在 Windows 上把 `Key::Unicode(c)`
/// 翻译成 `KEYEVENTF_UNICODE` 注入，目标程序收到的是 WM_CHAR 级别的字符，
/// 拿不到虚拟键码。于是：
///   - Ctrl+C / Ctrl+V / Ctrl+A 等快捷键完全不触发
///   - Shift+字母、Ctrl+Shift+T 等组合键失效
///   - 只有普通文本输入"看起来"正常
/// 用 `VkKeyScanW` 映射成 VK 后走 `Key::Other(vk)`，与物理键盘路径一致。
/// 返回的 VK 只保留低字节，Shift/Ctrl/Alt 位丢弃——修饰键由调用方的 mods 负责按下。
#[cfg(windows)]
fn vk_from_char(c: char) -> Option<u16> {
    use windows::Win32::UI::Input::KeyboardAndMouse::VkKeyScanW;
    let r = unsafe { VkKeyScanW(c as u16) };
    if r == -1 {
        None
    } else {
        Some((r as u16) & 0x00FF)
    }
}

#[cfg(not(windows))]
fn vk_from_char(c: char) -> Option<u16> {
    let c = c.to_ascii_lowercase();
    if c.is_ascii_alphabetic() {
        return Some(0x41 + (c as u8 - b'a') as u16);
    }
    if c.is_ascii_digit() {
        return Some(0x30 + (c as u8 - b'0') as u16);
    }
    if c == ' ' {
        return Some(0x20);
    }
    None
}

/// 协议键名 → enigo Key
/// 协议侧（app.js keyName）：单字符原样 / 命名键 Enter/Backspace/Tab/Escape/Space/ArrowUp.../F1..F12/Shift/Control/Alt/Meta
fn map_key(k: &str) -> Key {
    // 单字符（含 " " 空格）
    if k.chars().count() == 1 {
        let c = k.chars().next().unwrap();
        // ASCII 可打印字符 → 虚拟键码，保证组合快捷键生效
        if c.is_ascii() && !c.is_ascii_control() {
            if let Some(vk) = vk_from_char(c) {
                return Key::Other(vk as u32);
            }
        }
        return Key::Unicode(c);
    }
    // 功能键 F1..F12（enigo 0.6 为显式变体）
    match k {
        "F1" => return Key::F1,
        "F2" => return Key::F2,
        "F3" => return Key::F3,
        "F4" => return Key::F4,
        "F5" => return Key::F5,
        "F6" => return Key::F6,
        "F7" => return Key::F7,
        "F8" => return Key::F8,
        "F9" => return Key::F9,
        "F10" => return Key::F10,
        "F11" => return Key::F11,
        "F12" => return Key::F12,
        _ => {}
    }
    match k {
        "Enter" => Key::Return,
        "Backspace" => Key::Backspace,
        "Tab" => Key::Tab,
        "Escape" => Key::Escape,
        "Space" => Key::Space,
        "ArrowUp" => Key::UpArrow,
        "ArrowDown" => Key::DownArrow,
        "ArrowLeft" => Key::LeftArrow,
        "ArrowRight" => Key::RightArrow,
        "Home" => Key::Home,
        "End" => Key::End,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        "Delete" => Key::Delete,
        "Insert" => Key::Insert,
        "Shift" => Key::Shift,
        "Control" => Key::Control,
        "Alt" => Key::Alt,
        // Win 键：主控端 WebView 收不到系统 Win 键时由原生钩子发送 "LWin"/"RWin"
        "Meta" | "LWin" => Key::LWin,
        "RWin" => Key::RWin,
        other => {
            // 兜底：取首字符
            Key::Unicode(other.chars().next().unwrap_or(' '))
        }
    }
}

// ============================================================
// 剪贴板统一经输入线程访问，确保 OpenClipboard 位于当前输入桌面。
// ============================================================
pub fn set_clipboard(text: &str) -> Result<()> {
    let (done_tx, done_rx) = mpsc::channel();
    submit(InputCommand::SetClipboard {
        text: text.to_string(),
        done: done_tx,
    })
    .map_err(|e| anyhow!(e))
    .context("提交剪贴板写入失败")?;
    done_rx
        .recv_timeout(Duration::from_secs(2))
        .context("等待剪贴板写入超时")?
}

pub fn get_clipboard() -> Option<String> {
    let (done_tx, done_rx) = mpsc::channel();
    submit(InputCommand::ReadClipboard(done_tx)).ok()?;
    done_rx.recv_timeout(Duration::from_secs(2)).ok()?
}

#[cfg(windows)]
fn set_clipboard_now(text: &str) -> Result<()> {
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
    use windows::Win32::System::Ole::CF_UNICODETEXT;
    unsafe {
        OpenClipboard(None).context("OpenClipboard 失败")?;
        let _ = EmptyClipboard();
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let h = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2).context("GlobalAlloc 失败")?;
        let p = GlobalLock(h);
        if p.is_null() {
            let _ = CloseClipboard();
            return Err(anyhow!("GlobalLock 失败"));
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), p as *mut u16, wide.len());
        let _ = GlobalUnlock(h);
        // 成功后内存所有权移交系统
        let r = SetClipboardData(
            CF_UNICODETEXT.0 as u32,
            Some(windows::Win32::Foundation::HANDLE(h.0)),
        );
        if let Err(e) = r {
            let _ = CloseClipboard();
            return Err(e).context("SetClipboardData 失败");
        }
        let _ = CloseClipboard();
    }
    Ok(())
}

#[cfg(windows)]
fn get_clipboard_now() -> Option<String> {
    use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::System::Ole::CF_UNICODETEXT;
    unsafe {
        OpenClipboard(None).ok()?;
        let h = GetClipboardData(CF_UNICODETEXT.0 as u32).ok();
        let out = h.and_then(|h| {
            let hg = windows::Win32::Foundation::HGLOBAL(h.0);
            let p = GlobalLock(hg);
            if p.is_null() {
                return None;
            }
            let size = GlobalSize(hg);
            let max = size / 2;
            let slice = std::slice::from_raw_parts(p as *const u16, max);
            let len = slice.iter().position(|&c| c == 0).unwrap_or(max);
            let s = String::from_utf16_lossy(&slice[..len]);
            let _ = GlobalUnlock(hg);
            Some(s)
        });
        let _ = CloseClipboard();
        out
    }
}

#[cfg(not(windows))]
fn set_clipboard_now(_text: &str) -> Result<()> {
    Ok(())
}

#[cfg(not(windows))]
fn get_clipboard_now() -> Option<String> {
    None
}

/// 剪贴板序列号：内容变化时递增（轮询检测用）
#[cfg(windows)]
pub fn clipboard_seq() -> u32 {
    unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() }
}

#[cfg(not(windows))]
pub fn clipboard_seq() -> u32 {
    0
}

// ============================================================
// 文件接收（主控端 → 被控端，JSON + base64 分块，落盘 Downloads/rebleach）
// ============================================================
struct FileRx {
    name: String,
    size: u64,
    chunks: u32,
    buf: Vec<u8>,
    next_seq: u32,
}

static FILE_RX: Mutex<Option<FileRx>> = Mutex::new(None);

fn file_offer(name: String, size: u64, chunks: u32) {
    // 只保留文件名部分，防目录穿越
    let name = std::path::Path::new(&name)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty());
    let Some(name) = name else {
        warn!("📁 非法文件名，拒绝接收");
        return;
    };
    info!("📁 接收文件 {name}（{size} bytes / {chunks} 块）");
    *FILE_RX.lock() = Some(FileRx {
        name,
        size,
        chunks,
        buf: Vec::with_capacity(size as usize),
        next_seq: 0,
    });
}

fn file_chunk(seq: u32, data_b64: &str) {
    let mut guard = FILE_RX.lock();
    let Some(rx) = guard.as_mut() else { return };
    if seq != rx.next_seq {
        warn!("📁 块序号乱序 {seq} != {}，丢弃本次传输", rx.next_seq);
        *guard = None;
        return;
    }
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data_b64) else {
        warn!("📁 base64 解码失败，丢弃本次传输");
        *guard = None;
        return;
    };
    rx.buf.extend_from_slice(&bytes);
    rx.next_seq += 1;
}

fn file_done() {
    let Some(rx) = FILE_RX.lock().take() else {
        return;
    };
    if rx.next_seq != rx.chunks || rx.buf.len() as u64 != rx.size {
        warn!(
            "📁 传输不完整（{}/{} 块, {}/{} bytes），不落盘",
            rx.next_seq,
            rx.chunks,
            rx.buf.len(),
            rx.size
        );
        return;
    }
    let dir = std::env::var("USERPROFILE")
        .map(|p| std::path::PathBuf::from(p).join("Downloads"))
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("rebleach");
    std::fs::create_dir_all(&dir).ok();
    // 同名不覆盖：追加 (1)(2)...
    let mut path = dir.join(&rx.name);
    for i in 1..100 {
        if !path.exists() {
            break;
        }
        let stem = std::path::Path::new(&rx.name)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let ext = std::path::Path::new(&rx.name)
            .extension()
            .map(|s| format!(".{}", s.to_string_lossy()))
            .unwrap_or_default();
        path = dir.join(format!("{stem}({i}){ext}"));
    }
    match std::fs::write(&path, &rx.buf) {
        Ok(()) => info!("📁 已保存 {}（{} bytes）", path.display(), rx.buf.len()),
        Err(e) => warn!("📁 写入失败: {e}"),
    }
}

// ============================================================
// 桌面电源/锁定/分辨率（对应 RustDesk 主控端“控制操作/显示设置”）
// ============================================================
pub fn lock_screen() {
    // LockWorkStation 需要交互式会话；用 rundll32 调用可避免额外 windows feature
    match std::process::Command::new("rundll32.exe")
        .args(["user32.dll,LockWorkStation"])
        .spawn()
    {
        Ok(_) => info!("🔒 已锁定被控端桌面"),
        Err(e) => warn!("锁定桌面失败: {e}"),
    }
}

/// 虚拟键码 → 是否需要 Shift，用于把密码字符按"手动按键"的方式注入。
///
/// 为什么不用 KEYEVENTF_UNICODE：LogonUI 的密码框会丢弃 UNICODE 注入的字符
/// （主控端手动按键盘走的是虚拟键码，所以能输入）。密码注入必须走同一条路径。
/// 表按美式布局映射，字母/数字与布局无关；符号按美式布局，非 ASCII 才退回 UNICODE。
#[cfg(windows)]
fn vk_for_char(ch: char) -> Option<(u16, bool)> {
    const SHIFT: bool = true;
    Some(match ch {
        'a'..='z' => (0x41 + (ch as u8 - b'a') as u16, false),
        'A'..='Z' => (0x41 + (ch as u8 - b'A') as u16, SHIFT),
        '0'..='9' => (0x30 + (ch as u8 - b'0') as u16, false),
        ' ' => (0x20, false),
        '-' => (0xBD, false),
        '_' => (0xBD, SHIFT),
        '=' => (0xBB, false),
        '+' => (0xBB, SHIFT),
        '[' => (0xDB, false),
        '{' => (0xDB, SHIFT),
        ']' => (0xDD, false),
        '}' => (0xDD, SHIFT),
        '\\' => (0xDC, false),
        '|' => (0xDC, SHIFT),
        ';' => (0xBA, false),
        ':' => (0xBA, SHIFT),
        '\'' => (0xDE, false),
        '"' => (0xDE, SHIFT),
        ',' => (0xBC, false),
        '<' => (0xBC, SHIFT),
        '.' => (0xBE, false),
        '>' => (0xBE, SHIFT),
        '/' => (0xBF, false),
        '?' => (0xBF, SHIFT),
        '`' => (0xC0, false),
        '~' => (0xC0, SHIFT),
        '!' => (0x31, SHIFT),
        '@' => (0x32, SHIFT),
        '#' => (0x33, SHIFT),
        '$' => (0x34, SHIFT),
        '%' => (0x35, SHIFT),
        '^' => (0x36, SHIFT),
        '&' => (0x37, SHIFT),
        '*' => (0x38, SHIFT),
        '(' => (0x39, SHIFT),
        ')' => (0x30, SHIFT),
        _ => return None,
    })
}

/// 解锁被控端锁屏：附着输入桌面 →（必要时）SAS 唤醒 → 按虚拟键码逐字输入密码 → Enter
///
/// 需要托盘具备 Winlogon 桌面访问权限（SYSTEM 或 winlogon.exe 令牌派生）。
/// 普通用户态进程调用会失败并返回 Err（由调用方记录）。
#[cfg(windows)]
pub fn unlock_screen(password: &str) -> Result<()> {
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, SendInput,
        VIRTUAL_KEY,
    };
    use windows::core::{BOOL, PCWSTR};

    const VK_RETURN: u16 = 0x0D;
    const VK_SHIFT: u16 = 0x10;
    const VK_CONTROL: u16 = 0x11;
    const VK_A: u16 = 0x41;
    const VK_DELETE: u16 = 0x2E;

    fn push_key(inputs: &mut Vec<INPUT>, vk: u16, up: bool) {
        inputs.push(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: 0,
                    dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        });
    }

    fn push_unicode(inputs: &mut Vec<INPUT>, code: u16, up: bool) {
        inputs.push(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: code,
                    dwFlags: KEYEVENTF_UNICODE
                        | if up { KEYEVENTF_KEYUP } else { Default::default() },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        });
    }

    fn send(inputs: &[INPUT]) -> Result<()> {
        let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent as usize != inputs.len() {
            return Err(anyhow!(
                "SendInput {sent}/{} 条：{}（{}）",
                inputs.len(),
                std::io::Error::last_os_error(),
                crate::desktop::describe()
            ));
        }
        Ok(())
    }

    fn press_vk(vk: u16) -> Result<()> {
        let mut v = Vec::with_capacity(2);
        push_key(&mut v, vk, false);
        push_key(&mut v, vk, true);
        send(&v)
    }

    fn type_char(ch: char) -> Result<()> {
        match vk_for_char(ch) {
            Some((vk, shift)) => {
                let mut v = Vec::with_capacity(if shift { 4 } else { 2 });
                if shift {
                    push_key(&mut v, VK_SHIFT, false);
                }
                push_key(&mut v, vk, false);
                push_key(&mut v, vk, true);
                if shift {
                    push_key(&mut v, VK_SHIFT, true);
                }
                send(&v)
            }
            None => {
                // 非 ASCII：只有 UNICODE 才能表达，退回 UNICODE 注入
                let code = ch as u32 as u16;
                let mut v = Vec::with_capacity(2);
                push_unicode(&mut v, code, false);
                push_unicode(&mut v, code, true);
                send(&v)
            }
        }
    }

    unsafe {
        // 1) 附着到系统真正接收输入的桌面：锁屏凭据页在 Winlogon，锁屏遮罩/解锁态在 Default。
        //    禁止在此按桌面名中止：遮罩层阶段输入桌面就是 Default，中止会让"解锁"永远失效。
        let desk = crate::desktop::attach_for_injection()
            .ok_or_else(|| anyhow!("无法附着到输入桌面（{}）", crate::desktop::describe()))?;
        info!("🔓 解锁开始：已附着 {desk}（{}）", crate::desktop::describe());

        // 2) 只在不是 Winlogon 时才送 SAS 唤醒凭据界面（遮罩层 → 密码页）。
        //    已经在密码页时再送 SAS 会重置登录界面、丢掉输入焦点。
        if !desk.eq_ignore_ascii_case("Winlogon") {
            let sas_path: Vec<u16> = "sas.dll\0".encode_utf16().collect();
            match LoadLibraryW(PCWSTR(sas_path.as_ptr())) {
                Ok(hmod) => {
                    if let Some(proc) = GetProcAddress(hmod, windows::core::s!("SendSAS")) {
                        type SendSasFn = unsafe extern "system" fn(BOOL);
                        let send_sas: SendSasFn = std::mem::transmute(proc);
                        send_sas(false.into());
                    }
                }
                Err(e) => warn!("🔓 LoadLibrary sas.dll 失败: {e}"),
            }
            std::thread::sleep(std::time::Duration::from_millis(900));
            match crate::desktop::attach_for_injection() {
                Some(after) => info!("🔓 SAS 后附着 {after}"),
                None => warn!("🔓 SAS 后无法附着输入桌面"),
            }
        }

        // 3) 清空残留输入，避免上次失败的内容叠加。密码框支持 Ctrl+A + Delete。
        {
            let mut v = Vec::with_capacity(4);
            push_key(&mut v, VK_CONTROL, false);
            push_key(&mut v, VK_A, false);
            push_key(&mut v, VK_A, true);
            push_key(&mut v, VK_CONTROL, true);
            let _ = send(&v);
            std::thread::sleep(std::time::Duration::from_millis(80));
            let _ = press_vk(VK_DELETE);
            std::thread::sleep(std::time::Duration::from_millis(80));
        }

        // 4) 逐字符按虚拟键码注入（与手动输入同一条路径），留出 LogonUI 处理时间
        let mut typed = 0usize;
        for ch in password.chars() {
            type_char(ch)?;
            typed += 1;
            std::thread::sleep(std::time::Duration::from_millis(12));
        }

        // 5) 回车提交
        std::thread::sleep(std::time::Duration::from_millis(60));
        press_vk(VK_RETURN)?;
        info!("🔓 已注入 {typed} 个字符 + Enter，等待解锁");

        // 6) 观察是否真的解锁：成功后输入桌面会切回 Default
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let after = crate::desktop::input_desktop_name().unwrap_or_else(|| "?".into());
        if after.eq_ignore_ascii_case("Winlogon") {
            warn!("🔓 注入完成但输入桌面仍是 Winlogon（密码错误或凭据页未提交）");
        } else {
            info!("🔓 解锁成功，输入桌面已切到 {after}");
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn unlock_screen(_password: &str) -> Result<()> {
    Err(anyhow!("仅 Windows 支持"))
}

pub fn power(action: &str, delay_secs: u32) {
    let flag = match action {
        "restart" | "reboot" => "/r",
        "shutdown" => "/s",
        _ => {
            warn!("未知电源操作: {action}");
            return;
        }
    };
    match std::process::Command::new("shutdown.exe")
        .args([flag, "/t", &delay_secs.to_string(), "/f"])
        .spawn()
    {
        Ok(_) => info!("⚡ 已下发 {action}（{delay_secs}s 后）"),
        Err(e) => warn!("下发电源操作失败: {e}"),
    }
}

/// 每个显示器首次被修改前的原始分辨率（设备名 → (宽, 高)）。
/// 会话结束 / Agent 启动时用 restore_display_modes() 全部还原，避免降级档位残留（文档 P1-C）
static ORIGINAL_MODES: std::sync::OnceLock<Mutex<std::collections::HashMap<String, (u32, u32)>>> =
    std::sync::OnceLock::new();

fn original_modes() -> &'static Mutex<std::collections::HashMap<String, (u32, u32)>> {
    ORIGINAL_MODES.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// 当前正在被主控端观看的显示器设备名（\\.\DISPLAYn）。
/// 修改分辨率时优先作用于它，而不是永远改主屏
static ACTIVE_MONITOR: Mutex<Option<String>> = Mutex::new(None);

/// 采集线程切换显示器时同步：设备名 + 虚拟桌面原点 + 物理尺寸
pub fn set_active_monitor(name: &str, origin_x: i32, origin_y: i32, w: u32, h: u32) {
    *ACTIVE_MONITOR.lock() = Some(name.to_string());
    #[cfg(windows)]
    crate::window_placement::set_target(origin_x, origin_y, w as i32, h as i32);
}

fn device_name_ptr(name: &str) -> Vec<u16> {
    let mut buf: Vec<u16> = name.encode_utf16().collect();
    while buf.len() < 32 {
        buf.push(0);
    }
    buf.truncate(32);
    buf[31] = 0;
    buf
}

/// 在指定显示器上查找分辨率匹配且刷新率最高的模式
fn find_mode(name: &[u16], want_w: u32, want_h: u32) -> Option<(u32, u32, u32)> {
    use windows::Win32::Graphics::Gdi::*;
    use windows::core::PCWSTR;
    unsafe {
        let dev_name = PCWSTR(name.as_ptr());
        let mut best: Option<(u32, u32, u32)> = None;
        let mut i = 0u32;
        loop {
            let mut dm = DEVMODEW {
                dmSize: std::mem::size_of::<DEVMODEW>() as u16,
                ..Default::default()
            };
            if !EnumDisplaySettingsW(dev_name, ENUM_DISPLAY_SETTINGS_MODE(i), &mut dm).as_bool() {
                break;
            }
            if dm.dmPelsWidth == want_w && dm.dmPelsHeight == want_h {
                if best.map(|b| dm.dmDisplayFrequency > b.2).unwrap_or(true) {
                    best = Some((dm.dmPelsWidth, dm.dmPelsHeight, dm.dmDisplayFrequency));
                }
            }
            i += 1;
        }
        best
    }
}

fn apply_mode(name: &[u16], want_w: u32, want_h: u32) -> Result<()> {
    use windows::Win32::Graphics::Gdi::*;
    use windows::core::PCWSTR;
    unsafe {
        let dev_name = PCWSTR(name.as_ptr());
        let mode = find_mode(name, want_w, want_h)
            .ok_or_else(|| anyhow!("显示器不支持 {want_w}x{want_h}"))?;
        // 重新枚举并取匹配项，构造 DEVMODEW
        let mut best: Option<DEVMODEW> = None;
        let mut i = 0u32;
        loop {
            let mut dm = DEVMODEW {
                dmSize: std::mem::size_of::<DEVMODEW>() as u16,
                ..Default::default()
            };
            if !EnumDisplaySettingsW(dev_name, ENUM_DISPLAY_SETTINGS_MODE(i), &mut dm).as_bool() {
                break;
            }
            if dm.dmPelsWidth == mode.0 && dm.dmPelsHeight == mode.1 {
                if best
                    .as_ref()
                    .map(|b| dm.dmDisplayFrequency >= b.dmDisplayFrequency)
                    .unwrap_or(true)
                {
                    best = Some(dm);
                }
            }
            i += 1;
        }
        let mut dm = best.ok_or_else(|| anyhow!("显示器不支持 {want_w}x{want_h}"))?;
        dm.dmFields =
            DEVMODE_FIELD_FLAGS(DM_PELSWIDTH.0 | DM_PELSHEIGHT.0 | DM_DISPLAYFREQUENCY.0);
        let rc = ChangeDisplaySettingsExW(dev_name, Some(&dm), None, CDS_UPDATEREGISTRY, None);
        if rc == DISP_CHANGE_SUCCESSFUL {
            Ok(())
        } else {
            Err(anyhow!("切换分辨率失败: {rc:?}"))
        }
    }
}

/// 修改「当前观看的显示器」分辨率：选择最接近的受支持模式。
/// `width/height` 为 0 时恢复该显示器首次修改前的模式。
fn set_resolution(width: u32, height: u32) -> Result<String> {
    use windows::Win32::Graphics::Gdi::*;
    use windows::core::PCWSTR;

    unsafe {
        // 优先使用当前采集的显示器；没有则退回主显示器
        let mut name: Option<String> = ACTIVE_MONITOR.lock().clone().filter(|n| !n.is_empty());
        if name.is_none() {
            let mut dev = DISPLAY_DEVICEW {
                cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            let mut idx = 0u32;
            while EnumDisplayDevicesW(PCWSTR::null(), idx, &mut dev, 0).as_bool() {
                if dev.StateFlags.contains(DISPLAY_DEVICE_PRIMARY_DEVICE) {
                    name = Some(String::from_utf16_lossy(
                        &dev.DeviceName[..dev.DeviceName.iter().position(|&c| c == 0).unwrap_or(32)],
                    ));
                    break;
                }
                idx += 1;
            }
        }
        let name = name.ok_or_else(|| anyhow!("未找到可用显示器"))?;
        let buf = device_name_ptr(&name);
        let dev_name = PCWSTR(buf.as_ptr());

        let mut cur = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        if !EnumDisplaySettingsW(dev_name, ENUM_CURRENT_SETTINGS, &mut cur).as_bool() {
            return Err(anyhow!("读取当前分辨率失败"));
        }
        let current = (cur.dmPelsWidth, cur.dmPelsHeight);
        // 记录原始模式（仅第一次，按设备分别记录）
        {
            let mut orig = original_modes().lock();
            orig.entry(name.clone()).or_insert(current);
        }
        let (want_w, want_h) = if width == 0 || height == 0 {
            original_modes().lock().get(&name).copied().unwrap_or(current)
        } else {
            (width, height)
        };

        apply_mode(&buf, want_w, want_h)?;
        Ok(format!("{name} 分辨率已切换为 {want_w}x{want_h}"))
    }
}

/// 还原所有被修改过的显示器分辨率。会话结束 / Agent 退出时调用，
/// 避免下一次会话或本机使用仍停留在 1280x720（文档 P1-C）
pub fn restore_display_modes() {
    let saved: Vec<(String, (u32, u32))> = {
        let mut orig = original_modes().lock();
        if orig.is_empty() {
            return;
        }
        let all = orig.iter().map(|(k, v)| (k.clone(), *v)).collect();
        orig.clear();
        all
    };
    for (name, (w, h)) in saved {
        let buf = device_name_ptr(&name);
        match apply_mode(&buf, w, h) {
            Ok(()) => info!("🖥️ 已恢复 {name} 原始分辨率 {w}x{h}"),
            Err(e) => warn!("🖥️ 恢复 {name} 分辨率 {w}x{h} 失败: {e:#}"),
        }
    }
}

// ============================================================
// 虚拟屏增删 handler 注册
// ============================================================
// client 库（本文件）和 app bin（tauri）分层：虚拟屏驱动管理函数都在 app bin 的
// virtual_display.rs 里，client 库只负责协议解析。app bin 启动时调一次
// register_virtual_handlers 把真实实现注入进来。
#[cfg(windows)]
static mut ADD_VIRTUAL: Option<fn(u32, u32, u32) -> Result<()>> = None;
#[cfg(windows)]
static mut REMOVE_VIRTUAL: Option<fn(u32) -> Result<()>> = None;
/// 返回 (has_driver, driver_kind_label, hint)
#[cfg(windows)]
static mut DETECT_VIRTUAL_STATUS: Option<fn() -> (bool, String, String)> = None;

/// 注册虚拟屏增删 + 驱动状态函数指针。在 app bin 的托盘启动时调用一次即可。
#[cfg(windows)]
pub fn register_virtual_handlers(
    add: fn(u32, u32, u32) -> Result<()>,
    remove: fn(u32) -> Result<()>,
    detect: fn() -> (bool, String, String),
) {
    unsafe {
        ADD_VIRTUAL = Some(add);
        REMOVE_VIRTUAL = Some(remove);
        DETECT_VIRTUAL_STATUS = Some(detect);
    }
}

/// 查询驱动是否存在、驱动类型标签、以及给用户的安装指引。
pub fn detect_virtual_status() -> (bool, String, String) {
    #[cfg(windows)]
    {
        unsafe {
            DETECT_VIRTUAL_STATUS
                .map(|f| f())
                .unwrap_or_else(|| (false, "none".into(), "虚拟屏功能未启用：register_virtual_handlers 未调用".into()))
        }
    }
    #[cfg(not(windows))]
    {
        (false, "none".into(), "虚拟屏功能仅在 Windows 上可用".into())
    }
}

/// 新增 count 块虚拟屏（每块 width×height）。未注册 handler 时返回明确错误。
pub fn add_virtual(count: u32, width: u32, height: u32) -> Result<()> {
    #[cfg(windows)]
    {
        let f = unsafe { ADD_VIRTUAL };
        f.ok_or_else(|| anyhow!("虚拟屏功能未启用：请在安装目录的 virtual-display 子目录运行 Install-VirtualDisplay.ps1 安装驱动后重启，或检查 virtual_display::register_virtual_handlers 是否已调用"))
            .and_then(|f| f(count, width, height))
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!("虚拟屏功能仅在 Windows 上可用")
    }
}

/// 删除指定 index 的虚拟屏。index 取自主控端发来的 MonitorDesc.index。
pub fn remove_virtual(index: u32) -> Result<()> {
    #[cfg(windows)]
    {
        let f = unsafe { REMOVE_VIRTUAL };
        f.ok_or_else(|| anyhow!("虚拟屏功能未启用：请先安装虚拟显示驱动并重启"))
            .and_then(|f| f(index))
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!("虚拟屏功能仅在 Windows 上可用")
    }
}
