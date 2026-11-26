//! M1 媒体帧协议：18 字节帧头 + 1100B 分片，在不可靠、无序的 DataChannel 上重组。
//!
//! 设计要点（见《下一阶段网络与传输优化方案》4.2）：
//! - 每个 SCTP 消息 = 18B 帧头 + ≤1100B 负载，避免 IP 分片；
//! - 只维护"当前正在拼的一帧"，收到更新的 frame_seq 立即丢弃未拼完的旧帧；
//! - 缺片 / 超时 → 丢整帧并请求关键帧（限流 1 次/秒）；
//! - 关键帧允许更长等待（500ms），但不阻塞后续帧。
//!
//! 输出的完整帧格式与旧 `FrameAssembler` 一致：`[1B key][Annex-B]`。

/// 帧头魔数（'R'），用于与旧版 `[seq:4][len:4]` / 纯净 Annex-B 区分。
pub const MAGIC: u8 = 0x52;
pub const VERSION: u8 = 1;
pub const HDR_LEN: usize = 18;
/// 单分片负载上限：1100B + 18B 头 ≪ 常见 1400 MTU，避免 IP 层分片。
pub const FRAG_BYTES: usize = 1100;
/// 单帧上限，防异常头导致巨额分配
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

const FLAG_KEY: u8 = 0x01;
const FLAG_LAST: u8 = 0x02;

/// 非关键帧缺片/超时的等待上限
const WAIT_MS: u64 = 200;
/// 关键帧缺片/超时的等待上限
const WAIT_KEY_MS: u64 = 500;
/// 关键帧请求最小间隔
const KEYFRAME_REQ_INTERVAL_MS: u64 = 1000;

/// 把一整帧切成若干分片消息（每个都是可直接 `dc.send` 的完整 SCTP 消息）。
pub fn fragment_frame(
    frame_seq: u32,
    capture_ts_ms: u32,
    key: bool,
    w: u16,
    h: u16,
    data: &[u8],
) -> Vec<Vec<u8>> {
    let frag_cnt = data.len().div_ceil(FRAG_BYTES).max(1) as u16;
    let mut out = Vec::with_capacity(frag_cnt as usize);
    for (idx, chunk) in data.chunks(FRAG_BYTES).enumerate() {
        let idx = idx as u16;
        let mut msg = Vec::with_capacity(HDR_LEN + chunk.len());
        let mut flags = 0u8;
        if key {
            flags |= FLAG_KEY;
        }
        if idx + 1 == frag_cnt {
            flags |= FLAG_LAST;
        }
        msg.push(MAGIC);
        msg.push((VERSION << 4) | flags);
        msg.extend_from_slice(&frame_seq.to_le_bytes());
        msg.extend_from_slice(&idx.to_le_bytes());
        msg.extend_from_slice(&frag_cnt.to_le_bytes());
        msg.extend_from_slice(&capture_ts_ms.to_le_bytes());
        msg.extend_from_slice(&w.to_le_bytes());
        msg.extend_from_slice(&h.to_le_bytes());
        msg.extend_from_slice(chunk);
        out.push(msg);
    }
    out
}

