fn main() {
    // rebleach-ui.exe 带 uiAccess=true 清单（构建期 embed-manifest 嵌入），
    // 经 Authenticode 签名 + Program Files 安装路径 + UAC 开启后，
    // 服务以登录用户令牌派生它即被授予 UIAccess，锁屏安全桌面下也能注入键鼠。
    // 注意：该清单绝不能打在 rebleach.exe 上——SCM 拒绝启动带 uiAccess 的服务 exe（740）。
    println!("cargo:rerun-if-changed=uiaccess.manifest");
    embed_manifest::embed_manifest_file("uiaccess.manifest").expect("embed uiaccess.manifest");
}
