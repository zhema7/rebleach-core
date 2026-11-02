//! 虚拟显示器保障：让"没有物理显示器"的机器也能有可采集的桌面。
//!
//! - 优先复用系统里已存在的虚拟显示器（如 usbmmidd 的 WinDisc）
//! - 分辨率偏低时用 ChangeDisplaySettingsEx 提升到 1920x1080（驱动支持的最接近模式）
//! - 完全没有任何显示器时，尝试从随包目录安装 IDD 驱动
//! - 主控端可请求增删虚拟屏：`add_virtual_displays(count, w, h)` / `remove_virtual_display(index)`
//!   插拔统一走随包的 `PlugMonitor.ps1`（发 usbmmidd 的 IOCTL_PLUG），插好后
//!   用 QueryDisplayConfig + SetDisplayConfig 把新屏紧贴到物理屏右侧
//!
//! 注意：显示设置是按会话生效的，本函数必须在**用户会话内的托盘进程**里调用，
//! 会话 0 的服务调用不会影响用户桌面。

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tracing::{info, warn};

/// 目标分辨率上限
const TARGET_W: u32 = 1920;
const TARGET_H: u32 = 1080;
/// 低于这个宽度视为不可用的虚拟屏（例如驱动默认的 640x480/800x600）
const MIN_USABLE_W: u32 = 1024;

/// 虚拟显示驱动类型（`detect_driver` 探测）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverKind {
    /// 没探测到任何可用的虚拟显示驱动
    None,
    /// usbmmidd（USB Mobile Monitor Virtual Display）：用 `iddcmd.exe` 动态增删屏
    /// 典型 PnP ID: ROOT\DISPLAY\0000 / USB\VID_xxxx&PID_xxxx
    UsbMmidd,
    /// IddSampleDriver（微软官方 IDD）：用 IOCTL 或 iddsampleclient.exe 管理
    IddSampleDriver,
}

impl DriverKind {
    pub fn label(self) -> &'static str {
        match self {
            DriverKind::None => "无虚拟屏驱动",
            DriverKind::UsbMmidd => "usbmmidd",
            DriverKind::IddSampleDriver => "IddSampleDriver",
        }
    }
}

/// 探测驱动状态，返回 (has_driver, driver_kind_label, hint)。
/// 用于注册到 client::control::register_virtual_handlers。
pub fn detect_status() -> (bool, String, String) {
    let kind = detect_driver();
    let has_driver = kind != DriverKind::None;
    let label = kind.label().to_string();
    let hint = if has_driver {
        "驱动已就绪，可通过主控端远程增删虚拟屏".into()
    } else {
        "未探测到虚拟屏驱动。请以管理员权限运行安装目录下 virtual-display\\Install-VirtualDisplay.ps1 安装 usbmmidd".into()
    };
    (has_driver, label, hint)
}

/// 兜底路径：调用随包的 PlugMonitor.ps1 发同一套 IOCTL_PLUG。
/// 该脚本与 Install-VirtualDisplay.ps1 的 PlugMon::Run 使用完全相同的
/// GUID / IOCTL / 4 字节缓冲，已在现场手动验证可行。
#[cfg(windows)]
fn plug_via_powershell(count: u32, plug_in: bool) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let dir = driver_dir().ok_or_else(|| anyhow!("找不到 virtual-display 资源目录"))?;
    let script = dir.join("PlugMonitor.ps1");
    if !script.is_file() {
        bail!("找不到兜底脚本: {}", script.display());
    }

    let ps = if cfg!(target_pointer_width = "32") {
        // 32 位进程访问 System32 会被 WOW64 重定向到 SysWOW64（32 位 PowerShell），
        // 必须用 Sysnative 才能拿到 64 位解释器
        PathBuf::from(r"C:\Windows\Sysnative\WindowsPowerShell\v1.0\powershell.exe")
    } else {
        PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe")
    };

    let mode = if plug_in { "plug" } else { "unplug" };
    info!("🖥️ 兜底: {} -Mode {mode} -Count {count}", script.display());
    let out = std::process::Command::new(&ps)
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-WindowStyle",
            "Hidden",
            "-File",
            &script.to_string_lossy(),
            "-Mode",
            mode,
            "-Count",
            &count.to_string(),
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| anyhow!("启动 PowerShell 失败: {e}"))?;

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    info!("🖥️ 兜底脚本输出: {}", stdout.trim().replace('\n', " | "));
    if !stderr.trim().is_empty() {
        warn!("🖥️ 兜底脚本 stderr: {}", stderr.trim());
    }
    if !out.status.success() {
        bail!("PlugMonitor.ps1 退出码 {:?}: {}", out.status.code(), stderr.trim());
    }
    if !stdout.contains("RESULT:OK") {
        bail!("PlugMonitor.ps1 未返回成功: {}", stdout.trim().replace('\n', " | "));
    }
    Ok(())
}

