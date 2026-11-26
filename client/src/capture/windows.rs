// 自研 Windows Graphics Capture 屏幕采集
// 相比 xcap：会话常驻（只建一次），帧经 ID3D11VideoProcessor 在 GPU 上缩放到编码尺寸，
// 再拷进常驻 staging 纹理 Map 回读。只回读 1920x1200x4≈9.2MB，且直接拿到 BGRA，
// 省掉 xcap 每帧新建 staging、全尺寸回读 + 两次 20MB 克隆 + 通道交换的开销。
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// 全局采集后端独占锁：任何时刻只能有一个 Capture 实例在采集。
/// start_for 入口先停掉上一个（设 stop 信号 + 等待短暂时间让旧线程退出），
/// 然后再启动新的。保证 DDA / GDI / WGC 三种 backend 不会同时跑。
static ACTIVE_STOP: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);

/// DDA 连续重建失败次数：达到阈值后本次进程不再尝试 DDA，直接回退 GDI。
/// （实测故障时会以每秒数千次的频率重建复制会话，驱动级风暴把整台机器拖死）
static DDA_FAILS: AtomicU32 = AtomicU32::new(0);
const DDA_MAX_FAILS: u32 = 2;

use anyhow::{Context, Result, anyhow};
use tracing::{debug, info, warn};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{GetLastError, HMODULE, LPARAM, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEX2D_VPIV,
    D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, D3D11CreateDevice,
    ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D, ID3D11VideoContext,
    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_INVALID_CALL, DXGI_ERROR_NOT_CURRENTLY_AVAILABLE,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIDevice, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
    DeleteDC, DeleteObject, EnumDisplayDevicesW, EnumDisplayMonitors, GetDC, GetMonitorInfoW,
    HALFTONE, HDC, HGDIOBJ, HMONITOR, MONITORINFOEXW, ReleaseDC, SRCCOPY, SelectObject,
    SetStretchBltMode, StretchBlt, DISPLAY_DEVICEW,
};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{BOOL, IInspectable, Interface, factory};

use super::{MonitorInfo, ScreenCapture};

// ============================================================
// 显示器枚举
// ============================================================
pub fn enumerate() -> Vec<MonitorInfo> {
    let mut out: Vec<MonitorInfo> = Vec::new();
    // 缓存每个 DISPLAY_DEVICE 名称 → DeviceID + DeviceString，用于后续按名字查虚拟屏
    let virt_flags = virtual_flags_cache();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(enum_monitor),
            LPARAM(&mut out as *mut Vec<MonitorInfo> as isize),
        );
    }
    // 用缓存填上 is_virtual（enum_monitor 里闭包不能跨 FFI 传递局部缓存）
    for m in out.iter_mut() {
        m.is_virtual = virt_flags.is_virtual(&m.name);
    }
    out
}

/// 枚举所有 DISPLAY_DEVICE，建一张「设备名 → is_virtual」的缓存表。
/// Windows 上 HMONITOR 没有直接给虚拟屏标识，得按 PnP DeviceID 和描述文本猜：
///   ROOT\DISPLAY\*   → IddSampleDriver / usbmmidd 这类间接显示驱动
///   USB\VID_*        → 大概率虚拟（usbmmidd 走 USB 总线）
///   描述带 WinDisc/Virtual/idd/IddSample → 肯定虚拟
fn virtual_flags_cache() -> VirtualFlags {
    let mut flags = VirtualFlags::default();
    unsafe {
        let mut dd = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        let mut i = 0u32;
        while EnumDisplayDevicesW(None, i, &mut dd, 0).as_bool() {
            i += 1;
            let name = String::from_utf16_lossy(&dd.DeviceName)
                .split('\0')
                .next()
                .unwrap_or("")
                .to_string();
            let device_id = String::from_utf16_lossy(&dd.DeviceID)
                .split('\0')
                .next()
                .unwrap_or("")
                .to_string();
            let device_string = String::from_utf16_lossy(&dd.DeviceString)
                .split('\0')
                .next()
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                continue;
            }
            let virt = device_id.starts_with("ROOT\\DISPLAY\\")
                || device_id.starts_with("USB\\VID_")
                || device_string.to_ascii_lowercase().contains("virtual")
                || device_string.to_ascii_lowercase().contains("windisc")
                || device_string.to_ascii_lowercase().contains("idd")
                || device_string.to_ascii_lowercase().contains("iddsampledriver")
                || device_string.to_ascii_lowercase().contains("usb mobile monitor");
            flags.map.insert(name, virt);
            dd = DISPLAY_DEVICEW::default();
            dd.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
        }
    }
    flags
}

#[derive(Default)]
struct VirtualFlags {
    /// DISPLAY_DEVICE.DeviceName → is_virtual
    map: std::collections::HashMap<String, bool>,
}
impl VirtualFlags {
    fn is_virtual(&self, monitor_name: &str) -> bool {
        // MI\.szDevice 形如 \\.\DISPLAY1；DISPLAY_DEVICE.DeviceName 形如 \\?\DISPLAY#SKY0001#...
        // 直接等值命中概率不高，用「后半段包含」匹配更稳
        self.map.iter().any(|(dev, virt)| {
            *virt && (monitor_name.is_empty() || dev.contains(monitor_name) || monitor_name.contains(dev))
        })
    }
}

