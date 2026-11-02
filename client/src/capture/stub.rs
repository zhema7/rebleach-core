// 非 Windows 平台的屏幕采集占位实现
// 目前仅 Windows 有自研 WGC 采集；其它平台返回"不支持"错误，
// 保证代码可编译、可链接，待后续接入对应平台采集后端（如 macOS ScreenCaptureKit / Linux PipeWire）。
use std::time::Duration;

use anyhow::{Result, anyhow};

use super::{MonitorInfo, ScreenCapture};

/// 枚举显示器：非 Windows 平台暂不支持，返回空列表
pub fn enumerate() -> Vec<MonitorInfo> {
    Vec::new()
}

/// 采集会话占位：所有方法返回"不支持"
pub struct Capture {
    pub src_w: u32,
    pub src_h: u32,
    pub dst_w: u32,
    pub dst_h: u32,
    pub origin_x: i32,
    pub origin_y: i32,
}

impl Capture {
    pub fn start_for(_monitor: Option<usize>) -> Result<Self> {
        Err(anyhow!("屏幕采集暂不支持当前平台（仅 Windows）"))
    }

    pub fn recv_timeout(&self, _dur: Duration) -> Option<Vec<u8>> {
        None
    }

    pub fn first_frame(&self, _timeout: Duration) -> Option<Vec<u8>> {
        None
    }

    pub fn stop(&self) {}

    pub fn recycle(&self, _buf: Vec<u8>) {}
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
