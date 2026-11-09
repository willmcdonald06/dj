//! GPU waveform renderer.
//!
//! Each [`WaveformSummary`] is uploaded once into an `Rgba8Unorm` texture: all levels
//! one after another, wrapped into rows (`[low, mid, high, peak]` per texel). Drawing a
//! view picks the level whose bins are about a pixel wide, so the per-frame cost is one
//! small uniform write per view regardless of track length or zoom.

use std::collections::HashMap;
use std::sync::Arc;

use dj_analysis::WaveformSummary;

use crate::scene::{WaveformStyle, WaveformView};

/// Uniform stride per view; dynamic offsets must be multiples of this.
const VIEW_STRIDE: u64 = 256;

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ViewUniform {
    rect: [f32; 4],
    viewport: [f32; 2],
    level_offset: u32,
    level_len: u32,
    first_bin: i32,
    tex_width: u32,
    bin_frac: f32,
    bins_per_px: f32,
    span: u32,
    brightness: f32,
    /// 0 = spectrum colours, 1 = three bands.
    style: u32,
    _pad: f32,
}

/// A summary on the GPU.
struct Uploaded {
    /// Keeps the summary alive, so its address (the cache key) stays unique.
    data: Arc<WaveformSummary>,
    bind_group: wgpu::BindGroup,
    tex_width: u32,
    /// Texel offset of each level; levels below `first_level` were dropped to fit.
    offsets: Vec<u32>,
    first_level: usize,
    used: bool,
}

pub struct WaveformRenderer {
    pipeline: wgpu::RenderPipeline,
    view_layout: wgpu::BindGroupLayout,
    texture_layout: wgpu::BindGroupLayout,
    views: wgpu::Buffer,
    views_bind_group: wgpu::BindGroup,
    view_capacity: usize,
    max_texture: u32,
    uploaded: HashMap<usize, Uploaded>,
    /// This frame's draws: (cache key, uniform offset).
    draws: Vec<(usize, u32)>,
}

