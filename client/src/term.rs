//! 终端通道（term DataChannel）协议与 PTY 宿主。
//!
//! 帧格式：`[type: u8][sid: u16 BE][payload]`
//!
//! | type | 名称 | payload |
//! |------|------|---------|
//! | 0x01 | OPEN | JSON `{ shell, cols, rows, cwd }` |
//! | 0x02 | DATA | UTF-8 文本（含 ANSI 转义） |
//! | 0x03 | RESIZE | cols(u16 BE) + rows(u16 BE) |
//! | 0x04 | EXIT | code(i32 BE) |
//! | 0x05 | ERR | UTF-8 错误文本 |
//!
//! 一个会话只建一条 `term` DataChannel，用 `sid` 复用多个终端标签页。
//! 输出侧发送前做一次 UTF-8 流式清洗：非法字节替换为 U+FFFD，跨块被截断的
//! 多字节序列留到下一块，保证前端 xterm.js 永远只收到合法字符串。

use serde::{Deserialize, Serialize};

pub const TYPE_OPEN: u8 = 0x01;
pub const TYPE_DATA: u8 = 0x02;
pub const TYPE_RESIZE: u8 = 0x03;
pub const TYPE_EXIT: u8 = 0x04;
pub const TYPE_ERR: u8 = 0x05;

/// 单条 DATA 的最大字节数（大输出切片，避免打爆 SCTP 缓冲）
pub const MAX_CHUNK: usize = 4096;

#[derive(Debug, Clone, PartialEq)]
pub enum TermMsg {
    Open {
        sid: u16,
        shell: String,
        cols: u16,
        rows: u16,
        cwd: Option<String>,
    },
    Data {
        sid: u16,
        data: Vec<u8>,
    },
    Resize {
        sid: u16,
        cols: u16,
        rows: u16,
    },
    Exit {
        sid: u16,
        code: i32,
    },
    Err {
        sid: u16,
        msg: String,
    },
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct OpenPayload {
    #[serde(default)]
    shell: String,
    #[serde(default)]
    cols: u16,
    #[serde(default)]
    rows: u16,
    #[serde(default)]
    cwd: Option<String>,
}

impl TermMsg {
    pub fn sid(&self) -> u16 {
        match self {
            TermMsg::Open { sid, .. }
            | TermMsg::Data { sid, .. }
            | TermMsg::Resize { sid, .. }
            | TermMsg::Exit { sid, .. }
            | TermMsg::Err { sid, .. } => *sid,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let (ty, sid, payload): (u8, u16, Vec<u8>) = match self {
            TermMsg::Open {
                sid,
                shell,
                cols,
                rows,
                cwd,
            } => (
                TYPE_OPEN,
                *sid,
                serde_json::to_vec(&OpenPayload {
                    shell: shell.clone(),
                    cols: *cols,
                    rows: *rows,
                    cwd: cwd.clone(),
                })
                .unwrap_or_default(),
            ),
            TermMsg::Data { sid, data } => (TYPE_DATA, *sid, data.clone()),
            TermMsg::Resize { sid, cols, rows } => {
                let mut p = Vec::with_capacity(4);
                p.extend_from_slice(&cols.to_be_bytes());
                p.extend_from_slice(&rows.to_be_bytes());
                (TYPE_RESIZE, *sid, p)
            }
            TermMsg::Exit { sid, code } => (TYPE_EXIT, *sid, code.to_be_bytes().to_vec()),
            TermMsg::Err { sid, msg } => (TYPE_ERR, *sid, msg.as_bytes().to_vec()),
        };
        let mut out = Vec::with_capacity(payload.len() + 3);
        out.push(ty);
        out.extend_from_slice(&sid.to_be_bytes());
        out.extend_from_slice(&payload);
        out
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < 3 {
            return None;
        }
        let ty = buf[0];
        let sid = u16::from_be_bytes([buf[1], buf[2]]);
        let p = &buf[3..];
        Some(match ty {
            TYPE_OPEN => {
                let o: OpenPayload = serde_json::from_slice(p).ok()?;
                TermMsg::Open {
                    sid,
                    shell: o.shell,
                    cols: o.cols,
                    rows: o.rows,
                    cwd: o.cwd,
                }
            }
            TYPE_DATA => TermMsg::Data {
                sid,
                data: p.to_vec(),
            },
            TYPE_RESIZE => {
                if p.len() < 4 {
                    return None;
                }
                TermMsg::Resize {
                    sid,
                    cols: u16::from_be_bytes([p[0], p[1]]),
                    rows: u16::from_be_bytes([p[2], p[3]]),
                }
            }
            TYPE_EXIT => {
                if p.len() < 4 {
                    return None;
                }
                TermMsg::Exit {
                    sid,
                    code: i32::from_be_bytes([p[0], p[1], p[2], p[3]]),
                }
            }
            TYPE_ERR => TermMsg::Err {
                sid,
                msg: String::from_utf8_lossy(p).to_string(),
            },
            _ => return None,
        })
    }
}

/// 按字符边界把文本切成 ≤ MAX_CHUNK 字节的片段。
pub fn split_text(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        if rest.len() <= MAX_CHUNK {
            out.push(rest.to_string());
            break;
        }
        let mut cut = MAX_CHUNK;
        while cut > 0 && !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        if cut == 0 {
            // 理论上不会发生（4KB 远大于 4 字节上限），兜底防死循环
            cut = MAX_CHUNK;
        }
        out.push(rest[..cut].to_string());
        rest = &rest[cut..];
    }
    out
}

/// 流式 UTF-8 清洗器：非法字节 → U+FFFD，尾部不完整序列缓存到下一次。
#[derive(Default)]
pub struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut out = String::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(s) => {
                    out.push_str(s);
                    self.pending.clear();
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid > 0 {
                        // 已校验过，安全
                        out.push_str(std::str::from_utf8(&self.pending[..valid]).unwrap());
                    }
                    match e.error_len() {
                        Some(n) => {
                            out.push('\u{FFFD}');
                            self.pending.drain(..valid + n);
                        }
                        None => {
                            // 尾部是被截断的多字节序列，保留等下一块
                            self.pending.drain(..valid);
                            break;
                        }
                    }
                }
            }
        }
        // 异常输入兜底：避免缓存无限增长
        if self.pending.len() > 8 {
            out.push('\u{FFFD}');
            self.pending.clear();
        }
        out
    }
}