pub fn ensure() {
    #[cfg(windows)]
    unsafe {
        ensure_win()
    }
}

/// 打印当前显示器状态，便于日志排查
pub fn log_state(tag: &str) {
    let monitors = rebleach_client::capture::enumerate();
    if monitors.is_empty() {
        warn!("🖥️ [{tag}] 当前没有任何显示器");
        return;
    }
    for m in &monitors {
        info!(
            "🖥️ [{tag}] 显示器 {} {}x{}{}",
            m.name,
            m.width,
            m.height,
            if m.primary { "（主）" } else { "" }
        );
    }
}

#[cfg(windows)]
unsafe fn ensure_win() {
    let mut monitors = rebleach_client::capture::enumerate();

    if monitors.is_empty() {
        warn!("🖥️ 未检测到任何显示器，尝试安装随包虚拟显示器驱动");
        if try_install_driver() {
            std::thread::sleep(Duration::from_secs(3));
            monitors = rebleach_client::capture::enumerate();
        }
        if monitors.is_empty() {
            warn!("🖥️ 仍无可用显示器，远程画面将无法采集");
            return;
        }
    }

    for m in &monitors {
        if m.width >= MIN_USABLE_W {
            continue;
        }
        match unsafe { best_mode(&m.name) } {
            Some(dm) => {
                let dev: Vec<u16> = m.name.encode_utf16().chain(std::iter::once(0)).collect();
                let rc = unsafe {
                    windows::Win32::Graphics::Gdi::ChangeDisplaySettingsExW(
                        windows::core::PCWSTR(dev.as_ptr()),
                        Some(&dm),
                        None,
                        windows::Win32::Graphics::Gdi::CDS_UPDATEREGISTRY,
                        None,
                    )
                };
                if rc == windows::Win32::Graphics::Gdi::DISP_CHANGE_SUCCESSFUL {
                    info!(
                        "🖥️ 虚拟屏 {} 分辨率已调整到 {}x{}",
                        m.name, dm.dmPelsWidth, dm.dmPelsHeight
                    );
                    // 给显示子系统一点时间重建桌面
                    std::thread::sleep(Duration::from_millis(500));
                } else {
                    warn!("🖥️ 调整 {} 分辨率失败: {:?}", m.name, rc);
                }
            }
            None => warn!("🖥️ {} 没有可用的高分辨率模式", m.name),
        }
    }

    log_state("虚拟屏");
}

/// 在设备支持的模式里挑一个不超过 1920x1080 的最大 32 位色模式
#[cfg(windows)]
unsafe fn best_mode(device: &str) -> Option<windows::Win32::Graphics::Gdi::DEVMODEW> {
    use windows::Win32::Graphics::Gdi::{
        DEVMODEW, ENUM_DISPLAY_SETTINGS_MODE, EnumDisplaySettingsW,
    };
    let dev: Vec<u16> = device.encode_utf16().chain(std::iter::once(0)).collect();
    let mut best: Option<DEVMODEW> = None;
    let mut best_area = 0u32;
    for i in 0..1024u32 {
        let mut dm = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        let ok = unsafe {
            EnumDisplaySettingsW(
                windows::core::PCWSTR(dev.as_ptr()),
                ENUM_DISPLAY_SETTINGS_MODE(i),
                &mut dm,
            )
        };
        if !ok.as_bool() {
            break;
        }
        if dm.dmBitsPerPel != 32 || dm.dmPelsWidth > TARGET_W || dm.dmPelsHeight > TARGET_H {
            continue;
        }
        let area = dm.dmPelsWidth * dm.dmPelsHeight;
        if area > best_area {
            best_area = area;
            best = Some(dm);
        }
    }
    best
}

