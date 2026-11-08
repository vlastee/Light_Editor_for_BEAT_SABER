//! Software 3D preview drawn with egui meshes: perspective camera from the player's spot,
//! painter's-algorithm depth sorting, near-plane clipping and a cheap bloom halo on lights.

use crate::environment::EnvFile;
use crate::preview::LaneLights;
use crate::scene::{Obj, Prim, Quat, Scene};
use eframe::egui::{Color32, Mesh, Painter, Pos2, Rect, Shape};
use std::collections::HashSet;
use std::sync::OnceLock;

type V3 = [f32; 3];

pub struct Camera {
    pub pos: V3,
    pub yaw: f32,
    pub pitch: f32,
    pub vfov: f32,
}

impl Camera {
    /// From a ChroMapper camera pose "x,y,z,rx,ry,rz,fov" (its lane units, Unity euler degrees).
    pub fn from_chromapper_pose(pose: &str) -> Option<Camera> {
        let v: Vec<f32> = pose.split(',').filter_map(|s| s.trim().parse().ok()).collect();
        (v.len() >= 7).then(|| Camera {
            pos: [v[0] * 0.6, v[1] * 0.6, v[2] * 0.6],
            yaw: v[4],
            pitch: -v[3],
            vfov: v[6],
        })
    }
}

impl Default for Camera {
    fn default() -> Self {
        Self { pos: [0.0, 1.7, -1.0], yaw: 0.0, pitch: 8.0, vfov: 70.0 }
    }
}

fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn dot(a: V3, b: V3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn cross(a: V3, b: V3) -> V3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}
fn norm(a: V3) -> V3 {
    let l = dot(a, a).sqrt().max(1e-9);
    a.map(|c| c / l)
}

/// Unit primitives as triangle lists, matching Unity's built-in meshes.
pub fn mesh(p: Prim) -> &'static [[V3; 3]] {
    static CACHE: OnceLock<Vec<Vec<[V3; 3]>>> = OnceLock::new();
    let all = CACHE.get_or_init(|| {
        let quad = |a: V3, b: V3, c: V3, d: V3| vec![[a, b, c], [a, c, d]];
        let cube = {
            let h = 0.5;
            let v = |x: f32, y: f32, z: f32| [x * h, y * h, z * h];
            let mut t = Vec::new();
            for (a, b, c, d) in [
                (v(-1., -1., -1.), v(1., -1., -1.), v(1., 1., -1.), v(-1., 1., -1.)),
                (v(-1., -1., 1.), v(-1., 1., 1.), v(1., 1., 1.), v(1., -1., 1.)),
                (v(-1., -1., -1.), v(-1., 1., -1.), v(-1., 1., 1.), v(-1., -1., 1.)),
                (v(1., -1., -1.), v(1., -1., 1.), v(1., 1., 1.), v(1., 1., -1.)),
                (v(-1., 1., -1.), v(1., 1., -1.), v(1., 1., 1.), v(-1., 1., 1.)),
                (v(-1., -1., -1.), v(-1., -1., 1.), v(1., -1., 1.), v(1., -1., -1.)),
            ] {
                t.extend(quad(a, b, c, d));
            }
            t
        };
        let sphere = {
            let (seg, rings) = (14, 8);
            let pt = |i: usize, j: usize| {
                let th = std::f32::consts::TAU * i as f32 / seg as f32;
                let ph = std::f32::consts::PI * j as f32 / rings as f32;
                [0.5 * ph.sin() * th.cos(), 0.5 * ph.cos(), 0.5 * ph.sin() * th.sin()]
            };
            let mut t = Vec::new();
            for i in 0..seg {
                for j in 0..rings {
                    t.extend(quad(pt(i, j), pt(i + 1, j), pt(i + 1, j + 1), pt(i, j + 1)));
                }
            }
            t
        };
        let cylinder = {
            let seg = 16;
            let pt = |i: usize, y: f32| {
                let th = std::f32::consts::TAU * i as f32 / seg as f32;
                [0.5 * th.cos(), y, 0.5 * th.sin()]
            };
            let mut t = Vec::new();
            for i in 0..seg {
                t.extend(quad(pt(i, -1.0), pt(i + 1, -1.0), pt(i + 1, 1.0), pt(i, 1.0)));
                t.push([[0.0, 1.0, 0.0], pt(i, 1.0), pt(i + 1, 1.0)]);
                t.push([[0.0, -1.0, 0.0], pt(i + 1, -1.0), pt(i, -1.0)]);
            }
            t
        };
        let plane = quad([-5., 0., -5.], [-5., 0., 5.], [5., 0., 5.], [5., 0., -5.]);
        let quad_mesh = quad([-0.5, -0.5, 0.], [-0.5, 0.5, 0.], [0.5, 0.5, 0.], [0.5, -0.5, 0.]);
        let triangle = vec![[[-0.5, -0.5, 0.], [0., 0.5, 0.], [0.5, -0.5, 0.]]];
        vec![cube, sphere, cylinder, plane, quad_mesh, triangle]
    });
    let i = match p {
        Prim::Cube => 0,
        Prim::Sphere => 1,
        Prim::Cylinder => 2,
        Prim::Plane => 3,
        Prim::Quad => 4,
        Prim::Triangle => 5,
    };
    &all[i]
}

