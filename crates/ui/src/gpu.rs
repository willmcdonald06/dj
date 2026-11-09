//! wgpu device, window surface, offscreen rendering and the per-frame draw.

use std::sync::Arc;

use anyhow::Context as _;
use winit::event_loop::ActiveEventLoop;
use winit::window::Window;

use crate::rects::RectRenderer;
use crate::scene::Scene;
use crate::text::TextRenderer;
use crate::waveform::WaveformRenderer;

/// Draws a [`Scene`] into any texture view of its format.
struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    rects: RectRenderer,
    waveforms: WaveformRenderer,
    overlay: RectRenderer,
    text: TextRenderer,
    popup_rects: RectRenderer,
    popup_text: TextRenderer,
}

impl Renderer {
    fn new(device: wgpu::Device, queue: wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let rects = RectRenderer::new(&device, format);
        let waveforms = WaveformRenderer::new(&device, format);
        let overlay = RectRenderer::new(&device, format);
        let text = TextRenderer::new(&device, &queue, format);
        let popup_rects = RectRenderer::new(&device, format);
        let popup_text = TextRenderer::new(&device, &queue, format);
        Self {
            device,
            queue,
            rects,
            waveforms,
            overlay,
            text,
            popup_rects,
            popup_text,
        }
    }

    /// Record and submit the draw of `scene` into `view`.
    fn draw(
        &mut self,
        view: &wgpu::TextureView,
        scene: &Scene,
        width: u32,
        height: u32,
        extra: impl FnOnce(&mut wgpu::CommandEncoder),
    ) -> anyhow::Result<()> {
        let (device, queue) = (&self.device, &self.queue);
        self.rects
            .prepare(device, queue, &scene.rects, width, height);
        self.waveforms
            .prepare(device, queue, &scene.waveforms, width, height);
        self.overlay
            .prepare(device, queue, &scene.overlay, width, height);
        self.text
            .prepare(device, queue, &scene.texts, width, height)?;
        self.popup_rects
            .prepare(device, queue, &scene.popup_rects, width, height);
        self.popup_text
            .prepare(device, queue, &scene.popup_texts, width, height)?;

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("frame"),
        });
        {
            let bg = scene.background;
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("main"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: f64::from(bg.r),
                            g: f64::from(bg.g),
                            b: f64::from(bg.b),
                            a: f64::from(bg.a),
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.rects.draw(&mut pass);
            self.waveforms.draw(&mut pass);
            self.overlay.draw(&mut pass);
            self.text.draw(&mut pass)?;
            self.popup_rects.draw(&mut pass);
            self.popup_text.draw(&mut pass)?;
        }
        extra(&mut encoder);
        self.queue.submit([encoder.finish()]);
        Ok(())
    }
}

fn request_device(adapter: &wgpu::Adapter) -> anyhow::Result<(wgpu::Device, wgpu::Queue)> {
    let info = adapter.get_info();
    tracing::info!("GPU: {} ({:?})", info.name, info.backend);
    let device = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("dj"),
        // The CM5 GPU (VideoCore VII) supports downlevel limits only.
        required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
        ..Default::default()
    }))
    .context("creating GPU device")?;
    // wgpu panics on errors nobody captures; a glitch (a surface caught mid-resize, say)
    // shouldn't take the app down, so log them instead, a few at a time.
    let logged = Arc::new(std::sync::atomic::AtomicU64::new(0));
    device.0.on_uncaptured_error(Arc::new(move |err| {
        let n = logged.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n < 10 || n % 1000 == 0 {
            tracing::error!("GPU error (#{}): {err}", n + 1);
        }
    }));
    Ok(device)
}

/// Rendering to a window.
pub struct Gpu {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    renderer: Renderer,
}

impl Gpu {
    pub fn new(event_loop: &ActiveEventLoop, window: Arc<Window>) -> anyhow::Result<Self> {
        let display = Box::new(event_loop.owned_display_handle());
        let instance = wgpu::Instance::new(
            wgpu::InstanceDescriptor::new_with_display_handle_from_env(display),
        );
        let surface = instance
            .create_surface(Arc::clone(&window))
            .context("creating window surface")?;

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .context("no suitable GPU adapter")?;
        let (device, queue) = request_device(&adapter)?;

        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .context("surface not supported by adapter")?;
        let caps = surface.get_capabilities(&adapter);
        // Prefer an sRGB format so colors blend correctly.
        if let Some(format) = caps.formats.iter().copied().find(|f| f.is_srgb()) {
            config.format = format;
        }
        config.present_mode = wgpu::PresentMode::Fifo;
        config.desired_maximum_frame_latency = 2;
        surface.configure(&device, &config);

        let renderer = Renderer::new(device, queue, config.format);
        Ok(Self {
            surface,
            config,
            renderer,
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.renderer.device, &self.config);
    }

    pub fn render(&mut self, scene: &Scene) -> anyhow::Result<()> {
        let device = &self.renderer.device;
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => frame,
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                // Draw this one, reconfigure for the next.
                self.surface.configure(device, &self.config);
                frame
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(device, &self.config);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                // Seen while a window changes size or mode; try again next frame.
                tracing::warn!("surface validation error; reconfiguring");
                self.surface.configure(device, &self.config);
                return Ok(());
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let (width, height) = self.size();
        self.renderer.draw(&view, scene, width, height, |_| {})?;
        self.renderer.queue.present(frame);
        self.renderer.text.trim();
        self.renderer.popup_text.trim();
        Ok(())
    }
}

/// Renders scenes into memory, without a window: for screenshots and tests.
pub struct Offscreen {
    renderer: Renderer,
}

const OFFSCREEN_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

impl Offscreen {
    pub fn new() -> anyhow::Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: None,
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .context("no suitable GPU adapter")?;
        let (device, queue) = request_device(&adapter)?;
        Ok(Self {
            renderer: Renderer::new(device, queue, OFFSCREEN_FORMAT),
        })
    }

    /// Render `scene` at `width`×`height` and return tightly packed RGBA8 (sRGB) pixels.
    pub fn render(&mut self, scene: &Scene, width: u32, height: u32) -> anyhow::Result<Vec<u8>> {
        let device = &self.renderer.device;
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: OFFSCREEN_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        // Rows must be padded to 256 bytes for the copy.
        let row = width * 4;
        let padded_row =
            row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("offscreen readback"),
            size: padded_row as u64 * height as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.renderer.draw(&view, scene, width, height, |encoder| {
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(padded_row),
                        rows_per_image: Some(height),
                    },
                },
                size,
            );
        })?;

        let slice = buffer.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.renderer
            .device
            .poll(wgpu::PollType::wait_indefinitely())
            .context("waiting for the GPU")?;
        receiver
            .recv()
            .context("readback callback dropped")?
            .context("mapping readback buffer")?;
        let mapped = slice
            .get_mapped_range()
            .map_err(|err| anyhow::anyhow!("reading back pixels: {err:?}"))?;
        let mut pixels = Vec::with_capacity((row * height) as usize);
        for y in 0..height as usize {
            let start = y * padded_row as usize;
            pixels.extend_from_slice(&mapped[start..start + row as usize]);
        }
        drop(mapped);
        buffer.unmap();
        self.renderer.text.trim();
        Ok(pixels)
    }
}
