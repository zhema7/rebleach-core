//! 跨平台帧格式检测器 + 重组器。
//!
//! 独立于 desktop feature：Android controller 和 Windows controller 都用它
//! 来兼容两种帧格式（由发送端决定，与接收端平台无关）：
//!
//! | 发送端 | 帧格式 |
//! |---|---|
//! | Windows agent | 8-byte `[seq:4LE][len:4LE]` header + 16KB chunked |
//! | Android agent | 纯净 Annex-B（start code 开头），一帧一个 DC message |
//!
//! 检测规则：
//!   - 前 3~4 字节是 H.264 start code → 纯净 Annex-B 直通 finish
//!   - 否则 → 走 header+chunked 重组逻辑

use tracing::{error, info};

/// 主控端：收帧组装器（兼容 header+chunked 和纯净 Annex-B）。
///
/// `out` 是一个闭包，每收完一帧完整 payload 就调用一次。
/// 不再依赖 `tokio::sync::mpsc`，方便 Android 端直接 push 到 FrameQueue。
pub struct FrameAssembler {
    hdr: Vec<u8>,
    payload: Vec<u8>,
    expected: u32,
    seq: u32,
    out: Box<dyn FnMut(Vec<u8>) + Send>,
    frames: u64,
    total: u64,
}

impl FrameAssembler {
    pub fn new<F>(out: F) -> Self
    where
        F: FnMut(Vec<u8>) + Send + 'static,
    {
        Self {
            hdr: Vec::with_capacity(8),
            payload: Vec::new(),
            expected: 0,
            seq: 0,
            out: Box::new(out),
            frames: 0,
            total: 0,
        }
    }

    /// 收一帧（或一帧的一段 chunk）。自动检测格式。
    pub fn feed(&mut self, data: &[u8]) {
        // —— 格式检测：纯净 Annex-B？——
        if data.len() >= 3 {
            let is_anx = (data[0] == 0 && data[1] == 0 && data[2] == 0 && data[3] == 0x01) // 4-byte start code
                      || (data[0] == 0 && data[1] == 0 && data[2] == 0x01)               // 3-byte start code
                      || (data.len() >= 4 && data[0] == 0 && data[1] == 0 && data[2] == 0x00 && data[3] == 0x01); // 兼容变体
            if is_anx {
                let len = data.len();
                (self.out)(data.to_vec());
                self.frames += 1;
                self.total += len as u64;
                if self.frames % 60 == 1 {
                    info!("📥 [Annex-B 直通] 收到 {f} 帧, 共 {t} bytes", f = self.frames, t = self.total);
                }
                // 如果之前有未完成的 header 重组状态，重置它
                self.hdr.clear();
                self.payload.clear();
                self.expected = 0;
                return;
            }
        }
        // —— header+chunked 重组（桌面→桌面 / Windows→Android）——
        self.feed_header_chunks(data);
    }

    fn feed_header_chunks(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            if self.hdr.len() < 8 {
                let need = 8 - self.hdr.len();
                let take = need.min(data.len());
                self.hdr.extend_from_slice(&data[..take]);
                data = &data[take..];
                if self.hdr.len() < 8 {
                    return;
                }
                self.seq = u32::from_le_bytes(self.hdr[0..4].try_into().unwrap());
                self.expected = u32::from_le_bytes(self.hdr[4..8].try_into().unwrap());
                self.payload.clear();
                if self.expected == 0 {
                    self.finish_frame();
                    self.hdr.clear();
                    continue;
                }
                if self.expected > 32 * 1024 * 1024 {
                    error!("帧长度异常 {}，丢弃", self.expected);
                    self.hdr.clear();
                    continue;
                }
            }
            let need = self.expected as usize - self.payload.len();
            let take = need.min(data.len());
            self.payload.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.payload.len() == self.expected as usize {
                self.finish_frame();
                self.hdr.clear();
            }
        }
    }

    fn finish_frame(&mut self) {
        let len = self.payload.len();
        (self.out)(std::mem::take(&mut self.payload));
        self.frames += 1;
        self.total += len as u64;
        if self.frames % 60 == 1 {
            info!(
                "📥 收到 {f} 帧, 共 {t} bytes (seq={s}, len={l})",
                f = self.frames,
                t = self.total,
                s = self.seq,
                l = len
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::Arc;

    fn make_out() -> (Arc<Mutex<Vec<Vec<u8>>>>, FrameAssembler) {
        let out = Arc::new(Mutex::new(Vec::new()));
        let o2 = out.clone();
        let asm = FrameAssembler::new(move |v: Vec<u8>| {
            o2.lock().push(v);
        });
        (out, asm)
    }

    #[test]
    fn annex_b_passthrough() {
        let (out, mut asm) = make_out();
        let pkt = [0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x80, 0x40];
        asm.feed(&pkt);
        let g = out.lock();
        assert_eq!(g.len(), 1);
        assert_eq!(g[0], pkt);
    }

    #[test]
    fn annex_b_3byte_start() {
        let (out, mut asm) = make_out();
        let pkt = [0x00, 0x00, 0x01, 0x41, 0x9A, 0x21];
        asm.feed(&pkt);
        let g = out.lock();
        assert_eq!(g.len(), 1);
        assert_eq!(g[0], pkt);
    }

    #[test]
    fn header_single_chunk() {
        let (out, mut asm) = make_out();
        let payload = b"hello world";
        let mut pkt = Vec::new();
        pkt.extend_from_slice(&1u32.to_le_bytes());
        pkt.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        pkt.extend_from_slice(payload);
        asm.feed(&pkt);
        let g = out.lock();
        assert_eq!(g.len(), 1);
        assert_eq!(g[0], payload);
    }

    #[test]
    fn header_split_across_chunks() {
        let (out, mut asm) = make_out();
        let payload = b"abcdefghij";
        let mut pkt = Vec::new();
        pkt.extend_from_slice(&7u32.to_le_bytes());
        pkt.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        pkt.extend_from_slice(payload);

        asm.feed(&pkt[..6]);
        assert!(out.lock().is_empty(), "头未收完不应出帧");
        asm.feed(&pkt[6..]);
        let g = out.lock();
        assert_eq!(g.len(), 1);
        assert_eq!(g[0], payload);
    }

    #[test]
    fn header_multi_payload_chunks() {
        let (out, mut asm) = make_out();
        let payload: Vec<u8> = (0..500).map(|i| (i % 256) as u8).collect();
        let mut pkt = Vec::new();
        pkt.extend_from_slice(&42u32.to_le_bytes());
        pkt.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        pkt.extend_from_slice(&payload);

        asm.feed(&pkt[..10]);
        assert!(out.lock().is_empty());
        asm.feed(&pkt[10..200]);
        assert!(out.lock().is_empty());
        asm.feed(&pkt[200..]);
        let g = out.lock();
        assert_eq!(g.len(), 1);
        assert_eq!(g[0], payload);
    }

    #[test]
    fn header_zero_payload() {
        let (out, mut asm) = make_out();
        let mut pkt = Vec::new();
        pkt.extend_from_slice(&0u32.to_le_bytes());
        pkt.extend_from_slice(&0u32.to_le_bytes());
        asm.feed(&pkt);
        let g = out.lock();
        assert_eq!(g.len(), 1);
        assert!(g[0].is_empty());
    }
}