fn light(lane: i64, id: Option<i64>, emissive: bool) -> Option<(i64, Option<i64>)> {
    emissive.then_some((lane, id))
}

fn bar(pos: V3, rot_z: f32, scale: V3, l: Option<(i64, Option<i64>)>) -> Obj {
    Obj {
        prim: Prim::Cube,
        pos,
        rot: Quat::euler([0.0, 0.0, rot_z]),
        scale,
        tracks: Vec::new(),
        light: l,
        color: [1.0; 4],
        emissive: l.is_some(),
    }
}

/// A generic stand-in for the base environment. Light IDs used by custom geometry are left
/// out so they don't show up twice.
pub fn base_stage(lanes: &[LaneLights], rows: &[crate::map::Row], scene: &Scene) -> Vec<Obj> {
    let custom: HashSet<(i64, i64)> = scene
        .objects
        .iter()
        .filter_map(|o| o.light.and_then(|(l, id)| id.map(|i| (l, i))))
        .collect();
    let ids = |lane: i64, default: usize| -> Vec<Option<i64>> {
        let Some(l) = lanes.iter().find(|l| l.lane == lane) else { return Vec::new() };
        let list: Vec<Option<i64>> = l
            .ids
            .iter()
            .map(|(id, _)| *id)
            .filter(|id| !custom.contains(&(lane, *id)))
            .map(Some)
            .collect();
        if list.is_empty() { vec![None; default] } else { list }
    };
    let used = |lane: i64| rows.iter().any(|r| r.lane == lane && !r.keys.is_empty());

    let mut out = vec![Obj {
        color: [0.18, 0.18, 0.2, 1.0],
        ..bar([0.0, -0.25, 12.0], 0.0, [3.0, 0.5, 30.0], None)
    }];
    // Back lasers: a fan far behind the track.
    let back = ids(0, 5);
    let n = back.len().max(2) as f32 - 1.0;
    for (i, id) in back.iter().enumerate() {
        let t = i as f32 / n - 0.5;
        out.push(bar([t * 40.0, 22.0, 75.0], -t * 50.0, [0.3, 55.0, 0.3], light(0, *id, true)));
    }
    // Ring lights: square rings around the track.
    for (i, id) in ids(1, 10).iter().enumerate() {
        let z = 10.0 + i as f32 * 5.0;
        let l = light(1, *id, true);
        out.push(bar([0.0, 13.0, z], 0.0, [16.0, 0.25, 0.25], l));
        out.push(bar([0.0, -3.0, z], 0.0, [16.0, 0.25, 0.25], l));
        out.push(bar([-8.0, 5.0, z], 0.0, [0.25, 16.0, 0.25], l));
        out.push(bar([8.0, 5.0, z], 0.0, [0.25, 16.0, 0.25], l));
    }
    // Side lasers, plus the inner pair when the map uses it.
    for (lane, side, inner) in [(2, -1.0f32, false), (3, 1.0, false), (10, -1.0, true), (11, 1.0, true)] {
        if inner && !used(lane) {
            continue;
        }
        for (i, id) in ids(lane, 4).iter().enumerate() {
            let fi = i as f32;
            let (x, z, tilt) = if inner {
                (side * (5.0 + fi * 1.2), 16.0 + fi * 6.0, side * -8.0)
            } else {
                (side * (11.0 + fi * 2.0), 22.0 + fi * 9.0, side * -(18.0 + fi * 4.0))
            };
            out.push(bar([x, 14.0, z], tilt, [0.3, 40.0, 0.3], light(lane, *id, true)));
        }
    }
    // Center lights: a strip across the far end of the track.
    let center = ids(4, 1);
    let w = 26.0 / center.len() as f32;
    for (i, id) in center.iter().enumerate() {
        let x = -13.0 + w * (i as f32 + 0.5);
        out.push(bar([x, 0.3, 45.0], 0.0, [w * 0.9, 0.35, 0.35], light(4, *id, true)));
    }
    // Extra side lights as vertical columns.
    for (lane, side) in [(6, -1.0f32), (7, 1.0)] {
        if !used(lane) {
            continue;
        }
        let list = ids(lane, 1);
        let seg = 18.0 / list.len() as f32;
        for (i, id) in list.iter().enumerate() {
            let y = 2.0 + seg * (i as f32 + 0.5);
            out.push(bar([side * 20.0, y, 32.0], 0.0, [0.4, seg * 0.9, 0.4], light(lane, *id, true)));
        }
    }
    out
}

