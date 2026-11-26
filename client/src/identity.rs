// 设备身份（ed25519）+ TOFU（Trust On First Use）对端公钥固定
//
// 模型：
// - 每台设备首次启动生成 ed25519 密钥对，持久化到 identity.json
// - Register 上报真实公钥；SDP offer/answer 用私钥签名（sig 字段）
// - 对端首次接入时把「设备 ID → 公钥」固定到 known_peers.json（TOFU）
// - 之后同 ID 公钥变更 → 拒绝；SDP 验签失败 → 拒绝
//
// 这防的是「同 ID 冒充」与「信令服务器篡改 SDP」，不防首次接入前的中间人
// （TOFU 模型的固有边界，正式上线再叠加带外指纹校验）。
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

#[derive(Debug, Serialize, Deserialize)]
struct IdentityFile {
    secret: String,
    public: String,
}

/// 设备身份密钥对（私钥不出本结构）
#[derive(Debug, Clone)]
pub struct Identity {
    secret: [u8; 32],
    public: [u8; 32],
}

impl Identity {
    /// 加载既有身份，不存在则生成并持久化
    pub fn load_or_generate(path: &Path) -> Result<Self> {
        if let Ok(text) = std::fs::read_to_string(path) {
            let f: IdentityFile = serde_json::from_str(&text).context("身份文件损坏")?;
            let secret_vec = B64.decode(&f.secret).context("身份文件 secret 非 base64")?;
            let public_vec = B64.decode(&f.public).context("身份文件 public 非 base64")?;
            let secret: [u8; 32] = secret_vec
                .try_into()
                .map_err(|_| anyhow::anyhow!("bad secret"))?;
            let public: [u8; 32] = public_vec
                .try_into()
                .map_err(|_| anyhow::anyhow!("bad public"))?;
            return Ok(Self { secret, public });
        }
        let sk = SigningKey::generate(&mut rand::rngs::OsRng);
        let secret = sk.to_bytes();
        let public = sk.verifying_key().to_bytes();
        let f = IdentityFile {
            secret: B64.encode(secret),
            public: B64.encode(public),
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        std::fs::write(path, serde_json::to_string_pretty(&f)?)
            .with_context(|| format!("写入身份文件失败: {}", path.display()))?;
        Ok(Self { secret, public })
    }

    pub fn pubkey_b64(&self) -> String {
        B64.encode(self.public)
    }

    /// 对任意载荷签名（用于 SDP offer/answer）
    pub fn sign(&self, msg: &[u8]) -> String {
        let sk = SigningKey::from_bytes(&self.secret);
        B64.encode(sk.sign(msg).to_bytes())
    }
}

/// 用对端公钥验签
pub fn verify_detached(pubkey_b64: &str, msg: &[u8], sig_b64: &str) -> bool {
    let pk_vec = match B64.decode(pubkey_b64) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let sig_vec = match B64.decode(sig_b64) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let pk: [u8; 32] = match pk_vec.try_into() {
        Ok(a) => a,
        Err(_) => return false,
    };
    let sig: [u8; 64] = match sig_vec.try_into() {
        Ok(a) => a,
        Err(_) => return false,
    };
    let vk = match VerifyingKey::from_bytes(&pk) {
        Ok(v) => v,
        Err(_) => return false,
    };
    vk.verify(msg, &Signature::from_bytes(&sig)).is_ok()
}

// ============================================================
// TOFU 对端公钥固定
// ============================================================
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TofuResult {
    /// 首次见到该对端，已固定其公钥
    Pinned,
    /// 与已固定公钥一致
    Match,
    /// 与已固定公钥不一致 → 拒绝
    Mismatch,
}

fn load_store(path: &Path) -> HashMap<String, String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// 检查并（首次时）固定对端公钥
pub fn tofu_check(store_path: &Path, peer_id: &str, pubkey: &str) -> Result<TofuResult> {
    let mut map = load_store(store_path);
    match map.get(peer_id) {
        Some(pinned) if pinned == pubkey => Ok(TofuResult::Match),
        Some(_) => Ok(TofuResult::Mismatch),
        None => {
            map.insert(peer_id.to_string(), pubkey.to_string());
            if let Some(dir) = store_path.parent() {
                std::fs::create_dir_all(dir).ok();
            }
            std::fs::write(store_path, serde_json::to_string_pretty(&map)?)?;
            Ok(TofuResult::Pinned)
        }
    }
}

/// 读取某对端已固定的公钥
pub fn pinned_pubkey(store_path: &Path, peer_id: &str) -> Result<Option<String>> {
    Ok(load_store(store_path).get(peer_id).cloned())
}

// ============================================================
// 路径与展示辅助
// ============================================================
pub fn default_base_dir() -> PathBuf {
    // 被控端以 Windows 服务运行时，APPDATA 指向 systemprofile，与用户态托盘不一致；
    // App 启动早期写入 RBLEACH_DATA_DIR（%ProgramData%\Rebleach）统一三者的数据目录。
    if let Ok(dir) = std::env::var("RBLEACH_DATA_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        PathBuf::from(appdata).join("rebleach")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".rebleach")
    } else {
        PathBuf::from("./rebleach_data")
    }
}

pub fn default_key_file() -> PathBuf {
    default_base_dir().join("identity.json")
}

/// TOFU 存储与身份文件同目录
pub fn default_tofu_file(key_file: &Path) -> PathBuf {
    key_file
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("known_peers.json")
}

/// 公钥短指纹（日志/UI 展示用）
pub fn fingerprint(pubkey_b64: &str) -> String {
    pubkey_b64.chars().take(16).collect()
}

/// 主机名（Register 上报，管理端展示用）
pub fn hostname() -> Option<String> {
    std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify() {
        let dir = std::env::temp_dir().join(format!("rebleach_test_{}", std::process::id()));
        let key = dir.join("identity.json");
        let id = Identity::load_or_generate(&key).unwrap();
        let pk = id.pubkey_b64();
        let sig = id.sign(b"hello sdp");
        assert!(verify_detached(&pk, b"hello sdp", &sig));
        assert!(!verify_detached(&pk, b"tampered", &sig));
        // 再次加载应得到同一公钥
        let id2 = Identity::load_or_generate(&key).unwrap();
        assert_eq!(id2.pubkey_b64(), pk);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tofu_flow() {
        let dir = std::env::temp_dir().join(format!("rebleach_tofu_{}", std::process::id()));
        let store = dir.join("known_peers.json");
        assert_eq!(
            tofu_check(&store, "devA", "pk1").unwrap(),
            TofuResult::Pinned
        );
        assert_eq!(
            tofu_check(&store, "devA", "pk1").unwrap(),
            TofuResult::Match
        );
        assert_eq!(
            tofu_check(&store, "devA", "pk2").unwrap(),
            TofuResult::Mismatch
        );
        assert_eq!(
            pinned_pubkey(&store, "devA").unwrap().as_deref(),
            Some("pk1")
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
