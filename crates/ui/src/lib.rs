//! Window, input and GPU rendering.
//!
//! The UI crate knows nothing about the engine. The app implements [`Delegate`]: it
//! receives [`Input`] and returns a [`Scene`] each frame, built from the latest engine
//! snapshot. That keeps this crate reusable and testable.
//!
//! Not RT-safe; runs on the main thread.

mod gpu;
mod rects;
mod scene;
mod text;
mod waveform;

pub use gpu::Offscreen;
pub use scene::{Align, Color, Rect, Scene, Shape, Text, WaveformStyle, WaveformView, Weight};

use std::path::PathBuf;
use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::event::{
    ElementState, KeyEvent, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key as WinitKey, NamedKey};
use winit::window::{Window, WindowId};

/// Keys the app cares about. Kept small so winit types don't leak into the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Space,
    Up,
    Down,
    Left,
    Right,
    Escape,
    Enter,
    Tab,
    PageUp,
    PageDown,
    Backspace,
    Char(char),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    KeyDown {
        key: Key,
        repeat: bool,
    },
    KeyUp {
        key: Key,
    },
    /// A file was dragged onto the window and dropped.
    FileDropped(PathBuf),
    /// Left mouse button or a finger went down, in physical pixels.
    PointerDown {
        x: f32,
        y: f32,
    },
    /// The pointer moved (reported whether or not it is down).
    PointerMove {
        x: f32,
        y: f32,
    },
    /// The button or finger was released.
    PointerUp {
        x: f32,
        y: f32,
    },
    /// Text typed (with case, shifted symbols and so on), sent after the key's
    /// [`Input::KeyDown`]. For text fields; shortcuts should use `KeyDown`.
    Text(String),
    /// Mouse wheel at the cursor. Positive `lines` is away from the user (scroll up).
    Scroll {
        x: f32,
        y: f32,
        lines: f32,
    },
}

/// Whether the UI should keep running after handling input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Exit,
}

/// Size of the frame being drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameSize {
    /// Physical pixels.
    pub width: u32,
    pub height: u32,
    /// Physical pixels per logical pixel (1.0 on the 7" panel, 2.0 on a Retina display).
    pub scale: f32,
}

/// The app side of the UI.
pub trait Delegate {
    fn on_input(&mut self, input: Input) -> Flow;
    /// Build the scene for the next frame, in physical pixels.
    fn scene(&mut self, size: FrameSize) -> Scene;
    /// The window title. Called every frame; the window is only updated on change.
    fn title(&mut self) -> Option<String> {
        None
    }
}

#[derive(Debug, Clone)]
pub struct WindowConfig {
    pub title: String,
    /// Logical size of the window at startup.
    pub width: u32,
    pub height: u32,
    pub fullscreen: bool,
    /// Use native Wayland on Linux even when X11 (XWayland) is available.
    pub prefer_wayland: bool,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            title: "DJ".to_string(),
            // The target 7" panel.
            width: 1280,
            height: 800,
            fullscreen: false,
            prefer_wayland: false,
        }
    }
}

