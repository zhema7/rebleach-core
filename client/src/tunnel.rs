//! 隧道通道（tunnel DataChannel）：把被控端的 TCP 服务映射到主控本地端口。
//!
//! 帧格式与 term 通道同风格：`[type: u8][cid: u16 BE][payload]`，
//! 但承载的是**原始 TCP 字节流**（不是 UTF-8 文本），与 term/media 互不共用。
//!
//! | type | 名称 | payload |
//! |------|------|---------|
//! | 0x01 | OPEN | JSON `{ target }`，target 为 `host:port`；特殊值 `ssh` = Agent 内置 SSH 服务端 |
//! | 0x02 | DATA | 原始字节（≤ 16 KB） |
//! | 0x03 | CLOSE | 空（连接关闭，双向均可发） |
//! | 0x04 | ERR  | UTF-8 错误文本（OPEN 失败等） |
//!
//! 用 `cid` 在一条 DC 上复用多个 TCP 连接：`rebleach tunnel` 的每个本地
//! accept、`rebleach-proxy` 的整条 stdio，各占一个 cid。
//! 生命周期跟随会话：DC 关闭即全部断开，进程退出即断开（不做全局路由）。

use serde::{Deserialize, Serialize};

/// 单条 DATA 的最大字节数（与 M1 分片风格一致）
pub const MAX_CHUNK: usize = 16384;

pub const TYPE_OPEN: u8 = 0x01;
pub const TYPE_DATA: u8 = 0x02;
pub const TYPE_CLOSE: u8 = 0x03;
pub const TYPE_ERR: u8 = 0x04;

#[derive(Debug, Clone, PartialEq)]
pub enum TunnelMsg {
    Open { cid: u16, target: String },
    Data { cid: u16, data: Vec<u8> },
    Close { cid: u16 },
    Err { cid: u16, msg: String },
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenPayload {
    target: String,
}

impl TunnelMsg {
    pub fn cid(&self) -> u16 {
        match self {
            TunnelMsg::Open { cid, .. }
            | TunnelMsg::Data { cid, .. }
            | TunnelMsg::Close { cid, .. }
            | TunnelMsg::Err { cid, .. } => *cid,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let (ty, cid, payload): (u8, u16, Vec<u8>) = match self {
            TunnelMsg::Open { cid, target } => (
                TYPE_OPEN,
                *cid,
                serde_json::to_vec(&OpenPayload {
                    target: target.clone(),
                })
                .unwrap_or_default(),
            ),
            TunnelMsg::Data { cid, data } => (TYPE_DATA, *cid, data.clone()),
            TunnelMsg::Close { cid } => (TYPE_CLOSE, *cid, Vec::new()),
            TunnelMsg::Err { cid, msg } => (TYPE_ERR, *cid, msg.as_bytes().to_vec()),
        };
        let mut out = Vec::with_capacity(payload.len() + 3);
        out.push(ty);
        out.extend_from_slice(&cid.to_be_bytes());
        out.extend_from_slice(&payload);
        out
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < 3 {
            return None;
        }
        let ty = buf[0];
        let cid = u16::from_be_bytes([buf[1], buf[2]]);
        let p = &buf[3..];
        Some(match ty {
            TYPE_OPEN => {
                let o: OpenPayload = serde_json::from_slice(p).ok()?;
                if o.target.is_empty() {
                    return None;
                }
                TunnelMsg::Open { cid, target: o.target }
            }
            TYPE_DATA => TunnelMsg::Data { cid, data: p.to_vec() },
            TYPE_CLOSE => TunnelMsg::Close { cid },
            TYPE_ERR => TunnelMsg::Err {
                cid,
                msg: String::from_utf8_lossy(p).to_string(),
            },
            _ => return None,
        })
    }
}

/// 把字节流按 MAX_CHUNK 切片（纯字节切片，无需字符边界处理）。
pub fn split_bytes(data: &[u8]) -> Vec<Vec<u8>> {
    data.chunks(MAX_CHUNK).map(|c| c.to_vec()).collect()
}

// ============================================================
// 被控端：隧道宿主（收到 OPEN 后连接目标，双向泵字节）
// ============================================================

/// 一条隧道连接的写入句柄：DATA 帧送进通道，由写任务落盘到 TCP。
struct ConnHandle {
    input_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
}

#[cfg(all(feature = "desktop", windows))]
pub use host::TunnelHost;

#[cfg(all(feature = "desktop", windows))]
mod host {
    use super::{ConnHandle, TunnelMsg, split_bytes};
    use std::collections::HashMap;
    use std::sync::Arc;