unsafe extern "system" fn enum_monitor(
    hmon: HMONITOR,
    _hdc: HDC,
    _rc: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let out = unsafe { &mut *(lparam.0 as *mut Vec<MonitorInfo>) };
    let mut mi = MONITORINFOEXW::default();
    mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if unsafe { GetMonitorInfoW(hmon, &mut mi.monitorInfo) }.as_bool() {
        let rc = mi.monitorInfo.rcMonitor;
        let name = {
            let len = mi
                .szDevice
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(mi.szDevice.len());
            String::from_utf16_lossy(&mi.szDevice[..len])
        };
        out.push(MonitorInfo {
            handle: hmon.0 as u64,
            name,
            x: rc.left,
            y: rc.top,
            width: (rc.right - rc.left) as u32,
            height: (rc.bottom - rc.top) as u32,
            primary: mi.monitorInfo.dwFlags & 1 != 0, // MONITORINFOF_PRIMARY
            is_virtual: false,                       // 后面用 virtual_flags_cache 回填
        });
    }
    BOOL(1)
}

// ============================================================
// 采集会话
// ============================================================
pub struct Capture {
    pub src_w: u32,
    pub src_h: u32,
    pub dst_w: u32,
    pub dst_h: u32,
    /// 当前显示器设备名（\\.\DISPLAYn），主控端改分辨率时定位目标屏
    pub name: String,
    /// 当前显示器左上角在虚拟桌面中的坐标（鼠标物理坐标映射用）
    pub origin_x: i32,
    pub origin_y: i32,
    pool: Option<Direct3D11CaptureFramePool>,
    session: Option<GraphicsCaptureSession>,
    /// 帧池由它创建，保活到会话结束
    _rt_device: Option<IDirect3DDevice>,
    rx: Receiver<Vec<u8>>,
    /// 用完的回读缓冲环回采集回调复用（省掉每帧 9.2MB 分配）
    free_tx: SyncSender<Vec<u8>>,
    /// GDI 兜底采集线程的停止标志（WGC 不可用时才有值）
    gdi_stop: Option<Arc<AtomicBool>>,
    /// DXGI 桌面复制线程的停止标志（DDA 可用时才有值）
    dda_stop: Option<Arc<AtomicBool>>,
    /// 实际生效的采集后端（日志/排查用）：dda | wgc | gdi
    pub backend: &'static str,
}

impl Capture {
    /// 建立采集会话。后端优先顺序：**DXGI 桌面复制 (DDA)** → GDI。
    /// WGC 在本机（4K TV + 虚拟显示器共存）上 `CreateForMonitor` 固定返回 0x80070424，
    /// 已从 fallback 链中移除。详见 P1-2 修复说明。
    ///
    /// 全局独占：start_for 先停掉任何还在跑的旧采集线程（互斥锁），
    /// 再启动新的，保证同一时刻只有一个 backend。
    pub fn start_for(monitor: Option<usize>) -> Result<Self> {
        // —— 全局独占：先停上一个 ——
        if let Some(old) = ACTIVE_STOP.lock().take() {
            info!("🖥️ 停掉上一个采集后端，准备启动新的");
            old.store(true, Ordering::Relaxed);
            // 给旧线程 300ms 退出时间，避免同显示器 DuplicateOutput 冲突
            std::thread::sleep(Duration::from_millis(300));
        }

        let mut monitors = enumerate();
        info!("🖥️ 枚举到 {} 个显示器", monitors.len());
        if monitor.is_none() {
            // 未指定时优先物理屏：IDD 虚拟显示器（远程桌面/虚拟屏驱动）静止时
            // 不产生呈现帧，DDA 会长时间拿不到帧，主控端看到的就是"卡死"。
            monitors.sort_by_key(|m| (m.is_virtual, !m.primary));
            if let Some(m) = monitors.first() {
                if m.is_virtual {
                    warn!(
                        "🖥️ 只找到虚拟显示器 {}，桌面静止时帧率会明显下降",
                        m.name
                    );
                }
            }
        }
        if let Some(i) = monitor {
            // 显示器拓扑变化（拔掉虚拟屏/休眠唤醒）后索引可能失效：
            // 回退到 0 号屏继续采，不能让采集线程直接死掉（否则主控端黑屏）
            let idx = if i >= monitors.len() {
                warn!(
                    "🖥️ 显示器索引 {i} 超出范围（共 {} 个），回退到 0 号屏",
                    monitors.len()
                );
                0usize
            } else {
                i
            };
            if !monitors.is_empty() {
                let m = monitors.swap_remove(idx);
                monitors.clear();
                monitors.push(m);
            }
        }

        // —— 后端 1：DXGI 桌面复制 ——
        match Self::start_dda(&monitors) {
            Ok(c) => {
                // 注册停止信号
                if let Some(ref stop) = c.dda_stop {
                    *ACTIVE_STOP.lock() = Some(stop.clone());
                }
                return Ok(c);
            }
            Err(e) => {
                // P1-1 修复：DDA 首次启动 0x80070057 是时序问题（显卡驱动刚就绪前参数错误），
                // 等 1~2 秒重试一次大概率能成，不要立刻回退 GDI
                warn!("🖥️ DDA 首次启动失败，1.5s 后重试一次: {e:#}");
                std::thread::sleep(Duration::from_millis(1500));
                match Self::start_dda(&monitors) {
                    Ok(c) => {
                        if let Some(ref stop) = c.dda_stop {
                            *ACTIVE_STOP.lock() = Some(stop.clone());
                        }
                        return Ok(c);
                    }
                    Err(e2) => warn!("🖥️ DDA 重试仍失败: {e2:#}"),
                }
            }
        }

        // P1-2 修复：跳过 WGC——在本机（RTX 3080 + Win11 + 虚拟显示器共存）上
        // CreateForMonitor 固定返回 0x80070424（服务未安装），
        // 无例外地注定失败，不再浪费启动时间。DDA 不行就直接 GDI。
        warn!("🖥️ WGC 已跳过（本机固定 0x80070424），直接 GDI 兜底");

        // —— 后端 2：GDI ——
        let c = Self::start_gdi(&monitors)?;
        if let Some(ref stop) = c.gdi_stop {
            *ACTIVE_STOP.lock() = Some(stop.clone());
        }
        Ok(c)
    }

