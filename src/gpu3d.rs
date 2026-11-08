//! GPU renderer for the 3D preview: a port of ChroMapper's BloomFog environment rendering.
//!
//! Per frame: (1) light quads (one per BloomFogObject) into a 512² fog texture, blurred by an 8-level
//! pyramid with an auto-exposure knee and ACES, plus the sky gradient; (2) the scene into an HDR target
//! whose alpha is the "bloom amount": Lit surfaces, light tubes, fake glows, mirrors, spectrograms, all
//! fogged toward the fog texture, and the fog texture as the sky; (3) ChroMapper's post bloom
//! (alpha-gated pyramid, white boost) into the sRGB output. World units are ChroMapper's.
//! Shaders: gpu3d.wgsl.

use crate::environment::{EnvFile, EnvMaterial, FogParams};
use eframe::wgpu;
use std::collections::HashMap;
use std::path::Path;
use wgpu::util::DeviceExt;

const HDR: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
const DEPTH: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
pub const OUTPUT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
/// ChroMapper's `bloomFogResolution` and fog frustum (BloomfogRendererSO: 130° x 130°).
const FOG_SIZE: u32 = 512;
const FOG_HALF_FOV_TAN: f32 = 2.144_507; // tan(65°)
const FOG_LINE_WIDTH: f32 = 0.02;
/// Post bloom (Main Bloom Profile / PyramidBloomController).
const BLOOM_WIDTH: u32 = 928;
const BLOOM_BLEND: f32 = 0.3;

type V3 = [f32; 3];

fn dot(a: V3, b: V3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

/// sRGB to linear (Unity's conversion for Color properties in linear color space).
pub fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

/// Unity's `Mathf.LinearToGammaSpace`.
fn linear_to_gamma(v: f32) -> f32 {
    if v <= 0.0 {
        0.0
    } else if v <= 0.003_130_8 {
        12.92 * v
    } else if v < 1.0 {
        1.055 * v.powf(0.416_666_7) - 0.055
    } else if v == 1.0 {
        1.0
    } else {
        v.powf(0.454_545_45)
    }
}

/// A camera in ChroMapper units.
#[derive(Clone, Copy, Debug)]
pub struct View {
    pub pos: V3,
    pub right: V3,
    pub up: V3,
    pub fwd: V3,
    /// Vertical field of view in degrees.
    pub vfov: f32,
    pub near: f32,
    pub far: f32,
}

impl View {
    /// From Unity euler angles (degrees; Unity applies Z, then X, then Y).
    pub fn from_euler(pos: V3, euler: V3, vfov: f32, near: f32, far: f32) -> View {
        let (x, y, z) = (euler[0].to_radians(), euler[1].to_radians(), euler[2].to_radians());
        let rx = |v: V3| [v[0], v[1] * x.cos() - v[2] * x.sin(), v[1] * x.sin() + v[2] * x.cos()];
        let ry = |v: V3| [v[0] * y.cos() + v[2] * y.sin(), v[1], -v[0] * y.sin() + v[2] * y.cos()];
        let rz = |v: V3| [v[0] * z.cos() - v[1] * z.sin(), v[0] * z.sin() + v[1] * z.cos(), v[2]];
        let r = |v: V3| ry(rx(rz(v)));
        View { pos, right: r([1.0, 0.0, 0.0]), up: r([0.0, 1.0, 0.0]), fwd: r([0.0, 0.0, 1.0]), vfov, near, far }
    }

    /// ChroMapper-PropPreview pose "x,y,z,rx,ry,rz,fov[,near,far]".
    pub fn from_pose(pose: &str) -> Option<View> {
        let v: Vec<f32> = pose.split(',').filter_map(|s| s.trim().parse().ok()).collect();
        (v.len() >= 7).then(|| {
            View::from_euler([v[0], v[1], v[2]], [v[3], v[4], v[5]], v[6], v.get(7).copied().unwrap_or(0.3), v.get(8).copied().unwrap_or(5000.0))
        })
    }

    /// The editor camera (metres, yaw/pitch) in ChroMapper units.
    pub fn from_camera(cam: &crate::view3d::Camera, unit: f32) -> View {
        View::from_euler(cam.pos.map(|c| c / unit), [-cam.pitch, cam.yaw, 0.0], cam.vfov, 0.3, 5000.0)
    }

    fn to_view(&self, p: V3) -> V3 {
        let d = sub(p, self.pos);
        [dot(d, self.right), dot(d, self.up), dot(d, self.fwd)]
    }
}

/// ChroMapper's five custom directional lights (`_DirectionalLight*`, colors already linear).
#[derive(Clone, Copy, Default, Debug)]
pub struct DirLights {
    pub pos: [[f32; 4]; 5],
    pub dir: [[f32; 4]; 5],
    pub color: [[f32; 4]; 5],
    pub radius: [f32; 5],
}

/// A BloomFogObject this frame (its light quad in the fog texture).
#[derive(Clone, Debug, Default)]
pub struct FogLight {
    pub enabled: bool,
    /// Event color: sRGB rgb, alpha = intensity.
    pub color: [f32; 4],
    /// Controller transform, Unity row-major.
    pub matrix: [f32; 16],
    pub length: f32,
    pub center: f32,
    pub start_width: f32,
    pub end_width: f32,
    pub start_alpha: f32,
    pub end_alpha: f32,
    pub length_alpha_fog_mul: f32,
    pub length_alpha_mul: f32,
    pub width_mul: f32,
    pub intensity_mul: f32,
    pub boost_to_white: f32,
    pub limit_alpha: bool,
    pub min_alpha: f32,
    pub max_alpha: f32,
    pub disable_on_zero: bool,
    /// Max blending (the default material and MaxBloomPrePassLine) instead of additive.
    pub blend_max: bool,
    pub priority: i64,
}

/// A background gradient drawn into the fog texture after the blur (tone-mapped).
#[derive(Clone, Debug, Default)]
pub struct SkyGradient {
    /// (sRGB color, start t, exponent).
    pub elements: Vec<([f32; 4], f32, f32)>,
    pub tint: [f32; 4],
}

impl SkyGradient {
    /// The 128x1 RGBA8 gradient texture (BloomPrePassBackgroundColorsGradient.UpdatePixels).
    fn pixels(&self) -> Vec<u8> {
        let e = &self.elements;
        let mut out = Vec::with_capacity(128 * 4);
        for i in 0..128 {
            let t = i as f32 / 127.0;
            let mut c = e.last().map_or([0.0; 4], |x| x.0);
            for k in (0..e.len().saturating_sub(1)).rev() {
                if t >= e[k].1 {
                    let (a, b) = (&e[k], &e[k + 1]);
                    let span = (b.1 - a.1).max(1e-6);
                    let f = ((t - a.1) / span).max(0.0).powf(a.2);
                    c = std::array::from_fn(|j| a.0[j] + (b.0[j] - a.0[j]) * f);
                    break;
                }
            }
            out.extend(c.map(|v| (v.clamp(0.0, 1.0) * 255.0) as u8));
        }
        out
    }
}

/// Per-renderer values that lights set (MaterialPropertyBlock), linear rgb.
#[derive(Clone, Copy, Debug)]
pub struct Block {
    pub color: Option<[f32; 4]>,
    pub alpha_width: Option<[f32; 4]>,
    pub size_params: Option<[f32; 4]>,
    pub emission_tex_color: Option<[f32; 4]>,
    pub enabled: bool,
}

impl Default for Block {
    fn default() -> Self {
        Block { color: None, alpha_width: None, size_params: None, emission_tex_color: None, enabled: true }
    }
}

/// The environment's state this frame.
#[derive(Default)]
pub struct SceneState {
    /// Object index → mesh-to-world matrix where it moved.
    pub moved: HashMap<usize, [f32; 16]>,
    /// Object index → light values.
    pub blocks: HashMap<usize, Block>,
    pub fog_lights: Vec<FogLight>,
    pub lights: DirLights,
    pub sky: Option<SkyGradient>,
    /// The mirror plane (normal, offset), from the environment's PlanarReflection.
    pub mirror_plane: Option<[f32; 4]>,
    /// Particle system index → its particles this frame (else the exported ones).
    pub particles: HashMap<usize, Vec<crate::environment::Particle>>,
}

pub struct Frame<'a> {
    pub view: View,
    pub width: u32,
    pub height: u32,
    pub env: &'a EnvFile,
    pub state: &'a SceneState,
    pub fog: FogParams,
    pub post_bloom: bool,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    pos: [f32; 3],
    normal: [f32; 3],
    uv: [f32; 2],
    color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct QuadVertex {
    s: [f32; 2],
    color: [f32; 4],
    uv: [f32; 3],
    view: [f32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Globals {
    view_proj: [[f32; 4]; 4],
    cam_pos: [f32; 4],
    right: [f32; 4],
    up: [f32; 4],
    fwd: [f32; 4],
    fog: [f32; 4],
    ratio: [f32; 4],
    screen: [f32; 4],
    light_pos: [[f32; 4]; 5],
    light_dir: [[f32; 4]; 5],
    light_color: [[f32; 4]; 5],
    light_radius: [[f32; 4]; 2],
    /// Clip plane (normal, offset) for the reflected view.
    clip: [f32; 4],
    /// x: clip on, y: mirrored winding.
    flags: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ObjectData {
    model: [[f32; 4]; 4],
    normal: [[f32; 4]; 4],
    info: [u32; 4],
    b: [[f32; 4]; 4],
    p: [[f32; 4]; 12],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct PassParams {
    texel: [f32; 4],
    combine: [f32; 4],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Lit = 0,
    BoxOpaque = 1,
    BoxTransparent = 2,
    FakeGlow = 3,
    Mirror = 4,
    Spectrogram = 5,
    Particles = 6,
}

const F_DIFFUSE: u32 = 1;
const F_SPECULAR: u32 = 2;
const F_FALLOFF: u32 = 4;
const F_BOTH_SIDES: u32 = 8;
const F_FOG: u32 = 16;
const F_HEIGHT_FOG: u32 = 32;
const F_VERTEX_COLOR: u32 = 64;
const F_EMISSION_SIMPLE: u32 = 128;
const F_ACES_BEFORE: u32 = 256;
const F_DIRT: u32 = 512;
const F_MAIN_TEXTURE: u32 = 1024;
const F_ALPHA_RED: u32 = 2048;
const F_SQUARE_ALPHA: u32 = 4096;
const F_FOG_COLOR: u32 = 8192;
const F_FOG_ALPHA: u32 = 16384;
const F_FOG_LERP: u32 = 32768;
const F_WHITEBOOST_MAIN: u32 = 65536;

struct GpuMaterial {
    kind: Kind,
    flags: u32,
    p: [[f32; 4]; 12],
    color: [f32; 4],
    additive: bool,
    queue: i64,
    bind: wgpu::BindGroup,
}

struct GpuMesh {
    vb: wgpu::Buffer,
    ib: wgpu::Buffer,
    count: u32,
}

/// A view's fog texture and its blur pyramid.
struct FogTargets {
    raw: wgpu::Texture,
    down: Vec<wgpu::Texture>,
    up: Vec<wgpu::Texture>,
    fin: wgpu::Texture,
}

impl FogTargets {
    fn new(d: &wgpu::Device) -> FogTargets {
        let rt = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING;
        let (mut down, mut up) = (Vec::new(), Vec::new());
        let mut s = FOG_SIZE / 2;
        while s >= 2 {
            down.push(texture(d, "fog down", s, s, HDR, rt));
            up.push(texture(d, "fog up", s, s, HDR, rt));
            s /= 2;
        }
        FogTargets {
            raw: texture(d, "fog raw", FOG_SIZE, FOG_SIZE, HDR, rt),
            down,
            up,
            fin: texture(d, "fog final", FOG_SIZE, FOG_SIZE, HDR, rt | wgpu::TextureUsages::COPY_SRC),
        }
    }
}

/// MirrorRendererSO (High quality): reflection resolution.
const MIRROR_SIZE: u32 = 1024;

/// MirrorRendererSO's reflected layers (5, 7–14, 20, 29).
const MIRROR_LAYERS: u32 = 537_952_160;

struct DrawItem {
    /// Drawn in mirror reflections.
    reflect: bool,
    object: u32,
    mesh: usize,
    material: usize,
    additive: bool,
    queue: i64,
}

struct Targets {
    size: (u32, u32),
    hdr: wgpu::Texture,
    depth: wgpu::TextureView,
    clamped: wgpu::Texture,
    output: wgpu::Texture,
    fog: FogTargets,
    mirror_fog: FogTargets,
    /// The reflected view (sRGB 8-bit, as ChroMapper's mirror render texture) and its depth.
    reflection: wgpu::Texture,
    reflection_depth: wgpu::TextureView,
    bloom_down: Vec<wgpu::Texture>,
    bloom_up: Vec<wgpu::Texture>,
    bloom_final: wgpu::Texture,
    bloom_scale: f32,
}

struct Pipelines {
    mesh_opaque: wgpu::RenderPipeline,
    mesh_add: wgpu::RenderPipeline,
    quad_add: wgpu::RenderPipeline,
    quad_max: wgpu::RenderPipeline,
    down4: wgpu::RenderPipeline,
    down13: wgpu::RenderPipeline,
    up: wgpu::RenderPipeline,
    up_box: wgpu::RenderPipeline,
    fog_final: wgpu::RenderPipeline,
    mirror_opaque: wgpu::RenderPipeline,
    mirror_add: wgpu::RenderPipeline,
    mirror_sky: wgpu::RenderPipeline,
    sky_gradient: wgpu::RenderPipeline,
    sky: wgpu::RenderPipeline,
    clamp: wgpu::RenderPipeline,
    composite: wgpu::RenderPipeline,
}

pub struct Gpu3d {
    device: wgpu::Device,
    queue: wgpu::Queue,
    env_key: usize,
    meshes: Vec<GpuMesh>,
    materials: Vec<GpuMaterial>,
    mask: wgpu::TextureView,
    targets: Option<Targets>,
    objects: wgpu::Buffer,
    objects_cap: usize,
    clamp_sampler: wgpu::Sampler,
    repeat_sampler: wgpu::Sampler,
    scene_layout: wgpu::BindGroupLayout,
    tex2_layout: wgpu::BindGroupLayout,
    fogtex_layout: wgpu::BindGroupLayout,
    mat_layout: wgpu::BindGroupLayout,
    pass_layout: wgpu::BindGroupLayout,
    pipes: Pipelines,
    white: wgpu::TextureView,
    black: wgpu::TextureView,
}

fn texture(device: &wgpu::Device, label: &str, w: u32, h: u32, format: wgpu::TextureFormat, usage: wgpu::TextureUsages) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: w.max(1), height: h.max(1), depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    })
}

fn view(t: &wgpu::Texture) -> wgpu::TextureView {
    t.create_view(&wgpu::TextureViewDescriptor::default())
}

fn color_att(v: &wgpu::TextureView, clear: bool) -> Option<wgpu::RenderPassColorAttachment<'_>> {
    Some(wgpu::RenderPassColorAttachment {
        view: v,
        depth_slice: None,
        resolve_target: None,
        ops: wgpu::Operations {
            load: if clear { wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT) } else { wgpu::LoadOp::Load },
            store: wgpu::StoreOp::Store,
        },
    })
}