/// Build the event loop. On Linux desktops this prefers X11 (XWayland under a Wayland
/// session) because winit 0.30's Wayland backend doesn't deliver dropped files; native
/// Wayland is used when there is no X server or `prefer_wayland` is set.
fn event_loop(config: &WindowConfig) -> anyhow::Result<EventLoop<()>> {
    #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "openbsd"))]
    {
        use winit::platform::wayland::EventLoopBuilderExtWayland;
        use winit::platform::x11::EventLoopBuilderExtX11;
        let mut builder = EventLoop::builder();
        let has_x11 = std::env::var_os("DISPLAY").is_some();
        let has_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        if has_x11 && !(config.prefer_wayland && has_wayland) {
            tracing::info!("window: X11 (drag and drop supported)");
            builder.with_x11();
        } else if has_wayland {
            tracing::info!("window: Wayland (drag and drop not supported by winit 0.30)");
            builder.with_wayland();
        }
        Ok(builder.build()?)
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd", target_os = "openbsd")))]
    {
        let _ = config;
        Ok(EventLoop::new()?)
    }
}

/// Run the UI event loop on the current (main) thread until the window closes or the
/// delegate asks to exit.
pub fn run<D: Delegate>(config: WindowConfig, delegate: D) -> anyhow::Result<()> {
    let event_loop = event_loop(&config)?;
    // Redraw continuously; presentation is paced by vsync (FIFO).
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App {
        config,
        delegate,
        window: None,
        gpu: None,
        error: None,
        title: String::new(),
        cursor: (0.0, 0.0),
        touch: None,
        failed_frames: 0,
    };
    event_loop.run_app(&mut app)?;
    match app.error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

struct App<D> {
    config: WindowConfig,
    delegate: D,
    window: Option<Arc<Window>>,
    gpu: Option<gpu::Gpu>,
    error: Option<anyhow::Error>,
    /// Title currently shown, to avoid setting it every frame.
    title: String,
    /// Last cursor position, for mouse button events (which carry none).
    cursor: (f32, f32),
    /// The finger being tracked; other simultaneous touches are ignored for now.
    touch: Option<u64>,
    /// Frames in a row that failed to render; the app gives up after
    /// [`MAX_FAILED_FRAMES`].
    failed_frames: u32,
}

/// About five seconds of frames that all fail to render: something is badly wrong.
const MAX_FAILED_FRAMES: u32 = 300;
/// The smallest window the layout works in (logical pixels).
const MIN_WINDOW: (u32, u32) = (800, 500);

impl<D: Delegate> App<D> {
    fn create_window(&mut self, event_loop: &ActiveEventLoop) -> anyhow::Result<()> {
        let mut attributes = Window::default_attributes()
            .with_title(self.config.title.clone())
            .with_inner_size(winit::dpi::LogicalSize::new(
                self.config.width,
                self.config.height,
            ))
            .with_min_inner_size(winit::dpi::LogicalSize::new(MIN_WINDOW.0, MIN_WINDOW.1))
            .with_resizable(true);
        if self.config.fullscreen {
            attributes =
                attributes.with_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
        }
        let window = Arc::new(event_loop.create_window(attributes)?);
        let gpu = gpu::Gpu::new(event_loop, Arc::clone(&window))?;
        self.window = Some(window);
        self.gpu = Some(gpu);
        Ok(())
    }

    fn send(&mut self, event_loop: &ActiveEventLoop, input: Input) {
        if self.delegate.on_input(input) == Flow::Exit {
            event_loop.exit();
        }
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, err: anyhow::Error) {
        tracing::error!("UI error: {err:#}");
        self.error = Some(err);
        event_loop.exit();
    }
}

impl<D: Delegate> ApplicationHandler for App<D> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        if let Err(err) = self.create_window(event_loop) {
            self.fail(event_loop, err);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(gpu) = self.gpu.as_mut() {
                    gpu.resize(size.width, size.height);
                }
            }
            // F11 toggles fullscreen, handled here as it is the window's business.
            WindowEvent::KeyboardInput { event, .. }
                if event.logical_key == WinitKey::Named(NamedKey::F11) =>
            {
                if event.state == ElementState::Pressed && !event.repeat {
                    if let Some(window) = self.window.as_ref() {
                        let fullscreen = match window.fullscreen() {
                            Some(_) => None,
                            None => Some(winit::window::Fullscreen::Borderless(None)),
                        };
                        window.set_fullscreen(fullscreen);
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let Some(input) = translate_key(&event) {
                    if self.delegate.on_input(input) == Flow::Exit {
                        event_loop.exit();
                        return;
                    }
                }
                let typed = match (event.state, &event.text) {
                    (ElementState::Pressed, Some(text)) if !text.chars().any(char::is_control) => {
                        Some(text.to_string())
                    }
                    _ => None,
                };
                if let Some(text) = typed {
                    self.send(event_loop, Input::Text(text));
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x as f32, position.y as f32);
                let (x, y) = self.cursor;
                self.send(event_loop, Input::PointerMove { x, y });
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                let (x, y) = self.cursor;
                let input = match state {
                    ElementState::Pressed => Input::PointerDown { x, y },
                    ElementState::Released => Input::PointerUp { x, y },
                };
                self.send(event_loop, input);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    // Touchpads report pixels; about 40 per notch.
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / 40.0,
                };
                let (x, y) = self.cursor;
                self.send(event_loop, Input::Scroll { x, y, lines });
            }
            WindowEvent::Touch(touch) => {
                let (x, y) = (touch.location.x as f32, touch.location.y as f32);
                let input = match (touch.phase, self.touch) {
                    (TouchPhase::Started, None) => {
                        self.touch = Some(touch.id);
                        Some(Input::PointerDown { x, y })
                    }
                    (TouchPhase::Moved, Some(id)) if id == touch.id => {
                        Some(Input::PointerMove { x, y })
                    }
                    (TouchPhase::Ended | TouchPhase::Cancelled, Some(id)) if id == touch.id => {
                        self.touch = None;
                        Some(Input::PointerUp { x, y })
                    }
                    _ => None,
                };
                if let Some(input) = input {
                    self.send(event_loop, input);
                }
            }
            WindowEvent::DroppedFile(path) => {
                if self.delegate.on_input(Input::FileDropped(path)) == Flow::Exit {
                    event_loop.exit();
                }
            }
            WindowEvent::RedrawRequested => {
                let Some(gpu) = self.gpu.as_mut() else {
                    return;
                };
                let (width, height) = gpu.size();
                let scale = self
                    .window
                    .as_ref()
                    .map_or(1.0, |w| w.scale_factor() as f32);
                let scene = self.delegate.scene(FrameSize {
                    width,
                    height,
                    scale,
                });
                // One bad frame (a surface mid-resize, say) shouldn't end the app; only
                // give up if nothing renders for a while.
                match gpu.render(&scene) {
                    Ok(()) => self.failed_frames = 0,
                    Err(err) => {
                        self.failed_frames += 1;
                        if self.failed_frames == 1 {
                            tracing::warn!("frame failed to render: {err:#}");
                        }
                        if self.failed_frames >= MAX_FAILED_FRAMES {
                            self.fail(event_loop, err);
                            return;
                        }
                    }
                }
                if let Some(title) = self.delegate.title() {
                    if title != self.title {
                        if let Some(window) = self.window.as_ref() {
                            window.set_title(&title);
                        }
                        self.title = title;
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }
}

fn translate_key(event: &KeyEvent) -> Option<Input> {
    let key = match &event.logical_key {
        WinitKey::Named(NamedKey::Space) => Key::Space,
        WinitKey::Named(NamedKey::ArrowUp) => Key::Up,
        WinitKey::Named(NamedKey::ArrowDown) => Key::Down,
        WinitKey::Named(NamedKey::ArrowLeft) => Key::Left,
        WinitKey::Named(NamedKey::ArrowRight) => Key::Right,
        WinitKey::Named(NamedKey::Escape) => Key::Escape,
        WinitKey::Named(NamedKey::Enter) => Key::Enter,
        WinitKey::Named(NamedKey::Tab) => Key::Tab,
        WinitKey::Named(NamedKey::PageUp) => Key::PageUp,
        WinitKey::Named(NamedKey::PageDown) => Key::PageDown,
        WinitKey::Named(NamedKey::Backspace) => Key::Backspace,
        WinitKey::Character(text) => Key::Char(text.chars().next()?.to_ascii_lowercase()),
        _ => return None,
    };
    let input = match event.state {
        ElementState::Pressed => Input::KeyDown {
            key,
            repeat: event.repeat,
        },
        ElementState::Released => Input::KeyUp { key },
    };
    Some(input)
}
