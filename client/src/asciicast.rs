//! 终端审计：asciicast v2 录制（T4）。
//!
//! asciicast v2 = JSONL：第一行是头部 JSON，之后每行 `[时间s, "o", 输出文本]`。
//! 相比录像体积小约 1000 倍、可全文检索，且 `asciinema play` 等标准工具可直接回放。
//!
//! 与图形侧共用一个 `session_id`（本次会话的随机 ID），写入头部自定义字段
//! `vars`（asciicast 允许额外头部字段），管理端回放器据此把画面与终端并到同一条时间轴。
//!
//! 录制发生在**主控端**（拥有全部输出的那一侧）；默认开启，
//! 环境变量 `RBLEACH_TERM_RECORD=0` 可关（按设备开关策略后续接入 grant.caps）。

use std::collections::HashMap;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use tracing::{info, warn};

/// 默认录制目录：%APPDATA%/rebleach/recordings
pub fn record_dir() -> PathBuf {
    crate::identity::default_key_file()
        .parent()
        .map(|p| p.join("recordings"))
        .unwrap_or_else(|| PathBuf::from("recordings"))
}

/// 全局开关：默认开，RBLEACH_TERM_RECORD=0 关闭
fn enabled() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("RBLEACH_TERM_RECORD").ok().as_deref() != Some("0"))
}

/// 单个终端标签页的录制器
pub struct Recorder {
    writer: BufWriter<std::fs::File>,
    start: Instant,
}

impl Recorder {
    /// 创建录制文件并写入头部。`meta` 里的 session/device/sid 写入 `vars` 供回放关联。
    pub fn create(
        path: PathBuf,
        width: u16,
        height: u16,
        session_id: &str,
        device: &str,
        sid: u16,
        shell: &str,
    ) -> Result<Self> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)
                .with_context(|| format!("创建录制目录失败: {}", p.display()))?;
        }
        let f = std::fs::File::create(&path)
            .with_context(|| format!("创建录制文件失败: {}", path.display()))?;
        let mut w = BufWriter::new(f);
        let header = serde_json::json!({
            "version": 2,
            "width": width,
            "height": height,
            "timestamp": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            "env": {
                "SHELL": shell,
                "TERM": "xterm-256color",
            },
            "vars": {
                "rebleach_session": session_id,
                "rebleach_device": device,
                "rebleach_sid": sid,
            }
        });
        writeln!(w, "{header}").context("写入 asciicast 头部失败")?;
        info!("🎥 终端录制开始: {}", path.display());
        Ok(Self {
            writer: w,
            start: Instant::now(),
        })
    }

    /// 记录一段输出事件（"o" = output）
    pub fn record(&mut self, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        let ev = serde_json::json!([
            format!("{:.6}", self.start.elapsed().as_secs_f64()),
            "o",
            text,
        ]);
        writeln!(self.writer, "{ev}").context("写入 asciicast 事件失败")
    }

    /// 结束录制，flush 落盘
    pub fn finish(&mut self) {
        if let Err(e) = self.writer.flush() {
            warn!("asciicast flush 失败: {e}");
        }
    }
}

/// 读取 asciicast v2 文件，拼接全部 "o" 事件为纯文本（校验/检索用）。
pub fn replay_to_text(path: &std::path::Path) -> Result<String> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读取录制文件失败: {}", path.display()))?;
    let mut out = String::new();
    for (i, line) in content.lines().enumerate() {
        if i == 0 {
            continue; // 头部
        }
        if line.trim().is_empty() {
            continue;
        }
        let ev: serde_json::Value =
            serde_json::from_str(line).with_context(|| format!("第 {} 行不是合法 JSON", i + 1))?;
        if ev.get(1).and_then(|v| v.as_str()) == Some("o") {
            if let Some(text) = ev.get(2).and_then(|v| v.as_str()) {
                out.push_str(text);
            }
        }
    }
    Ok(out)
}