struct Tri {
    pts: [Pos2; 3],
    depth: f32,
    color: Color32,
}

/// A lit thin light drawn as a glowing screen-space line (stands in for bloom).
struct Beam {
    a: Pos2,
    b: Pos2,
    color: [f32; 3],
}

/// Glow around a light's line: (width in pixels at a 420-pixel-high view, additive strength).
const BEAM_GLOW: [(f32, f32); 3] = [(26.0, 0.06), (12.0, 0.16), (5.0, 0.4)];
const BEAM_CORE: f32 = 2.0;

/// Haze: the lights' color added over the frame, stronger at the top (top, bottom strength).
const HAZE: (f32, f32) = (0.16, 0.04);

struct Frame {
    tris: Vec<Tri>,
    beams: Vec<Beam>,
    /// The lights' average color: tints the dark geometry and the haze.
    ambient: [f32; 3],
}

fn rgba(c: [f32; 3], a: f32) -> Color32 {
    let [r, g, b] = c.map(|v| (v.clamp(0.0, 1.0) * 255.0) as u8);
    Color32::from_rgba_unmultiplied(r, g, b, (a.clamp(0.0, 1.0) * 255.0) as u8)
}

/// Added on top of what is behind it (premultiplied with zero alpha), like bloom.
fn additive(c: [f32; 3], strength: f32) -> Color32 {
    let [r, g, b] = c.map(|v| ((v * strength).clamp(0.0, 1.0) * 255.0) as u8);
    Color32::from_rgba_premultiplied(r, g, b, 0)
}

fn is_on(c: [f32; 3]) -> bool {
    c.iter().any(|v| *v > 0.02)
}

/// A lit light's surface: its color with a hot, whiter core, as ChroMapper's bloom shows it.
fn lit_surface(c: [f32; 3]) -> Color32 {
    let m = c.iter().copied().fold(0.0, f32::max).min(1.0);
    rgba(c.map(|v| (v * 1.25 + 0.35 * m).min(1.0)), 1.0)
}

/// Unlit geometry: ChroMapper's environment materials are nearly black, picking up a little of the
/// lights' color.
fn dark_surface(color: [f32; 4], n: V3, sun: V3, ambient: [f32; 3]) -> Color32 {
    let lum = (color[0] + color[1] + color[2]) / 3.0;
    let direct = 0.05 + 0.10 * dot(n, sun).abs();
    let fill = 0.35 + 0.65 * n[1].abs().max(n[2].abs());
    rgba(std::array::from_fn(|i| color[i] * direct + ambient[i] * 0.45 * fill * (0.4 + lum)), 1.0)
}

