//! russh 0.45 最小 SSH 服务端实现（T3）。
//!
//! 仅 Windows 桌面构建；被 tunnel 通道的 `ssh` 目标按需拉起，
//! 监听 127.0.0.1 随机端口，永不直接对外。协议范围刻意最小化：
//! - 认证：仅 publickey（授权列表 = ssh_authorized_keys 文件，改动即时生效）
//! - 会话：shell（ConPTY 交互 shell）、exec（一次性命令）、window-change
//! - 端口转发/X11/agent/子系统：全部拒绝（隧道走 tunnel 通道，不走 SSH 转发）

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use russh::keys::key::{KeyPair, PublicKey};
use russh::keys::{PublicKeyBase64, decode_secret_key, encode_pkcs8_pem, parse_public_key_base64};
use russh::server::{Auth, Config as SshConfig, Handler, Msg, Session};
use russh::{Channel, ChannelId, CryptoVec};
use tracing::{info, warn};

// ============================================================
// 密钥管理
// ============================================================

/// 生成 ed25519 密钥对，返回 (私钥 PKCS8-PEM，公钥 OpenSSH 行)
pub fn generate_keypair_openssh(comment: &str) -> Result<(String, String)> {
    let pair = KeyPair::generate_ed25519().context("生成 ed25519 密钥失败")?;
    let public = pair.clone_public_key()?;
    let mut pem = Vec::new();
    encode_pkcs8_pem(&pair, &mut pem).context("编码私钥失败")?;
    let priv_pem = String::from_utf8(pem).context("私钥 PEM 非 UTF-8")?;
    let pub_line = format!(
        "{} {} {comment}\n",
        public.name(),
        public.public_key_base64()
    );
    Ok((priv_pem, pub_line))
}

/// 加载或首次生成 SSH 主机密钥
fn load_or_gen_host_key() -> Result<KeyPair> {
    let path = crate::sshd::host_key_path();
    if let Ok(s) = std::fs::read_to_string(&path)
        && let Ok(k) = decode_secret_key(&s, None)
    {
        return Ok(k);
    }
    warn!("SSH 主机密钥缺失或损坏，重新生成: {}", path.display());
    let k = KeyPair::generate_ed25519().context("生成主机密钥失败")?;
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let mut pem = Vec::new();
    encode_pkcs8_pem(&k, &mut pem)?;
    std::fs::write(&path, &pem)?;
    info!("🔐 已生成 SSH 主机密钥: {}", path.display());
    Ok(k)
}

/// 读取授权公钥列表（每次连接时读取，文件改动即时生效）
fn load_authorized_keys() -> Result<Vec<PublicKey>> {
    let path = crate::sshd::authorized_keys_path();
    let content = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "授权公钥文件不存在: {}（用 `rebleach-client keygen --device <id> --add` 生成）",
            path.display()
        )
    })?;
    let mut keys = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // OpenSSH 行："ssh-ed25519 <base64> <注释>"，base64 是第二段
        let base64 = line.split_whitespace().nth(1).unwrap_or(line);
        match parse_public_key_base64(base64) {
            Ok(k) => keys.push(k),
            Err(e) => warn!("授权公钥行解析失败: {e}"),
        }
    }
    if keys.is_empty() {
        return Err(anyhow!("授权公钥文件为空: {}", path.display()));
    }
    Ok(keys)
}

// ============================================================
// 服务端
// ============================================================

/// 惰性启动：绑定 127.0.0.1 随机端口并常驻 accept 循环
pub(crate) async fn start_server() -> Result<u16> {
    let host_key = load_or_gen_host_key()?;
    let config = Arc::new(SshConfig {
        inactivity_timeout: None, // 长会话别掐（Tunnel 心跳足够）
        keepalive_interval: Some(std::time::Duration::from_secs(30)),
        keys: vec![host_key],
        ..Default::default()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("绑定 SSH 回环端口失败")?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { break };
            let config = config.clone();
            tokio::spawn(async move {
                let handler = SshHandler::default();
                match russh::server::run_stream(config, stream, handler).await {
                    Ok(session) => {
                        if let Err(e) = session.await {
                            warn!("SSH 会话异常结束: {e}");
                        }
                    }
                    Err(e) => warn!("SSH 握手失败: {e}"),
                }
            });
        }
    });
    Ok(port)
}

// ============================================================
// 连接处理器
// ============================================================

