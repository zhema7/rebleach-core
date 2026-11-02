// 音频管线：被控端麦克风采集 → 16kHz mono i16 PCM → 独立 "audio" DataChannel → 主控端播放
// 选 16kHz/mono 是语音清晰度与带宽的折中：32KB/s（256kbps），纯 Rust 无原生编解码依赖。
// 跨平台靠 cpal（Windows/WASAPI、macOS/CoreAudio、Linux/ALSA/PulseAudio）。
use std::collections::VecDeque;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, SupportedStreamConfig, SupportedStreamConfigRange};
use parking_lot::Mutex;
use tracing::{error, info, warn};
use webrtc::data_channel::RTCDataChannel;

/// 采样率：16kHz 语音段，人声清晰且带宽可控（256kbps raw PCM）
pub const SAMPLE_RATE: u32 = 16_000;
pub const CHANNELS: u16 = 1;
/// 单帧时长 20ms → 320 samples
const FRAME_SAMPLES: usize = (SAMPLE_RATE as usize * 20) / 1000;
/// 每包合并 5 帧（100ms），降低 DataChannel 小包开销
const FRAMES_PER_PACKET: usize = 5;

// ============================================================
// 被控端：采集麦克风并推送 PCM 到 audio DataChannel
// ============================================================
pub struct AudioCapture {
    stream: Stream,
}

// cpal 的 Stream 在某些平台（如 WASAPI）底层句柄可跨线程，但为可移植性被标记为 !Send/!Sync。
// 我们仅持有句柄以维持流存活，drop 时才调用 pause，不存在并发访问，因此安全地声明 Send/Sync。
unsafe impl Send for AudioCapture {}
unsafe impl Sync for AudioCapture {}

impl AudioCapture {
    /// 启动采集：找到支持 i16 单声道的输入设备，按 16kHz 采集，PCM 分块经 DC 二进制帧发出。
    pub fn start(dc: Arc<RTCDataChannel>) -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| anyhow!("无可用音频输入设备"))?;
        info!("🎙️ 音频输入设备: {}", device.name().unwrap_or_default());

        let config = pick_config(&device, true)?;
        let sample_rate = config.sample_rate();
        let channels = config.channels();
        info!(
            "🎙️ 音频采集格式: {}Hz {channels}ch {:?}",
            sample_rate.0,
            config.sample_format()
        );

        // 采集回调不能直接 await（cpal 线程非 tokio 上下文），
        // 用无界 mpsc 把字节转给一个专门的发送任务
        let (pc_tx, mut pc_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let dc_send = dc.clone();
        tokio::spawn(async move {
            while let Some(bytes) = pc_rx.recv().await {
                if dc_send.send(&bytes.into()).await.is_err() {
                    break;
                }
            }
        });

        let stream = device
            .build_input_stream(
                &config.into(),
                move |data: &[i16], _| {
                    let pcm = resample_to_16k_mono(data, sample_rate.0, channels);
                    for block in pcm.chunks(FRAME_SAMPLES * FRAMES_PER_PACKET) {
                        if block.is_empty() {
                            continue;
                        }
                        let bytes: Vec<u8> = block.iter().flat_map(|s| s.to_le_bytes()).collect();
                        // unbounded_channel::send 是同步的，永不阻塞
                        let _ = pc_tx.send(bytes);
                    }
                },
                |err| error!("音频采集错误: {err}"),
                None,
            )
            .context("build_input_stream 失败")?;
        stream.play().context("音频采集 stream.play 失败")?;
        Ok(Self { stream })
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        let _ = self.stream.pause();
    }
}

// ============================================================
// 主控端：从 audio DataChannel 收 PCM 并播放
// ============================================================
/// 播放缓冲：DC 接收线程往里推，cpal 输出回调从中取
#[derive(Clone)]
pub struct AudioSink {
    buf: Arc<Mutex<VecDeque<i16>>>,
}