// ============================================================
// 被控端 PTY 宿主
// ============================================================

#[cfg(all(feature = "desktop", windows))]
pub use host::TermHost;

#[cfg(all(feature = "desktop", windows))]
use std::sync::Arc;

/// 供 CLI / 内置 SSH 服务端直接使用的 PTY 句柄（不经 DataChannel）。
#[cfg(all(feature = "desktop", windows))]
pub struct PtyRun {
    /// 标准输入（字节流，UTF-8 编码由调用方保证）
    pub input: std::sync::mpsc::Sender<Vec<u8>>,
    hpc_raw: usize,
    kill: Option<Box<dyn FnOnce() + Send>>,
}

#[cfg(all(feature = "desktop", windows))]
impl PtyRun {
    /// ConPTY 尺寸调整（管道模式无 PTY，忽略）
    pub fn resize(&self, cols: u16, rows: u16) {
        if self.hpc_raw != 0 {
            use windows::Win32::System::Console::{
                COORD, ResizePseudoConsole,
            };
            let hpc = windows::Win32::System::Console::HPCON(self.hpc_raw as isize);
            unsafe {
                let _ = ResizePseudoConsole(
                    hpc,
                    COORD {
                        X: cols as i16,
                        Y: rows as i16,
                    },
                );
            }
        }
    }

    /// 终止进程并回收 PTY
    pub fn kill(mut self) {
        if let Some(f) = self.kill.take() {
            f();
        }
    }
}

/// 起一个交互式 shell（cmd/powershell/pwsh），供内置 SSH 服务端使用。
#[cfg(all(feature = "desktop", windows))]
pub fn spawn_interactive(
    shell: &str,
    cols: u16,
    rows: u16,
    cwd: Option<&str>,
    on_out: Arc<dyn Fn(String) + Send + Sync>,
    on_exit: Arc<dyn Fn(i32) + Send + Sync>,
) -> anyhow::Result<PtyRun> {
    let (app, conpty_cmd, pipe_args) = host::shell_cmd_pub(shell);
    let p = host::spawn_pty_pub(&app, &conpty_cmd, &pipe_args, cols, rows, cwd, on_out, on_exit)?;
    Ok(pty_run_from_proc(p))
}

/// 一次性命令执行（exec 语义）：无 PTY、干净输出、真实退出码。
/// `command` 是完整命令行，由被控端 cmd 解析（支持管道/重定向/引号）。
#[cfg(all(feature = "desktop", windows))]
pub fn spawn_exec(
    command: &str,
    cwd: Option<&str>,
    on_out: Arc<dyn Fn(String) + Send + Sync>,
    on_exit: Arc<dyn Fn(i32) + Send + Sync>,
) -> anyhow::Result<PtyRun> {
    let cmdline = host::exec_cmdline(command);
    let p = unsafe { host::spawn_exec_raw(&cmdline, cwd, on_out, on_exit) }?;
    Ok(pty_run_from_proc(p))
}

#[cfg(all(feature = "desktop", windows))]
fn pty_run_from_proc(p: host::TermProc) -> PtyRun {
    let hpc_raw = p.hpc_raw();
    PtyRun {
        input: p.input.clone(),
        hpc_raw,
        kill: Some(Box::new(move || drop(p))),
    }
}

#[cfg(all(feature = "desktop", windows))]
mod host {
    use super::{TermMsg, Utf8Stream, split_text};
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::io::{Read, Write};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
    use std::sync::Arc;
    use std::sync::mpsc::{Sender, channel};