/// Builds the depth-sorted triangles for one frame.
fn build(
    objects: &[(&Obj, &Scene)],
    env: Option<&EnvFile>,
    beat: f64,
    light_rgb: &dyn Fn(i64, &[i64]) -> [f32; 3],
    cam: &Camera,
    rect: Rect,
) -> Frame {
    let (yaw, pitch) = (cam.yaw.to_radians(), cam.pitch.to_radians());
    let fwd = [yaw.sin() * pitch.cos(), pitch.sin(), yaw.cos() * pitch.cos()];
    let right = norm(cross([0.0, 1.0, 0.0], fwd));
    let up = cross(fwd, right);
    let focal = rect.height() / 2.0 / (cam.vfov.to_radians() / 2.0).tan();
    let center = rect.center();
    let near = 0.1;
    let sun = norm([0.4, 1.0, -0.6]);
    let env_objects = env.map(|e| e.objects.as_slice()).unwrap_or_default();
    let env_lit: Vec<Option<[f32; 3]>> =
        env_objects.iter().map(|o| o.light.as_ref().map(|(lane, ids)| light_rgb(*lane, ids))).collect();
    let ambient = {
        let mut sum = [0.0f32; 3];
        let lit: Vec<[f32; 3]> = env_lit.iter().flatten().copied().collect();
        for c in &lit {
            for k in 0..3 {
                sum[k] += c[k].min(1.0);
            }
        }
        sum.map(|v| (v / lit.len().max(1) as f32).min(1.0))
    };

    let to_view = |p: V3| {
        let d = sub(p, cam.pos);
        [dot(d, right), dot(d, up), dot(d, fwd)]
    };
    let proj = |v: V3| Pos2::new(center.x + v[0] / v[2] * focal, center.y - v[1] / v[2] * focal);
    let mut beams = Vec::new();
    let mut beam = |a: V3, b: V3, c: [f32; 3]| {
        let (mut va, mut vb) = (to_view(a), to_view(b));
        if va[2] < near && vb[2] < near {
            return;
        }
        let clip = |p: V3, q: V3| {
            let t = (near - p[2]) / (q[2] - p[2]);
            [p[0] + (q[0] - p[0]) * t, p[1] + (q[1] - p[1]) * t, near]
        };
        if va[2] < near {
            va = clip(va, vb);
        } else if vb[2] < near {
            vb = clip(vb, va);
        }
        beams.push(Beam { a: proj(va), b: proj(vb), color: c });
    };
    let mut tris = Vec::new();
    let mut push = |world: [V3; 3], color: Color32, bias: f32| {
        let view: Vec<V3> = world.iter().map(|p| to_view(*p)).collect();
        // Clip against the near plane (gives 0, 1 or 2 triangles).
        let mut poly: Vec<V3> = Vec::with_capacity(4);
        for i in 0..3 {
            let (a, b) = (view[i], view[(i + 1) % 3]);
            if a[2] >= near {
                poly.push(a);
            }
            if (a[2] >= near) != (b[2] >= near) {
                let t = (near - a[2]) / (b[2] - a[2]);
                poly.push([a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, near]);
            }
        }
        if poly.len() < 3 {
            return;
        }
        let depth = poly.iter().map(|v| v[2]).sum::<f32>() / poly.len() as f32 + bias;
        for k in 1..poly.len() - 1 {
            tris.push(Tri { pts: [proj(poly[0]), proj(poly[k]), proj(poly[k + 1])], depth, color });
        }
    };

    for (o, scene) in objects {
        let (pos, rot, scale) = scene.transform(o, beat);
        let lit = o.light.map(|(lane, id)| light_rgb(lane, id.as_slice()));
        let to_world = |v: V3, grow: f32| {
            let s = [v[0] * (scale[0] + grow), v[1] * (scale[1] + grow), v[2] * (scale[2] + grow)];
            let r = rot.rotate(s);
            [r[0] + pos[0], r[1] + pos[1], r[2] + pos[2]]
        };
        for t in mesh(o.prim) {
            let w = t.map(|v| to_world(v, 0.0));
            let color = match (o.emissive, lit) {
                (true, Some(c)) if is_on(c) => lit_surface(c),
                (true, Some(_)) => Color32::from_rgb(10, 10, 13),
                (true, None) => rgba([o.color[0], o.color[1], o.color[2]], o.color[3]),
                (false, _) => dark_surface(o.color, norm(cross(sub(w[1], w[0]), sub(w[2], w[0]))), sun, ambient),
            };
            push(w, color, 0.0);
        }
        // Long thin lit cubes also get a glowing line along their length.
        if let Some(c) = lit.filter(|c| is_on(*c) && o.prim == Prim::Cube) {
            let (mut axis, mut long) = (0, scale[0].abs());
            for k in 1..3 {
                if scale[k].abs() > long {
                    (axis, long) = (k, scale[k].abs());
                }
            }
            let thick = (0..3).filter(|k| *k != axis).map(|k| scale[k].abs()).fold(0.0, f32::max);
            if long > 6.0 * thick {
                let mut e = [0.0; 3];
                e[axis] = 0.5;
                let m = [-e[0], -e[1], -e[2]];
                beam(to_world(m, 0.0), to_world(e, 0.0), c);
            }
        }
        // Bloom halo: a slightly larger, faint copy behind lit lights.
        if let Some(c) = lit.filter(|c| is_on(*c)) {
            for (grow, strength) in [(0.15, 0.22), (0.4, 0.10)] {
                for t in mesh(o.prim) {
                    push(t.map(|v| to_world(v, grow)), additive(c, strength), 0.05);
                }
            }
        }
    }
    // Imported base environment: static world-space triangles.
    for (o, lit) in env_objects.iter().zip(&env_lit) {
        let tri_at = |i: usize| -> [V3; 3] {
            let t = &o.tris[i * 9..i * 9 + 9];
            [[t[0], t[1], t[2]], [t[3], t[4], t[5]], [t[6], t[7], t[8]]]
        };
        let n_tris = o.tris.len() / 9;
        match *lit {
            Some(c) => {
                let on = is_on(c);
                let color = if on { lit_surface(c) } else { Color32::from_rgb(10, 10, 13) };
                for i in 0..n_tris {
                    push(tri_at(i), color, 0.0);
                }
                if let (true, Some((a, b))) = (on, o.beam) {
                    beam(a, b, c);
                }
                if on {
                    let mut centre = [0.0f32; 3];
                    for v in o.tris.chunks(3) {
                        for k in 0..3 {
                            centre[k] += v[k];
                        }
                    }
                    let n = (o.tris.len() / 3).max(1) as f32;
                    let centre = centre.map(|c| c / n);
                    // Lights without a glow line get a soft additive halo around them.
                    if o.beam.is_none() {
                        for (grow, strength) in [(1.15, 0.22), (1.5, 0.08)] {
                            for i in 0..n_tris {
                                let t = tri_at(i)
                                    .map(|v| std::array::from_fn(|k| centre[k] + (v[k] - centre[k]) * grow));
                                push(t, additive(c, strength), 0.05);
                            }
                        }
                    }
                }
            }
            None => {
                for i in 0..n_tris {
                    let w = tri_at(i);
                    let n = norm(cross(sub(w[1], w[0]), sub(w[2], w[0])));
                    push(w, dark_surface(o.color, n, sun, ambient), 0.0);
                }
            }
        }
    }
    tris.sort_by(|a, b| b.depth.total_cmp(&a.depth));
    Frame { tris, beams, ambient }
}