/// 是否是 M1 媒体帧（否则交给旧版 FrameAssembler 处理）
pub fn is_media_frame(msg: &[u8]) -> bool {
    msg.len() > HDR_LEN && msg[0] == MAGIC && (msg[1] >> 4) == VERSION
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Header {
    key: bool,
    frame_seq: u32,
    frag_idx: u16,
    frag_cnt: u16,
}

fn parse_header(msg: &[u8]) -> Option<Header> {
    if !is_media_frame(msg) {
        return None;
    }
    let flags = msg[1] & 0x0f;
    let frag_cnt = u16::from_le_bytes([msg[8], msg[9]]);
    if frag_cnt == 0 {
        return None;
    }
    Some(Header {
        key: flags & FLAG_KEY != 0,
        frame_seq: u32::from_le_bytes([msg[2], msg[3], msg[4], msg[5]]),
        frag_idx: u16::from_le_bytes([msg[6], msg[7]]),
        frag_cnt,
    })
}

/// 接收侧重组成帧。
#[derive(Default)]
pub struct MediaReassembler {
    seq: Option<u32>,
    key: bool,
    frag_cnt: u16,
    received: u16,
    frags: Vec<Option<Vec<u8>>>,
    started_ms: u64,
    /// 成功重组帧数
    pub frames: u64,
    /// 丢弃的整帧数
    pub lost: u64,
    /// 成功重组帧的负载字节数
    pub bytes: u64,
    /// 上一完整帧的到达时刻（用于反馈窗口统计）
    pub last_frame_ms: u64,
    /// 最近一次成功重组的 frame_seq：更旧的分片直接忽略
    last_done: Option<u32>,
    need_keyframe: bool,
    last_req_ms: Option<u64>,
    // 抖动估计：到达间隔相对均值的偏差（EWMA）
    last_msg_ms: u64,
    avg_interval: f32,
    jitter: f32,
}

impl MediaReassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一个 SCTP 消息。凑齐一帧时返回 `[1B key][Annex-B]`。
    pub fn feed(&mut self, msg: &[u8], now_ms: u64) -> Option<Vec<u8>> {
        let h = parse_header(msg)?;
        self.track_jitter(now_ms);

        if h.frag_idx >= h.frag_cnt
            || (h.frag_cnt as usize).saturating_mul(FRAG_BYTES) > MAX_FRAME_BYTES
        {
            return None;
        }

        // 已经完成过更新的帧：迟到的旧分片一律丢弃
        if let Some(done) = self.last_done
            && (h.frame_seq == done || !is_newer(h.frame_seq, done))
        {
            return None;
        }

        match self.seq {
            Some(cur) if h.frame_seq == cur => {}
            Some(cur) if is_newer(h.frame_seq, cur) => {
                // 更新的帧到了：旧帧已不可能拼完，立即丢弃（不等超时）
                if self.received < self.frag_cnt {
                    self.lost += 1;
                    self.need_keyframe = true;
                }
                self.begin(h, now_ms);
            }
            Some(_) => return None, // 迟到的旧帧分片，直接丢
            None => self.begin(h, now_ms),
        }

        let slot = &mut self.frags[h.frag_idx as usize];
        if slot.is_none() {
            *slot = Some(msg[HDR_LEN..].to_vec());
            self.received += 1;
        }
        if self.received < self.frag_cnt {
            return None;
        }

        // 收齐：按序拼接
        let total: usize = self.frags.iter().flatten().map(Vec::len).sum();
        let mut out = Vec::with_capacity(total + 1);
        out.push(self.key as u8);
        for f in self.frags.iter_mut() {
            if let Some(v) = f.take() {
                out.extend_from_slice(&v);
            }
        }
        let seq_completed = h.frame_seq;
        self.seq = None;
        self.last_done = Some(seq_completed);
        self.frames += 1;
        self.bytes += total as u64;
        self.last_frame_ms = now_ms;
        Some(out)
    }

    fn begin(&mut self, h: Header, now_ms: u64) {
        self.seq = Some(h.frame_seq);
        self.key = h.key;
        self.frag_cnt = h.frag_cnt;
        self.received = 0;
        self.started_ms = now_ms;
        self.frags.clear();
        self.frags.resize_with(h.frag_cnt as usize, || None);
    }

    /// 周期调用（建议 50–100ms）：超时未收齐 → 丢整帧。
    pub fn poll(&mut self, now_ms: u64) {
        let Some(cur) = self.seq else { return };
        let limit = if self.key { WAIT_KEY_MS } else { WAIT_MS };
        if now_ms.saturating_sub(self.started_ms) < limit {
            return;
        }
        // 记下已判丢的序号：迟到的分片不得让它"复活"再拼一次
        self.last_done = Some(cur);
        self.lost += 1;
        self.need_keyframe = true;
        self.seq = None;
        self.frags.clear();
        self.received = 0;
    }

    /// 取一次关键帧请求（内部限流 1 次/秒）
    pub fn take_keyframe_request(&mut self, now_ms: u64) -> bool {
        if !self.need_keyframe {
            return false;
        }
        if let Some(last) = self.last_req_ms {
            if now_ms.saturating_sub(last) < KEYFRAME_REQ_INTERVAL_MS {
                return false;
            }
        }
        self.last_req_ms = Some(now_ms);
        self.need_keyframe = false;
        true
    }

    /// 到达间隔抖动（ms），供 MediaFeedback 上报
    pub fn jitter_ms(&self) -> f32 {
        self.jitter
    }

    fn track_jitter(&mut self, now_ms: u64) {
        if self.last_msg_ms != 0 {
            let d = now_ms.saturating_sub(self.last_msg_ms) as f32;
            if self.avg_interval == 0.0 {
                self.avg_interval = d;
            } else {
                self.jitter = self.jitter * 0.9 + (d - self.avg_interval).abs() * 0.1;
                self.avg_interval = self.avg_interval * 0.9 + d * 0.1;
            }
        }
        self.last_msg_ms = now_ms;
    }
}