fn mat_rows_to_cols(m: &[f32; 16]) -> [[f32; 4]; 4] {
    std::array::from_fn(|c| std::array::from_fn(|r| m[r * 4 + c]))
}

/// The 3x3 inverse transpose of a row-major matrix (for normals), column-major 4x4.
fn normal_matrix(m: &[f32; 16]) -> [[f32; 4]; 4] {
    let a = |r: usize, c: usize| m[r * 4 + c] as f64;
    let det = a(0, 0) * (a(1, 1) * a(2, 2) - a(1, 2) * a(2, 1)) - a(0, 1) * (a(1, 0) * a(2, 2) - a(1, 2) * a(2, 0))
        + a(0, 2) * (a(1, 0) * a(2, 1) - a(1, 1) * a(2, 0));
    let d = if det.abs() < 1e-12 { 1.0 } else { det };
    let cof = |r: usize, c: usize| {
        let (r1, r2) = ((r + 1) % 3, (r + 2) % 3);
        let (c1, c2) = ((c + 1) % 3, (c + 2) % 3);
        (a(r1, c1) * a(r2, c2) - a(r1, c2) * a(r2, c1)) / d
    };
    std::array::from_fn(|c| std::array::from_fn(|r| if r < 3 && c < 3 { cof(r, c) as f32 } else if r == c { 1.0 } else { 0.0 }))
}

