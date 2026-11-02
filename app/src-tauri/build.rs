fn main() {
    // 注意：本 exe 同时承担 Windows 服务宿主角色，SCM 拒绝启动带 uiAccess=true
    // 清单的 exe（启动即 740 requires elevation）。uiAccess 清单只打在
    // ui-shim crate 产出的 rebleach-ui.exe 上（服务以用户令牌派生它获取 UIAccess）。
    //
    // 主控端/被控端共用同一套源码，通过 feature 选择入口：
    //   - controller（默认）：controller.rs，tauri::generate_context!() 读 tauri.conf.json
    //   - agent：agent_app.rs，tauri::generate_context!("tauri.agent.conf.json")
    //     纯托盘无窗口（避免 SYSTEM 下 WebView2 初始化失败）
    if let Err(e) = tauri_build::try_build(tauri_build::Attributes::default()) {
        panic!("tauri_build 失败: {e:#}");
    }
}
