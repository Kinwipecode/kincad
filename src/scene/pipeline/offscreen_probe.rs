//! Renders a drawing without a window and writes a PNG.
//!
//! Checking a tessellation change used to need a person, a running app and a
//! screenshot. This module closes that loop: it loads the file, builds exactly
//! the caches the app builds, uploads those meshes, draws a shaded frame into an
//! offscreen texture and writes it as a PNG next to a short text summary.
//!
//! Used by the ignored test `offscreen_render`; the app never calls it.

use std::path::Path;

use iced::futures::executor::block_on;

use crate::scene::model::mesh_model::MeshLodSet;
use crate::scene::view::camera::Camera;
use super::wgpu;

/// What one probe run reports back to the caller.
#[derive(Debug, Default)]
pub struct ProbeReport {
    /// The meshes drawn, in the order the batch saw them.
    pub meshes: Vec<MeshSummary>,
}

/// Per-solid numbers worth seeing without a window.
#[derive(Debug, Default, Clone)]
pub struct MeshSummary {
    pub name: String,
    pub triangles: usize,
    pub colors: usize,
    pub instanced: bool,
    pub bounds: [f32; 6],
}

/// Renders `file` from a viewpoint that shows the whole model and writes
/// `out_png`. Returns what was drawn.
pub fn render_file(
    file: &Path,
    out_png: &Path,
    width: u32,
    height: u32,
) -> Result<ProbeReport, String> {
    let doc = crate::io::load_file(file).map_err(|error| format!("load: {error:?}"))?;
    let caches = crate::scene::build_derived_caches_with_progress(&doc, &|_| {}, None);
    if let Ok(probe_path) = std::env::var("OCS_MESH_PROBE") {
        cadkernel::brep::mesh_probe::drain_to(std::path::Path::new(&probe_path));
    }

    let mut meshes: Vec<MeshLodSet> = caches
        .meshes
        .values()
        .chain(caches.block_meshes.values())
        .cloned()
        .collect();
    // Block solids are drawn as instances; an identity placement keeps them in
    // the frame without pretending to know where their INSERT sits.
    for mesh in &mut meshes {
        if mesh.instance_transform.is_none() && mesh.instance_source.is_some() {
            mesh.instance_transform = Some(acadrust::types::Transform::identity());
        }
    }

    // PROBE_ONLY=<name>: draw a single body, so the one that makes a hole is
    // obvious. Everything else keeps the whole model.
    if let Ok(only) = std::env::var("PROBE_ONLY") {
        meshes.retain(|set| {
            set.geometry_lods()
                .first()
                .is_some_and(|mesh| mesh.name == only)
        });
        if meshes.is_empty() {
            return Err(format!("PROBE_ONLY={only} matched no mesh"));
        }
    }

    let report_meshes: Vec<MeshSummary> = meshes
        .iter()
        .map(|set| MeshSummary {
            name: set
                .geometry_lods()
                .first()
                .map(|mesh| mesh.name.clone())
                .unwrap_or_default(),
            triangles: set
                .geometry_lods()
                .first()
                .map_or(0, |mesh| mesh.indices.len() / 3),
            colors: set
                .geometry_lods()
                .first()
                .map_or(0, |mesh| mesh.triangle_colors.len()),
            instanced: set.instance_transform.is_some(),
            bounds: [
                set.world_aabb[0],
                set.world_aabb[1],
                set.z_aabb[0],
                set.world_aabb[2],
                set.world_aabb[3],
                set.z_aabb[1],
            ],
        })
        .collect();

    let (mut min, mut max) = ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]);
    for summary in &report_meshes {
        for axis in 0..3 {
            min[axis] = min[axis].min(summary.bounds[axis]);
            max[axis] = max[axis].max(summary.bounds[axis + 3]);
        }
    }
    if !min[0].is_finite() {
        return Err("no mesh in the drawing".to_string());
    }

    let probe = request_probe_device()?;
    let device = &probe.device;
    let queue = &probe.queue;
    let mut pipeline = super::Pipeline::new(device, queue, wgpu::TextureFormat::Rgba8UnormSrgb);
    let size = iced::Size::new(width, height);


    let bounds = iced::Rectangle::new(
        iced::Point::ORIGIN,
        iced::Size::new(width as f32, height as f32),
    );
    let mut camera = Camera::default();
    let mut camera = Camera::default();
    // A three-quarter view: from straight above only the lid is visible, so a
    // hole in the skin of a vessel cannot be judged from a top view.
    let yaw: f32 = std::env::var("PROBE_YAW")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0.65);
    let pitch: f32 = std::env::var("PROBE_PITCH")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0.35);
    camera.yaw = yaw;
    camera.pitch = pitch;
    camera.rotation = crate::scene::view::camera::yaw_pitch_to_quat(yaw, pitch, 0.0);
    camera.fit_to_bounds_f64(
        glam::DVec3::new(f64::from(min[0]), f64::from(min[1]), f64::from(min[2])),
        glam::DVec3::new(f64::from(max[0]), f64::from(max[1]), f64::from(max[2])),
        width as f32 / height as f32,
    );
    let uniforms = super::Uniforms::new(&camera, bounds, false);
    pipeline.upload_uniforms(device, queue, &uniforms);
    pipeline.upload_mesh_batch(device, queue, &meshes);
    pipeline.ensure_depth_texture(device, size);

    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("probe.target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("probe.encoder"),
    });
    pipeline.render(
        &mut encoder,
        &view,
        size,
        iced::Rectangle {
            x: 0,
            y: 0,
            width,
            height,
        },
        [0.12, 0.12, 0.14, 1.0],
        false,
        false,
        false,
    );
    queue.submit(Some(encoder.finish()));

    let pixels = read_back(device, queue, &texture, width, height)?;
    if let Some(parent) = out_png.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("mkdir: {error}"))?;
    }
    let image = image::RgbaImage::from_raw(width, height, pixels)
        .ok_or_else(|| "pixel buffer size mismatch".to_string())?;
    image.save(out_png).map_err(|error| format!("png: {error}"))?;

    Ok(ProbeReport {
        meshes: report_meshes,
    })
}