    /// DXGI 桌面复制后端。
    ///
    /// 帧由 `AcquireNextFrame(timeout)` 事件驱动：桌面有更新才返回，静止时超时返回，
    /// 因此不会像 GDI 那样每帧都付一次全屏拷贝。拿到 D3D11 纹理后直接交给
    /// 现有的 `Gpu`（ID3D11VideoProcessor）缩放到编码尺寸，路径与 WGC 完全一致。
    ///
    /// 锁屏/UAC/分辨率切换会让复制会话失效（`DXGI_ERROR_ACCESS_LOST`），
    /// 线程内自动重建；重建期间画面由 `recv_timeout` 的 GDI 安全桌面兜底接管。
    fn start_dda(monitors: &[MonitorInfo]) -> Result<Self> {
        if DDA_FAILS.load(Ordering::Relaxed) >= DDA_MAX_FAILS {
            return Err(anyhow!("DDA 已连续故障，跳过直接回退 GDI"));
        }
        let device = create_device()?;
        let context = unsafe { device.GetImmediateContext() }.context("取 D3D11 上下文失败")?;
        let mut last_err = anyhow!("没有可用显示器");
        for m in monitors {
            let hmon = HMONITOR(m.handle as *mut core::ffi::c_void);
            let output: IDXGIOutput1 = match find_output(&device, hmon) {
                Ok(o) => o,
                Err(e) => {
                    debug!("🖥️ 显示器 {} 未找到 DXGI 输出: {e:#}", m.name);
                    last_err = e;
                    continue;
                }
            };
            let desc = unsafe { output.GetDesc() }.context("取 DXGI 输出描述失败")?;
            let (src_w, src_h) = (
                (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left) as u32,
                (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top) as u32,
            );
            if src_w == 0 || src_h == 0 {
                last_err = anyhow!("显示器 {} 输出尺寸为 0", m.name);
                continue;
            }
            let (dst_w, dst_h) = crate::video::target_size(src_w, src_h);
            let gpu = match Gpu::new(&device, &context, src_w, src_h, dst_w, dst_h) {
                Ok(g) => g,
                Err(e) => {
                    warn!("🖥️ 显示器 {} GPU 缩放初始化失败: {e:#}", m.name);
                    last_err = e;
                    continue;
                }
            };
            let dupl = match unsafe { output.DuplicateOutput(&device) } {
                Ok(d) => d,
                Err(e) => {
                    // 0x887A0001/0x887A0022 常见：已有复制会话、显示器不在活动桌面、
                    // 或该输出走的是间接显示（IDD 虚拟屏）
                    warn!("🖥️ 显示器 {} 创建桌面复制失败: {e}", m.name);
                    last_err = anyhow!("DuplicateOutput 失败: {e}");
                    continue;
                }
            };

            let (tx, rx) = sync_channel::<Vec<u8>>(2);
            let (free_tx, free_rx) = sync_channel::<Vec<u8>>(3);
            let stop = Arc::new(AtomicBool::new(false));
            let stop_t = stop.clone();
            let interval = Duration::from_millis(1000 / crate::capture::target_fps());
            let device_t = device.clone();
            let output_t = output.clone();
            let (ox, oy) = (m.x, m.y);
            let name = m.name.clone();
            std::thread::spawn(move || {
                dda_loop(
                    dupl, output_t, device_t, gpu, tx, free_rx, stop_t, interval, ox, oy, name,
                    dst_w, dst_h,
                )
            });

            info!(
                "🖥️ 选用显示器 {} {}x{} → {}x{}（backend=DDA + GPU 下采样）",
                m.name, src_w, src_h, dst_w, dst_h
            );
            return Ok(Self {
                src_w,
                src_h,
                dst_w,
                dst_h,
                name: m.name.clone(),
                origin_x: m.x,
                origin_y: m.y,
                pool: None,
                session: None,
                _rt_device: None,
                rx,
                free_tx,
                gdi_stop: None,
                dda_stop: Some(stop),
                backend: "dda",
            });
        }
        Err(last_err)
    }

