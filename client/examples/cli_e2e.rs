//! T2/T3 逻辑端到端验证（无界面，无需真实设备）：
//!
//! 1. 进程内迷你信令中继（WebSocket，按 id 转发 + TOFU 公钥表）
//! 2. 拉起一个真实被控端会话（run_session，Role::Controlled，同机同密钥）
//! 3. exec：term 通道跑 `echo ... & exit /b 7`，校验输出与真实退出码
//! 4. tunnel：本地 → 被控端回环 echo 服务，校验双向字节流
//! 5. `ssh` 目标：按需拉起内置 SSH 服务端，校验 TCP 接通
//!    （SSH 协议层握手的实机验证不在本示例范围）
//!
//! 运行：cargo run --example cli_e2e --features desktop
#![cfg(all(feature = "desktop", windows))]

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use rebleach_client::cli::CliEnv;
use rebleach_client::headless::{self, HeadlessConfig, LinkEvent};
use rebleach_client::protocol::{ControlMsg, Role, WsMsg};
use rebleach_client::session::{EventSink, SessionConfig, SessionEvent};
use rebleach_client::tunnel::TunnelMsg;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

fn main() -> Result<()> {
    println!("=== rebleach cli_e2e（T2/T3 逻辑链路） ===");
    // 初始化日志（RUST_LOG=debug 可看更多），便于观察 ICE/WebRTC 状态
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .try_init();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run())?;
    // 被控端会话是常驻循环，进程级收尾
    std::process::exit(0);
}