fn load_png(path: &Path) -> Option<(u32, u32, Vec<u8>)> {
    let file = std::fs::File::open(path).ok()?;
    let mut dec = png::Decoder::new(std::io::BufReader::new(file));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    let px = &buf[..info.buffer_size()];
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => px.to_vec(),
        png::ColorType::Rgb => px.chunks_exact(3).flat_map(|c| [c[0], c[1], c[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => px.chunks_exact(2).flat_map(|c| [c[0], c[0], c[0], c[1]]).collect(),
        png::ColorType::Grayscale => px.iter().flat_map(|&c| [c, c, c, 255]).collect(),
        _ => return None,
    };
    // Unity's ReadPixels rows run bottom to top.
    let row = (w * 4) as usize;
    let flipped: Vec<u8> = rgba.chunks_exact(row).rev().flatten().copied().collect();
    Some((w, h, flipped))
}

/// Writes RGB bytes as a PNG.
pub fn write_png(path: &Path, w: u32, h: u32, rgb: &[u8]) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(rgb).map_err(|e| e.to_string())
}

impl Gpu3d {
    pub fn new(device: wgpu::Device, queue: wgpu::Queue) -> Gpu3d {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gpu3d"),
            source: wgpu::ShaderSource::Wgsl(include_str!("gpu3d.wgsl").into()),
        });
        let uniform = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        };
        let tex_entry = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let sampler_entry = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        };
        let scene_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("scene"),
            entries: &[
                uniform(0),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        // One texture + sampler (the fog texture for meshes, the mask for light quads).
        let tex2_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("texture"),
            entries: &[tex_entry(0), sampler_entry(1)],
        });
        let mat_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("material"),
            entries: &[tex_entry(0), tex_entry(1), sampler_entry(2)],
        });
        let pass_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pass"),
            entries: &[tex_entry(0), tex_entry(1), tex_entry(2), sampler_entry(3), uniform(4)],
        });
        let layout = |label: &str, groups: &[&wgpu::BindGroupLayout]| {
            let g: Vec<Option<&wgpu::BindGroupLayout>> = groups.iter().map(|l| Some(*l)).collect();
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some(label), bind_group_layouts: &g, immediate_size: 0 })
        };
        // Mesh shaders: the fog texture, a clamp sampler and the mirror reflection.
        let fogtex_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("fog texture"),
            entries: &[tex_entry(0), sampler_entry(1), tex_entry(2)],
        });
        let mesh_layout = layout("mesh", &[&scene_layout, &fogtex_layout, &mat_layout]);
        let quad_layout = layout("quad", &[&scene_layout, &tex2_layout]);
        let full_layout = layout("full", &[&scene_layout, &pass_layout]);
        let add = wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::One, operation: wgpu::BlendOperation::Add };
        let max = wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::One, operation: wgpu::BlendOperation::Max };
        let mesh_vertex = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Float32x2, 3 => Float32x4],
        };
        let quad_vertex = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<QuadVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4, 2 => Float32x3, 3 => Float32x3],
        };
        let depth_state = |write: bool, compare: wgpu::CompareFunction| wgpu::DepthStencilState {
            format: DEPTH,
            depth_write_enabled: Some(write),
            depth_compare: Some(compare),
            stencil: Default::default(),
            bias: Default::default(),
        };
        let pipe = |label: &str,
                    layout: &wgpu::PipelineLayout,
                    vs: &str,
                    fs: &str,
                    vbuf: Option<wgpu::VertexBufferLayout>,
                    format: wgpu::TextureFormat,
                    blend: Option<wgpu::BlendState>,
                    depth: Option<wgpu::DepthStencilState>| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some(vs),
                    compilation_options: Default::default(),
                    buffers: &[vbuf],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState { format, blend, write_mask: wgpu::ColorWrites::ALL })],
                }),
                // Unity's front faces are clockwise on screen.
                primitive: wgpu::PrimitiveState { cull_mode: None, front_face: wgpu::FrontFace::Cw, ..Default::default() },
                depth_stencil: depth,
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let full = |label: &str, fs: &str, format: wgpu::TextureFormat, blend: Option<wgpu::BlendState>, depth: Option<wgpu::DepthStencilState>| {
            pipe(label, &full_layout, "vs_full", fs, None, format, blend, depth)
        };
        let add_blend = Some(wgpu::BlendState { color: add, alpha: add });
        let pipes = Pipelines {
            mesh_opaque: pipe("mesh opaque", &mesh_layout, "vs_mesh", "fs_mesh", Some(mesh_vertex.clone()), HDR, None,
                Some(depth_state(true, wgpu::CompareFunction::LessEqual))),
            mesh_add: pipe("mesh add", &mesh_layout, "vs_mesh", "fs_mesh", Some(mesh_vertex.clone()), HDR, add_blend,
                Some(depth_state(false, wgpu::CompareFunction::LessEqual))),
            quad_add: pipe("quad add", &quad_layout, "vs_quad", "fs_quad", Some(quad_vertex.clone()), HDR, add_blend, None),
            quad_max: pipe("quad max", &quad_layout, "vs_quad", "fs_quad", Some(quad_vertex.clone()), HDR,
                Some(wgpu::BlendState { color: max, alpha: max }), None),
            down4: full("down4", "fs_down4", HDR, None, None),
            down13: full("down13", "fs_down13", HDR, None, None),
            up: full("up", "fs_up", HDR, None, None),
            up_box: full("up box", "fs_up_box", HDR, None, None),
            mirror_opaque: pipe("mirror opaque", &mesh_layout, "vs_mesh", "fs_mesh", Some(mesh_vertex.clone()), OUTPUT, None,
                Some(depth_state(true, wgpu::CompareFunction::LessEqual))),
            mirror_add: pipe("mirror add", &mesh_layout, "vs_mesh", "fs_mesh", Some(mesh_vertex.clone()), OUTPUT, add_blend,
                Some(depth_state(false, wgpu::CompareFunction::LessEqual))),
            mirror_sky: full("mirror sky", "fs_sky", OUTPUT, None, Some(depth_state(false, wgpu::CompareFunction::LessEqual))),
            fog_final: full("fog final", "fs_fog_final", HDR, None, None),
            sky_gradient: full("sky gradient", "fs_sky_gradient", HDR, add_blend, None),
            sky: full("sky", "fs_sky", HDR, None, Some(depth_state(false, wgpu::CompareFunction::LessEqual))),
            clamp: full("clamp", "fs_clamp", HDR, None, None),
            composite: full("composite", "fs_composite", OUTPUT, None, None),
        };
        let objects_cap = 256;
        let objects = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("objects"),
            size: (objects_cap * std::mem::size_of::<ObjectData>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let sampler = |mode: wgpu::AddressMode| {
            device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("sampler"),
                address_mode_u: mode,
                address_mode_v: mode,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            })
        };
        let solid = |rgba: [u8; 4]| {
            let t = device.create_texture_with_data(
                &queue,
                &wgpu::TextureDescriptor {
                    label: Some("solid"),
                    size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::util::TextureDataOrder::LayerMajor,
                &rgba,
            );
            view(&t)
        };
        let white = solid([255, 255, 255, 255]);
        let black = solid([0, 0, 0, 0]);
        Gpu3d {
            clamp_sampler: sampler(wgpu::AddressMode::ClampToEdge),
            repeat_sampler: sampler(wgpu::AddressMode::Repeat),
            mask: white.clone(),
            device,
            queue,
            env_key: 0,
            meshes: Vec::new(),
            materials: Vec::new(),
            targets: None,
            objects,
            objects_cap,
            scene_layout,
            tex2_layout,
            fogtex_layout,
            mat_layout,
            pass_layout,
            pipes,
            white,
            black,
        }
    }

    /// A renderer on its own device (for `--render`).
    pub fn headless() -> Result<Gpu3d, String> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .map_err(|e| format!("no GPU adapter: {e}"))?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("bs-light-timeline headless"),
            ..Default::default()
        }))
        .map_err(|e| format!("no GPU device: {e}"))?;
        Ok(Gpu3d::new(device, queue))
    }

    fn upload_texture(&self, dir: Option<&Path>, key: &str, srgb: bool) -> Option<wgpu::TextureView> {
        let (w, h, px) = load_png(&dir?.join(format!("{key}.png")))?;
        let t = self.device.create_texture_with_data(
            &self.queue,
            &wgpu::TextureDescriptor {
                label: Some(key),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: if srgb { wgpu::TextureFormat::Rgba8UnormSrgb } else { wgpu::TextureFormat::Rgba8Unorm },
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            &px,
        );
        Some(view(&t))
    }

    /// Uploads the environment's meshes, materials and textures (`textures`: the exported PNG folder).
    pub fn set_environment(&mut self, env: &EnvFile, textures: Option<&Path>) {
        let key = env as *const EnvFile as usize ^ env.meshes.len();
        if key == self.env_key && !self.meshes.is_empty() {
            return;
        }
        self.env_key = key;
        let index: serde_json::Value = textures
            .and_then(|d| std::fs::read_to_string(d.join("index.json")).ok())
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let mut cache: HashMap<String, Option<wgpu::TextureView>> = HashMap::new();
        let mut tex = |key: &str| -> Option<wgpu::TextureView> {
            cache
                .entry(key.to_owned())
                .or_insert_with(|| self.upload_texture(textures, key, index[key]["srgb"].as_bool().unwrap_or(true)))
                .clone()
        };
        let mask = index
            .as_object()
            .and_then(|o| o.iter().find(|(_, v)| v["name"] == "BloomfogAlphaMask").map(|(k, _)| k.clone()))
            .and_then(|k| tex(&k));
        let meshes: Vec<GpuMesh> = env
            .meshes
            .iter()
            .map(|m| {
                let verts: Vec<Vertex> = (0..m.pos.len())
                    .map(|i| Vertex {
                        pos: m.pos[i],
                        normal: m.normal.get(i).copied().unwrap_or([0.0, 1.0, 0.0]),
                        uv: m.uv.get(i).copied().unwrap_or([0.0, 0.0]),
                        color: m.color.get(i).copied().unwrap_or([1.0; 4]),
                    })
                    .collect();
                let verts = if verts.is_empty() { vec![Vertex { pos: [0.0; 3], normal: [0.0; 3], uv: [0.0; 2], color: [0.0; 4] }] } else { verts };
                let idx = if m.idx.is_empty() { vec![0u32, 0, 0] } else { m.idx.clone() };
                GpuMesh {
                    vb: self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some(&m.name),
                        contents: bytemuck::cast_slice(&verts),
                        usage: wgpu::BufferUsages::VERTEX,
                    }),
                    ib: self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some(&m.name),
                        contents: bytemuck::cast_slice(&idx),
                        usage: wgpu::BufferUsages::INDEX,
                    }),
                    count: if m.idx.is_empty() { 0 } else { idx.len() as u32 },
                }
            })
            .collect();
        let mut materials = Vec::new();
        for m in &env.materials {
            let (kind, flags, p, color, additive, _depth_write, t0, t1) = material_params(m);
            let v0 = t0.and_then(|k| tex(&k)).unwrap_or_else(|| self.white.clone());
            let v1 = t1.and_then(|k| tex(&k)).unwrap_or_else(|| self.white.clone());
            let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(&m.name),
                layout: &self.mat_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&v0) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&v1) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.repeat_sampler) },
                ],
            });
            materials.push(GpuMaterial { kind, flags, p, color, additive, queue: m.queue, bind });
        }
        let mask = mask.unwrap_or_else(|| self.white.clone());
        self.meshes = meshes;
        self.materials = materials;
        self.mask = mask;
    }

    fn ensure_targets(&mut self, w: u32, h: u32) {
        if self.targets.as_ref().is_some_and(|t| t.size == (w, h)) {
            return;
        }
        let rt = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING;
        let d = &self.device;
        // Post bloom: 928 x H, CalculatePyramidParameters(radius 5).
        let bw = BLOOM_WIDTH;
        let bh = ((BLOOM_WIDTH as u64 * h as u64 / w.max(1) as u64) as u32).max(1);
        let logs = (bw.max(bh) as f32).log2() + 5.0 - 10.0;
        let levels = (logs.floor() as i32).clamp(1, 16) as usize;
        let bloom_scale = 0.5 + logs - logs.floor();
        let (mut bloom_down, mut bloom_up) = (Vec::new(), Vec::new());
        let (mut lw, mut lh) = (bw, bh);
        for _ in 0..levels {
            bloom_down.push(texture(d, "bloom down", lw, lh, HDR, rt));
            bloom_up.push(texture(d, "bloom up", lw, lh, HDR, rt));
            lw = (lw / 2).max(1);
            lh = (lh / 2).max(1);
        }
        self.targets = Some(Targets {
            size: (w, h),
            hdr: texture(d, "hdr", w, h, HDR, rt),
            depth: view(&texture(d, "depth", w, h, DEPTH, wgpu::TextureUsages::RENDER_ATTACHMENT)),
            clamped: texture(d, "clamped", w, h, HDR, rt),
            output: texture(d, "output", w, h, OUTPUT, rt | wgpu::TextureUsages::COPY_SRC),
            fog: FogTargets::new(d),
            mirror_fog: FogTargets::new(d),
            reflection: texture(d, "reflection", MIRROR_SIZE, MIRROR_SIZE, OUTPUT, rt | wgpu::TextureUsages::COPY_SRC),
            reflection_depth: view(&texture(d, "reflection depth", MIRROR_SIZE, MIRROR_SIZE, DEPTH, wgpu::TextureUsages::RENDER_ATTACHMENT)),
            bloom_down,
            bloom_up,
            bloom_final: texture(d, "bloom final", bw, bh, HDR, rt),
            bloom_scale,
        });
    }

    /// Globals for a view (`clip`: the mirror plane for the reflected view).
    /// `w`, `h`: the view's aspect; `target`: the size of what it renders into (the square mirror texture
    /// for reflections, which keep the main view's projection).
    fn globals(v: &View, w: u32, h: u32, target: (u32, u32), f: &Frame, clip: Option<[f32; 4]>) -> (Globals, f32, f32) {
        let aspect = w as f32 / h as f32;
        let fy = 1.0 / (v.vfov.to_radians() / 2.0).tan();
        let fx = fy / aspect;
        let ratio = [(1.0 / (FOG_HALF_FOV_TAN * fx)).clamp(0.0, 1.0), (1.0 / (FOG_HALF_FOV_TAN * fy)).clamp(0.0, 1.0)];
        let (pfx, pfy) = (fx * ratio[0], fy * ratio[1]);
        let (za, zb) = (v.far / (v.far - v.near), -v.far * v.near / (v.far - v.near));
        let rows = [
            [v.right[0] * fx, v.right[1] * fx, v.right[2] * fx, -dot(v.right, v.pos) * fx],
            [v.up[0] * fy, v.up[1] * fy, v.up[2] * fy, -dot(v.up, v.pos) * fy],
            [v.fwd[0] * za, v.fwd[1] * za, v.fwd[2] * za, -dot(v.fwd, v.pos) * za + zb],
            [v.fwd[0], v.fwd[1], v.fwd[2], -dot(v.fwd, v.pos)],
        ];
        let l = &f.state.lights;
        let g = Globals {
            view_proj: std::array::from_fn(|c| std::array::from_fn(|r| rows[r][c])),
            cam_pos: [v.pos[0], v.pos[1], v.pos[2], 1.0],
            right: [v.right[0], v.right[1], v.right[2], 1.0 / pfx],
            up: [v.up[0], v.up[1], v.up[2], 1.0 / pfy],
            fwd: [v.fwd[0], v.fwd[1], v.fwd[2], 0.0],
            fog: [f.fog.offset, f.fog.attenuation, f.fog.start_y, f.fog.height],
            ratio: [ratio[0], ratio[1], f.fog.auto_exposure_limit, if f.fog.legacy_auto_exposure { 1.0 } else { 0.0 }],
            screen: [target.0 as f32, target.1 as f32, 1.0 / target.0 as f32, 1.0 / target.1 as f32],
            light_pos: l.pos,
            light_dir: l.dir,
            light_color: l.color,
            light_radius: [[l.radius[0], l.radius[1], l.radius[2], l.radius[3]], [l.radius[4], 0.0, 0.0, 0.0]],
            clip: clip.unwrap_or([0.0; 4]),
            flags: [if clip.is_some() { 1.0 } else { 0.0 }, if clip.is_some() { 1.0 } else { 0.0 }, 0.0, 0.0],
        };
        (g, pfx, pfy)
    }

    fn scene_group(&self, g: &Globals) -> wgpu::BindGroup {
        let buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("globals"),
            contents: bytemuck::bytes_of(g),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scene"),
            layout: &self.scene_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: self.objects.as_entire_binding() },
            ],
        })
    }

    fn pass_group(&self, a: &wgpu::TextureView, b: &wgpu::TextureView, c: &wgpu::TextureView, p: PassParams) -> wgpu::BindGroup {
        let buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pass params"),
            contents: bytemuck::bytes_of(&p),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pass"),
            layout: &self.pass_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(a) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(b) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(c) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&self.clamp_sampler) },
                wgpu::BindGroupEntry { binding: 4, resource: buf.as_entire_binding() },
            ],
        })
    }

    fn fullscreen(&self, enc: &mut wgpu::CommandEncoder, pipe: &wgpu::RenderPipeline, target: &wgpu::TextureView, clear: bool, scene: &wgpu::BindGroup, bg: &wgpu::BindGroup) {
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("fullscreen"),
            color_attachments: &[color_att(target, clear)],
            ..Default::default()
        });
        pass.set_pipeline(pipe);
        pass.set_bind_group(0, scene, &[]);
        pass.set_bind_group(1, bg, &[]);
        pass.draw(0..3, 0..1);
    }

    /// The fog texture for a view: light quads, the blur pyramid (box intermediates for mirrors), and
    /// the sky gradient.
    #[allow(clippy::too_many_arguments)]
    /// `sky_scene`: globals for the sky gradient's view directions (the main camera's, also for mirrors).
    #[allow(clippy::too_many_arguments)]
    fn fog_pass(
        &self,
        enc: &mut wgpu::CommandEncoder,
        scene: &wgpu::BindGroup,
        sky_scene: &wgpu::BindGroup,
        v: &View,
        pfx: f32,
        pfy: f32,
        f: &Frame,
        fog: &FogTargets,
        box_up: bool,
    ) {
        let texel = |t: &wgpu::Texture| [1.0 / t.width() as f32, 1.0 / t.height() as f32, t.width() as f32, t.height() as f32];
        let quads = fog_quads(v, pfx, pfy, &f.state.fog_lights);
        let raw = view(&fog.raw);
        {
            let qbuf = (!quads.is_empty()).then(|| {
                let verts: Vec<QuadVertex> = quads.iter().flat_map(|q| q.verts).collect();
                self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("quads"),
                    contents: bytemuck::cast_slice(&verts),
                    usage: wgpu::BufferUsages::VERTEX,
                })
            });
            let qidx: Vec<u32> = (0..quads.len() as u32).flat_map(|q| [0, 1, 2, 2, 3, 0].map(|k| q * 4 + k)).collect();
            let ibuf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("quad indices"),
                contents: bytemuck::cast_slice(if qidx.is_empty() { &[0u32, 0, 0][..] } else { &qidx }),
                usage: wgpu::BufferUsages::INDEX,
            });
            let mask = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("mask"),
                layout: &self.tex2_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&self.mask) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.clamp_sampler) },
                ],
            });
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("fog prepass"),
                color_attachments: &[color_att(&raw, true)],
                ..Default::default()
            });
            if let Some(qb) = &qbuf {
                pass.set_bind_group(0, scene, &[]);
                pass.set_bind_group(1, &mask, &[]);
                pass.set_vertex_buffer(0, qb.slice(..));
                pass.set_index_buffer(ibuf.slice(..), wgpu::IndexFormat::Uint32);
                let mut i = 0;
                while i < quads.len() {
                    let max = quads[i].max;
                    let mut j = i;
                    while j < quads.len() && quads[j].max == max {
                        j += 1;
                    }
                    pass.set_pipeline(if max { &self.pipes.quad_max } else { &self.pipes.quad_add });
                    pass.draw_indexed(i as u32 * 6..j as u32 * 6, 0, 0..1);
                    i = j;
                }
            }
        }
        // Bloom Fog Profile: intensity 0.75, first 1.2, final 0.25, sample scale 0.5.
        let down: Vec<wgpu::TextureView> = fog.down.iter().map(view).collect();
        let up: Vec<wgpu::TextureView> = fog.up.iter().map(view).collect();
        let n = down.len();
        let mut src = raw.clone();
        let mut src_t = &fog.raw;
        for (i, dv) in down.iter().enumerate() {
            let bg = self.pass_group(&src, &self.black, &self.black, PassParams { texel: texel(src_t), combine: [0.0; 4] });
            self.fullscreen(enc, &self.pipes.down4, dv, true, scene, &bg);
            src = dv.clone();
            src_t = &fog.down[i];
        }
        let fin = view(&fog.fin);
        for i in (0..n - 1).rev() {
            let (s, st) = if i == n - 2 { (&down[n - 1], &fog.down[n - 1]) } else { (&up[i + 1], &fog.up[i + 1]) };
            let dest = (0.75 * (i as f32 + 1.0) / (n as f32 - 1.0)).min(1.0);
            let acc = (1.0 + 1.0 - dest).min(1.0);
            let bright = if i == 0 { 0.25 } else if i == n - 2 { 1.2 } else { 1.0 };
            let bg = self.pass_group(s, &down[i], &down[n - 1], PassParams { texel: texel(st), combine: [dest * bright, acc * bright, 0.5, 0.0] });
            if i == 0 {
                self.fullscreen(enc, &self.pipes.fog_final, &fin, true, scene, &bg);
            } else {
                self.fullscreen(enc, if box_up { &self.pipes.up_box } else { &self.pipes.up }, &up[i], true, scene, &bg);
            }
        }
        if let Some(sky) = &f.state.sky {
            let gt = self.device.create_texture_with_data(
                &self.queue,
                &wgpu::TextureDescriptor {
                    label: Some("sky gradient"),
                    size: wgpu::Extent3d { width: 128, height: 1, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8UnormSrgb,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::util::TextureDataOrder::LayerMajor,
                &sky.pixels(),
            );
            let tint = [srgb_to_linear(sky.tint[0]), srgb_to_linear(sky.tint[1]), srgb_to_linear(sky.tint[2]), sky.tint[3]];
            let bg = self.pass_group(&view(&gt), &self.black, &self.black, PassParams { texel: [0.0; 4], combine: tint });
            self.fullscreen(enc, &self.pipes.sky_gradient, &fin, false, sky_scene, &bg);
        }
    }

    /// Particle systems as world-space triangles for a view (billboards face it).
    fn particle_vertices(&self, f: &Frame, v: &View, systems: &[(usize, u32, usize)]) -> (Vec<Vertex>, Vec<(u32, std::ops::Range<u32>, usize)>) {
        let mut pverts: Vec<Vertex> = Vec::new();
        let mut pdraws = Vec::new();
        for &(si, object, mi) in systems {
            let ps = &f.env.particle_systems[si];
            let particles = f.state.particles.get(&si).unwrap_or(&ps.particles);
            let start = pverts.len() as u32;
            for pt in particles {
                let color = [srgb_to_linear(pt.color[0]), srgb_to_linear(pt.color[1]), srgb_to_linear(pt.color[2]), pt.color[3]];
                let center = if ps.local { crate::environment::transform_point(&ps.matrix, pt.pos) } else { pt.pos };
                match ps.mesh.and_then(|m| f.env.meshes.get(m)) {
                    Some(mesh) => {
                        let (x, y, z) = (pt.rot[0].to_radians(), pt.rot[1].to_radians(), pt.rot[2].to_radians());
                        let rot = |p: V3| -> V3 {
                            let p = [p[0] * z.cos() - p[1] * z.sin(), p[0] * z.sin() + p[1] * z.cos(), p[2]];
                            let p = [p[0], p[1] * x.cos() - p[2] * x.sin(), p[1] * x.sin() + p[2] * x.cos()];
                            [p[0] * y.cos() + p[2] * y.sin(), p[1], -p[0] * y.sin() + p[2] * y.cos()]
                        };
                        for &i in &mesh.idx {
                            let Some(&lp) = mesh.pos.get(i as usize) else { continue };
                            let local = rot([lp[0] * pt.size[0], lp[1] * pt.size[1], lp[2] * pt.size[2]]);
                            let wp = if ps.local {
                                let m = &ps.matrix;
                                let d: V3 = std::array::from_fn(|r| m[r * 4] * local[0] + m[r * 4 + 1] * local[1] + m[r * 4 + 2] * local[2]);
                                [center[0] + d[0], center[1] + d[1], center[2] + d[2]]
                            } else {
                                [center[0] + local[0], center[1] + local[1], center[2] + local[2]]
                            };
                            pverts.push(Vertex { pos: wp, normal: [0.0; 3], uv: mesh.uv.get(i as usize).copied().unwrap_or([0.0; 2]), color });
                        }
                    }
                    None => {
                        let a = -pt.rot[2].to_radians();
                        let (c, s) = (a.cos(), a.sin());
                        let corner = |x: f32, y: f32| -> V3 {
                            let (rx, ry) = ((x * c - y * s) * pt.size[0], (x * s + y * c) * pt.size[1]);
                            std::array::from_fn(|k| center[k] + v.right[k] * rx + v.up[k] * ry)
                        };
                        let q = [(-0.5, -0.5, 0.0, 0.0), (0.5, -0.5, 1.0, 0.0), (0.5, 0.5, 1.0, 1.0), (-0.5, 0.5, 0.0, 1.0)];
                        for k in [0, 1, 2, 2, 3, 0] {
                            let (x, y, u, w) = q[k];
                            pverts.push(Vertex { pos: corner(x, y), normal: [0.0; 3], uv: [u, w], color });
                        }
                    }
                }
            }
            if pverts.len() as u32 > start {
                pdraws.push((object, start..pverts.len() as u32, mi));
            }
        }
        (pverts, pdraws)
    }

    /// The scene for a view into `target`: opaque objects, the sky, additive objects and particles.
    #[allow(clippy::too_many_arguments)]
    fn scene_pass(
        &self,
        enc: &mut wgpu::CommandEncoder,
        scene: &wgpu::BindGroup,
        fog: &FogTargets,
        reflection: &wgpu::TextureView,
        items: &[DrawItem],
        particles: (&[Vertex], &[(u32, std::ops::Range<u32>, usize)]),
        target: &wgpu::TextureView,
        depth: &wgpu::TextureView,
        mirror_view: bool,
    ) {
        let fin = view(&fog.fin);
        let fog_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("fog texture"),
            layout: &self.fogtex_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&fin) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.clamp_sampler) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(reflection) },
            ],
        });
        let sky_bg = self.pass_group(&fin, &self.black, &self.black, PassParams { texel: [0.0; 4], combine: [0.0; 4] });
        let pbuf = (!particles.0.is_empty()).then(|| {
            self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("particles"),
                contents: bytemuck::cast_slice(particles.0),
                usage: wgpu::BufferUsages::VERTEX,
            })
        });
        let (opaque, add, sky) = if mirror_view {
            (&self.pipes.mirror_opaque, &self.pipes.mirror_add, &self.pipes.mirror_sky)
        } else {
            (&self.pipes.mesh_opaque, &self.pipes.mesh_add, &self.pipes.sky)
        };
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("scene"),
            color_attachments: &[color_att(target, true)],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: depth,
                depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Store }),
                stencil_ops: None,
            }),
            ..Default::default()
        });
        pass.set_bind_group(0, scene, &[]);
        pass.set_bind_group(1, &fog_bg, &[]);
        let draw_sky = |pass: &mut wgpu::RenderPass| {
            pass.set_pipeline(sky);
            pass.set_bind_group(1, &sky_bg, &[]);
            pass.draw(0..3, 0..1);
            pass.set_bind_group(1, &fog_bg, &[]);
        };
        let mut drawn_sky = false;
        for d in items {
            if d.additive && !drawn_sky {
                draw_sky(&mut pass);
                drawn_sky = true;
            }
            let mat = &self.materials[d.material];
            // No mirrors inside the reflection.
            if mirror_view && (mat.kind == Kind::Mirror || !d.reflect) {
                continue;
            }
            let Some(m) = self.meshes.get(d.mesh) else { continue };
            if m.count == 0 {
                continue;
            }
            pass.set_pipeline(if d.additive { add } else { opaque });
            pass.set_bind_group(2, &mat.bind, &[]);
            pass.set_vertex_buffer(0, m.vb.slice(..));
            pass.set_index_buffer(m.ib.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..m.count, 0, d.object..d.object + 1);
        }
        if !drawn_sky {
            draw_sky(&mut pass);
        }
        if let Some(pb) = &pbuf {
            pass.set_pipeline(add);
            pass.set_vertex_buffer(0, pb.slice(..));
            for (object, range, mi) in particles.1 {
                pass.set_bind_group(2, &self.materials[*mi].bind, &[]);
                pass.draw(range.clone(), *object..*object + 1);
            }
        }
    }

    /// Renders a frame into the output texture.
    pub fn render(&mut self, f: &Frame) -> &wgpu::Texture {
        let (w, h) = (f.width.max(1), f.height.max(1));
        self.ensure_targets(w, h);
        // Objects (shared by the main and reflected views).
        let mut data = Vec::new();
        let mut items = Vec::new();
        for (i, o) in f.env.objects.iter().enumerate() {
            let (Some(mesh), true) = (o.mesh, o.enabled) else { continue };
            let Some(mi) = o.materials.first().and_then(|m| usize::try_from(*m).ok()).filter(|m| *m < self.materials.len()) else { continue };
            let mat = &self.materials[mi];
            let block = f.state.blocks.get(&i).copied().unwrap_or_default();
            if !block.enabled {
                continue;
            }
            let matrix = f.state.moved.get(&i).copied().unwrap_or(o.matrix);
            let reflect = o.layer.is_none_or(|l| l < 32 && MIRROR_LAYERS >> l & 1 == 1);
            items.push(DrawItem { reflect, object: data.len() as u32, mesh, material: mi, additive: mat.additive, queue: mat.queue });
            data.push(ObjectData {
                model: mat_rows_to_cols(&matrix),
                normal: normal_matrix(&matrix),
                info: [mat.kind as u32, mat.flags, 0, 0],
                b: [
                    block.color.unwrap_or(mat.color),
                    block.alpha_width.unwrap_or([1.0; 4]),
                    block.size_params.unwrap_or([1.0, 1.0, 1.0, 0.0]),
                    block.emission_tex_color.unwrap_or([0.0; 4]),
                ],
                p: mat.p,
            });
        }
        items.sort_by_key(|d| (d.additive, d.queue));
        let identity: [f32; 16] = std::array::from_fn(|i| if i % 5 == 0 { 1.0 } else { 0.0 });
        let mut systems = Vec::new();
        for (si, ps) in f.env.particle_systems.iter().enumerate() {
            let Some(mi) = ps.materials.first().and_then(|m| usize::try_from(*m).ok()).filter(|m| *m < self.materials.len()) else { continue };
            if !ps.enabled {
                continue;
            }
            let mat = &self.materials[mi];
            systems.push((si, data.len() as u32, mi));
            data.push(ObjectData {
                model: mat_rows_to_cols(&identity),
                normal: normal_matrix(&identity),
                info: [mat.kind as u32, mat.flags, 0, 0],
                b: [mat.color, [1.0; 4], [1.0, 1.0, 1.0, 0.0], [0.0; 4]],
                p: mat.p,
            });
        }
        if data.len() > self.objects_cap {
            self.objects_cap = data.len().next_power_of_two();
            self.objects = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("objects"),
                size: (self.objects_cap * std::mem::size_of::<ObjectData>()) as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        if !data.is_empty() {
            self.queue.write_buffer(&self.objects, 0, bytemuck::cast_slice(&data));
        }
        let t = self.targets.as_ref().expect("targets");
        let mut enc = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });

        // Mirror: the view reflected about the mirror plane (shared by all mirrors), with its own fog.
        let has_mirror = items.iter().any(|d| self.materials[d.material].kind == Kind::Mirror);
        let reflection = view(&t.reflection);
        if let (true, Some(plane)) = (has_mirror, f.state.mirror_plane) {
            let (n, d) = ([plane[0], plane[1], plane[2]], plane[3]);
            let v = f.view;
            let side = dot(n, v.pos) + d;
            if side > 0.0 {
                let refl_p = |p: V3| -> V3 { let k = 2.0 * (dot(n, p) + d); [p[0] - k * n[0], p[1] - k * n[1], p[2] - k * n[2]] };
                let refl_d = |p: V3| -> V3 { let k = 2.0 * dot(n, p); [p[0] - k * n[0], p[1] - k * n[1], p[2] - k * n[2]] };
                let mv = View { pos: refl_p(v.pos), right: refl_d(v.right), up: refl_d(v.up), fwd: refl_d(v.fwd), ..v };
                let (g, pfx, pfy) = Self::globals(&mv, w, h, (MIRROR_SIZE, MIRROR_SIZE), f, Some(plane));
                let scene = self.scene_group(&g);
                let (gm, _, _) = Self::globals(&v, w, h, (w, h), f, None);
                let main_scene = self.scene_group(&gm);
                self.fog_pass(&mut enc, &scene, &main_scene, &mv, pfx, pfy, f, &t.mirror_fog, true);
                let (pv, pd) = self.particle_vertices(f, &mv, &systems);
                self.scene_pass(&mut enc, &scene, &t.mirror_fog, &self.black, &items, (&pv, &pd), &reflection, &t.reflection_depth, true);
            }
        }

        // Main view.
        let v = f.view;
        let (g, pfx, pfy) = Self::globals(&v, w, h, (w, h), f, None);
        let scene = self.scene_group(&g);
        self.fog_pass(&mut enc, &scene, &scene, &v, pfx, pfy, f, &t.fog, false);
        let (pv, pd) = self.particle_vertices(f, &v, &systems);
        let hdr = view(&t.hdr);
        self.scene_pass(&mut enc, &scene, &t.fog, &reflection, &items, (&pv, &pd), &hdr, &t.depth, false);

        // Post bloom.
        let texel = |t: &wgpu::Texture| [1.0 / t.width() as f32, 1.0 / t.height() as f32, t.width() as f32, t.height() as f32];
        let out = view(&t.output);
        let clamped = view(&t.clamped);
        let bg = self.pass_group(&hdr, &self.black, &self.black, PassParams { texel: texel(&t.hdr), combine: [0.0; 4] });
        self.fullscreen(&mut enc, &self.pipes.clamp, &clamped, true, &scene, &bg);
        let bloom_final = view(&t.bloom_final);
        if f.post_bloom {
            let bd: Vec<wgpu::TextureView> = t.bloom_down.iter().map(view).collect();
            let bu: Vec<wgpu::TextureView> = t.bloom_up.iter().map(view).collect();
            let n = bd.len();
            let mut src = clamped.clone();
            let mut src_t = &t.clamped;
            for (i, dv) in bd.iter().enumerate() {
                let gate = if i == 0 { 4.0 } else { 0.0 };
                let bg = self.pass_group(&src, &self.black, &self.black, PassParams { texel: texel(src_t), combine: [0.0, 0.0, 0.0, gate] });
                self.fullscreen(&mut enc, &self.pipes.down13, dv, true, &scene, &bg);
                src = dv.clone();
                src_t = &t.bloom_down[i];
            }
            let weights = |i: usize| -> f32 { if n <= 1 { 1.0 } else { ((i as f32 + 1.0) / (n as f32 - 1.0)).powf(0.01).min(1.0) } };
            let mut last = (bd[n - 1].clone(), &t.bloom_down[n - 1]);
            for i in (1..n.saturating_sub(1)).rev() {
                let s = if i == n - 2 { (bd[n - 1].clone(), &t.bloom_down[n - 1]) } else { (bu[i + 1].clone(), &t.bloom_up[i + 1]) };
                let bg = self.pass_group(&s.0, &bd[i], &self.black, PassParams { texel: texel(s.1), combine: [weights(i), 1.0, t.bloom_scale, 0.0] });
                self.fullscreen(&mut enc, &self.pipes.up, &bu[i], true, &scene, &bg);
                last = (bu[i].clone(), &t.bloom_up[i]);
            }
            let combine = if n == 1 { [1.0, 0.0, t.bloom_scale, 0.0] } else { [weights(0), 1.0, t.bloom_scale, 0.0] };
            let bg = self.pass_group(&last.0, &bd[0], &self.black, PassParams { texel: texel(last.1), combine });
            self.fullscreen(&mut enc, &self.pipes.up, &bloom_final, true, &scene, &bg);
        }
        let bg = self.pass_group(
            &clamped,
            &bloom_final,
            &self.black,
            PassParams { texel: texel(&t.clamped), combine: [if f.post_bloom { BLOOM_BLEND } else { 0.0 }, 0.0, 0.0, 0.0] },
        );
        self.fullscreen(&mut enc, &self.pipes.composite, &out, true, &scene, &bg);
        self.queue.submit([enc.finish()]);
        &self.targets.as_ref().expect("targets").output
    }

    /// The fog texture of the last frame as linear RGB bytes (512 x 512), for comparisons.
    pub fn read_fog_rgb(&self) -> Vec<u8> {
        let Some(t) = &self.targets else { return Vec::new() };
        let raw = self.read_texture(&t.fog.fin, 8);
        let half = |b: &[u8]| -> f32 {
            let h = u16::from_le_bytes([b[0], b[1]]);
            let (s, e, m) = ((h >> 15) as u32, ((h >> 10) & 0x1f) as i32, (h & 0x3ff) as f32);
            let v = if e == 0 { m / 1024.0 * 2f32.powi(-14) } else if e == 31 { f32::INFINITY } else { (1.0 + m / 1024.0) * 2f32.powi(e - 15) };
            if s == 1 { -v } else { v }
        };
        raw.chunks_exact(8).flat_map(|p| [0, 2, 4].map(|k| (half(&p[k..k + 2]).clamp(0.0, 1.0) * 255.0 + 0.5) as u8)).collect()
    }

    /// The mirror reflection of the last frame as RGB bytes (1024 x 1024).
    pub fn read_reflection_rgb(&self) -> Vec<u8> {
        let Some(t) = &self.targets else { return Vec::new() };
        self.read_texture(&t.reflection, 4).chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect()
    }

    /// Raw texel bytes of a texture (rows packed), `bpp` bytes per texel.
    fn read_texture(&self, tex: &wgpu::Texture, bpp: u32) -> Vec<u8> {
        let (w, h) = (tex.width(), tex.height());
        let row = (w * bpp).div_ceil(256) * 256;
        let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (row * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("readback") });
        enc.copy_texture_to_buffer(
            tex.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(h) },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.submit([enc.finish()]);
        let slice = buf.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        let Ok(data) = slice.get_mapped_range() else { return Vec::new() };
        (0..h).flat_map(|y| data[(y * row) as usize..(y * row + w * bpp) as usize].to_vec()).collect()
    }

    /// Reads the output texture back as RGB bytes.
    pub fn read_rgb(&self) -> Vec<u8> {
        let Some(t) = &self.targets else { return Vec::new() };
        let (w, h) = t.size;
        let row = (w * 4).div_ceil(256) * 256;
        let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (row * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("readback") });
        enc.copy_texture_to_buffer(
            t.output.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(h) },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.submit([enc.finish()]);
        let slice = buf.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        let Ok(data) = slice.get_mapped_range() else { return Vec::new() };
        let mut out = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            let r = &data[(y * row) as usize..(y * row + w * 4) as usize];
            for px in r.chunks_exact(4) {
                out.extend_from_slice(&px[..3]);
            }
        }
        out
    }
}