/// 主控端会话级录制登记表：TermOpened → 建 Recorder；TermData → 追加；TermExit → 收尾。
pub struct AuditRegistry {
    session_id: String,
    device: String,
    dir: PathBuf,
    map: Mutex<HashMap<u16, Recorder>>,
}

impl AuditRegistry {
    pub fn new(session_id: String, device: String) -> Arc<Self> {
        Arc::new(Self {
            session_id,
            device,
            dir: record_dir(),
            map: Mutex::new(HashMap::new()),
        })
    }

    pub fn note_open(&self, sid: u16, cols: u16, rows: u16, shell: &str) {
        if !enabled() {
            return;
        }
        let path = self.dir.join(&self.session_id).join(format!("term-{sid}.cast"));
        match Recorder::create(path, cols.max(80), rows.max(24), &self.session_id, &self.device, sid, shell) {
            Ok(r) => {
                self.map.lock().insert(sid, r);
            }
            Err(e) => warn!("终端录制启动失败 (sid={sid}): {e:#}"),
        }
    }

    pub fn note_data(&self, sid: u16, text: &str) {
        if let Some(r) = self.map.lock().get_mut(&sid) {
            if let Err(e) = r.record(text) {
                warn!("终端录制写入失败 (sid={sid}): {e:#}");
            }
        }
    }

    pub fn note_exit(&self, sid: u16) {
        if let Some(mut r) = self.map.lock().remove(&sid) {
            r.finish();
            info!("🎥 终端录制完成 (sid={sid})");
        }
    }

    /// 会话结束：收尾所有未退出的终端
    pub fn finish_all(&self) {
        for (sid, mut r) in self.map.lock().drain() {
            r.finish();
            info!("🎥 终端录制随会话结束 (sid={sid})");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_replay_roundtrip() {
        let dir = std::env::temp_dir().join(format!("rbl-cast-test-{}", std::process::id()));
        let path = dir.join("t.cast");
        let mut r = Recorder::create(path.clone(), 120, 30, "sess-001", "dev-42", 3, "cmd")
            .expect("创建录制器失败");
        r.record("hello ").unwrap();
        r.record("中文\r\n").unwrap();
        r.record("\u{1b}[31mred\u{1b}[0m").unwrap();
        r.finish();

        let text = replay_to_text(&path).expect("回放失败");
        assert_eq!(text, "hello 中文\r\n\u{1b}[31mred\u{1b}[0m");

        // 头部字段可回读：session/device/sid 供回放器关联
        let head = std::fs::read_to_string(&path).unwrap();
        let head_line = head.lines().next().unwrap();
        let v: serde_json::Value = serde_json::from_str(head_line).unwrap();
        assert_eq!(v["version"], 2);
        assert_eq!(v["width"], 120);
        assert_eq!(v["vars"]["rebleach_session"], "sess-001");
        assert_eq!(v["vars"]["rebleach_device"], "dev-42");
        assert_eq!(v["vars"]["rebleach_sid"], 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn registry_lifecycle() {
        let reg = AuditRegistry::new("sess-002".into(), "dev-7".into());
        reg.note_open(1, 100, 25, "cmd");
        reg.note_data(1, "abc");
        reg.note_data(1, "def");
        reg.finish_all();

        let path = record_dir().join("sess-002").join("term-1.cast");
        if path.exists() {
            let text = replay_to_text(&path).unwrap();
            assert_eq!(text, "abcdef");
            let _ = std::fs::remove_file(&path);
        }
        // 未开启的 sid：无副作用
        reg.note_data(9, "x");
        reg.note_exit(9);
    }

    #[test]
    fn empty_record_is_noop() {
        let dir = std::env::temp_dir().join(format!("rbl-cast-empty-{}", std::process::id()));
        let path = dir.join("e.cast");
        let mut r = Recorder::create(path.clone(), 80, 24, "s", "d", 1, "cmd").unwrap();
        r.record("").unwrap();
        r.finish();
        let text = replay_to_text(&path).unwrap();
        assert_eq!(text, "");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