/// Number of triangles a frame produces; used by the map checker.
pub fn triangle_count(
    objects: &[(&Obj, &Scene)],
    env: Option<&EnvFile>,
    beat: f64,
    light_rgb: &dyn Fn(i64, &[i64]) -> [f32; 3],
) -> usize {
    let rect = Rect::from_min_size(Pos2::ZERO, eframe::egui::vec2(1280.0, 400.0));
    let tris = build(objects, env, beat, light_rgb, &Camera::default(), rect).tris;
    assert!(tris.iter().all(|t| t.pts.iter().all(|p| p.x.is_finite() && p.y.is_finite())));
    tris.len()
}

pub fn paint(
    painter: &Painter,
    rect: Rect,
    objects: &[(&Obj, &Scene)],
    env: Option<&EnvFile>,
    beat: f64,
    light_rgb: &dyn Fn(i64, &[i64]) -> [f32; 3],
    cam: &Camera,
) {
    painter.rect_filled(rect, 0.0, Color32::from_rgb(4, 4, 8));
    paint_frame(painter, rect, build(objects, env, beat, light_rgb, cam, rect), true);
}

/// Objects only, over what is already drawn (Chroma geometry over the GPU-rendered environment).
pub fn paint_overlay(
    painter: &Painter,
    rect: Rect,
    objects: &[(&Obj, &Scene)],
    beat: f64,
    light_rgb: &dyn Fn(i64, &[i64]) -> [f32; 3],
    cam: &Camera,
) {
    paint_frame(painter, rect, build(objects, None, beat, light_rgb, cam, rect), false);
}

fn paint_frame(painter: &Painter, rect: Rect, frame: Frame, with_haze: bool) {
    let Frame { tris, beams, ambient } = frame;
    // egui meshes use 32-bit indices; split very large frames anyway to keep buffers modest.
    for chunk in tris.chunks(60_000) {
        let mut m = Mesh::default();
        for t in chunk {
            let base = m.vertices.len() as u32;
            for p in t.pts {
                m.colored_vertex(p, t.color);
            }
            m.add_triangle(base, base + 1, base + 2);
        }
        painter.add(Shape::mesh(m));
    }
    if with_haze {
    let mut haze = Mesh::default();
    let (top, bottom) = (additive(ambient, HAZE.0), additive(ambient, HAZE.1));
    haze.colored_vertex(rect.left_top(), top);
    haze.colored_vertex(rect.right_top(), top);
    haze.colored_vertex(rect.right_bottom(), bottom);
    haze.colored_vertex(rect.left_bottom(), bottom);
    haze.add_triangle(0, 1, 2);
    haze.add_triangle(0, 2, 3);
    painter.add(Shape::mesh(haze));
    }
    let k = rect.height() / 420.0;
    for b in &beams {
        for (w, strength) in BEAM_GLOW {
            painter.line_segment([b.a, b.b], eframe::egui::Stroke::new(w * k, additive(b.color, strength)));
        }
        painter.line_segment([b.a, b.b], eframe::egui::Stroke::new(BEAM_CORE * k, lit_surface(b.color)));
    }
}