/// u32 序号回绕安全的新旧比较
fn is_newer(a: u32, b: u32) -> bool {
    a.wrapping_sub(b) as i32 > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> u64 {
        0
    }

    #[test]
    fn fragment_and_reassemble_single() {
        let data = b"hello frame".to_vec();
        let msgs = fragment_frame(1, 100, false, 1920, 1080, &data);
        assert_eq!(msgs.len(), 1);
        assert!(is_media_frame(&msgs[0]));
        let mut r = MediaReassembler::new();
        let out = r.feed(&msgs[0], now()).expect("应重组完成");
        assert_eq!(out[0], 0);
        assert_eq!(&out[1..], &data[..]);
        assert_eq!(r.frames, 1);
    }

    #[test]
    fn fragment_multi_and_out_of_order() {
        let data: Vec<u8> = (0..(FRAG_BYTES * 3 + 17)).map(|i| (i % 251) as u8).collect();
        let msgs = fragment_frame(7, 0, true, 1280, 720, &data);
        assert_eq!(msgs.len(), 4);
        let mut r = MediaReassembler::new();
        // 乱序喂：3,1,0,2
        assert!(r.feed(&msgs[3], 1).is_none());
        assert!(r.feed(&msgs[1], 2).is_none());
        assert!(r.feed(&msgs[0], 3).is_none());
        let out = r.feed(&msgs[2], 4).expect("应重组完成");
        assert_eq!(out[0], 1);
        assert_eq!(&out[1..], &data[..]);
    }

    #[test]
    fn missing_frag_drops_whole_frame() {
        let data: Vec<u8> = vec![9u8; FRAG_BYTES * 2];
        let msgs = fragment_frame(10, 0, false, 1920, 1080, &data);
        let mut r = MediaReassembler::new();
        assert!(r.feed(&msgs[0], 1).is_none());
        // 下一帧到来 → 旧帧放弃
        let next = fragment_frame(11, 0, false, 1920, 1080, &data);
        assert!(r.feed(&next[0], 10).is_none());
        assert_eq!(r.lost, 1);
        assert!(r.take_keyframe_request(10));
        // 限流：1 秒内不重复请求（即使再次丢帧）
        let n2 = fragment_frame(12, 0, false, 1920, 1080, &data);
        assert!(r.feed(&n2[0], 600).is_none());
        let n3 = fragment_frame(13, 0, false, 1920, 1080, &data);
        assert!(r.feed(&n3[0], 700).is_none());
        assert!(!r.take_keyframe_request(800));
        assert!(r.take_keyframe_request(1100));
    }

    #[test]
    fn timeout_drops_frame() {
        let data: Vec<u8> = vec![1u8; FRAG_BYTES + 1];
        let msgs = fragment_frame(1, 0, false, 800, 600, &data);
        let mut r = MediaReassembler::new();
        assert!(r.feed(&msgs[0], 0).is_none());
        r.poll(199);
        assert_eq!(r.lost, 0);
        r.poll(201);
        assert_eq!(r.lost, 1);
        assert!(r.take_keyframe_request(201));
    }

    #[test]
    fn keyframe_gets_longer_wait() {
        let data: Vec<u8> = vec![1u8; FRAG_BYTES + 1];
        let msgs = fragment_frame(1, 0, true, 800, 600, &data);
        let mut r = MediaReassembler::new();
        assert!(r.feed(&msgs[0], 0).is_none());
        r.poll(400);
        assert_eq!(r.lost, 0, "关键帧 500ms 内不判丢");
        r.poll(501);
        assert_eq!(r.lost, 1);
    }

    #[test]
    fn late_old_fragment_ignored() {
        let d1 = vec![1u8; 10];
        let d2 = vec![2u8; 10];
        let mut r = MediaReassembler::new();
        let m2 = fragment_frame(2, 0, false, 10, 10, &d2);
        let m1 = fragment_frame(1, 0, false, 10, 10, &d1);
        assert!(r.feed(&m2[0], 1).is_some());
        assert!(r.feed(&m1[0], 2).is_none(), "旧帧迟到分片应忽略");
    }
}
