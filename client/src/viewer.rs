// 主控端查看器：winit 0.30 窗口 + softbuffer 0.4 软渲染 + 键鼠输入捕获
// 渲染：解码出的 RGBA 帧经最近邻缩放画到窗口（等比缩放 + 居中黑边）
// 输入：窗口内键鼠事件换算为远程坐标 → ControlMsg → tokio channel → 网络侧转发
use std::num::NonZeroU32;
use std::sync::Arc;

use tokio::sync::mpsc::UnboundedSender;
use tracing::debug;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key as WKey, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use crate::protocol::ControlMsg;

/// 异步网络侧 → UI 线程事件（经 EventLoopProxy）
pub enum ViewerEvent {
    /// 一帧解码完成的 RGBA 图像
    Frame { w: u32, h: u32, rgba: Arc<Vec<u8>> },
    /// 连接状态文本（显示在标题栏）
    Status(String),
}

/// 远程画面在窗口内的布局（输入坐标换算用）
#[derive(Clone, Copy)]
struct ViewTransform {
    off_x: u32,
    off_y: u32,
    draw_w: u32,
    remote_w: u32,
    remote_h: u32,
}

const BG: u32 = 0x00_1E_2A_38; // 深蓝背景（黑边区域）

pub struct ViewerApp {
    input_tx: UnboundedSender<ControlMsg>,
    window: Option<Arc<Window>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    frame: Option<(u32, u32, Arc<Vec<u8>>)>,
    view: Option<ViewTransform>,
    status: String,
    frames_drawn: u64,
    /// 当前按下的修饰键（组合键随 KeyDown/KeyUp 一起发给被控端）
    mods: ModifiersState,
}

impl ViewerApp {
    pub fn new(input_tx: UnboundedSender<ControlMsg>) -> Self {
        Self {
            input_tx,
            window: None,
            surface: None,
            frame: None,
            view: None,
            status: "连接中…".into(),
            frames_drawn: 0,
            mods: ModifiersState::empty(),
        }
    }

    fn send_input(&self, msg: ControlMsg) {
        let _ = self.input_tx.send(msg);
    }

    /// 当前修饰键 → 协议 mods 列表（排除按键本身，如单按 Shift 时 mods 不带 Shift）
    fn mods_vec(&self, exclude: &str) -> Vec<String> {
        let mut v = Vec::new();
        if self.mods.shift_key() {
            v.push("Shift".to_string());
        }
        if self.mods.control_key() {
            v.push("Control".to_string());
        }
        if self.mods.alt_key() {
            v.push("Alt".to_string());
        }
        if self.mods.super_key() {
            v.push("Meta".to_string());
        }
        v.retain(|m| m != exclude);
        v
    }

    fn render(&mut self) {
        let Some(win) = self.window.clone() else {
            return;
        };
        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        let size = win.inner_size();
        if size.width == 0 || size.height == 0 {
            return;
        }
        let (bw, bh) = (size.width as usize, size.height as usize);

        let Some((rw, rh, rgba)) = self.frame.as_ref() else {
            // 还没有帧：纯背景
            if surface
                .resize(
                    NonZeroU32::new(size.width).unwrap(),
                    NonZeroU32::new(size.height).unwrap(),
                )
                .is_ok()
            {
                if let Ok(mut buf) = surface.buffer_mut() {
                    buf[..bw * bh].fill(BG);
                    let _ = buf.present();
                }
            }
            return;
        };
        let (rw, rh) = (*rw as usize, *rh as usize);
        let rgba = rgba.clone();

        // 等比缩放 + 居中
        let scale = (bw as f64 / rw as f64).min(bh as f64 / rh as f64);
        let draw_w = (rw as f64 * scale) as usize;
        let draw_h = (rh as f64 * scale) as usize;
        let off_x = (bw - draw_w) / 2;
        let off_y = (bh - draw_h) / 2;
        self.view = Some(ViewTransform {
            off_x: off_x as u32,
            off_y: off_y as u32,
            draw_w: draw_w as u32,
            remote_w: rw as u32,
            remote_h: rh as u32,
        });

        if surface
            .resize(
                NonZeroU32::new(size.width).unwrap(),
                NonZeroU32::new(size.height).unwrap(),
            )
            .is_err()
        {
            return;
        }
        let mut buf = match surface.buffer_mut() {
            Ok(b) => b,
            Err(_) => return,
        };

        // 背景填充 + 中央区域最近邻采样（0x00RRGGBB）
        buf[..bw * bh].fill(BG);
        for dy in 0..draw_h {
            let sy = dy * rh / draw_h;
            let row = (off_y + dy) * bw + off_x;
            let src_row = sy * rw;
            for dx in 0..draw_w {
                let sx = dx * rw / draw_w;
                let i = (src_row + sx) * 4;
                buf[row + dx] =
                    (rgba[i] as u32) << 16 | (rgba[i + 1] as u32) << 8 | rgba[i + 2] as u32;
            }
        }
        let _ = buf.present();

        self.frames_drawn += 1;
        if self.frames_drawn % 60 == 1 {
            debug!("🖼️ 已渲染 {} 帧", self.frames_drawn);
        }
    }
}

