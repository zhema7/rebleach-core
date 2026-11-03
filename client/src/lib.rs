// Rebleach 客户端核心库
// protocol / identity / tls / session 与平台无关；
// audio / capture / control / video / viewer / desktop 属于桌面端专属（需要 cpal / openh264 / enigo / winit / windows）。
// Android 端走 `android` feature，仅编译纯 Rust 核心，平台能力（MediaProjection / MediaCodec / 无障碍）由 Kotlin 提供。

#[cfg(feature = "desktop")]
pub mod audio;
#[cfg(feature = "desktop")]
pub mod capture;
#[cfg(feature = "desktop")]
pub mod control;
#[cfg(all(feature = "desktop", target_os = "windows"))]
pub mod desktop;
pub mod frame_assembler;
pub mod identity;
pub mod protocol;
pub mod session;
pub mod tls;
#[cfg(target_os = "android")]
pub mod jni;
#[cfg(feature = "desktop")]
pub mod video;
#[cfg(feature = "desktop")]
pub mod viewer;