fn read_back(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, String> {
    let bytes_per_row = (width * 4).next_multiple_of(256);
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("probe.readback"),
        size: u64::from(bytes_per_row) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("probe.copy"),
    });
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(Some(encoder.finish()));

    let slice = buffer.slice(..);
    let (sender, receiver) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = sender.send(result);
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|error| format!("poll: {error}"))?;
    receiver
        .recv()
        .map_err(|error| format!("map channel: {error}"))?
        .map_err(|error| format!("map: {error}"))?;

    let mapped = slice.get_mapped_range();
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for row in 0..height {
        let start = (row * bytes_per_row) as usize;
        pixels.extend_from_slice(&mapped[start..start + (width * 4) as usize]);
    }
    drop(mapped);
    buffer.unmap();
    Ok(pixels)
}

/// A device without a window, for the probe.
pub struct ProbeDevice {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

/// Requests an adapter and device headlessly - the same call the GPU tests make.
pub fn request_probe_device() -> Result<ProbeDevice, String> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
        .map_err(|error| format!("no GPU adapter: {error}"))?;
    let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        required_limits: adapter.limits(),
        ..Default::default()
    }))
    .map_err(|error| format!("device: {error}"))?;
    Ok(ProbeDevice { device, queue })
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    /// `cargo test --lib offscreen_render -- --ignored --nocapture`
    ///
    /// Set `PROBE_FILE` to the drawing and `PROBE_PNG` to where the picture goes.
    #[test]
    #[ignore = "requires a GPU adapter; run on demand with PROBE_FILE set"]
    fn offscreen_render() {
        let file = std::env::var("PROBE_FILE").expect("PROBE_FILE fehlt");
        let out = std::env::var("PROBE_PNG")
            .unwrap_or_else(|_| "probe.png".to_string());
        let started = std::time::Instant::now();
        let report = render_file(Path::new(&file), Path::new(&out), 900, 1200)
            .expect("render");
        println!("[probe] {} in {:.1} s", out, started.elapsed().as_secs_f64());
        for mesh in &report.meshes {
            println!(
                "[probe] {} tris={} colors={} instanced={} bounds={:?}",
                mesh.name, mesh.triangles, mesh.colors, mesh.instanced, mesh.bounds
            );
        }
    }
}