impl AudioSink {
    /// 推入一段收到的 PCM（i16 切片，已按 16kHz mono）
    pub fn push(&self, pcm: &[i16]) {
        let mut q = self.buf.lock();
        let cap = SAMPLE_RATE as usize;
        // 单次推入超过上限时只保留最后 cap 个样本（丢弃最旧的）
        let pcm = if pcm.len() > cap {
            &pcm[pcm.len() - cap..]
        } else {
            pcm
        };
        // 从队首丢弃旧样本，腾出空间容纳新数据
        let need = q.len() + pcm.len();
        if need > cap {
            let drop_n = need - cap;
            for _ in 0..drop_n {
                q.pop_front();
            }
        }
        q.extend(pcm.iter().copied());
    }
}

pub struct AudioPlayback {
    stream: Stream,
    sink: AudioSink,
}

unsafe impl Send for AudioPlayback {}
unsafe impl Sync for AudioPlayback {}

impl AudioPlayback {
    /// 启动播放：找到支持 i16 单声道的输出设备，输出回调从环形缓冲取数据。
    pub fn start() -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("无可用音频输出设备"))?;
        info!("🔊 音频输出设备: {}", device.name().unwrap_or_default());

        let config = pick_config(&device, false)?;
        let sample_rate = config.sample_rate();
        let channels = config.channels();
        info!(
            "🔊 音频播放格式: {}Hz {channels}ch {:?}",
            sample_rate.0,
            config.sample_format()
        );

        let buf: Arc<Mutex<VecDeque<i16>>> =
            Arc::new(Mutex::new(VecDeque::with_capacity(SAMPLE_RATE as usize)));
        let buf_cb = buf.clone();
        let stream = device
            .build_output_stream(
                &config.into(),
                move |out: &mut [i16], _| {
                    let mut q = buf_cb.lock();
                    // 若缓冲不足，补静音（避免爆音）
                    for s in out.iter_mut() {
                        *s = q.pop_front().unwrap_or(0);
                    }
                },
                |err| error!("音频播放错误: {err}"),
                None,
            )
            .context("build_output_stream 失败")?;
        stream.play().context("音频播放 stream.play 失败")?;
        Ok(Self {
            stream,
            sink: AudioSink { buf },
        })
    }

    pub fn sink(&self) -> &AudioSink {
        &self.sink
    }
}

impl Drop for AudioPlayback {
    fn drop(&mut self) {
        let _ = self.stream.pause();
    }
}

// ============================================================
// 工具：选设备支持的 i16 配置；PCM 重采样；分片发送
// ============================================================
fn pick_config(device: &cpal::Device, input: bool) -> Result<SupportedStreamConfig> {
    let mut best: Option<SupportedStreamConfig> = None;
    // supported_input_configs / supported_output_configs 返回不同的迭代器包装类型，
    // 但元素都是 SupportedStreamConfigRange，统一用 trait object 迭代
    let configs: Box<dyn Iterator<Item = SupportedStreamConfigRange>> = if input {
        Box::new(
            device
                .supported_input_configs()
                .map_err(|e| anyhow!("枚举音频配置失败: {e}"))?,
        )
    } else {
        Box::new(
            device
                .supported_output_configs()
                .map_err(|e| anyhow!("枚举音频配置失败: {e}"))?,
        )
    };
    for c in configs {
        if c.sample_format() != SampleFormat::I16 {
            continue;
        }
        let ch = c.channels();
        if ch != CHANNELS as u16 {
            continue;
        }
        let sr = c
            .min_sample_rate()
            .0
            .max(SAMPLE_RATE)
            .min(c.max_sample_rate().0);
        let cfg = c.with_sample_rate(cpal::SampleRate(sr));
        // 命中 16kHz 直接用
        if sr == SAMPLE_RATE {
            return Ok(cfg);
        }
        best = Some(cfg);
    }
    best.ok_or_else(|| anyhow!("设备不支持 i16 单声道音频"))
}

