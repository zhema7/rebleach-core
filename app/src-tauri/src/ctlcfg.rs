// 主控端配置：本机主控 ID + 中转节点列表（内置默认节点，支持后续新增）
// 存储：%APPDATA%/rebleach-controller/config.json
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 内置默认中转节点（部署时用 REBLEACH_DEFAULT_SERVER 环境变量指定自己的服务器）
pub const DEFAULT_NODE_URL: &str = match option_env!("REBLEACH_DEFAULT_SERVER") {
    Some(v) => v,
    None => "wss://rebleach.example.com/ws",
};
pub const DEFAULT_NODE_NAME: &str = "默认节点";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub name: String,
    pub url: String,
    /// 内置节点不可删除
    #[serde(default)]
    pub builtin: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CtlConfig {
    /// 本机主控端设备 ID（9 位数字，首次生成后固定）
    pub id: String,
    /// 中转节点列表
    #[serde(default)]
    pub nodes: Vec<Node>,
    /// 当前选用的节点 url
    #[serde(default)]
    pub active: String,
    /// 服务端管理员账号（用于接入配置 / SSH 公钥 / 安装令牌，明文存本机）
    #[serde(default)]
    pub admin_user: String,
    /// 服务端管理员密码（明文存本机）
    #[serde(default)]
    pub admin_pass: String,
}

impl Default for CtlConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            nodes: vec![Node {
                name: DEFAULT_NODE_NAME.to_string(),
                url: DEFAULT_NODE_URL.to_string(),
                builtin: true,
            }],
            active: DEFAULT_NODE_URL.to_string(),
            admin_user: String::new(),
            admin_pass: String::new(),
        }
    }
}

fn data_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("rebleach-controller")
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

/// 主控端身份密钥路径（与被控端 %APPDATA%/rebleach 隔离）
pub fn identity_path() -> PathBuf {
    data_dir().join("identity.json")
}

/// 主控端日志路径（%APPDATA%/rebleach-controller/controller.log）。
/// 主控端是 GUI 子系统，stdout 无处可去，不落盘就完全没有可观测性。
pub fn log_path() -> PathBuf {
    data_dir().join("controller.log")
}

fn gen_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 + d.as_secs())
        .unwrap_or(0);
    let v = nanos
        .wrapping_mul(6364136223846793005)
        .wrapping_add(std::process::id() as u64)
        % 900_000_000
        + 100_000_000;
    v.to_string()
}

pub fn load() -> CtlConfig {
    let mut cfg: CtlConfig = std::fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    // 内置节点必须始终存在（升级后旧配置缺节点时补回）
    if !cfg.nodes.iter().any(|n| n.url == DEFAULT_NODE_URL) {
        cfg.nodes.insert(
            0,
            Node {
                name: DEFAULT_NODE_NAME.to_string(),
                url: DEFAULT_NODE_URL.to_string(),
                builtin: true,
            },
        );
    }
    if cfg.id.trim().is_empty() {
        cfg.id = gen_id();
    }
    if cfg.active.trim().is_empty() || !cfg.nodes.iter().any(|n| n.url == cfg.active) {
        cfg.active = DEFAULT_NODE_URL.to_string();
    }
    cfg
}

pub fn save(cfg: &CtlConfig) -> Result<(), String> {
    std::fs::create_dir_all(data_dir()).map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(config_path(), json).map_err(|e| e.to_string())
}
