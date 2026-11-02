// 信令协议 + 控制消息协议（client 与 server 共用的定义副本）
use serde::{Deserialize, Serialize};

// ============================================================
// WebSocket 信令（client ↔ server）
// ============================================================
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsMsg {
    Register {
        id: String,
        role: Role,
        pubkey: String,
        /// 主机名（管理端展示用，可选）
        #[serde(default)]
        hostname: Option<String>,
        /// 设备详细信息（被控端 Agent 上报，管理端展示）
        #[serde(default)]
        info: Option<DeviceInfo>,
    },
    Connect {
        target: String,
        /// 局域网直连验证码（仅被控端内嵌迷你信令校验；正式服务器忽略此字段）
        #[serde(default)]
        auth: Option<String>,
    },
    Incoming {
        from: String,
        peer_pubkey: String,
    },
    /// 注册成功后下发的 TURN 临时凭据（TURN REST）
    Turn {
        urls: Vec<String>,
        username: String,
        credential: String,
        ttl: u64,
    },
    Sdp {
        to: String,
        #[serde(with = "serde_sdp")]
        data: Vec<u8>,
        kind: SdpKind,
        /// 发送方对 data 的 ed25519 签名（TOFU 验签用）
        #[serde(default)]
        sig: Option<String>,
    },
    Ice {
        to: String,
        candidates: Vec<String>,
    },
    SessionEnd {
        peer: String,
        reason: String,
    },
    Keepalive,
    /// 心跳：客户端周期性上报，服务端据此刷新 last_seen
    /// `busy` = 当前是否有主控端连接
    Heartbeat {
        #[serde(default)]
        busy: bool,
    },
    Evict {
        peer: String,
        reason: String,
    },
    Error {
        code: String,
        message: String,
    },
    /// 管理端 → 被控端：紧急控制指令（绕过会话层，单独线程执行）
    /// action: restart / shutdown / unlock / reload / disconnect
    Emergency {
        action: String,
        #[serde(default)]
        delay_secs: u32,
    },
    EmergencyAck {
        action: String,
        ok: bool,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Controller,
    Controlled,
}

/// 设备信息（被控端 Agent 注册/心跳时上报，全部可选以兼容旧客户端）
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DeviceInfo {
    #[serde(default)]
    pub hostname: Option<String>,
    /// 设备型号（厂商 + 机型）
    #[serde(default)]
    pub model: Option<String>,
    /// 操作系统版本
    #[serde(default)]
    pub os: Option<String>,
    /// CPU 型号
    #[serde(default)]
    pub cpu: Option<String>,
    /// 物理内存（MB）
    #[serde(default)]
    pub memory_mb: Option<u64>,
    /// 磁盘总容量（GB）
    #[serde(default)]
    pub storage_gb: Option<u64>,
    /// 公网 IP（由 Agent 通过 IP 定位服务获取）
    #[serde(default)]
    pub ip: Option<String>,
    /// 地区（国家/省/市）
    #[serde(default)]
    pub region: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SdpKind {
    Offer,
    Answer,
}

mod serde_sdp {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S>(v: &Vec<u8>, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_str(&base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            v,
        ))
    }
    pub fn deserialize<'de, D>(d: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(d)?;
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &s)
            .map_err(serde::de::Error::custom)
    }
}

pub fn role_from_str(s: &str) -> Role {
    match s.to_ascii_lowercase().as_str() {
        "controller" => Role::Controller,
        _ => Role::Controlled,
    }
}