/// GPU parameters for a material (see the layouts in gpu3d.wgsl).
#[allow(clippy::type_complexity)]
fn material_params(m: &EnvMaterial) -> (Kind, u32, [[f32; 4]; 12], [f32; 4], bool, bool, Option<String>, Option<String>) {
    let lin = |name: &str, d: [f32; 4]| {
        let c = m.vec4(name, d);
        if m.colors.iter().any(|n| n == name) {
            [srgb_to_linear(c[0]), srgb_to_linear(c[1]), srgb_to_linear(c[2]), c[3]]
        } else {
            c
        }
    };
    let f = |n: &str, d: f32| m.float(n, d);
    let tex = |n: &str| m.props.get(n).and_then(|v| v.as_str()).map(str::to_owned);
    let mut p = [[0.0f32; 4]; 12];
    let mut flags = 0;
    let kw = |k: &str, flag: u32, flags: &mut u32| {
        if m.has(k) {
            *flags |= flag;
        }
    };
    kw("DIFFUSE", F_DIFFUSE, &mut flags);
    kw("SPECULAR", F_SPECULAR, &mut flags);
    kw("LIGHT_FALLOFF", F_FALLOFF, &mut flags);
    kw("BOTH_SIDES_DIFFUSE", F_BOTH_SIDES, &mut flags);
    kw("FOG", F_FOG, &mut flags);
    kw("HEIGHT_FOG", F_HEIGHT_FOG, &mut flags);
    kw("_VERTEXMODE_COLOR", F_VERTEX_COLOR, &mut flags);
    kw("_EMISSIONTEXTURE_SIMPLE", F_EMISSION_SIMPLE, &mut flags);
    kw("_ACES_APPROACH_BEFORE_EMISSIVE", F_ACES_BEFORE, &mut flags);
    kw("DIRT", F_DIRT, &mut flags);
    kw("MAIN_TEXTURE", F_MAIN_TEXTURE, &mut flags);
    kw("_ALPHACHANNEL_RED", F_ALPHA_RED, &mut flags);
    kw("SQUARE_ALPHA", F_SQUARE_ALPHA, &mut flags);
    kw("_FOGTYPE_COLOR", F_FOG_COLOR, &mut flags);
    kw("_FOGTYPE_ALPHA", F_FOG_ALPHA, &mut flags);
    kw("_FOGTYPE_LERP", F_FOG_LERP, &mut flags);
    kw("_WHITEBOOSTTYPE_MAINEFFECT", F_WHITEBOOST_MAIN, &mut flags);
    kw("VERTEX_COLOR", F_VERTEX_COLOR, &mut flags);
    p[3] = [f("_FogStartOffset", 0.0), f("_FogScale", 1.0), f("_FogHeightScale", 1.0), f("_FogHeightOffset", 0.0)];
    let color = lin("_Color", [0.5, 0.5, 0.5, 1.0]);
    let (kind, additive, depth_write, t0, t1) = match m.shader.as_str() {
        "ChroMapper/Parametric Box Opaque" => (Kind::BoxOpaque, false, true, None, None),
        "ChroMapper/Parametric Box Transparent" => (Kind::BoxTransparent, true, false, None, None),
        "ChroMapper/Parametric Box Fake Glow" => {
            p[1][0] = f("_AngleDisappearParam", 0.0);
            (Kind::FakeGlow, true, false, tex("_MainTex"), None)
        }
        "ChroMapper/Mirror" => {
            p[1] = [f("_BumpIntensity", 0.0), f("_ReflectionIntensity", 1.0), f("_Metallic", 0.0), f("_DirtIntensity", 0.0)];
            p[2] = lin("_TintColor", [1.0; 4]);
            p[5] = m.vec4("_NormalTex_ST", [1.0, 1.0, 0.0, 0.0]);
            p[6] = m.vec4("_DirtTex_ST", [1.0, 1.0, 0.0, 0.0]);
            p[7] = m.vec4("_TextureScrolling", [0.0; 4]);
            (Kind::Mirror, false, true, tex("_NormalTex"), tex("_DirtTex"))
        }
        "ChroMapper/Particles" => {
            p[0] = color;
            p[1] = [f("_AlphaMultiplier", 1.0), f("_Intensity", 1.0), f("_BloomMultiplier", 1.0), 0.0];
            p[4] = [f("_BlendModeSrcA", 0.0), f("_BlendModeDst", 1.0), 0.0, 0.0];
            p[5] = m.vec4("_MainTex_ST", [1.0, 1.0, 0.0, 0.0]);
            (Kind::Particles, f("_BlendModeDst", 1.0) >= 0.5, false, tex("_MainTex"), None)
        }
        "ChroMapper/Spectrogram" => {
            p[0] = m.vec4("_Color", [0.5; 4]);
            p[1] = [f("_Metallic", 0.0), f("_Smoothness", 0.0), f("_SpecularIntensity", 1.0), 0.0];
            p[4] = m.vec4("_PeakOffset", [0.0, 10.0, 0.0, 0.0]);
            (Kind::Spectrogram, false, true, None, None)
        }
        _ => {
            let nominal = lin("_NominalDiffuseLevel", [0.0; 4]);
            let amb = f("_AmbientMultiplier", 1.0);
            p[1] = [f("_Metallic", 0.0), f("_Smoothness", 0.0), f("_SpecularIntensity", 1.0), f("_BothSidesDiffuseMultiplier", 1.0)];
            p[2] = [amb * nominal[0], amb * nominal[1], amb * nominal[2], f("_AmbientMinimalValue", 0.0)];
            p[4] = [f("_BlendModeSrcA", 0.0), f("_BlendModeDst", 0.0), 0.0, f("_EmissionBrightness", 1.0)];
            p[5] = m.vec4("_EmissionTex_ST", [1.0, 1.0, 0.0, 0.0]);
            p[6] = [f("_EmissionTexBloomIntensity", 1.0), f("_MainEffectFogSuppression", 0.0), f("_EmissionTexSpeed", 0.0), 0.0];
            let additive = f("_BlendModeDst", 0.0) >= 0.5;
            (Kind::Lit, additive, f("_ZWrite", 1.0) >= 0.5, tex("_EmissionTex"), None)
        }
    };
    (kind, flags, p, color, additive, depth_write, t0, t1)
}