/// 把采集到的 i16 PCM（任意采样率/声道）线性重采样到 16kHz mono
fn resample_to_16k_mono(data: &[i16], src_rate: u32, src_ch: u16) -> Vec<i16> {
    if src_rate == SAMPLE_RATE && src_ch == 1 {
        return data.to_vec();
    }
    // 先混到 mono
    let mono: Vec<i16> = if src_ch == 1 {
        data.to_vec()
    } else {
        data.chunks(src_ch as usize)
            .map(|c| {
                let sum: i32 = c.iter().map(|&s| s as i32).sum();
                (sum / src_ch as i32) as i16
            })
            .collect()
    };
    if src_rate == SAMPLE_RATE {
        return mono;
    }
    // 线性插值重采样
    let ratio = src_rate as f64 / SAMPLE_RATE as f64;
    let out_len = (mono.len() as f64 / ratio) as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_pos = i as f64 * ratio;
        let idx = src_pos as usize;
        let frac = src_pos - idx as f64;
        let a = mono.get(idx).copied().unwrap_or(0);
        let b = mono.get(idx + 1).copied().unwrap_or(a);
        let a = a as f64;
        let b = b as f64;
        out.push((a + (b - a) * frac) as i16);
    }
    out
}

/// 主控端：把 DC 收到的二进制帧解析成 i16 并推入播放缓冲
pub fn feed_playback(sink: &AudioSink, data: &[u8]) {
    if data.len() % 2 != 0 {
        warn!("音频数据长度非偶数，丢弃");
        return;
    }
    let pcm: Vec<i16> = data
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    sink.push(&pcm);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_passthrough_16k_mono() {
        // 16kHz mono 直接原样返回
        let input: Vec<i16> = (0..100).collect();
        let out = resample_to_16k_mono(&input, SAMPLE_RATE, 1);
        assert_eq!(out, input);
    }

    #[test]
    fn resample_stereo_to_mono_mixes_channels() {
        // 立体声：左右声道分别为 100 和 200，mono 混出应为 150
        let input = vec![100i16, 200, 100, 200, 100, 200];
        let out = resample_to_16k_mono(&input, SAMPLE_RATE, 2);
        // 采样率不变，只混声道
        assert_eq!(out.len(), 3);
        assert!(
            out.iter().all(|&s| s == 150),
            "mono 混出应为 150, got {:?}",
            out
        );
    }

    #[test]
    fn resample_48k_to_16k_shrinks_by_third() {
        // 48kHz → 16kHz：输出长度约为输入的 1/3
        let input: Vec<i16> = vec![1000; 96]; // 96 samples @ 48kHz = 2ms
        let out = resample_to_16k_mono(&input, 48_000, 1);
        // 96 * 16000/48000 = 32
        assert_eq!(out.len(), 32);
        // 常量信号重采样后应保持近似值
        assert!(out.iter().all(|&s| (s - 1000).abs() <= 1));
    }

    #[test]
    fn feed_playback_decodes_little_endian_i16() {
        // 构造两个 i16 样本的小端字节：1 和 -1
        let mut data = Vec::new();
        data.extend_from_slice(&1i16.to_le_bytes());
        data.extend_from_slice(&(-1i16).to_le_bytes());

        let sink = AudioSink {
            buf: Arc::new(Mutex::new(VecDeque::new())),
        };
        feed_playback(&sink, &data);

        let q = sink.buf.lock();
        assert_eq!(q.len(), 2);
        assert_eq!(q[0], 1);
        assert_eq!(q[1], -1);
    }

    #[test]
    fn feed_playback_odd_length_ignored() {
        let sink = AudioSink {
            buf: Arc::new(Mutex::new(VecDeque::new())),
        };
        feed_playback(&sink, &[0, 1, 2]); // 3 字节，奇数
        assert!(sink.buf.lock().is_empty());
    }

    #[test]
    fn audio_sink_caps_at_one_second() {
        let sink = AudioSink {
            buf: Arc::new(Mutex::new(VecDeque::new())),
        };
        // 推入超过 1 秒的数据（16001 samples）
        let over = vec![0i16; SAMPLE_RATE as usize + 1];
        sink.push(&over);
        assert_eq!(sink.buf.lock().len(), SAMPLE_RATE as usize);
    }
}