// ============================================================
// 控制消息（主控端 → 被控端，DataChannel 文本帧 JSON）
// 视频帧是二进制帧，is_string 区分，零帧头开销
// ============================================================
/// 鼠标按键：0=左 1=右 2=中
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ControlMsg {
    /// 鼠标移动（编码帧坐标，被控端按采集缩放比+显示器原点换算到物理坐标）
    MouseMove {
        x: u32,
        y: u32,
    },
    MouseDown {
        b: u8,
    },
    MouseUp {
        b: u8,
    },
    /// 滚轮：dy 正=向上
    Wheel {
        dy: i32,
    },
    /// 键盘：k 为单字符（如 "a"）或命名键（Enter/Backspace/Tab/Escape/ArrowUp 等）
    /// mods 为组合键修饰（"Shift"/"Control"/"Alt"/"Meta"），KeyDown 时先按修饰键
    KeyDown {
        k: String,
        #[serde(default)]
        mods: Vec<String>,
    },
    KeyUp {
        k: String,
        #[serde(default)]
        mods: Vec<String>,
    },
    /// 主控端 → 被控端：切换采集的显示器
    SwitchMonitor {
        index: u32,
    },
    /// 被控端 → 主控端：显示器清单（DataChannel 打开后推送）
    Monitors {
        list: Vec<MonitorDesc>,
    },
    /// 双向：剪贴板文本同步（对端剪贴板变化时推送）
    Clipboard {
        text: String,
    },
    /// 被控端 → 主控端：链路统计（状态栏展示 + 码率自适应依据）
    Stats {
        fps: f32,
        kbps: f32,
        buffered: u64,
    },
    /// 文件传输（主控端 → 被控端，文本帧 JSON + base64 分块）
    FileOffer {
        name: String,
        size: u64,
        chunks: u32,
    },
    FileChunk {
        seq: u32,
        data: String,
    },
    FileDone,
    FileCancel,
    /// 主控端 → 被控端：强制刷新画面（重建采集/编码，立即出 IDR）
    Refresh,
    /// 主控端 → 被控端：切换画质档位
    /// level: low（流畅）/ balanced（均衡）/ high（高清）/ custom（自定义码率）
    SetQuality {
        level: String,
        #[serde(default)]
        bitrate_kbps: u32,
    },
    /// 主控端 → 被控端：修改当前显示器分辨率（0x0 表示恢复原始）
    SetResolution {
        width: u32,
        height: u32,
    },
    /// 主控端 → 被控端：新增 N 块虚拟显示器（依赖被控端已装虚拟屏驱动，如 usbmmidd / IddSampleDriver）
    /// 创建成功后被控端自动推新的 Monitors 清单，主控端重新枚举
    AddVirtualDisplay {
        count: u32,
        #[serde(default = "default_virtual_w")]
        width: u32,
        #[serde(default = "default_virtual_h")]
        height: u32,
    },
    /// 主控端 → 被控端：删除指定 index 的虚拟显示器（索引取自最新 Monitors 清单里 is_virtual=true 的项）
    RemoveVirtualDisplay {
        index: u32,
    },
    /// 主控端 → 被控端：锁定被控端桌面
    Lock,
    /// 主控端 → 被控端：解锁被控端锁屏（密码经加密 DataChannel 传输，被控端不持久化）
    Unlock {
        password: String,
    },
    /// 主控端 → 被控端：重启/关机
    /// action: restart / shutdown
    Power {
        action: String,
        #[serde(default)]
        delay_secs: u32,
    },
    /// 被控端 → 主控端：回传一条状态/日志（增删虚拟屏等异步操作的结果、错误、提示）
    /// level: info / warn / error
    Log {
        level: String,
        msg: String,
    },
    /// 主控端 → 被控端：查询被控端虚拟屏驱动状态；被控端收到后回 DriverStatus
    QueryDriverStatus,
    /// 被控端 → 主控端：虚拟屏驱动状态（响应 QueryDriverStatus 或增删虚拟屏后自动推送）
    DriverStatus {
        /// ROOT\DISPLAY 设备节点是否存在（驱动是否已安装入库）
        has_driver: bool,
        /// 是否有活动的虚拟屏（is_virtual=true 的显示器数量）
        active_count: u32,
        /// 驱动名称/版本（探测到 usbmmidd / iddsampledriver / none）
        driver_kind: String,
        /// 驱动安装指引（has_driver=false 时给用户的操作提示）
        #[serde(default)]
        hint: String,
    },
}

