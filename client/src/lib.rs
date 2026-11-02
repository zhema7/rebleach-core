// Rebleach 客户端核心库
// protocol / video / control / session 与 UI 解耦，供 CLI 与 Tauri 桌面端复用
// viewer 仅 CLI 使用（winit 软渲染），桌面端改用 WebView + WebCodecs
pub mod audio;
pub mod capture;
pub mod control;
#[cfg(windows)]
pub mod desktop;
pub mod identity;
pub mod protocol;
pub mod session;
pub mod tls;
pub mod video;
pub mod viewer;