/// 尝试安装随包/用户放置的 IDD 虚拟显示驱动。
/// 查找顺序：安装目录下 virtual-display → C:\ProgramData\Rebleach\virtual-display
#[cfg(windows)]
fn try_install_driver() -> bool {
    let dir = driver_dir();
    let Some(dir) = dir else {
        warn!("🖥️ 未找到虚拟显示驱动目录（virtual-display），跳过安装");
        return false;
    };

    // 驱动包自带安装脚本：负责 pnputil 入库 + SetupAPI 建 ROOT\DISPLAY 设备 + IOCTL 插屏
    let ps1 = dir.join("Install-VirtualDisplay.ps1");
    if ps1.exists() {
        let driver_dir = dir.join("usbmmidd");
        let driver_dir = if driver_dir.is_dir() {
            driver_dir
        } else {
            dir.clone()
        };
        let ps = PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe");
        info!("🖥️ 安装虚拟显示器驱动: {}", ps1.display());
        return run(
            &ps,
            &[
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                &ps1.to_string_lossy(),
                "-DriverDir",
                &driver_dir.to_string_lossy(),
            ],
        );
    }

    // 退路：直接执行包内的 install.bat
    let bat = dir.join("install.bat");
    if bat.exists() {
        info!("🖥️ 执行驱动安装脚本: {}", bat.display());
        return run(&bat, &[]);
    }

    // 退路：pnputil 只登记驱动，无法创建虚拟设备，仍需用户手动执行一次
    let mut inf: Option<PathBuf> = None;
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            if e.path().extension().and_then(|s| s.to_str()) == Some("inf") {
                inf = Some(e.path());
                break;
            }
        }
    }
    match inf {
        Some(p) => {
            info!("🖥️ pnputil 登记驱动: {}", p.display());
            let ok = run(
                Path::new("pnputil.exe"),
                &["/add-driver", &p.to_string_lossy(), "/install"],
            );
            warn!("🖥️ pnputil 已执行（虚拟设备需再执行一次驱动包的安装脚本）: {ok}");
            ok
        }
        None => {
            warn!("🖥️ {} 下既没有 install.bat 也没有 .inf", dir.display());
            false
        }
    }
}

#[cfg(windows)]
fn driver_dir() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(p) = exe.parent() {
            candidates.push(p.join("virtual-display"));
            candidates.push(p.join("resources").join("virtual-display"));
        }
    }
    candidates.push(crate::agent::data_dir().join("virtual-display"));
    candidates.into_iter().find(|p| p.is_dir())
}

#[cfg(windows)]
fn run(program: &Path, args: &[&str]) -> bool {
    run_result(program, args).map(|s| s.success()).unwrap_or(false)
}

#[cfg(windows)]
fn run_result(program: &Path, args: &[&str]) -> Result<std::process::ExitStatus> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new(program)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|e| anyhow!("执行 {} 失败: {e}", program.display()))
}

// ============================================================
// 驱动探测 + 主动增删虚拟屏（主控端 AddVirtualDisplay / RemoveVirtualDisplay 消息入口）
// ============================================================

/// 探测当前系统里可用的虚拟显示驱动类型。
/// 判断依据：
/// - `iddcmd.exe` 在 PATH 或驱动目录里 → usbmmidd
/// - PnP 里有 ROOT\DISPLAY\* 或 USB\VID_* → usbmmidd（IOCTL_PLUG 兼容）
pub fn detect_driver() -> DriverKind {
    #[cfg(windows)]
    {
        // 有 ROOT\DISPLAY / USB\VID_ 设备 → usbmmidd（插拔统一走 PlugMonitor.ps1）
        if has_root_display_device() {
            return DriverKind::UsbMmidd;
        }
        DriverKind::None
    }
    #[cfg(not(windows))]
    {
        DriverKind::None
    }
}

