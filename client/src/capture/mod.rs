// 屏幕采集：平台中立接口 + 条件编译分发
// Windows 走自研 WGC + D3D11VideoProcessor GPU 下采样（capture/windows.rs）
// 非 Windows 走 stub（capture/stub.rs），返回"不支持"错误
//
// MonitorInfo 与 Capture 的公开字段/方法在所有平台保持一致，
// 调用方（video.rs / session.rs）无需条件编译。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;

/// 目标帧率（DDA 采集线程限速用；video 在建立采集前调用 `set_target_fps` 写入）
static TARGET_FPS: AtomicU64 = AtomicU64::new(30);

pub fn set_target_fps(fps: u64) {
    TARGET_FPS.store(fps.max(1), Ordering::Relaxed);
}

pub fn target_fps() -> u64 {
    TARGET_FPS.load(Ordering::Relaxed).max(1)
}

/// 平台中立的显示器信息
#[derive(Debug, Clone)]
pub struct MonitorInfo {
    /// 平台中立的显示器句柄：
    /// - Windows: HMONITOR 指针的整数值
    /// - 其它平台: 0（无意义）
    pub handle: u64,
    pub name: String,
    /// 显示器左上角在虚拟桌面中的坐标（GDI 兜底截屏 + 鼠标坐标映射用）
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
    /// 是否为虚拟显示器（usbmmidd / IddSampleDriver / ROOT\DISPLAY 这类间接显示）
    /// 主控端 UI 用它在下拉里加「💻」标识，也决定能否在虚拟屏上安全做 SetResolution
    pub is_virtual: bool,
}

/// 屏幕采集会话的统一接口。各平台 Capture 必须实现这些方法，
/// 且暴露相同的公开字段（src_w/src_h/dst_w/dst_h/origin_x/origin_y）。
pub trait ScreenCapture {
    /// 建立采集会话。`monitor`: Some(i) 只试第 i 个；None 逐个尝试直到成功
    fn start_for(monitor: Option<usize>) -> Result<Self>
    where
        Self: Sized;
    /// 带超时等下一帧（编码尺寸 BGRA）；超时或源关闭返回 None
    fn recv_timeout(&self, dur: Duration) -> Option<Vec<u8>>;
    /// 首帧：WGC 只在画面变化时出帧，桌面静止可能等不到，超时后用 GDI 兜底
    fn first_frame(&self, timeout: Duration) -> Option<Vec<u8>>;
    /// 停止采集（释放帧池/会话）
    fn stop(&self);
    /// 归还用完的回读缓冲给采集回调复用
    fn recycle(&self, buf: Vec<u8>);
}

#[cfg(not(windows))]
mod stub;
#[cfg(windows)]
mod windows;

#[cfg(not(windows))]
pub use stub::{Capture, enumerate};
#[cfg(windows)]
pub use windows::{Capture, enumerate};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_info_is_cloneable_and_debug() {
        let m = MonitorInfo {
            handle: 42,
            name: "test".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary: true,
            is_virtual: false,
        };
        let m2 = m.clone();
        assert_eq!(m2.name, "test");
        assert_eq!(m2.width, 1920);
        assert!(m2.primary);
        assert!(!m2.is_virtual);
        // Debug 输出不 panic
        let _ = format!("{m:?}");
    }

    #[cfg(windows)]
    #[test]
    fn enumerate_returns_at_least_one_monitor() {
        let monitors = enumerate();
        // 在 Windows 上至少有一个显示器；每个 MonitorInfo 字段应合理
        assert!(!monitors.is_empty(), "Windows 上应至少枚举到一个显示器");
        for m in &monitors {
            assert!(m.width > 0 && m.height > 0);
            assert!(!m.name.is_empty());
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn stub_enumerate_returns_empty() {
        assert!(enumerate().is_empty());
    }

    #[cfg(not(windows))]
    #[test]
    fn stub_capture_start_for_errors() {
        assert!(Capture::start_for(None).is_err());
        assert!(Capture::start_for(Some(0)).is_err());
    }
}
