// 信令连接 TLS：内置 pin 服务端的 Caddy 内部 CA。
// 服务端用自签 CA（Caddy `tls internal`），公网只开放 443，因此客户端必须走 wss://。
// 直接信任系统根证书会让自签 CA 校验失败，这里把 CA 证书编进二进制，免去用户手动导入。
use std::io::Cursor;
use std::sync::Arc;

use tokio_tungstenite::Connector;

const ROOT_CA_PEM: &[u8] = include_bytes!("../certs/rebleach-root.crt");

/// 构造仅信任内置 CA 的 TLS connector（同时支持 ws:// 与 wss://）
pub fn connector() -> Connector {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut Cursor::new(ROOT_CA_PEM)).flatten() {
        roots.add(cert).expect("内置 CA 证书解析失败");
    }

    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();

    Connector::Rustls(Arc::new(config))
}