    /// WGC 后端：逐个显示器试建，全部失败才返回 Err。
    fn start_wgc(monitors: &[MonitorInfo]) -> Result<Self> {
        let device = create_device()?;
        let context = unsafe { device.GetImmediateContext() }.context("取 D3D11 上下文失败")?;
        let dxgi: IDXGIDevice = device.cast().context("D3D11 设备转 DXGI 失败")?;
        let rt_device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }
            .cast()
            .context("创建 WinRT D3D11 设备失败")?;
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;

        let mut last_err_wgc: Option<anyhow::Error> = None;
        // 逐个试建：副屏/虚拟屏上 CreateForMonitor 可能返回 E_INVALIDARG(0x80070057)，
        // IDD 虚拟显示器上还会返回 0x80070424（WGC 不支持间接显示设备）
        for m in monitors {
            let hmon = HMONITOR(m.handle as *mut core::ffi::c_void);
            let item = match unsafe { interop.CreateForMonitor::<GraphicsCaptureItem>(hmon) } {
                Ok(item) => item,
                Err(e) => {
                    warn!(
                        "🖥️ 显示器 {} ({}x{}) 创建捕获项失败: {e}",
                        m.name, m.width, m.height
                    );
                    last_err_wgc = Some(anyhow!("CreateForMonitor({}) 失败: {e}", m.name));
                    continue;
                }
            };
            let size = item.Size().context("取捕获项尺寸失败")?;
            let (src_w, src_h) = (size.Width as u32, size.Height as u32);
            let (dst_w, dst_h) = crate::video::target_size(src_w, src_h);

            let gpu = match Gpu::new(&device, &context, src_w, src_h, dst_w, dst_h) {
                Ok(g) => g,
                Err(e) => {
                    warn!("🖥️ 显示器 {} GPU 缩放初始化失败: {e:#}", m.name);
                    continue;
                }
            };

            let pool = match Direct3D11CaptureFramePool::CreateFreeThreaded(
                &rt_device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                size,
            ) {
                Ok(p) => p,
                Err(e) => {
                    warn!("🖥️ 显示器 {} 创建帧池失败: {e}", m.name);
                    continue;
                }
            };

            let (tx, rx) = sync_channel::<Vec<u8>>(2);
            // 回读缓冲池：转换线程用完把 Vec 还回来，回调直接复用，不再每帧分配 9.2MB
            let (free_tx, free_rx) = sync_channel::<Vec<u8>>(3);
            pool.FrameArrived(
                &TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(
                    move |pool, _| {
                        let Some(pool) = pool.as_ref() else {
                            return Ok(());
                        };
                        match pool.TryGetNextFrame() {
                            Ok(frame) => {
                                let mut buf = free_rx.try_recv().unwrap_or_default();
                                let res = gpu.convert(&frame, &mut buf);
                                // 帧必须在处理完后显式关闭，否则帧池会很快耗尽
                                let _ = frame.Close();
                                match res {
                                    Ok(()) => {
                                        let _ = tx.try_send(buf);
                                    }
                                    Err(e) => warn!("采集帧转换失败: {e:#}"),
                                }
                            }
                            Err(e) => debug!("TryGetNextFrame: {e}"),
                        }
                        Ok(())
                    },
                ),
            )?;

            let session = pool
                .CreateCaptureSession(&item)
                .context("创建捕获会话失败")?;
            // 老系统/缺少 graphicsCaptureWithoutBorder 权限时会失败，不影响采集
            if let Err(e) = session.SetIsBorderRequired(false) {
                debug!("SetIsBorderRequired(false) 失败（可忽略）: {e}");
            }
            if let Err(e) = session.SetIsCursorCaptureEnabled(false) {
                debug!("SetIsCursorCaptureEnabled(false) 失败（可忽略）: {e}");
            }
            match session.StartCapture() {
                Ok(()) => {}
                Err(e) => {
                    warn!("🖥️ 显示器 {} 启动捕获失败: {e}", m.name);
                    continue;
                }
            }

            info!(
                "🖥️ 选用显示器 {} {}x{} → {}x{}（WGC + GPU 下采样）",
                m.name, src_w, src_h, dst_w, dst_h
            );
            return Ok(Self {
                src_w,
                src_h,
                dst_w,
                dst_h,
                name: m.name.clone(),
                origin_x: m.x,
                origin_y: m.y,
                pool: Some(pool),
                session: Some(session),
                _rt_device: Some(rt_device),
                rx,
                free_tx,
                gdi_stop: None,
                dda_stop: None,
                backend: "wgc",
            });
        }
        Err(last_err_wgc.unwrap_or_else(|| anyhow!("所有显示器都无法启动 WGC 采集")))
    }

    /// GDI 兜底采集：BitBlt 抓整块显示器再 StretchBlt 缩到编码尺寸。
    /// WGC 在 IDD/虚拟显示器上不可用（0x80070424），此时仍能出画面，代价是 CPU 回读。
    fn start_gdi(monitors: &[MonitorInfo]) -> Result<Self> {
        let m = monitors.first().ok_or_else(|| anyhow!("没有可用显示器"))?;
        let (src_w, src_h) = (m.width, m.height);
        let (dst_w, dst_h) = crate::video::target_size(src_w, src_h);
        let (tx, rx) = sync_channel::<Vec<u8>>(2);
        let (free_tx, _free_rx) = sync_channel::<Vec<u8>>(3);
        let stop = Arc::new(AtomicBool::new(false));
        let (ox, oy) = (m.x, m.y);
        let stop_t = stop.clone();
        std::thread::spawn(move || {
            let mut last_warn: Option<Instant> = None;
            while !stop_t.load(Ordering::Relaxed) {
                match gdi_grab(ox, oy, src_w, src_h, dst_w, dst_h) {
                    Ok(f) => {
                        last_warn = None;
                        let _ = tx.try_send(f);
                    }
                    // 每 40ms 一次失败会刷爆日志（26 行/秒），节流到 5 秒一次
                    Err(e) => {
                        if last_warn.is_none_or(|t| t.elapsed() >= Duration::from_secs(5)) {
                            warn!("GDI 采集失败: {e:#}");
                            last_warn = Some(Instant::now());
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(40));
            }
        });
        info!(
            "🖥️ 选用显示器 {} {}x{} → {}x{}（GDI 兜底，进程 {}）",
            m.name,
            src_w,
            src_h,
            dst_w,
            dst_h,
            crate::desktop::describe()
        );
        Ok(Self {
            src_w,
            src_h,
            dst_w,
            dst_h,
            name: m.name.clone(),
            origin_x: ox,
            origin_y: oy,
            pool: None,
            session: None,
            _rt_device: None,
            rx,
            free_tx,
            gdi_stop: Some(stop),
            dda_stop: None,
            backend: "gdi",
        })
    }

    /// 阻塞等下一帧（编码尺寸 BGRA）。采集源关闭时返回 None
    pub fn recv(&self) -> Option<Vec<u8>> {
        self.rx.recv().ok()
    }

    /// 带超时等帧：超时返回 None（采集循环借此轮询切换显示器的命令），源关闭也返回 None
    pub fn recv_timeout(&self, dur: Duration) -> Option<Vec<u8>> {
        // WGC 只采当前用户桌面：锁屏/登录界面/UAC 时它完全不出帧（画面停在最后一帧）。
        // 检测到安全桌面就改用 GDI 直接抓一帧 —— 采集线程已由 gdi_grab 附着到输入桌面，
        // 所以这里拿到的是 Winlogon 桌面的真实画面（SYSTEM 托盘才有这个权限）。
        if self.session.is_some() || self.dda_stop.is_some() {
            if crate::desktop::is_secure_desktop() {
                if let Ok(f) = self.gdi_snapshot() {
                    return Some(f);
                }
            }
        }
        self.rx.recv_timeout(dur).ok()
    }

    /// 首帧：WGC 只在画面变化时出帧，桌面静止可能等不到，
    /// 超时后改用 GDI 截一帧，避免主控端一直停在「等待画面」
    pub fn first_frame(&self, timeout: Duration) -> Option<Vec<u8>> {
        match self.rx.recv_timeout(timeout) {
            Ok(f) => Some(f),
            Err(_) => match self.gdi_snapshot() {
                Ok(f) => {
                    info!("🖥️ 首帧超时（backend={}），改用 GDI 截屏兜底", self.backend);
                    Some(f)
                }
                Err(e) => {
                    warn!("🖥️ GDI 首帧兜底失败: {e:#}");
                    None
                }
            },
        }
    }

    pub fn stop(&self) {
        if let Some(s) = &self.session {
            let _ = s.Close();
        }
        if let Some(p) = &self.pool {
            let _ = p.Close();
        }
        if let Some(f) = &self.gdi_stop {
            f.store(true, Ordering::Relaxed);
        }
        if let Some(f) = &self.dda_stop {
            f.store(true, Ordering::Relaxed);
        }
    }

    /// 归还用完的回读缓冲给采集回调复用（队列满则丢弃，下次重新分配）
    pub fn recycle(&self, buf: Vec<u8>) {
        let _ = self.free_tx.try_send(buf);
    }

    /// GDI 截屏 + StretchBlt 直接缩到编码尺寸（首帧兜底 / GDI 采集线程共用）
    fn gdi_snapshot(&self) -> Result<Vec<u8>> {
        gdi_grab(
            self.origin_x,
            self.origin_y,
            self.src_w,
            self.src_h,
            self.dst_w,
            self.dst_h,
        )
    }
}

/// GDI 截取虚拟桌面中 (origin, src) 区域，并缩放到 (dst_w, dst_h) 的 BGRA
fn gdi_grab(
    origin_x: i32,
    origin_y: i32,
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
) -> Result<Vec<u8>> {
    // 锁屏/UAC 时输入桌面是 Winlogon：线程不切过去，GetDC(NULL) 拿到的是
    // 用户桌面而不是安全桌面，画面会全黑。SYSTEM 托盘才有权限切换。
    // 切不过去就别再往下试了——直接报出窗口站/桌面，否则只会刷满 StretchBlt 失败。
    if !crate::desktop::attach_input_desktop() {
        return Err(anyhow!(
            "无法附着到输入桌面（{}）",
            crate::desktop::describe()
        ));
    }
    let (w, h) = (dst_w as i32, dst_h as i32);
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));

        let mut bi = BITMAPINFO::default();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = w;
        bi.bmiHeader.biHeight = -h; // 负值：自上而下，省一次翻转
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB.0;

        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(Some(mem), &bi, DIB_RGB_COLORS, &mut bits, None, 0);
        let dib = match dib {
            Ok(d) => d,
            Err(e) => {
                let _ = DeleteDC(mem);
                ReleaseDC(None, screen);
                return Err(e).context("CreateDIBSection 失败");
            }
        };

        let old = SelectObject(mem, HGDIOBJ(dib.0));
        let _ = SetStretchBltMode(mem, HALFTONE);
        let ok = StretchBlt(
            mem,
            0,
            0,
            w,
            h,
            Some(screen),
            origin_x,
            origin_y,
            src_w as i32,
            src_h as i32,
            SRCCOPY,
        )
        .as_bool();
        // 必须在任何后续 GDI 调用前取错误码，否则会被重置
        let last_err = GetLastError();

        let mut out = vec![0u8; (dst_w * dst_h * 4) as usize];
        if ok && !bits.is_null() {
            std::ptr::copy_nonoverlapping(bits as *const u8, out.as_mut_ptr(), out.len());
        }

        SelectObject(mem, old);
        let _ = DeleteObject(HGDIOBJ(dib.0));
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);

        if !ok {
            return Err(anyhow!(
                "StretchBlt 失败（err={}，{}x{}@{},{} → {}x{}，{}）",
                last_err.0,
                src_w,
                src_h,
                origin_x,
                origin_y,
                dst_w,
                dst_h,
                crate::desktop::describe()
            ));
        }
        Ok(out)
    }
}