/// 新增 N 块虚拟显示器，每块分辨率 width×height。
/// usbmmidd 的 IOCTL 不接受分辨率参数（由注册表 Monitors 的默认值决定），width/height 仅记录日志。
pub fn add_virtual_displays(count: u32, width: u32, height: u32) -> Result<()> {
    if count == 0 {
        bail!("count 必须大于 0");
    }
    let (w, h) = (width.clamp(640, 7680), height.clamp(480, 4320));
    let driver = detect_driver();
    info!(
        "🖥️ 请求新增 {count} 块虚拟屏（{w}x{h}），探测到驱动: {}，进程: x{} {}",
        driver.label(),
        if cfg!(target_pointer_width = "64") { 64 } else { 32 },
        std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default()
    );

    // 分辨率参数目前仅 iddcmd 能传；IOCTL_PLUG 走驱动 INF 里预设的模式表
    // （驱动 INF 里 Parameters\Monitors 已配置好 1024x768~3840x2160，足够覆盖常见场景）

    // 统一走 PlugMonitor.ps1：现场已逐步验证过的唯一路径（GUID/IOCTL/4 字节缓冲/64 位解释器）
    // 不再维护 iddcmd 与 Rust DeviceIoControl 两套并行实现，避免同类事故重复发生。
    match driver {
        DriverKind::UsbMmidd | DriverKind::IddSampleDriver => {
            plug_via_powershell(count, true)?;
        }
        DriverKind::None => bail!(
            "未探测到任何虚拟显示驱动。请先在被控端完成驱动安装：\
             从安装目录的 virtual-display 子目录以管理员权限运行 Install-VirtualDisplay.ps1"
        ),
    }

    // 等 PnP/DWM 把新屏挂上（否则 SetDisplayConfig 看不到它）
    std::thread::sleep(Duration::from_millis(800));

    // 把新插的虚拟屏摆到主屏右侧紧贴，消除坐标空洞（失败只告警，不影响插屏结果）
    if let Err(e) = arrange_virtual_monitors() {
        warn!("🖥️ 自动排布虚拟屏失败（可手动在显示设置里拖动）: {e:#}");
    }
    Ok(())
}

/// 删除一块虚拟显示器。没有 iddcmd 时只能按 LIFO（最后插的先删）。
pub fn remove_virtual_display(index: u32) -> Result<()> {
    let driver = detect_driver();
    info!("🖥️ 请求删除虚拟屏 #{index}，探测到驱动: {}", driver.label());

    // usbmmidd 的 0x00 每次拔掉最早插入的一块（FIFO），不支持按 index 精确拔
    match driver {
        DriverKind::UsbMmidd | DriverKind::IddSampleDriver => plug_via_powershell(1, false),
        DriverKind::None => bail!("未探测到虚拟显示驱动，无法删除虚拟屏"),
    }
}

#[cfg(windows)]
fn has_root_display_device() -> bool {
    let Ok(output) = std::process::Command::new("pnputil.exe")
        .args(["/enum-devices", "/class", "Display"])
        .creation_flags(0x0800_0000u32)
        .output()
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    // pnputil 输出里 Device ID 行形如：
    //   Device ID: ROOT\DISPLAY\0000
    //   Device ID: USB\VID_xxxx&PID_yyyy
    text.contains("ROOT\\DISPLAY\\") || text.contains("USB\\VID_")
}