struct Quad {
    verts: [QuadVertex; 4],
    max: bool,
}

/// BloomFogObject.ApplyToQuad: each light as a screen-space quad in the 130° fog frustum.
fn fog_quads(v: &View, pfx: f32, pfy: f32, lights: &[FogLight]) -> Vec<Quad> {
    let mut order: Vec<usize> = (0..lights.len()).collect();
    order.sort_by_key(|&i| lights[i].priority);
    let mut out = Vec::new();
    for i in order {
        let l = &lights[i];
        if !l.enabled || (l.disable_on_zero && l.color[3] < 0.01) {
            continue;
        }
        let len = l.length * l.length_alpha_fog_mul;
        let m = &l.matrix;
        let tp = |p: V3| -> V3 { std::array::from_fn(|r| m[r * 4] * p[0] + m[r * 4 + 1] * p[1] + m[r * 4 + 2] * p[2] + m[r * 4 + 3]) };
        let (mut a_view, mut b_view) = (v.to_view(tp([0.0, -len * l.center, 0.0])), v.to_view(tp([0.0, len * (1.0 - l.center), 0.0])));
        // Clip coordinates (x, y, w = view z) and clipping against the frustum planes.
        let clip = |p: V3| [p[0] * pfx, p[1] * pfy, p[2]];
        let (mut a, mut b) = (clip(a_view), clip(b_view));
        let planes: [&dyn Fn(&V3, &V3) -> f32; 6] = [
            &|c, _| c[0] + c[2],
            &|c, _| c[2] - c[0],
            &|c, _| c[1] + c[2],
            &|c, _| c[2] - c[1],
            &|_, vw| v.far - vw[2],
            &|_, vw| vw[2] - v.near,
        ];
        let mut culled = false;
        for plane in planes {
            let (da, db) = (plane(&a, &a_view), plane(&b, &b_view));
            if da < 0.0 && db < 0.0 {
                culled = true;
                break;
            }
            if da < 0.0 || db < 0.0 {
                let t = da / (da - db);
                let lerp = |p: V3, q: V3| -> V3 { std::array::from_fn(|k| p[k] + (q[k] - p[k]) * t) };
                let (nc, nv) = (lerp(a, b), lerp(a_view, b_view));
                if da < 0.0 {
                    a = nc;
                    a_view = nv;
                } else {
                    b = nc;
                    b_view = nv;
                }
            }
        }
        if culled || a[2] <= 0.0 || b[2] <= 0.0 {
            continue;
        }
        let mut sa = [a[0] / a[2] * 0.5 + 0.5, a[1] / a[2] * 0.5 + 0.5];
        let mut sb = [b[0] / b[2] * 0.5 + 0.5, b[1] / b[2] * 0.5 + 0.5];
        let d = [sb[0] - sa[0], sb[1] - sa[1]];
        let dl = (d[0] * d[0] + d[1] * d[1]).sqrt().max(1e-6);
        let dir = [d[0] / dl, d[1] / dl];
        sb = [sb[0] + dir[0] / 64.0, sb[1] + dir[1] / 64.0];
        sa = [sa[0] - dir[0] / 64.0, sa[1] - dir[1] / 64.0];
        let w = FOG_LINE_WIDTH * l.width_mul;
        let perp = [-dir[1] * w, dir[0] * w];
        let (oa, ob) = ([perp[0] * l.start_width, perp[1] * l.start_width], [perp[0] * l.end_width, perp[1] * l.end_width]);
        let rgb = [l.color[0] + l.boost_to_white, l.color[1] + l.boost_to_white, l.color[2] + l.boost_to_white];
        let mut af = l.color[3] * l.intensity_mul;
        if l.limit_alpha {
            af = af.clamp(l.min_alpha, l.max_alpha);
        }
        let af = linear_to_gamma(af);
        let cs = [rgb[0], rgb[1], rgb[2], af].map(|c| c * l.start_alpha);
        let ce = [rgb[0], rgb[1], rgb[2], af].map(|c| c * l.end_alpha * l.length_alpha_mul);
        let qv = |s: [f32; 2], color: [f32; 4], uv: [f32; 3], view: V3| QuadVertex { s, color, uv, view };
        out.push(Quad {
            verts: [
                qv([sa[0] - oa[0], sa[1] - oa[1]], cs, [0.0, 0.0, l.start_width], a_view),
                qv([sa[0] + oa[0], sa[1] + oa[1]], cs, [l.start_width, 0.0, l.start_width], a_view),
                qv([sb[0] + ob[0], sb[1] + ob[1]], ce, [l.end_width, 1.0, l.end_width], b_view),
                qv([sb[0] - ob[0], sb[1] - ob[1]], ce, [0.0, 1.0, l.end_width], b_view),
            ],
            max: l.blend_max,
        });
    }
    out
}
