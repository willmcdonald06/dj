//! Instanced shape renderer: rectangles (rounded, outlined, gradient), arcs and
//! triangles, the primitives for meters, markers and widgets.

use crate::scene::{Rect, Shape};

/// Floats per instance: rect, colour, bottom colour, misc (radius, kind, viewport),
/// shape parameters; see `rects.wgsl`.
const FLOATS_PER_RECT: usize = 20;
const BYTES_PER_RECT: u64 = (FLOATS_PER_RECT * std::mem::size_of::<f32>()) as u64;

pub struct RectRenderer {
    pipeline: wgpu::RenderPipeline,
    buffer: wgpu::Buffer,
    capacity: usize,
    staging: Vec<f32>,
    count: u32,
}

impl RectRenderer {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::include_wgsl!("rects.wgsl"));
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rects"),
            bind_group_layouts: &[],
            immediate_size: 0,
        });
        let attributes = wgpu::vertex_attr_array![
            0 => Float32x4,
            1 => Float32x4,
            2 => Float32x4,
            3 => Float32x4,
            4 => Float32x4
        ];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rects"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: BYTES_PER_RECT,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &attributes,
                })],
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

        let capacity = 256;
        Self {
            pipeline,
            buffer: create_buffer(device, capacity),
            capacity,
            staging: Vec::with_capacity(capacity * FLOATS_PER_RECT),
            count: 0,
        }
    }

    /// Convert rects to clip space and upload them.
    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        rects: &[Rect],
        width: u32,
        height: u32,
    ) {
        self.staging.clear();
        let (vw, vh) = (width.max(1) as f32, height.max(1) as f32);
        for r in rects {
            let (kind, params) = match r.shape {
                Shape::Fill => (0.0, [0.0; 4]),
                Shape::Stroke { width } => (1.0, [width, 0.0, 0.0, 0.0]),
                Shape::Arc {
                    thickness,
                    start,
                    end,
                } => (2.0, [thickness, start, end, 0.0]),
                Shape::Triangle { quarter_turns } => (3.0, [quarter_turns as f32, 0.0, 0.0, 0.0]),
                Shape::Spike { quarter_turns } => (4.0, [quarter_turns as f32, 0.0, 0.0, 0.0]),
            };
            let (c, c2) = (r.color, r.bottom.unwrap_or(r.color));
            self.staging.extend_from_slice(&[
                r.x, r.y, r.width, r.height, c.r, c.g, c.b, c.a, c2.r, c2.g, c2.b, c2.a, r.radius,
                kind, vw, vh,
            ]);
            self.staging.extend_from_slice(&params);
        }

        if rects.len() > self.capacity {
            self.capacity = rects.len().next_power_of_two();
            self.buffer = create_buffer(device, self.capacity);
        }
        if !self.staging.is_empty() {
            queue.write_buffer(&self.buffer, 0, bytemuck::cast_slice(&self.staging));
        }
        self.count = rects.len() as u32;
    }

    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        if self.count == 0 {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_vertex_buffer(0, self.buffer.slice(..));
        pass.draw(0..6, 0..self.count);
    }
}

fn create_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("rect instances"),
        size: capacity as u64 * BYTES_PER_RECT,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}