/// 显示器描述（被控端 → 主控端）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorDesc {
    pub index: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
    /// 是否为虚拟显示器（usbmmidd / IddSampleDriver 间接显示）。
    /// 主控端下拉框显示「💻」标识；虚拟屏上 SetResolution 兼容性更好
    #[serde(default)]
    pub is_virtual: bool,
}

fn default_virtual_w() -> u32 {
    1920
}
fn default_virtual_h() -> u32 {
    1080
}

impl ControlMsg {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
    pub fn from_json(s: &str) -> Option<Self> {
        serde_json::from_str(s).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_control(m: ControlMsg) {
        let json = m.to_json();
        assert!(!json.is_empty());
        let back = ControlMsg::from_json(&json).expect("反序列化失败");
        // 用 Debug 字符串比较（枚举变体 + 字段全量）
        assert_eq!(format!("{m:?}"), format!("{back:?}"));
    }

    #[test]
    fn control_mouse_move_roundtrip() {
        roundtrip_control(ControlMsg::MouseMove { x: 100, y: 200 });
    }

    #[test]
    fn control_mouse_buttons_roundtrip() {
        roundtrip_control(ControlMsg::MouseDown { b: 0 });
        roundtrip_control(ControlMsg::MouseUp { b: 2 });
        roundtrip_control(ControlMsg::Wheel { dy: -3 });
    }

    #[test]
    fn control_key_with_mods_roundtrip() {
        roundtrip_control(ControlMsg::KeyDown {
            k: "c".into(),
            mods: vec!["Control".into()],
        });
        roundtrip_control(ControlMsg::KeyUp {
            k: "Shift".into(),
            mods: vec![],
        });
    }

    #[test]
    fn control_monitors_roundtrip() {
        roundtrip_control(ControlMsg::Monitors {
            list: vec![MonitorDesc {
                index: 0,
                name: r"\\.\DISPLAY1".into(),
                width: 1920,
                height: 1080,
                primary: true,
                is_virtual: false,
            }],
        });
        // 没有 is_virtual 字段的旧 JSON 反序列化应兼容（serde default = false）
        let json_old =
            r#"{"t":"monitors","list":[{"index":1,"name":"DISPLAY2","width":1920,"height":1080,"primary":false}]}"#;
        let back = ControlMsg::from_json(json_old).expect("旧格式应可反序列化");
        match back {
            ControlMsg::Monitors { list } => assert!(!list[0].is_virtual),
            _ => panic!("unexpected variant"),
        }
    }

    #[test]
    fn control_virtual_display_roundtrip() {
        roundtrip_control(ControlMsg::AddVirtualDisplay {
            count: 2,
            width: 1920,
            height: 1080,
        });
        // 省略 width/height 时应走默认值
        let json_default = r#"{"t":"add_virtual_display","count":1}"#;
        let back = ControlMsg::from_json(json_default).expect("可反序列化");
        match back {
            ControlMsg::AddVirtualDisplay {
                count,
                width,
                height,
            } => {
                assert_eq!(count, 1);
                assert_eq!(width, 1920);
                assert_eq!(height, 1080);
            }
            _ => panic!("unexpected variant"),
        }
        roundtrip_control(ControlMsg::RemoveVirtualDisplay { index: 3 });
    }

    #[test]
    fn control_switch_monitor_roundtrip() {
        roundtrip_control(ControlMsg::SwitchMonitor { index: 1 });
    }

    #[test]
    fn control_stats_roundtrip() {
        roundtrip_control(ControlMsg::Stats {
            fps: 29.97,
            kbps: 4096.5,
            buffered: 16384,
        });
    }

    #[test]
    fn control_file_transfer_roundtrip() {
        roundtrip_control(ControlMsg::FileOffer {
            name: "report.pdf".into(),
            size: 1048576,
            chunks: 64,
        });
        roundtrip_control(ControlMsg::FileChunk {
            seq: 0,
            data: "AAAA".into(),
        });
        roundtrip_control(ControlMsg::FileDone);
        roundtrip_control(ControlMsg::FileCancel);
    }

    #[test]
    fn control_clipboard_roundtrip() {
        roundtrip_control(ControlMsg::Clipboard {
            text: "copied text".into(),
        });
    }

    #[test]
    fn ws_register_roundtrip() {
        let m = WsMsg::Register {
            id: "dev-001".into(),
            role: Role::Controller,
            pubkey: "base64pubkey".into(),
            hostname: Some("laptop".into()),
            info: Some(DeviceInfo {
                memory_mb: Some(16384),
                storage_gb: Some(512),
                ..Default::default()
            }),
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: WsMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(format!("{m:?}"), format!("{back:?}"));
    }

    #[test]
    fn ws_turn_creds_roundtrip() {
        let m = WsMsg::Turn {
            urls: vec!["turn:turn.example.com:3478".into()],
            username: "1730000000".into(),
            credential: "c2VjcmV0".into(),
            ttl: 3600,
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"type\":\"turn\""));
        let back: WsMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(format!("{m:?}"), format!("{back:?}"));
    }

    #[test]
    fn ws_sdp_base64_roundtrip() {
        let sdp = b"v=0\r\no=- 123 1 IN IP4 127.0.0.1\r\n";
        let m = WsMsg::Sdp {
            to: "peer".into(),
            data: sdp.to_vec(),
            kind: SdpKind::Offer,
            sig: Some("sig".into()),
        };
        let json = serde_json::to_string(&m).unwrap();
        // data 必须以 base64 字符串形式出现在 JSON 中
        assert!(json.contains("\"data\""));
        let back: WsMsg = serde_json::from_str(&json).unwrap();
        match back {
            WsMsg::Sdp { data, kind, .. } => {
                assert_eq!(data, sdp);
                assert_eq!(kind, SdpKind::Offer);
            }
            _ => panic!("反序列化类型错误"),
        }
    }

    #[test]
    fn ws_session_end_roundtrip() {
        let m = WsMsg::SessionEnd {
            peer: "p".into(),
            reason: "pubkey_mismatch".into(),
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: WsMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(format!("{m:?}"), format!("{back:?}"));
    }

    #[test]
    fn ws_keepalive_roundtrip() {
        let m = WsMsg::Keepalive;
        let json = serde_json::to_string(&m).unwrap();
        let back: WsMsg = serde_json::from_str(&json).unwrap();
        matches!(back, WsMsg::Keepalive);
    }

    #[test]
    fn control_desktop_actions_roundtrip() {
        roundtrip_control(ControlMsg::Refresh);
        roundtrip_control(ControlMsg::SetQuality {
            level: "balanced".into(),
            bitrate_kbps: 2000,
        });
        roundtrip_control(ControlMsg::SetResolution {
            width: 1920,
            height: 1080,
        });
        roundtrip_control(ControlMsg::Lock);
        roundtrip_control(ControlMsg::Power {
            action: "restart".into(),
            delay_secs: 5,
        });
    }

    #[test]
    fn ws_heartbeat_roundtrip() {
        let m = WsMsg::Heartbeat { busy: true };
        let back: WsMsg = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert!(matches!(back, WsMsg::Heartbeat { busy: true }));
    }

    #[test]
    fn role_from_str_parses() {
        assert_eq!(role_from_str("controller"), Role::Controller);
        assert_eq!(role_from_str("Controller"), Role::Controller);
        assert_eq!(role_from_str("controlled"), Role::Controlled);
        assert_eq!(role_from_str("anything"), Role::Controlled);
    }
}