impl ApplicationHandler<ViewerEvent> for ViewerApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title(format!("Rebleach Viewer — {}", self.status))
            .with_inner_size(LogicalSize::new(1280.0f64, 800.0f64));
        let window = Arc::new(event_loop.create_window(attrs).expect("创建窗口失败"));
        let context =
            softbuffer::Context::new(window.clone()).expect("softbuffer Context 创建失败");
        let surface = softbuffer::Surface::new(&context, window.clone())
            .expect("softbuffer Surface 创建失败");
        self.window = Some(window);
        self.surface = Some(surface);
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: ViewerEvent) {
        match event {
            ViewerEvent::Frame { w, h, rgba } => {
                self.frame = Some((w, h, rgba));
                if let Some(win) = &self.window {
                    win.request_redraw();
                }
            }
            ViewerEvent::Status(s) => {
                self.status = s;
                if let Some(win) = &self.window {
                    win.set_title(&format!("Rebleach Viewer — {}", self.status));
                }
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.render(),

            // —— 鼠标 ——
            WindowEvent::CursorMoved { position, .. } => {
                if let Some(v) = self.view {
                    let scale = v.draw_w as f64 / v.remote_w as f64;
                    let rx = ((position.x - v.off_x as f64) / scale)
                        .clamp(0.0, (v.remote_w - 1) as f64) as u32;
                    let ry = ((position.y - v.off_y as f64) / scale)
                        .clamp(0.0, (v.remote_h - 1) as f64) as u32;
                    self.send_input(ControlMsg::MouseMove { x: rx, y: ry });
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let b = match button {
                    MouseButton::Left => 0u8,
                    MouseButton::Right => 1,
                    MouseButton::Middle => 2,
                    _ => return,
                };
                let msg = match state {
                    ElementState::Pressed => ControlMsg::MouseDown { b },
                    ElementState::Released => ControlMsg::MouseUp { b },
                };
                self.send_input(msg);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => (y * 40.0) as i32,
                    MouseScrollDelta::PixelDelta(p) => p.y as i32,
                };
                if dy != 0 {
                    self.send_input(ControlMsg::Wheel { dy });
                }
            }

            // —— 键盘 ——
            WindowEvent::ModifiersChanged(m) => {
                self.mods = m.state();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let Some(k) = key_label(&event.logical_key) {
                    let mods = self.mods_vec(&k);
                    let msg = match event.state {
                        ElementState::Pressed => ControlMsg::KeyDown { k, mods },
                        ElementState::Released => ControlMsg::KeyUp { k, mods },
                    };
                    self.send_input(msg);
                }
            }
            _ => {}
        }
    }
}

/// winit logical_key → 协议键名（单字符小写或命名键）
fn key_label(k: &WKey) -> Option<String> {
    match k {
        WKey::Character(s) => Some(s.to_lowercase()),
        WKey::Named(n) => {
            let label = match n {
                NamedKey::Enter => "Enter",
                NamedKey::Backspace => "Backspace",
                NamedKey::Tab => "Tab",
                NamedKey::Escape => "Escape",
                NamedKey::Space => "Space",
                NamedKey::ArrowUp => "ArrowUp",
                NamedKey::ArrowDown => "ArrowDown",
                NamedKey::ArrowLeft => "ArrowLeft",
                NamedKey::ArrowRight => "ArrowRight",
                NamedKey::Home => "Home",
                NamedKey::End => "End",
                NamedKey::PageUp => "PageUp",
                NamedKey::PageDown => "PageDown",
                NamedKey::Delete => "Delete",
                NamedKey::Insert => "Insert",
                NamedKey::Shift => "Shift",
                NamedKey::Control => "Control",
                NamedKey::Alt => "Alt",
                NamedKey::Meta => "Meta",
                NamedKey::F1 => "F1",
                NamedKey::F2 => "F2",
                NamedKey::F3 => "F3",
                NamedKey::F4 => "F4",
                NamedKey::F5 => "F5",
                NamedKey::F6 => "F6",
                NamedKey::F7 => "F7",
                NamedKey::F8 => "F8",
                NamedKey::F9 => "F9",
                NamedKey::F10 => "F10",
                NamedKey::F11 => "F11",
                NamedKey::F12 => "F12",
                _ => return None,
            };
            Some(label.to_string())
        }
        _ => None,
    }
}