impl WaveformRenderer {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::include_wgsl!("waveform.wgsl"));
        let view_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("waveform view"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: wgpu::BufferSize::new(
                        std::mem::size_of::<ViewUniform>() as u64
                    ),
                },
                count: None,
            }],
        });
        let texture_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("waveform bins"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("waveform"),
            bind_group_layouts: &[Some(&view_layout), Some(&texture_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("waveform"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let view_capacity = 8;
        let (views, views_bind_group) = create_views(device, &view_layout, view_capacity);
        Self {
            pipeline,
            view_layout,
            texture_layout,
            views,
            views_bind_group,
            view_capacity,
            max_texture: device.limits().max_texture_dimension_2d,
            uploaded: HashMap::new(),
            draws: Vec::new(),
        }
    }

    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        views: &[WaveformView],
        width: u32,
        height: u32,
    ) {
        for entry in self.uploaded.values_mut() {
            entry.used = false;
        }
        self.draws.clear();
        if views.len() > self.view_capacity {
            self.view_capacity = views.len().next_power_of_two();
            (self.views, self.views_bind_group) =
                create_views(device, &self.view_layout, self.view_capacity);
        }

        let mut uniforms = vec![0u8; views.len() * VIEW_STRIDE as usize];
        for (i, view) in views.iter().enumerate() {
            let key = Arc::as_ptr(&view.data) as usize;
            if !self.uploaded.contains_key(&key) {
                match self.upload(device, queue, &view.data) {
                    Some(uploaded) => {
                        self.uploaded.insert(key, uploaded);
                    }
                    None => continue,
                }
            }
            let Some(uploaded) = self.uploaded.get_mut(&key) else {
                continue;
            };
            uploaded.used = true;
            let Some(uniform) = view_uniform(view, uploaded, width, height) else {
                continue;
            };
            let offset = i * VIEW_STRIDE as usize;
            uniforms[offset..offset + std::mem::size_of::<ViewUniform>()]
                .copy_from_slice(bytemuck::bytes_of(&uniform));
            self.draws.push((key, offset as u32));
        }
        if !uniforms.is_empty() {
            queue.write_buffer(&self.views, 0, &uniforms);
        }
        self.uploaded.retain(|_, entry| entry.used);
    }

    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        if self.draws.is_empty() {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        for &(key, offset) in &self.draws {
            let Some(uploaded) = self.uploaded.get(&key) else {
                continue;
            };
            pass.set_bind_group(0, &self.views_bind_group, &[offset]);
            pass.set_bind_group(1, &uploaded.bind_group, &[]);
            pass.draw(0..6, 0..1);
        }
    }

    fn upload(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        data: &Arc<WaveformSummary>,
    ) -> Option<Uploaded> {
        let tex_width = self.max_texture.min(4096);
        let capacity = tex_width as usize * self.max_texture as usize;
        // Drop the finest levels if the whole pyramid doesn't fit (very long tracks).
        let mut first_level = 0;
        while first_level < data.levels.len()
            && data.levels[first_level..]
                .iter()
                .map(Vec::len)
                .sum::<usize>()
                > capacity
        {
            first_level += 1;
        }
        let levels = &data.levels[first_level..];
        let total: usize = levels.iter().map(Vec::len).sum();
        if total == 0 {
            return None;
        }
        let rows = total.div_ceil(tex_width as usize) as u32;
        let mut texels = Vec::with_capacity(rows as usize * tex_width as usize * 4);
        let mut offsets = Vec::with_capacity(data.levels.len());
        offsets.resize(first_level, 0);
        for level in levels {
            offsets.push((texels.len() / 4) as u32);
            for bin in level {
                texels.extend_from_slice(bin);
            }
        }
        texels.resize(rows as usize * tex_width as usize * 4, 0);

        let size = wgpu::Extent3d {
            width: tex_width,
            height: rows,
            depth_or_array_layers: 1,
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("waveform bins"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &texels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(tex_width * 4),
                rows_per_image: Some(rows),
            },
            size,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("waveform bins"),
            layout: &self.texture_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            }],
        });
        Some(Uploaded {
            data: Arc::clone(data),
            bind_group,
            tex_width,
            offsets,
            first_level,
            used: true,
        })
    }
}

fn create_views(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    capacity: usize,
) -> (wgpu::Buffer, wgpu::BindGroup) {
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("waveform views"),
        size: capacity as u64 * VIEW_STRIDE,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("waveform views"),
        layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                buffer: &buffer,
                offset: 0,
                size: wgpu::BufferSize::new(std::mem::size_of::<ViewUniform>() as u64),
            }),
        }],
    });
    (buffer, bind_group)
}

/// Uniforms for one view: which level to read and where its left edge falls.
fn view_uniform(
    view: &WaveformView,
    uploaded: &Uploaded,
    width: u32,
    height: u32,
) -> Option<ViewUniform> {
    if view.width <= 0.0 || view.height <= 0.0 || view.frames_per_pixel <= 0.0 {
        return None;
    }
    let data = &uploaded.data;
    let level = data
        .level_for(view.frames_per_pixel)
        .max(uploaded.first_level);
    let bin_frames = data.bin_frames(level) as f64;
    let position = view.start_frame / bin_frames;
    let first_bin = position.floor();
    let bins_per_px = (view.frames_per_pixel / bin_frames) as f32;
    Some(ViewUniform {
        rect: [view.x, view.y, view.width, view.height],
        viewport: [width as f32, height as f32],
        level_offset: uploaded.offsets[level],
        level_len: data.levels[level].len() as u32,
        first_bin: first_bin.clamp(i32::MIN as f64, i32::MAX as f64) as i32,
        tex_width: uploaded.tex_width,
        bin_frac: (position - first_bin) as f32,
        bins_per_px,
        span: (bins_per_px.ceil() as u32 + 1).clamp(1, 4),
        brightness: view.brightness,
        style: match view.style {
            WaveformStyle::Spectrum => 0,
            WaveformStyle::ThreeBand => 1,
        },
        _pad: 0.0,
    })
}