impl ScreenCapture for Capture {
    fn start_for(monitor: Option<usize>) -> Result<Self> {
        Capture::start_for(monitor)
    }
    fn recv_timeout(&self, dur: Duration) -> Option<Vec<u8>> {
        self.recv_timeout(dur)
    }
    fn first_frame(&self, timeout: Duration) -> Option<Vec<u8>> {
        self.first_frame(timeout)
    }
    fn stop(&self) {
        self.stop();
    }
    fn recycle(&self, buf: Vec<u8>) {
        self.recycle(buf);
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop();
    }
}

// ============================================================
// DXGI 桌面复制（DDA）
// ============================================================

/// 找到与 HMONITOR 对应的 DXGI 输出（DDA 必须先拿到具体输出）
fn find_output(device: &ID3D11Device, hmon: HMONITOR) -> Result<IDXGIOutput1> {
    unsafe {
        let dxgi: IDXGIDevice = device.cast().context("D3D11 设备转 DXGI 失败")?;
        let adapter = dxgi.GetAdapter().context("取 DXGI 适配器失败")?;
        let mut i = 0u32;
        while let Ok(output) = adapter.EnumOutputs(i) {
            if let Ok(desc) = output.GetDesc() {
                if desc.Monitor.0 == hmon.0 {
                    return output.cast().context("DXGI 输出转 IDXGIOutput1 失败");
                }
            }
            i += 1;
            if i > 8 {
                break;
            }
        }
        Err(anyhow!("适配器上没有与 HMONITOR 匹配的输出"))
    }
}