    use parking_lot::Mutex;
    use tracing::{info, warn};
    use webrtc::data_channel::RTCDataChannel;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows::Win32::System::Console::{
        COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
    };
    use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
    use windows::Win32::System::Pipes::CreatePipe;
    use windows::Win32::System::RemoteDesktop::{
        ProcessIdToSessionId, WTSGetActiveConsoleSessionId, WTSQueryUserToken,
    };
    use windows::Win32::System::Threading::{
        CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, CreateProcessW,
        DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcessId,
        GetExitCodeProcess, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
        PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTF_USESTDHANDLES,
        STARTUPINFOEXW, STARTUPINFOW, TerminateProcess, UpdateProcThreadAttribute,
        WaitForSingleObject,
    };
    use windows::Win32::Security::SECURITY_ATTRIBUTES;
    use windows::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT, HANDLE_FLAGS};
    use windows::core::{PCWSTR, PWSTR};

    type OutCb = Arc<dyn Fn(String) + Send + Sync>;
    type ExitCb = Arc<dyn Fn(i32) + Send + Sync>;

    /// 一个终端标签页的后端进程。
    pub(super) struct TermProc {
        pub(super) input: Sender<Vec<u8>>,
        hpc: Option<HPCON>,
        process: HANDLE,
        /// 管道模式保留 Child 所有权（句柄有效性）
        _child: Option<std::process::Child>,
    }

    impl TermProc {
        /// ConPTY 句柄原始值（0 = 管道模式）；供 PtyRun.resize 复原
        pub(super) fn hpc_raw(&self) -> usize {
            self.hpc.map(|h| h.0 as usize).unwrap_or(0)
        }
    }

    // HANDLE 是裸指针，但进程句柄本身可跨线程使用
    unsafe impl Send for TermProc {}

    impl Drop for TermProc {
        fn drop(&mut self) {
            if let Some(child) = self._child.as_mut() {
                let _ = child.kill();
            }
            unsafe {
                let _ = TerminateProcess(self.process, 1);
                let _ = CloseHandle(self.process);
            }
            if let Some(hpc) = self.hpc.take() {
                // ClosePseudoConsole 会等待 conhost 退出，放到独立线程避免阻塞
                std::thread::spawn(move || unsafe { ClosePseudoConsole(hpc) });
            }
        }
    }

    /// 一台被控端上的终端复用器：按 sid 管理多个 shell。
    pub struct TermHost {
        dc: Arc<RTCDataChannel>,
        rt: Option<tokio::runtime::Handle>,
        procs: Mutex<HashMap<u16, TermProc>>,
    }

    impl TermHost {
        pub fn new(dc: Arc<RTCDataChannel>) -> Arc<Self> {
            Arc::new(Self {
                dc,
                rt: tokio::runtime::Handle::try_current().ok(),
                procs: Mutex::new(HashMap::new()),
            })
        }

        fn send(&self, m: TermMsg) {
            let Some(rt) = self.rt.clone() else {
                return;
            };
            let dc = self.dc.clone();
            let bytes = bytes::Bytes::from(m.encode());
            rt.spawn(async move {
                let _ = dc.send(&bytes).await;
            });
        }

        pub fn handle(self: &Arc<Self>, m: TermMsg) {
            match m {
                TermMsg::Open {
                    sid,
                    shell,
                    cols,
                    rows,
                    cwd,
                } => self.open(sid, shell, cols, rows, cwd),
                TermMsg::Data { sid, data } => {
                    let map = self.procs.lock();
                    if let Some(p) = map.get(&sid) {
                        let _ = p.input.send(data);
                    }
                }
                TermMsg::Resize { sid, cols, rows } => {
                    let map = self.procs.lock();
                    if let Some(p) = map.get(&sid)
                        && let Some(hpc) = p.hpc
                    {
                        unsafe {
                            let _ = ResizePseudoConsole(hpc, size(cols, rows));
                        }
                    }
                }
                TermMsg::Exit { sid, .. } => self.close(sid),
                TermMsg::Err { .. } => {}
            }
        }

        fn open(
            self: &Arc<Self>,
            sid: u16,
            shell: String,
            cols: u16,
            rows: u16,
            cwd: Option<String>,
        ) {
            self.close(sid);
            let cols = cols.clamp(20, 500);
            let rows = rows.clamp(5, 200);

            let this = self.clone();
            let on_out: OutCb = Arc::new(move |text: String| {
                for chunk in split_text(&text) {
                    this.send(TermMsg::Data {
                        sid,
                        data: chunk.into_bytes(),
                    });
                }
            });
            let this = self.clone();
            let on_exit: ExitCb = Arc::new(move |code: i32| {
                this.close(sid);
                this.send(TermMsg::Exit { sid, code });
            });

            // exec: 前缀 = 一次性命令执行（exec 语义），其余按交互式 shell 处理
            let (result, ack_shell) = if let Some(cmd) = shell.strip_prefix("exec:") {
                let r = unsafe { spawn_exec_raw(&exec_cmdline(cmd), cwd.as_deref(), on_out.clone(), on_exit.clone()) };
                (r, format!("exec:{cmd}"))
            } else {
                (spawn_pty(&shell, cols, rows, cwd.as_deref(), on_out.clone(), on_exit.clone()), shell.clone())
            };
            match result {
                Ok(p) => {
                    info!("🖥️ 终端 sid={sid} 已启动（{}×{}）。", cols, rows);
                    self.procs.lock().insert(sid, p);
                    // 回执：主控端据此认为终端已就绪
                    self.send(TermMsg::Open {
                        sid,
                        shell: ack_shell,
                        cols,
                        rows,
                        cwd: None,
                    });
                }
                Err(e) => {
                    warn!("🖥️ 终端 sid={sid} 启动失败: {e:#}");
                    self.send(TermMsg::Err {
                        sid,
                        msg: format!("终端启动失败: {e:#}"),
                    });
                }
            }
        }

        fn close(&self, sid: u16) {
            // 取出的 TermProc 在锁外 drop：终止进程 + 关闭 ConPTY
            let removed = self.procs.lock().remove(&sid);
            if removed.is_some() {
                info!("🖥️ 终端 sid={sid} 已关闭");
            }
            drop(removed);
        }
    }

    /// 测试专用：直接以 ConPTY 运行一条命令（不经过 DataChannel）
    #[cfg(test)]
    pub(super) unsafe fn test_spawn(
        cmd: &str,
        on_out: Arc<dyn Fn(String) + Send + Sync>,
        on_exit: Arc<dyn Fn(i32) + Send + Sync>,
    ) -> anyhow::Result<TermProc> {
        unsafe { spawn_conpty("cmd.exe", cmd, 80, 24, None, on_out, on_exit) }
    }

    fn size(cols: u16, rows: u16) -> COORD {
        COORD {
            X: cols as i16,
            Y: rows as i16,
        }
    }

    fn spawn_pty(
        shell: &str,
        cols: u16,
        rows: u16,
        cwd: Option<&str>,
        on_out: OutCb,
        on_exit: ExitCb,
    ) -> anyhow::Result<TermProc> {
        let (app, conpty_cmd, pipe_args) = shell_cmd(shell);
        match unsafe { spawn_conpty(&app, &conpty_cmd, cols, rows, cwd, on_out.clone(), on_exit.clone()) }
        {
            Ok(p) => Ok(p),
            Err(e) => {
                warn!("ConPTY 不可用（{e:#}），降级为管道模式（无 ANSI）");
                unsafe { spawn_pipe(&app, &pipe_args, on_out, on_exit) }
            }
        }
    }

    /// 返回 (可执行文件, ConPTY 命令行, 管道模式参数)
    fn shell_cmd(shell: &str) -> (String, String, Vec<String>) {
        match shell {
            "powershell" => (
                "powershell.exe".into(),
                "powershell.exe -NoLogo".into(),
                vec!["-NoLogo".into()],
            ),
            "pwsh" => (
                "pwsh.exe".into(),
                "pwsh.exe -NoLogo".into(),
                vec!["-NoLogo".into()],
            ),
            _ => {
                let comspec = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
                (
                    comspec.clone(),
                    format!("\"{comspec}\" /K chcp 65001>nul"),
                    vec!["/Q".into()],
                )
            }
        }
    }

    // ---- 供 PtyRun 公开接口使用的内部转发 ----
    pub(super) fn shell_cmd_pub(shell: &str) -> (String, String, Vec<String>) {
        shell_cmd(shell)
    }

    pub(super) fn spawn_pty_pub(
        app: &str,
        conpty_cmd: &str,
        pipe_args: &[String],
        cols: u16,
        rows: u16,
        cwd: Option<&str>,
        on_out: OutCb,
        on_exit: ExitCb,
    ) -> anyhow::Result<TermProc> {
        match unsafe { spawn_conpty(app, conpty_cmd, cols, rows, cwd, on_out.clone(), on_exit.clone()) } {
            Ok(p) => Ok(p),
            Err(e) => {
                warn!("ConPTY 不可用（{e:#}），降级为管道模式（无 ANSI）");
                unsafe { spawn_pipe(app, pipe_args, on_out, on_exit) }
            }
        }
    }

    /// exec 命令行构造：强制 UTF-8（chcp 65001）+ 关闭回显 + 透传真实退出码。
    /// 纯函数，便于测试。
    pub(super) fn exec_cmdline(command: &str) -> String {
        let comspec = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
        format!("\"{comspec}\" /Q /C chcp 65001>nul & {command}")
    }

    /// exec 专用进程派生：匿名管道接管 stdout/stderr（干净输出、无 ANSI 转义），
    /// stdin 置为无效句柄（命令读到立即 EOF，不会挂在交互输入上）。
    /// 退出码用 WaitForSingleObject 精确获取——exec 的退出码就是返回值，不能猜。
    pub(super) unsafe fn spawn_exec_raw(
        cmdline: &str,
        cwd: Option<&str>,
        on_out: OutCb,
        on_exit: ExitCb,
    ) -> anyhow::Result<TermProc> {
        unsafe {
            // stdout / stderr 各一根管道：写端可继承，读端显式关闭继承
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: std::ptr::null_mut(),
                bInheritHandle: true.into(),
            };
            let mut out_read = HANDLE::default();
            let mut out_write = HANDLE::default();
            let mut err_read = HANDLE::default();
            let mut err_write = HANDLE::default();
            CreatePipe(&mut out_read, &mut out_write, Some(&sa), 0)?;
            // windows 0.62 签名：SetHandleInformation(h, dwmask: u32, dwflags: HANDLE_FLAGS)
            // 掩码=继承位、值=0 → 清除读端句柄的继承位
            SetHandleInformation(out_read, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0))?;
            if let Err(e) = CreatePipe(&mut err_read, &mut err_write, Some(&sa), 0) {
                let _ = CloseHandle(out_read);
                let _ = CloseHandle(out_write);
                return Err(e.into());
            }
            SetHandleInformation(err_read, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0))?;

            let mut si = STARTUPINFOW::default();
            si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            si.dwFlags = STARTF_USESTDHANDLES;
            si.hStdInput = INVALID_HANDLE_VALUE;
            si.hStdOutput = out_write;
            si.hStdError = err_write;

            // CreateProcessW 可能改写命令行缓冲，必须传可变副本
            let mut cmd_w: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();
            let cwd_w: Option<Vec<u16>> = cwd.map(|c| c.encode_utf16().chain(std::iter::once(0)).collect());
            let cwd_p = cwd_w
                .as_ref()
                .map(|v| PCWSTR(v.as_ptr()))
                .unwrap_or(PCWSTR::null());
            let mut pi = PROCESS_INFORMATION::default();
            CreateProcessW(
                PCWSTR::null(),
                Some(PWSTR(cmd_w.as_mut_ptr())),
                None,
                None,
                true, // bInheritHandles：管道写端传给子进程
                CREATE_NO_WINDOW, // 隐藏控制台窗口（Agent 无桌面上下文时不闪黑框）
                None,
                cwd_p,
                &si,
                &mut pi,
            )?;
            let _ = CloseHandle(pi.hThread);
            let _ = CloseHandle(out_write);
            let _ = CloseHandle(err_write);
            let process = pi.hProcess;

            // 读线程 ×2 → 合并输出（与 term 通道单 DATA 流一致）
            for h in [out_read, err_read] {
                let raw = h.0 as usize;
                let cb = on_out.clone();
                std::thread::spawn(move || {
                    let mut f = std::fs::File::from_raw_handle(raw as RawHandle);
                    let mut buf = [0u8; 8192];
                    let mut utf8 = Utf8Stream::new();
                    loop {
                        match f.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let text = utf8.push(&buf[..n]);
                                if !text.is_empty() {
                                    cb(text);
                                }
                            }
                        }
                    }
                });
            }

            // 退出监视
            let process_raw = process.0 as usize;
            std::thread::spawn(move || {
                let h = HANDLE(process_raw as *mut c_void);
                let _ = WaitForSingleObject(h, u32::MAX);
                let mut code: u32 = 0;
                let _ = GetExitCodeProcess(h, &mut code);
                on_exit(code as i32);
            });

            Ok(TermProc {
                input: {
                    // exec 不接收标准输入；给一个丢弃通道避免调用方 send 报错
                    let (tx, rx) = channel::<Vec<u8>>();
                    std::thread::spawn(move || drop(rx));
                    tx
                },
                hpc: None,
                process,
                _child: None,
            })
        }
    }


    /// 登录用户令牌 + 对应环境块；拿不到就返回 None（回退当前进程身份）。
    struct UserCtx {
        token: Option<HANDLE>,
        env: *mut c_void,
    }

    impl UserCtx {
        fn acquire() -> Self {
            unsafe {
                // 优先用 Agent 自身所在会话（RDP 场景下与控制台会话不同）
                let mut own = 0u32;
                let _ = ProcessIdToSessionId(GetCurrentProcessId(), &mut own);
                let mut candidates = vec![own];
                let active = WTSGetActiveConsoleSessionId();
                if active != u32::MAX && active != own {
                    candidates.push(active);
                }
                for sid in candidates {
                    if sid == 0 {
                        continue;
                    }
                    let mut tok = HANDLE::default();
                    if WTSQueryUserToken(sid, &mut tok).is_ok() {
                        let mut env: *mut c_void = std::ptr::null_mut();
                        if CreateEnvironmentBlock(&mut env, Some(tok), false).is_err() {
                            env = std::ptr::null_mut();
                        }
                        info!("🖥️ 终端将使用登录用户会话 {sid} 的身份启动");
                        return Self { token: Some(tok), env };
                    }
                }
            }
            warn!("🖥️ 无法取得登录用户令牌，终端将以当前进程身份启动");
            Self {
                token: None,
                env: std::ptr::null_mut(),
            }
        }
    }

    impl Drop for UserCtx {
        fn drop(&mut self) {
            unsafe {
                if !self.env.is_null() {
                    let _ = DestroyEnvironmentBlock(self.env);
                }
                if let Some(t) = self.token.take() {
                    let _ = CloseHandle(t);
                }
            }
        }
    }

    /// STARTUPINFOEX 属性列表释放守卫
    struct AttrGuard(LPPROC_THREAD_ATTRIBUTE_LIST);
    impl Drop for AttrGuard {
        fn drop(&mut self) {
            unsafe { DeleteProcThreadAttributeList(self.0) }
        }
    }

    unsafe fn spawn_conpty(
        app: &str,
        cmd: &str,
        cols: u16,
        rows: u16,
        cwd: Option<&str>,
        on_out: OutCb,
        on_exit: ExitCb,
    ) -> anyhow::Result<TermProc> {
        unsafe {
            let mut in_read = HANDLE::default();
            let mut in_write = HANDLE::default();
            let mut out_read = HANDLE::default();
            let mut out_write = HANDLE::default();
            CreatePipe(&mut in_read, &mut in_write, None, 0)?;
            if let Err(e) = CreatePipe(&mut out_read, &mut out_write, None, 0) {
                let _ = CloseHandle(in_read);
                let _ = CloseHandle(in_write);
                return Err(e.into());
            }
            let hpc = match CreatePseudoConsole(size(cols, rows), in_read, out_write, 0) {
                Ok(h) => h,
                Err(e) => {
                    let _ = CloseHandle(in_read);
                    let _ = CloseHandle(in_write);
                    let _ = CloseHandle(out_read);
                    let _ = CloseHandle(out_write);
                    return Err(e.into());
                }
            };
            // ConPTY 已持有这两个句柄的副本
            let _ = CloseHandle(in_read);
            let _ = CloseHandle(out_write);

            let ctx = UserCtx::acquire();
            let pi = match create_attached(app, cmd, hpc, cwd, &ctx) {
                Ok(p) => p,
                Err(e) => {
                    ClosePseudoConsole(hpc);
                    let _ = CloseHandle(in_write);
                    let _ = CloseHandle(out_read);
                    return Err(e);
                }
            };
            let process = pi.hProcess;

            // 写线程：ConPTY 输入管道
            let (tx, rx) = channel::<Vec<u8>>();
            let in_write_raw = in_write.0 as usize;
            std::thread::spawn(move || {
                let mut f = std::fs::File::from_raw_handle(in_write_raw as RawHandle);
                while let Ok(buf) = rx.recv() {
                    if f.write_all(&buf).is_err() {
                        break;
                    }
                    let _ = f.flush();
                }
            });

            // 读线程：ConPTY 输出管道 → UTF-8 清洗 → DATA
            let out_read_raw = out_read.0 as usize;
            std::thread::spawn(move || {
                let mut f = std::fs::File::from_raw_handle(out_read_raw as RawHandle);
                let mut buf = [0u8; 8192];
                let mut utf8 = Utf8Stream::new();
                loop {
                    match f.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let text = utf8.push(&buf[..n]);
                            if !text.is_empty() {
                                on_out(text);
                            }
                        }
                        Err(_) => break,
                    }
                }
            });

            // 退出监视：ConPTY 输出管道要等 ClosePseudoConsole 才会 EOF，
            // 因此必须用进程句柄判断退出，不能依赖 read 返回 0。
            let process_raw = process.0 as usize;
            let exit_cb = on_exit.clone();
            std::thread::spawn(move || {
                let h = HANDLE(process_raw as *mut c_void);
                let _ = WaitForSingleObject(h, u32::MAX);
                let mut code: u32 = 0;
                let _ = GetExitCodeProcess(h, &mut code);
                exit_cb(code as i32);
            });

            Ok(TermProc {
                input: tx,
                hpc: Some(hpc),
                process,
                _child: None,
            })
        }
    }

    unsafe fn create_attached(
        _app: &str,
        cmd: &str,
        hpc: HPCON,
        cwd: Option<&str>,
        ctx: &UserCtx,
    ) -> anyhow::Result<PROCESS_INFORMATION> {
        unsafe {
            let mut list_size = 0usize;
            let _ = InitializeProcThreadAttributeList(None, 1, None, &mut list_size);
            if list_size == 0 {
                anyhow::bail!("InitializeProcThreadAttributeList 取长度失败");
            }
            let mut buf = vec![0u8; list_size];
            let list = LPPROC_THREAD_ATTRIBUTE_LIST(buf.as_mut_ptr() as *mut c_void);
            InitializeProcThreadAttributeList(Some(list), 1, None, &mut list_size)?;
            let _guard = AttrGuard(list);
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                // 该属性要求直接传 HPCON 值本身，而不是它的地址
                Some(hpc.0 as *const c_void),
                std::mem::size_of::<HPCON>(),
                None,
                None,
            )?;

            let mut si = STARTUPINFOEXW::default();
            si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            // 关键：显式声明标准句柄无效，否则子进程可能继承父进程被重定向的
            // stdout/stderr（例如 Agent 自身日志），命令输出就不会进入 PTY。
            si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
            si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
            si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
            si.lpAttributeList = list;

            // lpApplicationName 传 NULL：由命令行首 token 解析，支持 PATH 查找
            let mut cmd_w: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
            let cwd_w: Option<Vec<u16>> = cwd.map(|c| {
                c.encode_utf16()
                    .chain(std::iter::once(0))
                    .collect::<Vec<u16>>()
            });
            let cwd_p = cwd_w
                .as_ref()
                .map(|v| PCWSTR(v.as_ptr()))
                .unwrap_or(PCWSTR::null());

            let mut pi = PROCESS_INFORMATION::default();
            let flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT;
            let env = if ctx.env.is_null() {
                None
            } else {
                Some(ctx.env as *const c_void)
            };
            match ctx.token {
                Some(tok) => {
                    match CreateProcessAsUserW(
                        Some(tok),
                        PCWSTR::null(),
                        Some(PWSTR(cmd_w.as_mut_ptr())),
                        None,
                        None,
                        false,
                        flags,
                        env,
                        cwd_p,
                        &si.StartupInfo,
                        &mut pi,
                    ) {
                        Ok(()) => {}
                        Err(e) => {
                            // 用户令牌派生失败时不放弃，回退当前进程身份
                            warn!("以登录用户身份启动终端失败（{e}），回退当前进程身份");
                            CreateProcessW(
                                PCWSTR::null(),
                                Some(PWSTR(cmd_w.as_mut_ptr())),
                                None,
                                None,
                                false,
                                flags,
                                env,
                                cwd_p,
                                &si.StartupInfo,
                                &mut pi,
                            )?;
                        }
                    }
                }
                None => {
                    CreateProcessW(
                        PCWSTR::null(),
                        Some(PWSTR(cmd_w.as_mut_ptr())),
                        None,
                        None,
                        false,
                        flags,
                        env,
                        cwd_p,
                        &si.StartupInfo,
                        &mut pi,
                    )?;
                }
            }
            let _ = CloseHandle(pi.hThread);
            Ok(pi)
        }
    }

    /// 无 ConPTY 的降级：匿名管道 + cmd /Q，只适合一次性命令。
    unsafe fn spawn_pipe(
        app: &str,
        args: &[String],
        on_out: OutCb,
        on_exit: ExitCb,
    ) -> anyhow::Result<TermProc> {
        use std::process::{Command, Stdio};
        let mut child = Command::new(app)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdin = child.stdin.take().expect("stdin");
        let mut stdout = child.stdout.take().expect("stdout");
        let mut stderr = child.stderr.take().expect("stderr");
        let process = HANDLE(child.as_raw_handle() as RawHandle);
        let process_raw = process.0 as usize;

        let (tx, rx) = channel::<Vec<u8>>();
        std::thread::spawn(move || {
            while let Ok(buf) = rx.recv() {
                if stdin.write_all(&buf).is_err() {
                    break;
                }
                let _ = stdin.flush();
            }
        });

        let out2 = on_out.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut utf8 = Utf8Stream::new();
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let text = utf8.push(&buf[..n]);
                        if !text.is_empty() {
                            out2(text);
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut utf8 = Utf8Stream::new();
            loop {
                match stderr.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let text = utf8.push(&buf[..n]);
                        if !text.is_empty() {
                            on_out(text);
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_millis(400));
            let mut code: u32 = 0;
            let h = HANDLE(process_raw as *mut c_void);
            unsafe {
                if GetExitCodeProcess(h, &mut code).is_err() {
                    break;
                }
            }
            if code != 259 {
                on_exit(code as i32);
                break;
            }
        });

        Ok(TermProc {
            input: tx,
            hpc: None,
            process,
            _child: Some(child),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(m: TermMsg) {
        let buf = m.encode();
        let back = TermMsg::decode(&buf).expect("解码失败");
        assert_eq!(m, back);
    }

    #[test]
    fn roundtrip_all_variants() {
        roundtrip(TermMsg::Open {
            sid: 1,
            shell: "cmd".into(),
            cols: 120,
            rows: 30,
            cwd: Some("C:\\Windows".into()),
        });
        roundtrip(TermMsg::Data {
            sid: 2,
            data: "你好\r\n".as_bytes().to_vec(),
        });
        roundtrip(TermMsg::Resize {
            sid: 3,
            cols: 80,
            rows: 24,
        });
        roundtrip(TermMsg::Exit { sid: 4, code: -1 });
        roundtrip(TermMsg::Err {
            sid: 5,
            msg: "boom".into(),
        });
    }

    #[test]
    fn header_layout() {
        let buf = TermMsg::Data {
            sid: 0x1234,
            data: b"abc".to_vec(),
        }
        .encode();
        assert_eq!(buf[0], TYPE_DATA);
        assert_eq!(&buf[1..3], &[0x12, 0x34]);
        assert_eq!(&buf[3..], b"abc");
    }

    #[test]
    fn decode_rejects_bad_input() {
        assert!(TermMsg::decode(&[]).is_none());
        assert!(TermMsg::decode(&[TYPE_DATA, 0]).is_none());
        assert!(TermMsg::decode(&[0x7f, 0, 0]).is_none());
        assert!(TermMsg::decode(&[TYPE_RESIZE, 0, 0, 1]).is_none());
        assert!(TermMsg::decode(&[TYPE_OPEN, 0, 0, b'{']).is_none());
    }

    #[test]
    fn open_defaults_when_fields_missing() {
        let buf = [TYPE_OPEN, 0, 9, b'{', b'}'];
        match TermMsg::decode(&buf).unwrap() {
            TermMsg::Open {
                sid,
                shell,
                cols,
                rows,
                cwd,
            } => {
                assert_eq!(sid, 9);
                assert!(shell.is_empty());
                assert_eq!(cols, 0);
                assert_eq!(rows, 0);
                assert!(cwd.is_none());
            }
            _ => panic!("类型错误"),
        }
    }

    #[test]
    fn split_text_respects_char_boundary() {
        let s = "中".repeat(2000); // 6000 字节
        let parts = split_text(&s);
        assert_eq!(parts.iter().map(|p| p.len()).sum::<usize>(), s.len());
        for p in &parts {
            assert!(p.len() <= MAX_CHUNK);
            assert!(std::str::from_utf8(p.as_bytes()).is_ok());
        }
        assert!(parts.len() >= 2);
    }

    #[test]
    fn utf8_stream_keeps_split_sequence() {
        let bytes = "中文测试".as_bytes().to_vec();
        let (a, b) = bytes.split_at(4); // 从多字节字符中间切开
        let mut st = Utf8Stream::new();
        let first = st.push(a);
        let second = st.push(b);
        assert_eq!(format!("{first}{second}"), "中文测试");
        assert!(!first.contains('\u{FFFD}'));
    }

    #[test]
    fn utf8_stream_replaces_illegal_bytes() {
        let mut st = Utf8Stream::new();
        let out = st.push(&[0x41, 0xFF, 0x42]);
        assert_eq!(out, "A\u{FFFD}B");
    }

    /// ConPTY 端到端冒烟：进程创建 → 管道读取 → UTF-8 输出 → 退出码
    #[cfg(all(feature = "desktop", windows))]
    #[test]
    fn conpty_echo_smoke() {
        use std::sync::{Arc, Mutex, mpsc};
        use std::time::Duration;

        let text = Arc::new(Mutex::new(String::new()));
        let (tx, rx) = mpsc::channel::<i32>();

        let out2 = text.clone();
        let on_out = Arc::new(move |s: String| out2.lock().unwrap().push_str(&s));
        let on_exit = Arc::new(move |code: i32| {
            let _ = tx.send(code);
        });

        let proc = unsafe {
            host::test_spawn("cmd.exe /K chcp 65001>nul", on_out, on_exit)
        }
        .expect("ConPTY 启动失败");

        // 模拟主控端输入：等 shell 起来后执行命令
        std::thread::sleep(Duration::from_millis(800));
        let _ = proc
            .input
            .send("echo rebleach-term-中文OK\r\n".as_bytes().to_vec());
        let _ = proc.input.send(b"exit\r\n".to_vec());

        let code = rx
            .recv_timeout(Duration::from_secs(20))
            .expect("等待 shell 退出超时");
        assert_eq!(code, 0, "退出码异常: {code}");
        // conhost 的输出可能略晚于进程退出，给读线程一点时间
        let mut got = String::new();
        for _ in 0..60 {
            got = text.lock().unwrap().clone();
            if got.contains("rebleach-term-中文OK") {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            got.contains("rebleach-term-中文OK"),
            "输出未命中，实际为: {got:?}"
        );
        drop(proc);
    }

    #[cfg(all(feature = "desktop", windows))]
    #[test]
    fn exec_cmdline_forces_utf8() {
        let line = host::exec_cmdline("echo hi");
        assert!(line.contains("/Q /C chcp 65001>nul & echo hi"), "{line}");
        assert!(line.starts_with('"'), "COMSPEC 需要引号包裹: {line}");
    }

    /// exec 端到端冒烟：干净输出 + 真实退出码（exit /b 3）
    #[cfg(all(feature = "desktop", windows))]
    #[test]
    fn exec_pipe_smoke() {
        use std::sync::{Arc, Mutex, mpsc};
        use std::time::Duration;

        let text = Arc::new(Mutex::new(String::new()));
        let (tx, rx) = mpsc::channel::<i32>();
        let out2 = text.clone();
        let on_out: Arc<dyn Fn(String) + Send + Sync> =
            Arc::new(move |s: String| out2.lock().unwrap().push_str(&s));
        let on_exit: Arc<dyn Fn(i32) + Send + Sync> = Arc::new(move |code: i32| {
            let _ = tx.send(code);
        });

        let run = crate::term::spawn_exec("echo rebleach-exec-OK & exit /b 3", None, on_out, on_exit)
            .expect("exec 启动失败");
        let code = rx
            .recv_timeout(Duration::from_secs(15))
            .expect("exec 等待退出超时");
        assert_eq!(code, 3, "退出码应透传: {code}");
        let got = text.lock().unwrap().clone();
        assert!(got.contains("rebleach-exec-OK"), "exec 输出未命中: {got:?}");
        assert!(!got.contains("\u{1b}["), "exec 输出不应含 ANSI 转义: {got:?}");
        drop(run);
    }
}
