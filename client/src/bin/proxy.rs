//! rebleach-proxy（T3）：OpenSSH ProxyCommand 传输替身。
//!
//! ~/.ssh/config.d/rebleach.conf 里：
//!   ProxyCommand "rebleach-proxy" --server ws://... --device dev-0001 --stdio rblssh
//!
//! ssh 把本进程的 stdin/stdout 当作到 sshd 的原始 TCP 连接；
//! 本进程把字节经 WebRTC tunnel 通道打到被控端：
//! - `rblssh` → Agent 内置 SSH 服务端（被控端无需装 sshd）
//! - `%h:%p`（HostName 127.0.0.1 时）→ 被控端本机 sshd / 任意端口服务
#![cfg(feature = "desktop")]

use clap::Parser;
use rebleach_client::cli::{CliEnv, proxy_stdio};

#[derive(Debug, Parser)]
#[command(
    name = "rebleach-proxy",
    about = "Rebleach ProxyCommand：让标准 ssh 经 WebRTC 直连被控端"
)]
struct Args {
    /// 信令服务器 WebSocket 地址
    #[arg(long, default_value = "ws://127.0.0.1:8088/ws")]
    server: String,

    /// 目标被控端设备 ID
    #[arg(long)]
    device: String,

    /// 主控端本机设备 ID（默认取主机名）
    #[arg(long)]
    id: Option<String>,

    /// 隧道目标：rblssh（内置 SSH 服务端）或 host:port
    #[arg(default_value = "rblssh")]
    target: String,

    /// 局域网直连验证码
    #[arg(long)]
    auth: Option<String>,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr) // stdout 归 ssh 协议，日志只能走 stderr
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let args = Args::parse();
    let target = normalize_target(&args.target);

    let env = CliEnv::new(
        Some(args.server.clone()),
        args.id.clone(),
        None,
        Vec::new(),
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let dev = args.device.clone();
    let auth = args.auth.clone();
    rt.block_on(async move {
        if let Err(e) = proxy_stdio(env, &dev, target, auth).await {
            eprintln!("rebleach-proxy: {e:#}");
            std::process::exit(1);
        }
    });
    Ok(())
}

/// %h:%p 形式透传；rblssh → 内置 SSH 服务端
fn normalize_target(t: &str) -> String {
    if t == "rblssh" || t == "ssh" {
        "ssh".into()
    } else {
        t.to_string()
    }
}