async fn run() -> Result<()> {
    // 1. 迷你信令中继
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("绑定迷你信令端口失败")?;
    let port = listener.local_addr()?.port();
    let url = format!("ws://127.0.0.1:{port}/ws");
    tokio::spawn(mini_signaling(listener));
    println!("[信令] {url}");

    // 2. 被控端/主控端：各自独立身份目录（模拟两台机器；
    //    若共用目录，两个进程对同一 TOFU 文件的读改写会互相覆盖）
    let key_dir = std::env::temp_dir().join(format!("rbleach-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&key_dir)?;
    let key_ctrl = key_dir.join("ctrl").join("identity.json");
    let key_cli = key_dir.join("cli").join("identity.json");

    // 预热身份（避免被控端/主控端并发首生成）+ 验证 sign/verify 往返
    let id = rebleach_client::identity::Identity::load_or_generate(&key_ctrl)?;
    let sig0 = id.sign(b"rebleach-e2e-probe");
    assert!(
        rebleach_client::identity::verify_detached(&id.pubkey_b64(), b"rebleach-e2e-probe", &sig0),
        "identity sign/verify 往返失败"
    );
    println!("[身份] pubkey={}", id.pubkey_b64());

    let cfg_c = SessionConfig {
        server: url.clone(),
        id: "e2e-ctrl".into(),
        role: Role::Controlled,
        peer: None,
        fps: 15,
        ice_servers: vec![],
        key_file: Some(key_ctrl),
        auth: None,
    };
    let (_in_tx, in_rx) = mpsc::unbounded_channel::<ControlMsg>();
    let sink: EventSink = Arc::new(|ev| {
        if let SessionEvent::Status(s) = ev {
            println!("[被控端] {s}");
        }
    });
    tokio::spawn(rebleach_client::session::run_session(cfg_c, in_rx, sink));
    tokio::time::sleep(Duration::from_millis(1200)).await; // 等注册完成

    // 3. exec：干净输出 + 真实退出码
    println!("\n--- 断言 1：exec ---");
    let env_exec =
        CliEnv::new(Some(url.clone()), Some("e2e-cli".into()), Some(key_cli.clone()), vec![]);
    let code = rebleach_client::cli::exec(
        env_exec,
        "e2e-ctrl",
        "echo rebleach-e2e-OK & exit /b 7",
        false,
        Some(120),
        None,
    )
    .await
    .context("exec 失败")?;
    assert_eq!(code, 7, "exec 退出码应为 7（拿到 {code}）");
    println!("✅ exec 退出码 = 7");
    // 诊断：打印两端 TOFU 存储内容（固定记录应完整无覆盖）
    for (tag, f) in [
        ("被控端", key_dir.join("ctrl").join("known_peers.json")),
        ("主控端", key_dir.join("cli").join("known_peers.json")),
    ] {
        match std::fs::read_to_string(&f) {
            Ok(s) => println!("[诊断] {tag} tofu: {s}"),
            Err(e) => println!("[诊断] {tag} tofu 不可读: {e}"),
        }
    }

    // 4/5. tunnel + 内置 SSH 服务端
    println!("\n--- 断言 2/3：tunnel 回环 echo + ssh 目标 ---");
    let cfg_t = HeadlessConfig {
        server: url.clone(),
        // 用不同的主控端 id：与 exec 链路互为独立会话（同 id 重建会走
        // 被控端「关闭旧会话后重建」路径，其 teardown 与新会话存在竞争）
        id: "e2e-cli2".into(),
        peer: "e2e-ctrl".into(),
        ice_servers: vec![],
        key_file: Some(key_cli),
        auth: None,
    };
    let (link, mut events) = headless::connect(cfg_t, true).await?;

    // 被控端侧可回环访问的 echo 服务（主控端 bind，被控端经隧道连回主控端回环）
    // 同机场景下主控端/被控端共享回环，target 127.0.0.1:<port> 直接命中
    let echo = TcpListener::bind("127.0.0.1:0").await?;
    let echo_port = echo.local_addr()?.port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = echo.accept().await else { break };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                loop {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if s.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });

    // cid=1：回环 echo；cid=2：内置 SSH 服务端（按需拉起）
    link.send_tunnel(TunnelMsg::Open {
        cid: 1,
        target: format!("127.0.0.1:{echo_port}"),
    })
    .await?;
    link.send_tunnel(TunnelMsg::Open {
        cid: 2,
        target: "ssh".into(),
    })
    .await?;
    link.send_tunnel(TunnelMsg::Data {
        cid: 1,
        data: b"rebleach-tunnel-ping".to_vec(),
    })
    .await?;

    let mut echo_back: Option<Vec<u8>> = None;
    let mut ssh_ok = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while (echo_back.is_none() || !ssh_ok) && tokio::time::Instant::now() < deadline {
        let ev = tokio::time::timeout(Duration::from_secs(5), events.recv()).await;
        match ev {
            Ok(Some(LinkEvent::Tunnel(TunnelMsg::Open { cid, target }))) => {
                println!("[tunnel] cid={cid} 接通: {target}");
                if cid == 2 {
                    ssh_ok = true;
                }
            }
            Ok(Some(LinkEvent::Tunnel(TunnelMsg::Data { cid: 1, data }))) => {
                echo_back = Some(data);
            }
            Ok(Some(LinkEvent::Tunnel(TunnelMsg::Err { cid, msg }))) => {
                if cid == 2 {
                    bail!("内置 SSH 服务端启动失败: {msg}");
                }
                bail!("隧道 cid={cid} 错误: {msg}");
            }
            Ok(Some(LinkEvent::Status(st))) if st == "term-open" => {}
            Ok(Some(_)) => {}
            Ok(None) => bail!("链路关闭"),
            Err(_) => {}
        }
    }
    let got = echo_back.context("隧道回显超时（60s）")?;
    assert_eq!(
        got,
        b"rebleach-tunnel-ping".to_vec(),
        "隧道回显内容不一致"
    );
    println!("✅ tunnel 双向字节流 OK（{} bytes）", got.len());
    assert!(ssh_ok, "内置 SSH 服务端未接通（60s 超时）");
    println!("✅ 内置 SSH 服务端按需拉起 + TCP 接通");

    let _ = link.close().await;
    Ok(())
}

// ============================================================
// 迷你信令中继：进程内按 id 转发（仅覆盖 e2e 用到的消息子集）
// ============================================================

async fn mini_signaling(listener: TcpListener) -> Result<()> {
    use parking_lot::Mutex;
    use tokio::sync::mpsc::UnboundedSender;
    use tokio_tungstenite::tungstenite::Message;

    type Conns = Arc<Mutex<HashMap<String, UnboundedSender<Message>>>>;

    let conns: Conns = Default::default();
    let pubkeys: Arc<Mutex<HashMap<String, String>>> = Default::default();

    loop {
        let (stream, _) = listener.accept().await?;
        let conns = conns.clone();
        let pubkeys = pubkeys.clone();
        tokio::spawn(async move {
            let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            let (mut ws_out, mut ws_in) = ws.split();
            let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
            // 每连接写任务
            tokio::spawn(async move {
                while let Some(m) = rx.recv().await {
                    if ws_out.send(m).await.is_err() {
                        break;
                    }
                }
            });
            // 读循环：注册 / Connect 派发 Incoming / Sdp+Ice 按 to 转发
            let mut self_id: Option<String> = None;
            while let Some(Ok(msg)) = ws_in.next().await {
                let text = match msg {
                    Message::Text(t) => t.to_string(),
                    Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
                    Message::Close(_) => break,
                    _ => continue,
                };
                let Ok(m) = serde_json::from_str::<WsMsg>(&text) else { continue };
                match m {
                    WsMsg::Register { id, pubkey, .. } => {
                        self_id = Some(id.clone());
                        conns.lock().insert(id.clone(), tx.clone());
                        pubkeys.lock().insert(id, pubkey);
                    }
                    WsMsg::Connect { target, .. } => {
                        // 与真实服务器一致的双向 Incoming：
                        // - target 收 Incoming{from: 请求者, peer_pubkey: 请求者公钥}
                        // - 请求者收 Incoming{from: target, peer_pubkey: target 公钥}（TOFU 固定用）
                        let Some(from) = self_id.clone() else { continue };
                        let from_pk = pubkeys.lock().get(&from).cloned().unwrap_or_default();
                        let target_pk = pubkeys.lock().get(&target).cloned().unwrap_or_default();
                        let t = conns.lock().get(&target).cloned();
                        match t {
                            Some(t) => {
                                let _ = t.send(Message::text(
                                    serde_json::to_string(&WsMsg::Incoming {
                                        from: from.clone(),
                                        peer_pubkey: from_pk,
                                    })
                                    .unwrap_or_default(),
                                ));
                                let _ = tx.send(Message::text(
                                    serde_json::to_string(&WsMsg::Incoming {
                                        from: target,
                                        peer_pubkey: target_pk,
                                    })
                                    .unwrap_or_default(),
                                ));
                            }
                            None => {
                                // 目标不在线：回 Error（主控端 30s 等待里直接失败退出）
                                let _ = tx.send(Message::text(
                                    serde_json::to_string(&WsMsg::Error {
                                        code: "offline".into(),
                                        message: format!("设备 {target} 不在线"),
                                    })
                                    .unwrap_or_default(),
                                ));
                            }
                        }
                    }
                    WsMsg::Sdp { to, data, kind, sig } => {
                        println!(
                            "[信令] Sdp kind={kind:?} ->{to} data_len={} sig={}",
                            data.len(),
                            sig.as_deref().map(|s| &s[..s.len().min(16)]).unwrap_or("(无)")
                        );
                        // 与真实服务器一致：转发时把 to 重写为发送者 id
                        // （接收方用 to 作为「对端 id」查 TOFU 固定表）
                        let Some(from) = self_id.clone() else { continue };
                        let t = conns.lock().get(&to).cloned();
                        if let Some(t) = t {
                            let fw = serde_json::to_string(&WsMsg::Sdp {
                                to: from,
                                data,
                                kind,
                                sig,
                            })
                            .unwrap_or_default();
                            let _ = t.send(Message::text(fw));
                        }
                    }
                    WsMsg::Ice { to, candidates } => {
                        println!("[信令] Ice ->{to} n={}", candidates.len());
                        // 同 Sdp：to 重写为发送者 id（被控端 pending_ice 按 to 键控）
                        let Some(from) = self_id.clone() else { continue };
                        let t = conns.lock().get(&to).cloned();
                        if let Some(t) = t {
                            let fw = serde_json::to_string(&WsMsg::Ice { to: from, candidates })
                                .unwrap_or_default();
                            let _ = t.send(Message::text(fw));
                        }
                    }
                    // Heartbeat/Keepalive/Turn/SessionEnd 等：e2e 不需要，忽略
                    _ => {}
                }
            }
            // 连接断开：注销
            if let Some(id) = self_id {
                conns.lock().remove(&id);
            }
        });
    }
}