    use parking_lot::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio::sync::mpsc;
    use tracing::{info, warn};
    use webrtc::data_channel::RTCDataChannel;

    /// 被控端隧道复用器：按 cid 管理多条到目标服务的 TCP 连接。
    pub struct TunnelHost {
        dc: Arc<RTCDataChannel>,
        conns: Mutex<HashMap<u16, ConnHandle>>,
        rt: Option<tokio::runtime::Handle>,
    }

    impl TunnelHost {
        pub fn new(dc: Arc<RTCDataChannel>) -> Arc<Self> {
            Arc::new(Self {
                dc,
                conns: Mutex::new(HashMap::new()),
                rt: tokio::runtime::Handle::try_current().ok(),
            })
        }

        fn send(&self, m: TunnelMsg) {
            let Some(rt) = self.rt.clone() else { return };
            let dc = self.dc.clone();
            let bytes = bytes::Bytes::from(m.encode());
            rt.spawn(async move {
                let _ = dc.send(&bytes).await;
            });
        }

        /// 处理一条来自主控端的隧道帧
        pub async fn handle(self: &Arc<Self>, m: TunnelMsg) {
            match m {
                TunnelMsg::Open { cid, target } => self.open(cid, &target).await,
                TunnelMsg::Data { cid, data } => {
                    let tx = self.conns.lock().get(&cid).map(|h| h.input_tx.clone());
                    if let Some(tx) = tx {
                        let _ = tx.send(data).await;
                    } else {
                        self.send(TunnelMsg::Err {
                            cid,
                            msg: format!("连接 {cid} 不存在"),
                        });
                    }
                }
                TunnelMsg::Close { cid } => {
                    let removed = self.conns.lock().remove(&cid);
                    drop(removed); // drop 写句柄 → 写任务退出 → socket 关闭
                    info!("🔌 隧道 cid={cid} 主控端请求关闭");
                }
                TunnelMsg::Err { .. } => {}
            }
        }

        async fn open(self: &Arc<Self>, cid: u16, target: &str) {
            self.close_local(cid);
            let Some(addr) = resolve_target(target).await else {
                self.send(TunnelMsg::Err {
                    cid,
                    msg: format!("无法解析隧道目标: {target}"),
                });
                return;
            };
            match TcpStream::connect(&addr).await {
                Ok(stream) => {
                    info!("🔌 隧道 cid={cid} → {addr} 已连接");
                    self.pump(cid, stream);
                    self.send(TunnelMsg::Open {
                        cid,
                        target: addr.to_string(),
                    });
                }
                Err(e) => {
                    warn!("🔌 隧道 cid={cid} 连接 {addr} 失败: {e}");
                    self.send(TunnelMsg::Err {
                        cid,
                        msg: format!("连接 {addr} 失败: {e}"),
                    });
                }
            }
        }

        /// 启动双向泵：socket → DATA 帧，DATA 帧 → socket
        fn pump(self: &Arc<Self>, cid: u16, stream: TcpStream) {
            let (mut reader, mut writer) = stream.into_split();

            // 写任务：主控端 DATA → TCP
            let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
            tokio::spawn(async move {
                while let Some(buf) = rx.recv().await {
                    if writer.write_all(&buf).await.is_err() {
                        break;
                    }
                }
                // 输入通道关闭：半关写侧，读侧继续（等远端收尾）
                let _ = writer.shutdown().await;
            });

            // 读任务：TCP → DATA 帧 → DC
            let this = self.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                loop {
                    match reader.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            for chunk in split_bytes(&buf[..n]) {
                                this.send(TunnelMsg::Data { cid, data: chunk });
                            }
                        }
                    }
                }
                info!("🔌 隧道 cid={cid} 目标侧关闭，通知主控端");
                this.send(TunnelMsg::Close { cid });
                this.close_local(cid);
            });

