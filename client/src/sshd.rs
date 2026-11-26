//! 内置最小 SSH 服务端（T3）：让标准 `ssh` 客户端经 `rebleach-proxy` 直连 Agent，
//! 无需在被控端安装/配置 sshd。
//!
//! 架构：russh 监听 127.0.0.1:<随机端口>（仅回环，永不对外），
//! tunnel 通道的 `ssh` 目标 connect 到该端口。由此 tunnel.rs 的 TCP 泵 100% 复用，
//! SSH 协议字节流原样穿过 WebRTC。
//!
//! - 主机密钥：%APPDATA%/rebleach/ssh_host_ed25519（首次自动生成）
//! - 授权公钥：%APPDATA%/rebleach/ssh_authorized_keys（OpenSSH 格式，一行一把；
//!   `rebleach keygen --out <file>` 生成后由管理员拷贝追加）
//! - 认证：仅 publickey，禁止密码；shell/exec 走 ConPTY（与 term 通道同一套 PTY 栈）

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tracing::info;

/// 已授权公钥文件的默认路径
pub fn authorized_keys_path() -> std::path::PathBuf {
    crate::identity::default_key_file()
        .parent()
        .map(|p| p.join("ssh_authorized_keys"))
        .unwrap_or_else(|| std::path::PathBuf::from("ssh_authorized_keys"))
}

/// 主机密钥文件的默认路径
pub fn host_key_path() -> std::path::PathBuf {
    crate::identity::default_key_file()
        .parent()
        .map(|p| p.join("ssh_host_ed25519"))
        .unwrap_or_else(|| std::path::PathBuf::from("ssh_host_ed25519"))
}

/// 百分号编码 URL 组件（Base64 签名含 + / =，query 里必须编码）
pub fn percent_encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 从服务端拉取本机（被控端）应信任的 SSH 公钥并原子写入 authorized_keys。
///
/// 鉴权：用设备身份私钥对 `rebleach-agent-keys\n{id}\n{ts}` 签名；
/// 服务端只返回「当前存在有效 grant 的主控端」登记的公钥，撤销授权后下一轮自动移除。
/// 返回写入的公钥条数。
pub fn sync_authorized_keys(server: &str, device_id: &str, identity_path: &Path) -> Result<usize> {
    let identity = crate::identity::Identity::load_or_generate(identity_path)?;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("系统时间早于 UNIX 纪元")?
        .as_secs() as i64;
    let msg = format!("rebleach-agent-keys\n{device_id}\n{ts}");
    let sig = identity.sign(msg.as_bytes());
    let base = crate::cli::signaling_to_http(server);
    let url = format!(
        "{base}/agent/ssh-keys?id={device_id}&ts={ts}&sig={}",
        percent_encode_component(&sig)
    );

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(5))
        .build();
    let resp = agent
        .get(&url)
        .call()
        .with_context(|| format!("拉取 SSH 公钥失败: {base}"))?;
    let body: serde_json::Value = resp.into_json().context("公钥响应 JSON 解析失败")?;

    let mut keys: Vec<String> = Vec::new();
    for k in body
        .get("keys")
        .and_then(|v| v.as_array())
        .map(|a| a.as_slice())
        .unwrap_or_default()
    {
        let Some(pk) = k.get("pubkey").and_then(|v| v.as_str()) else {
            continue;
        };
        let pk = pk.trim();
        if !pk.is_empty() && !keys.iter().any(|e| e == pk) {
            keys.push(pk.to_string());
        }
    }

    let content = if keys.is_empty() {
        String::new()
    } else {
        format!("{}\n", keys.join("\n"))
    };
    write_authorized_keys(&content)?;
    Ok(keys.len())
}

/// 原子写入：先写 .tmp 再 rename，避免 SSH 读取到半截文件
fn write_authorized_keys(content: &str) -> Result<()> {
    let path = authorized_keys_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("创建目录失败: {}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content).with_context(|| format!("写入失败: {}", tmp.display()))?;
    #[cfg(windows)]
    restrict_authorized_keys_acl(&tmp);
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("替换失败: {}", path.display()))?;
    info!("🔐 已同步 SSH 授权公钥 → {}", path.display());
    Ok(())
}

/// 收紧 ACL：SYSTEM/Administrators 可写，Users 只读（SID 形式避免系统语言差异）
#[cfg(windows)]
fn restrict_authorized_keys_acl(path: &Path) {
    use std::os::windows::process::CommandExt;
    let _ = std::process::Command::new("icacls")
        .arg(path)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg("*S-1-5-18:F") // SYSTEM
        .arg("*S-1-5-32-544:F") // Administrators
        .arg("*S-1-5-32-545:R") // Users（用户态托盘只需读）
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output();
}

/// 惰性启动内置 SSH 服务端，返回回环端口（tokio OnceCell 保证只启动一次；
/// 失败不缓存，下次隧道重试会重新拉起）。
/// 失败时返回 Err（调用方发送 tunnel ERR 帧，主控端提示降级用 127.0.0.1:22）。
pub async fn ensure_started() -> Result<u16> {
    // OnceCell 存成功端口；get_or_try_init 的闭包失败不缓存，下次重试重新拉起
    static PORT: tokio::sync::OnceCell<u16> = tokio::sync::OnceCell::const_new();
    match PORT
        .get_or_try_init(|| async { start().await.map_err(|e| format!("{e:#}")) })
        .await
    {
        Ok(p) => Ok(*p),
        Err(e) => Err(anyhow::anyhow!("{e}")),
    }
}

#[cfg(all(feature = "desktop", windows))]
async fn start() -> Result<u16> {
    crate::sshd_impl::start_server()
        .await
        .inspect(|p| info!("🔐 内置 SSH 服务端已启动: 127.0.0.1:{p}"))
}

#[cfg(not(all(feature = "desktop", windows)))]
async fn start() -> Result<u16> {
    Err(anyhow::anyhow!("内置 SSH 服务端仅支持 Windows 桌面构建"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encode_keeps_base64_safe() {
        assert_eq!(percent_encode_component("abcXYZ019-_.~"), "abcXYZ019-_.~");
        assert_eq!(percent_encode_component("a+b/c="), "a%2Bb%2Fc%3D");
    }
}