/// Rasterizes one frame to RGB bytes (for `--render` snapshots; same triangles as the UI).
pub fn render_rgb(
    objects: &[(&Obj, &Scene)],
    env: Option<&EnvFile>,
    beat: f64,
    light_rgb: &dyn Fn(i64, &[i64]) -> [f32; 3],
    cam: &Camera,
    w: usize,
    h: usize,
) -> Vec<u8> {
    let rect = Rect::from_min_size(Pos2::ZERO, eframe::egui::vec2(w as f32, h as f32));
    let mut img = vec![[4.0f32, 4.0, 8.0]; w * h];
    let frame = build(objects, env, beat, light_rgb, cam, rect);
    for t in frame.tris {
        let [a, b, c] = t.pts;
        let (x0, x1) = (a.x.min(b.x).min(c.x).max(0.0) as usize, (a.x.max(b.x).max(c.x).ceil() as usize).min(w));
        let (y0, y1) = (a.y.min(b.y).min(c.y).max(0.0) as usize, (a.y.max(b.y).max(c.y).ceil() as usize).min(h));
        let area = (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x);
        if area.abs() < 1e-6 {
            continue;
        }
        // Color32 is premultiplied: zero alpha adds.
        let alpha = f32::from(t.color.a()) / 255.0;
        let col = [t.color.r(), t.color.g(), t.color.b()].map(f32::from);
        for y in y0..y1 {
            for x in x0..x1 {
                let p = Pos2::new(x as f32 + 0.5, y as f32 + 0.5);
                let e = |p0: Pos2, p1: Pos2| ((p1.x - p0.x) * (p.y - p0.y) - (p1.y - p0.y) * (p.x - p0.x)) / area;
                if e(a, b) >= 0.0 && e(b, c) >= 0.0 && e(c, a) >= 0.0 {
                    let px = &mut img[y * w + x];
                    for i in 0..3 {
                        px[i] = px[i] * (1.0 - alpha) + col[i];
                    }
                }
            }
        }
    }
    for y in 0..h {
        let t = y as f32 / h.max(1) as f32;
        let s = HAZE.0 + (HAZE.1 - HAZE.0) * t;
        for px in &mut img[y * w..(y + 1) * w] {
            for i in 0..3 {
                px[i] += frame.ambient[i] * s * 255.0;
            }
        }
    }
    let k = h as f32 / 420.0;
    for b in &frame.beams {
        let (d, len2) = (b.b - b.a, (b.b - b.a).length_sq().max(1e-6));
        let r = BEAM_GLOW[0].0 * k / 2.0;
        let (x0, x1) = ((b.a.x.min(b.b.x) - r).max(0.0) as usize, ((b.a.x.max(b.b.x) + r).ceil() as usize).min(w));
        let (y0, y1) = ((b.a.y.min(b.b.y) - r).max(0.0) as usize, ((b.a.y.max(b.b.y) + r).ceil() as usize).min(h));
        let core = lit_surface(b.color);
        let core = [core.r(), core.g(), core.b()].map(f32::from);
        for y in y0..y1 {
            for x in x0..x1 {
                let p = Pos2::new(x as f32 + 0.5, y as f32 + 0.5);
                let t = ((p - b.a).dot(d) / len2).clamp(0.0, 1.0);
                let dist = (p - (b.a + d * t)).length();
                let px = &mut img[y * w + x];
                for (gw, strength) in BEAM_GLOW {
                    let half = gw * k / 2.0;
                    if dist < half {
                        let f = 1.0 - dist / half;
                        for i in 0..3 {
                            px[i] += b.color[i].min(1.0) * strength * f * 255.0;
                        }
                    }
                }
                if dist < BEAM_CORE * k / 2.0 {
                    *px = core;
                }
            }
        }
    }
    img.iter().flat_map(|p| p.map(|v| v.clamp(0.0, 255.0) as u8)).collect()
}