            self.conns
                .lock()
                .insert(cid, ConnHandle { input_tx: tx });
        }

        fn close_local(&self, cid: u16) {
            let removed = self.conns.lock().remove(&cid);
            drop(removed);
        }
    }

    /// 隧道目标解析：`ssh` → Agent 内置 SSH 服务端；其余必须是
    /// 被控端**回环/私网**地址（IP:port），拒绝域名与公网目标——
    /// 隧道定位是「把被控端的服务临时拉到主控本地」，不做代理跳板。
    async fn resolve_target(target: &str) -> Option<std::net::SocketAddr> {
        if target == "ssh" {
            let port = crate::sshd::ensure_started().await.ok()?;
            return Some(std::net::SocketAddr::from(([127, 0, 0, 1], port)));
        }
        let (h, p) = target.rsplit_once(':')?;
        let port: u16 = p.parse().ok()?;
        let host = h.trim_start_matches('[').trim_end_matches(']');
        let ip: std::net::IpAddr = match host {
            "localhost" => std::net::IpAddr::from([127, 0, 0, 1]),
            _ => host.parse().ok()?,
        };
        if !is_private_ip(&ip) {
            return None;
        }
        Some(std::net::SocketAddr::new(ip, port))
    }

    pub(crate) fn is_private_ip(ip: &std::net::IpAddr) -> bool {
        match ip {
            std::net::IpAddr::V4(v4) => {
                v4.is_private() || v4.is_loopback() || v4.is_link_local()
            }
            std::net::IpAddr::V6(v6) => v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all_variants() {
        for m in [
            TunnelMsg::Open {
                cid: 1,
                target: "127.0.0.1:3306".into(),
            },
            TunnelMsg::Open {
                cid: 2,
                target: "ssh".into(),
            },
            TunnelMsg::Data {
                cid: 3,
                data: vec![0, 1, 2, 0xFF, 0xFE],
            },
            TunnelMsg::Close { cid: 4 },
            TunnelMsg::Err {
                cid: 5,
                msg: "连接失败".into(),
            },
        ] {
            let buf = m.encode();
            assert_eq!(TunnelMsg::decode(&buf).unwrap(), m);
        }
    }

    #[test]
    fn header_layout() {
        let buf = TunnelMsg::Data {
            cid: 0x1234,
            data: b"abc".to_vec(),
        }
        .encode();
        assert_eq!(buf[0], TYPE_DATA);
        assert_eq!(&buf[1..3], &[0x12, 0x34]);
        assert_eq!(&buf[3..], b"abc");
    }

    #[test]
    fn decode_rejects_bad_input() {
        assert!(TunnelMsg::decode(&[]).is_none());
        assert!(TunnelMsg::decode(&[TYPE_DATA, 0]).is_none());
        assert!(TunnelMsg::decode(&[0x7f, 0, 0]).is_none());
        assert!(TunnelMsg::decode(&[TYPE_OPEN, 0, 0, b'{']).is_none());
        assert!(TunnelMsg::decode(&[TYPE_OPEN, 0, 0, b'{', b'}']).is_none()); // 空 target
    }

    #[test]
    fn split_bytes_chunks() {
        let data = vec![0u8; MAX_CHUNK + 1];
        let parts = split_bytes(&data);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].len(), MAX_CHUNK);
        assert_eq!(parts[1].len(), 1);
        assert_eq!(split_bytes(&[]).len(), 0);
    }

    #[test]
    fn private_ip_filter() {
        // 隧道只允许回环/私网目标：公网 IP 与域名都会被 resolve_target 拒绝
        #[cfg(all(feature = "desktop", windows))]
        {
            use std::net::IpAddr;
            let host_fn = || {
                host::is_private_ip(&"127.0.0.1".parse::<IpAddr>().unwrap())
                    && host::is_private_ip(&"192.168.1.10".parse::<IpAddr>().unwrap())
                    && !host::is_private_ip(&"1.2.3.4".parse::<IpAddr>().unwrap())
                    && !host::is_private_ip(&"8.8.8.8".parse::<IpAddr>().unwrap())
            };
            assert!(host_fn());
        }
        #[cfg(not(all(feature = "desktop", windows)))]
        assert!(true);
    }
}