#[derive(Default)]
struct SshHandler {
    /// ChannelId → 已派生的 PTY
    ptys: HashMap<ChannelId, crate::term::PtyRun>,
    /// pty_request 记录的初始尺寸（shell_request 使用）
    last_cols: u16,
    last_rows: u16,
}

impl SshHandler {
    fn kill_pty(&mut self, id: ChannelId) {
        if let Some(p) = self.ptys.remove(&id) {
            p.kill();
        }
    }
}

// russh 的 Handler trait 用 #[async_trait] 定义，impl 必须加同名属性
#[async_trait::async_trait]
impl Handler for SshHandler {
    type Error = anyhow::Error;

    /// 仅 publickey；授权列表每连接现读，文件改动即时生效
    async fn auth_publickey(
        &mut self,
        _user: &str,
        public_key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        let incoming = public_key.public_key_base64();
        match load_authorized_keys() {
            Ok(keys) => {
                let ok = keys
                    .iter()
                    .any(|k| k.public_key_base64() == incoming);
                if ok {
                    info!("🔐 SSH 公钥认证通过");
                    Ok(Auth::Accept)
                } else {
                    warn!("🔐 SSH 公钥不在授权列表，拒绝");
                    Ok(Auth::Reject {
                        proceed_with_methods: None,
                    })
                }
            }
            Err(e) => {
                warn!("🔐 授权列表不可用，拒绝: {e:#}");
                Ok(Auth::Reject {
                    proceed_with_methods: None,
                })
            }
        }
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn pty_request(
        &mut self,
        _channel: ChannelId,
        _term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // 记录初始尺寸（shell_request 时使用）
        self.last_cols = col_width.max(1).min(u16::MAX as u32) as u16;
        self.last_rows = row_height.max(1).min(u16::MAX as u32) as u16;
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.spawn_pty(channel, session, None, self.last_cols, self.last_rows)
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let cmd = String::from_utf8_lossy(data).into_owned();
        info!("🔐 SSH exec: {cmd}");
        // exec 无 PTY 尺寸概念，用默认 120×30
        self.spawn_pty(channel, session, Some(cmd), 120, 30)
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(p) = self.ptys.get(&channel) {
            // std mpsc 的 send 是同步的，直接转发字节流
            let _ = p.input.send(data.to_vec());
        }
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(p) = self.ptys.get(&channel) {
            p.resize(
                col_width.max(1).min(u16::MAX as u32) as u16,
                row_height.max(1).min(u16::MAX as u32) as u16,
            );
        }
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        _channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // stdin 关闭：保留 PTY（半关语义），等 shell 退出
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.kill_pty(channel);
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        _name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // 不支持 sftp 等子系统
        let _ = session.channel_failure(channel);
        Ok(())
    }
}

impl SshHandler {
    /// 派生 PTY 并把输出泵回 SSH 通道；退出码 → exit-status + eof + close
    fn spawn_pty(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
        exec: Option<String>,
        cols: u16,
        rows: u16,
    ) -> Result<(), anyhow::Error> {
        self.kill_pty(channel); // 同通道重复请求：先回收旧进程
        // 回调跑在 PTY 读取线程（非 tokio worker），需要 runtime Handle 做 block_on
        let rt = tokio::runtime::Handle::current();
        let handle = session.handle();

        let out = Arc::new(move |text: String| {
            let _ = rt.block_on(handle.data(channel, CryptoVec::from(text.into_bytes())));
        });
        let rt2 = tokio::runtime::Handle::current();
        let handle2 = session.handle();
        let on_exit: Arc<dyn Fn(i32) + Send + Sync> = Arc::new(move |code: i32| {
            let _ = rt2.block_on(async {
                let _ = handle2.exit_status_request(channel, code.max(0) as u32).await;
                let _ = handle2.eof(channel).await;
                let _ = handle2.close(channel).await;
            });
        });

        let pty = match exec {
            Some(cmd) => {
                info!("🔐 SSH exec ({}×{}): {cmd}", cols.max(80), rows.max(24));
                crate::term::spawn_exec(&cmd, None, out, on_exit)?
            }
            None => {
                let (c, r) = (cols.max(80), rows.max(24));
                info!("🔐 SSH shell ({c}×{r})");
                crate::term::spawn_interactive("cmd", c, r, None, out, on_exit)?
            }
        };
        self.ptys.insert(channel, pty);
        Ok(())
    }
}