// ============================================================
// 插屏后自动排布：把虚拟屏摆到「物理屏最右边缘」紧贴着，消除默认落位
// （如 x=7680）造成的坐标空洞，否则本地鼠标跨不过去。
// ChangeDisplaySettingsEx 会被 IDD 驱动回滚，所以走 QueryDisplayConfig + SetDisplayConfig。
// 任何一步失败都只返回 Err，由调用方告警，不影响插屏本身。
// ============================================================
#[cfg(windows)]
fn arrange_virtual_monitors() -> Result<()> {
    use windows::Win32::Devices::Display::{
        DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
        SetDisplayConfig, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, DISPLAYCONFIG_MODE_INFO,
        DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE, DISPLAYCONFIG_PATH_INFO,
        DISPLAYCONFIG_TARGET_DEVICE_NAME, QDC_ALL_PATHS, SDC_ALLOW_CHANGES, SDC_APPLY,
        SDC_USE_SUPPLIED_DISPLAY_CONFIG,
    };

    // 刚插屏时显示配置可能还在变化（QueryDisplayConfig 返回 ERROR_INSUFFICIENT_BUFFER），重试几次
    let mut paths: Vec<DISPLAYCONFIG_PATH_INFO> = Vec::new();
    let mut modes: Vec<DISPLAYCONFIG_MODE_INFO> = Vec::new();
    let mut queried = false;
    for attempt in 0..3u32 {
        let mut num_paths: u32 = 0;
        let mut num_modes: u32 = 0;
        let rc = unsafe { GetDisplayConfigBufferSizes(QDC_ALL_PATHS, &mut num_paths, &mut num_modes) };
        if rc.0 != 0 {
            bail!("GetDisplayConfigBufferSizes 失败: {}", rc.0);
        }
        let mut p = vec![DISPLAYCONFIG_PATH_INFO::default(); num_paths as usize];
        let mut m = vec![DISPLAYCONFIG_MODE_INFO::default(); num_modes as usize];
        let rc = unsafe {
            QueryDisplayConfig(
                QDC_ALL_PATHS,
                &mut num_paths,
                p.as_mut_ptr(),
                &mut num_modes,
                m.as_mut_ptr(),
                None,
            )
        };
        if rc.0 == 0 {
            p.truncate(num_paths as usize);
            m.truncate(num_modes as usize);
            paths = p;
            modes = m;
            queried = true;
            break;
        }
        if attempt == 2 {
            bail!("QueryDisplayConfig 失败（重试 3 次）: {}", rc.0);
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    if !queried {
        bail!("QueryDisplayConfig 未取到显示配置");
    }

    // 只统计真正激活的路径：sourceInfo.modeInfoIdx == 0xFFFFFFFF 表示该路径未激活
    let mut physical_right = 0i32;
    let mut physical_top = i32::MAX;
    let mut virtuals: Vec<(usize, u32)> = Vec::new(); // (路径索引, 源宽)
    for (idx, p) in paths.iter().enumerate() {
        let mi = unsafe { p.sourceInfo.Anonymous.modeInfoIdx } as usize;
        if mi >= modes.len() || modes[mi].infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
            continue;
        }
        let sm = unsafe { modes[mi].Anonymous.sourceMode };
        if sm.width == 0 || sm.height == 0 {
            continue;
        }
        // 取 target 的 device path 判断是否 usbmmidd 虚拟屏（\\?\root#display#0000#...）
        let mut name = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
        name.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
        name.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
        name.header.adapterId = p.targetInfo.adapterId;
        name.header.id = p.targetInfo.id;
        let rc = unsafe { DisplayConfigGetDeviceInfo(&mut name.header) };
        let is_virtual = rc == 0
            && wide_to_string(&name.monitorDevicePath)
                .to_uppercase()
                .contains("ROOT#DISPLAY");

        if is_virtual {
            virtuals.push((idx, sm.width));
        } else {
            physical_right = physical_right.max(sm.position.x + sm.width as i32);
            physical_top = physical_top.min(sm.position.y);
        }
    }

    if virtuals.is_empty() {
        info!("🖥️ 自动排布: 当前没有激活的虚拟屏，跳过");
        return Ok(());
    }
    if physical_right <= 0 || physical_top == i32::MAX {
        info!("🖥️ 自动排布: 没找到物理屏，跳过");
        return Ok(());
    }
    // 按 target id 排序，保证多块虚拟屏每次排列顺序稳定
    virtuals.sort_by_key(|(idx, _)| paths[*idx].targetInfo.id);

    // 期望位置：从物理屏最右边缘起横向紧密排列，纵向与物理屏顶边对齐
    let mut expect: Vec<(usize, i32)> = Vec::new();
    let mut cursor_x = physical_right;
    for (idx, w) in &virtuals {
        expect.push((*idx, cursor_x));
        cursor_x += *w as i32;
    }

    // 写回 source mode 位置
    let mut changed = false;
    for (idx, want_x) in &expect {
        let mi = unsafe { paths[*idx].sourceInfo.Anonymous.modeInfoIdx } as usize;
        if mi >= modes.len() {
            continue;
        }
        let mut sm = unsafe { modes[mi].Anonymous.sourceMode };
        if sm.position.x == *want_x && sm.position.y == physical_top {
            continue;
        }
        info!(
            "🖥️ 自动排布: target {} 从 ({},{}) 移到 ({},{})",
            paths[*idx].targetInfo.id, sm.position.x, sm.position.y, want_x, physical_top
        );
        sm.position.x = *want_x;
        sm.position.y = physical_top;
        unsafe { modes[mi].Anonymous.sourceMode = sm };
        changed = true;
    }

    if !changed {
        info!("🖥️ 自动排布: 位置已正确，无需调整（物理屏右边界 x={physical_right}）");
        return Ok(());
    }

    let flags = SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES;
    let rc = unsafe { SetDisplayConfig(Some(paths.as_slice()), Some(modes.as_slice()), flags) };
    if rc != 0 {
        bail!("SetDisplayConfig 返回 {}", rc);
    }
    info!(
        "🖥️ 自动排布完成: 虚拟屏贴到 x={physical_right}（物理屏顶边 y={physical_top}），共 {} 块",
        virtuals.len()
    );
    Ok(())
}

#[cfg(windows)]
fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}