/// DDA 采集线程：`AcquireNextFrame` 事件驱动 → GPU 缩放 → 回读 → 交给编码流水。
///
/// 关键点：
/// - 用 16ms 超时轮询（文档 P1-B）：有更新立即返回；静止桌面时也不会因为超时过长
///   而把"新帧已经产生"拖到下一个 100ms 周期，降低交互延迟。
/// - 复制会话在锁屏/UAC/分辨率切换后失效（ACCESS_LOST），这里自动重建，
///   重建期间由 `recv_timeout` 的 GDI 安全桌面兜底出帧。
/// - 按目标帧率限速（高刷屏 120Hz 会把队列打满）。
#[allow(clippy::too_many_arguments)]
fn dda_loop(
    mut dupl: IDXGIOutputDuplication,
    output: IDXGIOutput1,
    device: ID3D11Device,
    gpu: Gpu,
    tx: SyncSender<Vec<u8>>,
    free_rx: Receiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
    interval: Duration,
    origin_x: i32,
    origin_y: i32,
    name: String,
    dst_w: u32,
    dst_h: u32,
) {
    let _ = (origin_x, origin_y); // 与 GDI 兜底共用 origin，这里不需要
    let mut buf: Vec<u8> = free_rx.try_recv().unwrap_or_else(|_| Vec::new());
    let mut last_warn: Option<Instant> = None;
    // 重建次数与上次重建时刻：故障时 AcquireNextFrame 立刻返回 ACCESS_LOST、
    // DuplicateOutput 却秒成功，不退避会形成每秒数千次的"重建→立即失效"驱动风暴
    let mut rebuilds: u32 = 0;
    let mut last_rebuild: Option<Instant> = None;
    let mut gpu_ms_avg = 0.0f64;
    let mut gpu_samples: u32 = 0;
    while !stop.load(Ordering::Relaxed) {
        let t0 = Instant::now();
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res: Option<IDXGIResource> = None;
        let acquired = unsafe { dupl.AcquireNextFrame(16, &mut info, &mut res) };
        match acquired {
            Ok(()) => {
                if let Some(r) = res.take() {
                    match r.cast::<ID3D11Texture2D>() {
                        Ok(tex) => {
                            // 单独计时 GPU 下采样+回读：整机 GPU 被占满时这里会明显变长
                            let tc = Instant::now();
                            let cres = gpu.convert_texture(&tex, &mut buf);
                            let conv_ms = tc.elapsed().as_secs_f64() * 1000.0;
                            gpu_ms_avg += conv_ms;
                            gpu_samples += 1;
                            if gpu_samples >= 30 {
                                let avg = gpu_ms_avg / gpu_samples as f64;
                                if avg > 100.0 {
                                    warn!(
                                        "🖥️ GPU 下采样+回读偏慢（{name} 平均 {avg:.0}ms），GPU 可能被其它任务占满"
                                    );
                                }
                                gpu_ms_avg = 0.0;
                                gpu_samples = 0;
                            }
                            match cres {
                                Ok(()) => {
                                    buf = {
                                        let out = std::mem::take(&mut buf);
                                        match tx.try_send(out) {
                                            Ok(()) => free_rx.try_recv().unwrap_or_default(),
                                            // 编码跟不上：丢弃本帧，取回缓冲继续
                                            Err(std::sync::mpsc::TrySendError::Full(v)) => v,
                                            Err(std::sync::mpsc::TrySendError::Disconnected(v)) => v,
                                        }
                                    };
                                }
                                Err(e) => {
                                    if last_warn
                                        .is_none_or(|t| t.elapsed() >= Duration::from_secs(5))
                                    {
                                        warn!("DDA 帧转换失败（{name} {dst_w}x{dst_h}）: {e:#}");
                                        last_warn = Some(Instant::now());
                                    }
                                    buf = tx_recover(buf, &free_rx);
                                }
                            }
                        }
                        Err(e) => debug!("桌面复制资源转纹理失败: {e}"),
                    }
                }
                let _ = unsafe { dupl.ReleaseFrame() };
                rebuilds = 0;
                // 限速到目标帧率
                if let Some(rest) = interval.checked_sub(t0.elapsed()) {
                    std::thread::sleep(rest);
                }
            }
            Err(e) => {
                let code = e.code();
                if code == DXGI_ERROR_WAIT_TIMEOUT {
                    continue;
                }
                // 锁屏/UAC/分辨率切换/驱动异常后复制会话失效 → 退避重建
                if code == DXGI_ERROR_ACCESS_LOST
                    || code == DXGI_ERROR_INVALID_CALL
                    || code == DXGI_ERROR_NOT_CURRENTLY_AVAILABLE
                {
                    rebuilds += 1;
                    // 关键：重建必须限速（两次至少间隔 500ms），否则会"重建→立刻失效"风暴
                    if let Some(t) = last_rebuild {
                        let elapsed = t.elapsed();
                        if elapsed < Duration::from_millis(500) {
                            std::thread::sleep(Duration::from_millis(500) - elapsed);
                        }
                    }
                    last_rebuild = Some(Instant::now());
                    if rebuilds <= 3 || rebuilds % 100 == 0 {
                        warn!("🖥️ DDA 复制会话失效（{name}）：{e}，第 {rebuilds} 次重建");
                    }
                    if rebuilds > 10 {
                        warn!(
                            "🖥️ DDA 重建过于频繁（{name} 已达 {rebuilds} 次），放弃 DDA 回退 GDI"
                        );
                        DDA_FAILS.fetch_add(1, Ordering::Relaxed);
                        break;
                    }
                    let mut ok = None;
                    // 单次重建最多试 3 次、间隔 400ms（原来是 25×400ms，会卡住 10s 不出帧）
                    for _ in 0..3 {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        match unsafe { output.DuplicateOutput(&device) } {
                            Ok(d) => {
                                ok = Some(d);
                                break;
                            }
                            Err(_) => std::thread::sleep(Duration::from_millis(400)),
                        }
                    }
                    match ok {
                        Some(d) => {
                            dupl = d;
                            if rebuilds <= 3 {
                                info!("🖥️ DDA 复制会话已重建（{name}）");
                            }
                        }
                        None => {
                            warn!(
                                "🖥️ DDA 复制会话重建失败（{name}），线程退出，等上层重建采集"
                            );
                            DDA_FAILS.fetch_add(1, Ordering::Relaxed);
                            break;
                        }
                    }
                    continue;
                }
                if last_warn.is_none_or(|t| t.elapsed() >= Duration::from_secs(5)) {
                    warn!("DDA 采集失败（{name}）: {e}");
                    last_warn = Some(Instant::now());
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
    info!("DDA 采集线程结束（{name}）");
}

/// 转换失败后回收缓冲：优先复用环回回来的，否则清空原缓冲继续用。
/// （Vec 的容量会保留，避免每帧重新分配 ~9MB）
fn tx_recover(buf: Vec<u8>, free_rx: &Receiver<Vec<u8>>) -> Vec<u8> {
    free_rx.try_recv().ok().unwrap_or(buf)
}

fn create_device() -> Result<ID3D11Device> {
    unsafe {
        let mut device = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
        device.context("D3D11CreateDevice 未返回设备")
    }
}

// ============================================================
// GPU 缩放：帧纹理 → VideoProcessorBlt → 常驻 staging → Map
// ============================================================
struct Gpu {
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    /// GPU 缩放输出（编码尺寸）
    target: ID3D11Resource,
    /// 常驻回读纹理，避免每帧新建
    staging: ID3D11Resource,
    out_view: ID3D11VideoProcessorOutputView,
    in_desc: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    w: u32,
    h: u32,
}

impl Gpu {
    fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        src_w: u32,
        src_h: u32,
        dst_w: u32,
        dst_h: u32,
    ) -> Result<Self> {
        let video_device: ID3D11VideoDevice = device.cast().context("取 ID3D11VideoDevice 失败")?;
        let video_context: ID3D11VideoContext =
            context.cast().context("取 ID3D11VideoContext 失败")?;

        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            InputWidth: src_w,
            InputHeight: src_h,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            OutputWidth: dst_w,
            OutputHeight: dst_h,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&content) }
            .context("创建视频处理器枚举器失败")?;
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
            .context("创建视频处理器失败")?;

        let target = create_texture(
            device,
            dst_w,
            dst_h,
            D3D11_USAGE_DEFAULT,
            D3D11_BIND_RENDER_TARGET.0 as u32,
            0,
        )
        .context("创建缩放目标纹理失败")?;
        let staging = create_texture(
            device,
            dst_w,
            dst_h,
            D3D11_USAGE_STAGING,
            0,
            D3D11_CPU_ACCESS_READ.0 as u32,
        )
        .context("创建回读纹理失败")?;
        let target: ID3D11Resource = target.cast()?;
        let staging: ID3D11Resource = staging.cast()?;

        let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut out_view = None;
        unsafe {
            video_device.CreateVideoProcessorOutputView(
                &target,
                &enumerator,
                &out_desc,
                Some(&mut out_view),
            )
        }
        .context("创建视频处理器输出视图失败")?;
        let out_view = out_view.context("视频处理器输出视图为空")?;

        let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };

        Ok(Self {
            context: context.clone(),
            video_device,
            video_context,
            enumerator,
            processor,
            target,
            staging,
            out_view,
            in_desc,
            w: dst_w,
            h: dst_h,
        })
    }

    /// 一帧（WGC）：从帧表面取 D3D11 纹理后走统一缩放/回读路径
    fn convert(&self, frame: &Direct3D11CaptureFrame, out: &mut Vec<u8>) -> Result<()> {
        let surface = frame.Surface().context("取帧表面失败")?;
        let access: IDirect3DDxgiInterfaceAccess =
            surface.cast().context("帧表面转 DXGI 接口失败")?;
        let texture: ID3D11Texture2D = unsafe { access.GetInterface() }.context("取帧纹理失败")?;
        self.convert_texture(&texture, out)
    }

    /// GPU 缩放到编码尺寸 → Map 回读 BGRA 到调用方缓冲（复用，不重新分配）。
    /// WGC 与 DDA 两个后端共用。
    fn convert_texture(&self, texture: &ID3D11Texture2D, out: &mut Vec<u8>) -> Result<()> {
        let texture_res: ID3D11Resource = texture.cast()?;

        let mut in_view: Option<ID3D11VideoProcessorInputView> = None;
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                &texture_res,
                &self.enumerator,
                &self.in_desc,
                Some(&mut in_view),
            )
        }
        .context("创建视频处理器输入视图失败")?;
        let in_view = in_view.context("视频处理器输入视图为空")?;

        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM::default();
        stream.Enable = BOOL(1);
        stream.pInputSurface = std::mem::ManuallyDrop::new(Some(in_view));
        let blt = unsafe {
            self.video_context.VideoProcessorBlt(
                &self.processor,
                &self.out_view,
                0,
                std::slice::from_ref(&stream),
            )
        };
        // pInputSurface 用 ManuallyDrop 声明（结构体本身只借指针），这里手动释放我们持有的引用
        unsafe { std::mem::ManuallyDrop::drop(&mut stream.pInputSurface) };
        blt.context("VideoProcessorBlt（GPU 缩放）失败")?;

        unsafe { self.context.CopyResource(&self.staging, &self.target) };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }
        .context("回读纹理 Map 失败")?;
        let row_bytes = (self.w * 4) as usize;
        out.resize(row_bytes * self.h as usize, 0);
        unsafe {
            let src = mapped.pData as *const u8;
            for row in 0..self.h as usize {
                std::ptr::copy_nonoverlapping(
                    src.add(row * mapped.RowPitch as usize),
                    out.as_mut_ptr().add(row * row_bytes),
                    row_bytes,
                );
            }
            self.context.Unmap(&self.staging, 0);
        }
        Ok(())
    }
}

fn create_texture(
    device: &ID3D11Device,
    w: u32,
    h: u32,
    usage: windows::Win32::Graphics::Direct3D11::D3D11_USAGE,
    bind: u32,
    cpu: u32,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: usage,
        BindFlags: bind,
        CPUAccessFlags: cpu,
        MiscFlags: 0,
    };
    let mut tex = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }
        .context("CreateTexture2D 失败")?;
    tex.context("CreateTexture2D 未返回纹理")
}
