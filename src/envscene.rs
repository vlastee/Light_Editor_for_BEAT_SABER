//! Drives the GPU renderer from the light model: what ChroMapper's light components do with an event
//! color (sRGB rgb, alpha = intensity) before it reaches the shaders. Uses the environment's exported
//! component settings (components.json) and fog lights (bloomfog.json).

use crate::environment::EnvFile;
use crate::gpu3d::{srgb_to_linear, Block, DirLights, FogLight, SceneState, SkyGradient};
use crate::map::{MoveTarget, Row, RowKind};
use crate::scene::Quat;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;

type Key = (i64, Vec<i64>);

pub fn key_of(v: &Value) -> Option<Key> {
    let a = v.as_array()?;
    let lane = a.first()?.as_i64()?;
    let ids = match a.get(1)? {
        Value::Array(ids) => ids.iter().filter_map(Value::as_i64).collect(),
        x => vec![x.as_i64()?],
    };
    Some((lane, ids))
}

fn f(v: &Value, d: f32) -> f32 {
    v.as_f64().map_or(d, |x| x as f32)
}

fn v4(v: &Value) -> [f32; 4] {
    std::array::from_fn(|i| v.get(i).map_or(0.0, |x| f(x, 0.0)))
}

fn lin(c: [f32; 4]) -> [f32; 4] {
    [srgb_to_linear(c[0]), srgb_to_linear(c[1]), srgb_to_linear(c[2]), c[3]]
}

/// A tube light (ParametricBoxLight) on an environment object.
struct BoxLight {
    object: usize,
    key: Option<Key>,
    static_color: [f32; 4],
    alpha_mul: f32,
    min_alpha: f32,
    alpha_width: [f32; 4],
}

/// A RectangleFakeGlowLightController on an environment object.
struct FakeGlow {
    object: usize,
    key: Option<Key>,
    static_color: [f32; 4],
    alpha_mul: f32,
    min_alpha: f32,
    size_params: [f32; 4],
}

/// A ParametricSpriteLight (glow sprite along a laser) on an environment object.
struct SpriteLight {
    id: i64,
    object: usize,
    key: Option<Key>,
    static_color: [f32; 4],
    alpha_mul: f32,
    min_alpha: f32,
    alpha_width: [f32; 4],
    size_params: [f32; 4],
}

/// Which renderer property a material light writes.
#[derive(Clone, Copy, PartialEq)]
enum Prop {
    Color,
    Emission,
    EmissionTex,
    Other,
}

impl Prop {
    fn of(name: &str) -> Prop {
        match name {
            "_Color" => Prop::Color,
            "_EmissionColor" => Prop::Emission,
            "_EmissionTexColor" => Prop::EmissionTex,
            _ => Prop::Other,
        }
    }
}

/// InstancedMaterialLightController (through a MaterialPropertyBlockColorSetter) or
/// MaterialLightController: an event color written into a property of some renderers.
struct MaterialLight {
    key: Option<Key>,
    static_color: [f32; 4],
    objects: Vec<usize>,
    prop: Prop,
    rule: MaterialRule,
}

enum MaterialRule {
    Instanced {
        intensity: f32,
        hdr: bool,
        min_alpha: f32,
        set_color_only: Option<f32>,
        /// 0 none, 1 before applying min alpha, 2 after.
        multiply: u8,
        saturate: bool,
        inverse_alpha: bool,
        multiply_with_alpha: bool,
        disable_on_zero: bool,
    },
    Material {
        set_alpha_only: bool,
        alpha_intensity: f32,
        alpha_into_color: bool,
        set_color_only: Option<f32>,
        multiply_with_alpha: bool,
        color_multiplier: Option<f32>,
    },
}

impl MaterialLight {
    /// The property value (sRGB rgb, alpha) and whether the renderers stay shown.
    fn value(&self, c: [f32; 4]) -> ([f32; 4], bool) {
        match self.rule {
            MaterialRule::Instanced { intensity, hdr, min_alpha, set_color_only, multiply, saturate, inverse_alpha, multiply_with_alpha, disable_on_zero } => {
                let mut c = c;
                let mut a = match set_color_only {
                    Some(start) => start,
                    None => {
                        let a = c[3].max(min_alpha) * intensity;
                        if saturate { a.clamp(0.0, 1.0) } else { a }
                    }
                };
                match multiply {
                    1 => (0..3).for_each(|k| c[k] *= c[3]),
                    2 => (0..3).for_each(|k| c[k] *= a),
                    _ => {}
                }
                c[3] = a;
                if hdr {
                    c = c.map(|v| v * intensity);
                }
                if inverse_alpha {
                    c[3] = 1.0 - c[3];
                }
                if multiply_with_alpha {
                    (0..3).for_each(|k| c[k] *= c[3]);
                }
                a = c[3];
                (c, !disable_on_zero || a > 0.01)
            }
            MaterialRule::Material { set_alpha_only, alpha_intensity, alpha_into_color, set_color_only, multiply_with_alpha, color_multiplier } => {
                let a = c[3] * alpha_intensity;
                let mut out = if set_alpha_only {
                    [c[0], c[1], c[2], a]
                } else if alpha_into_color {
                    [a, a, a, 1.0]
                } else {
                    c
                };
                if let Some(alpha) = set_color_only {
                    out[3] = alpha;
                }
                let mut k = 1.0;
                if multiply_with_alpha {
                    k *= a;
                }
                if let Some(m) = color_multiplier {
                    k *= m;
                }
                if multiply_with_alpha || color_multiplier.is_some() {
                    (0..3).for_each(|i| out[i] *= k);
                }
                (out, true)
            }
        }
    }
}

/// SpriteLightController / SpriteArrayLightController: an event color as SpriteRenderer colors.
/// EnableRendererLightController only shows or hides its renderer (`color_rule` None).
struct SpriteColorLight {
    key: Option<Key>,
    static_color: [f32; 4],
    objects: Vec<usize>,
    /// Each sprite's color at export (kept by SetAlphaOnly and SetColorOnly).
    initial: Vec<[f32; 4]>,
    color_rule: Option<SpriteRule>,
    hide_range: Option<(f32, f32)>,
}

struct SpriteRule {
    intensity: f32,
    min_alpha: f32,
    /// 0 none, 1 before applying min alpha, 2 after.
    multiply: u8,
    set_color_only: bool,
    set_alpha_only: bool,
}

/// Unity's AnimationCurve.Evaluate on keys [time, value, inTangent, outTangent] (Hermite).
pub fn eval_curve(keys: &[[f32; 4]], t: f32) -> f32 {
    let (Some(first), Some(last)) = (keys.first(), keys.last()) else { return 0.0 };
    if t <= first[0] {
        return first[1];
    }
    if t >= last[0] {
        return last[1];
    }
    let i = keys.partition_point(|k| k[0] <= t).max(1);
    let (a, b) = (keys[i - 1], keys[i]);
    let dt = b[0] - a[0];
    if dt <= 0.0 {
        return b[1];
    }
    let u = (t - a[0]) / dt;
    let (m0, m1) = (a[3] * dt, b[2] * dt);
    if !m0.is_finite() || !m1.is_finite() {
        return a[1];
    }
    let (u2, u3) = (u * u, u * u * u);
    (2.0 * u3 - 3.0 * u2 + 1.0) * a[1] + (u3 - 2.0 * u2 + u) * m0 + (-2.0 * u3 + 3.0 * u2) * b[1] + (u3 - u2) * m1
}

/// BurstFireEffect / ContinuousFireEffect (Pyro): flipbook fire started by light group events.
struct FireLight {
    key: Option<Key>,
    burst: bool,
    fade_in: f32,
    fade_out: f32,
    sustain: f32,
    flip_curve: Vec<[f32; 4]>,
    bloom_curve: Vec<[f32; 4]>,
    bloom_mul: f32,
    use_emission: bool,
    point_color: [f32; 4],
    flip_objects: Vec<usize>,
    bloom_objects: Vec<usize>,
    emission_objects: Vec<usize>,
    prepass: Vec<usize>,
}

/// A Unity constraint (PositionConstraint, RotationConstraint, ParentConstraint): its transform follows
/// source transforms. The exported pose gives the offsets; moving sources move it the same way.
struct Constraint {
    kind: ConstraintKind,
    tid: i64,
    m0: [f32; 16],
    weight: f32,
    /// Source transform IDs, weights and their exported world matrices.
    sources: Vec<(i64, f32, [f32; 16])>,
    objects: Vec<usize>,
    fogs: Vec<usize>,
}

#[derive(Clone, Copy, PartialEq)]
enum ConstraintKind {
    Position,
    Rotation,
    Parent,
}

/// What a VFX group element drives (FxTarget components, CollectionFx expanded).
enum FxTarget {
    /// MoveInDirectionFx: a world move per unit of value.
    Move { per_value: [f32; 3], objects: Vec<usize>, fogs: Vec<usize> },
    /// SwitchGameObjectFx (thresholds [0, 0.0…01]) and SwitchGameObjectArrayFx: the last object whose
    /// threshold the value reaches shows, the others hide.
    Switch { stages: Vec<(f32, Part)> },
    /// LocalScaleFx: scale about the transform's origin.
    Scale { m0: [f32; 16], bounds: [f32; 2], objects: Vec<usize> },
    /// ParametricSliceEndWidthFx: a sprite light's end width.
    EndWidth { sprite: usize, mul: f32, bounds: [f32; 2] },
    /// AlphaFx: a static color with the value as alpha.
    Alpha { color: [f32; 4], prop: Prop, objects: Vec<usize> },
}

struct FxEntry {
    group: i64,
    element: i64,
    targets: Vec<FxTarget>,
}

/// A ColorArrayLightsController: lights writing entries of a material's `_ColorsArray`, and the
/// renderers reading it with their `_ColorsArrayOffset`.
struct ColorArray {
    id: i64,
    material: Option<usize>,
    entries: Vec<(usize, Option<Key>, [f32; 4])>,
    len: usize,
    offsets: Vec<(usize, f32)>,
}

struct DirLightDef {
    path: String,
    pos: [f32; 4],
    dir: [f32; 4],
    intensity: f32,
    radius: f32,
    static_color: [f32; 4],
    controller: Option<Combined>,
}

/// CombinedLightsController (DirectionalLightsController).
struct Combined {
    children: Vec<(Option<Key>, [f32; 4], f32)>,
    intensity: f32,
    max_intensity: f32,
    multiply_by_alpha: bool,
    maximum: bool,
    intensity_only: Option<[f32; 4]>,
}

impl Combined {
    fn color(&self, light: &dyn Fn(&Key) -> [f32; 4]) -> [f32; 4] {
        let mut c = [0.0f32; 4];
        for (key, fallback, intensity) in &self.children {
            let mut p = key.as_ref().map_or(*fallback, |k| light(k));
            p[3] *= intensity;
            if self.maximum {
                p[3] = p[3].max(0.0).sqrt();
            }
            if self.multiply_by_alpha {
                for k in 0..3 {
                    p[k] *= p[3];
                }
            }
            for k in 0..4 {
                if self.maximum {
                    c[k] = c[k].max(p[k]);
                } else if k < 3 {
                    c[k] += p[k];
                }
            }
        }
        if self.multiply_by_alpha {
            c = c.map(|v| v * self.intensity);
            let gray = 0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2];
            if gray > self.max_intensity {
                c = c.map(|v| v / (gray / self.max_intensity));
            }
        } else {
            c[3] = (c[3] * self.intensity).min(self.max_intensity);
        }
        if let Some(d) = self.intensity_only {
            // Color.WithValue: the default color's hue and saturation at the alpha as value.
            let max = d[0].max(d[1]).max(d[2]).max(1e-6);
            c = [d[0] / max * c[3], d[1] / max * c[3], d[2] / max * c[3], d[3]];
        }
        c
    }
}

/// Objects and fog lights under one GameObject.
#[derive(Clone, Default)]
struct Part {
    objects: Vec<usize>,
    fogs: Vec<usize>,
}

/// A SmoothStepPositionEventEffect: moves a transform to the position of each event's value.
struct SmoothStep {
    ty: i64,
    parent: [f32; 16],
    init: [f32; 3],
    vector: [f32; 3],
    step: f32,
    clamp: Option<(i64, i64)>,
    part: Part,
}

/// A GameObjectIntSwitch: an event value picks which group of objects is active.
struct Switch {
    ty: i64,
    default: i64,
    containers: Vec<(i64, Part)>,
}

/// One transform a light group moves, and what's drawn under it.
struct GroupTransform {
    /// The transform's instance ID, when exported.
    tid: Option<i64>,
    motion: crate::groups::GroupMove,
    m0: [f32; 16],
    parent: [f32; 16],
    objects: Vec<usize>,
    fogs: Vec<usize>,
}

/// The rotation part of a matrix given by rows (columns of unit length; mirroring removed).
fn rotation_of(m: &[f32; 16]) -> [[f32; 3]; 3] {
    let mut c: [[f32; 3]; 3] = std::array::from_fn(|k| {
        let v = [m[k], m[4 + k], m[8 + k]];
        let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-6);
        v.map(|x| x / len)
    });
    let [a, b, d] = c;
    let det = a[0] * (b[1] * d[2] - b[2] * d[1]) - a[1] * (b[0] * d[2] - b[2] * d[0]) + a[2] * (b[0] * d[1] - b[1] * d[0]);
    if det < 0.0 {
        c[0] = a.map(|x| -x);
    }
    c
}

/// A world matrix (by rows) turned by `q` around the world point `pivot`.
fn turned(m: &[f32; 16], q: Quat, pivot: [f32; 3]) -> [f32; 16] {
    let mut out = *m;
    for k in 0..3 {
        let c = q.rotate([m[k], m[4 + k], m[8 + k]]);
        (out[k], out[4 + k], out[8 + k]) = (c[0], c[1], c[2]);
    }
    let t = q.rotate([m[3] - pivot[0], m[7] - pivot[1], m[11] - pivot[2]]);
    (out[3], out[7], out[11]) = (pivot[0] + t[0], pivot[1] + t[1], pivot[2] + t[2]);
    out
}

fn group_transforms(comps: &[Value], env: &EnvFile, fog: &[(FogLight, Option<Key>, String)], fog_tids: &[Vec<i64>]) -> Vec<GroupTransform> {
    use crate::motion::under;
    let matrix = |v: &Value| -> Option<[f32; 16]> {
        let a = v.as_array()?;
        (a.len() == 16).then(|| std::array::from_fn(|i| f(&a[i], 0.0)))
    };
    // Same-named transforms are told apart by which is nearest to each object.
    let mut positions: HashMap<String, Vec<[f32; 3]>> = HashMap::new();
    let mut out: Vec<(usize, GroupTransform)> = Vec::new();
    let mut raw = Vec::new();
    for manager_type in ["LightRotationGroupEffectManager", "LightTranslationGroupEffectManager"] {
        let translation = manager_type.contains("Translation");
        for entry in comps.iter().filter(|c| c["type"] == manager_type).flat_map(|c| c["fields"]["effectEntries"].as_array().into_iter().flatten()) {
            let Some(group) = entry["Group"].as_i64() else { continue };
            let Some(effect) = comps.iter().find(|c| c["id"] == entry["Effect"]["id"] && c["id"].is_i64()) else { continue };
            let x = &effect["fields"];
            for e in x["transformEntries"].as_array().into_iter().flatten() {
                let axis = match e["Axis"].as_str() {
                    Some("Y") => 1,
                    Some("Z") => 2,
                    _ => 0,
                };
                let limits = translation.then(|| {
                    let pair = |k: &str| -> [f32; 2] { [f(&x[k][axis as usize][0], -1.0), f(&x[k][axis as usize][1], 1.0)] };
                    (pair("TranslationLimits"), pair("DistributionLimits"))
                });
                for t in e["Transforms"].as_array().into_iter().flatten() {
                    let (Some(path), Some(m0)) = (t["path"].as_str(), matrix(&t["m"])) else { continue };
                    let parent = matrix(&t["pm"]).unwrap_or(std::array::from_fn(|i| if i % 5 == 0 { 1.0 } else { 0.0 }));
                    positions.entry(path.to_owned()).or_default().push([m0[3], m0[7], m0[11]]);
                    raw.push((path, t["id"].as_i64(), group, e["ID"].as_i64().unwrap_or(0), axis, e["Mirrored"].as_bool().unwrap_or(false), limits, m0, parent));
                }
            }
        }
    }
    // Same name, same place, different elements of one group: separate objects we can't tell apart
    // (Lattice's cubes all start stacked). Leave those at rest rather than piling every move on them.
    let mut elements: HashMap<(&str, i64, i64, bool, [i32; 3]), HashSet<i64>> = HashMap::new();
    let cell = |m: &[f32; 16]| [(m[3] * 100.0) as i32, (m[7] * 100.0) as i32, (m[11] * 100.0) as i32];
    for (path, _, group, element, axis, _, limits, m0, _) in &raw {
        elements.entry((*path, *group, *axis, limits.is_some(), cell(m0))).or_default().insert(*element);
    }
    // Exports with transform instance IDs say exactly what's under each transform.
    let by_id = env.objects.iter().any(|o| !o.tids.is_empty());
    for (path, tid, group, element, axis, mirrored, limits, m0, parent) in raw {
        let motion = crate::groups::GroupMove { group, element, axis, mirrored, limits };
        if let (true, Some(id)) = (by_id, tid) {
            let objects = env.objects.iter().enumerate().filter(|(_, o)| o.tids.contains(&id)).map(|(i, _)| i).collect();
            let fogs = fog_tids.iter().enumerate().filter(|(_, t)| t.contains(&id)).map(|(i, _)| i).collect();
            out.push((path.matches('/').count(), GroupTransform { tid, motion, m0, parent, objects, fogs }));
            continue;
        }
        if elements[&(path, group, axis, limits.is_some(), cell(&m0))].len() > 1 {
            continue;
        }
        let distance = |p: [f32; 3], m: &[f32; 16]| (p[0] - m[3]).powi(2) + (p[1] - m[7]).powi(2) + (p[2] - m[11]).powi(2);
        let own = [m0[3], m0[7], m0[11]];
        let others = &positions[path];
        let mine = |m: &[f32; 16]| others.iter().all(|p| distance(own, m) <= distance(*p, m) + 1e-6);
        let objects = env.objects.iter().enumerate().filter(|(_, o)| under(&o.path, path) && mine(&o.matrix)).map(|(i, _)| i).collect();
        let fogs = fog.iter().enumerate().filter(|(_, (l, _, p))| under(p, path) && mine(&l.matrix)).map(|(i, _)| i).collect();
        out.push((path.matches('/').count(), GroupTransform { tid, motion, m0, parent, objects, fogs }));
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.into_iter().map(|e| e.1).collect()
}

/// VFX groups: FloatFxGroupEffectManager entries and their targets.
fn find_id<'a>(comps: &'a [Value], r: &Value) -> Option<&'a Value> {
    let id = r["id"].as_i64()?;
    comps.iter().find(|k| k["id"].as_i64() == Some(id))
}

fn read_fx(comps: &[Value], env: &EnvFile, lights: &EnvLights, fog_tids: &[Vec<i64>]) -> Vec<FxEntry> {
    let by_id = |r: &Value| find_id(comps, r);
    let mat = |v: &Value| -> Option<[f32; 16]> {
        let a = v.as_array()?;
        (a.len() == 16).then(|| std::array::from_fn(|i| f(&a[i], 0.0)))
    };
    // Objects and fog lights under a transform (by instance ID when exported, else by path).
    let under_transform = |t: &Value| -> (Vec<usize>, Vec<usize>) {
        match (t["id"].as_i64(), env.objects.iter().any(|o| !o.tids.is_empty())) {
            (Some(id), true) => (
                env.objects.iter().enumerate().filter(|(_, o)| o.tids.contains(&id)).map(|(i, _)| i).collect(),
                fog_tids.iter().enumerate().filter(|(_, t)| t.contains(&id)).map(|(i, _)| i).collect(),
            ),
            _ => {
                let p = t["path"].as_str().unwrap_or("\u{0}");
                (
                    env.objects.iter().enumerate().filter(|(_, o)| crate::motion::under(&o.path, p)).map(|(i, _)| i).collect(),
                    lights.fog.iter().enumerate().filter(|(_, (_, _, fp))| crate::motion::under(fp, p)).map(|(i, _)| i).collect(),
                )
            }
        }
    };
    let part_of = |go: &Value| -> Part {
        let p = go["gameObject"].as_str().unwrap_or("\u{0}");
        Part {
            objects: env.objects.iter().enumerate().filter(|(_, o)| crate::motion::under(&o.path, p)).map(|(i, _)| i).collect(),
            fogs: lights.fog.iter().enumerate().filter(|(_, (_, _, fp))| crate::motion::under(fp, p)).map(|(i, _)| i).collect(),
        }
    };
    fn targets_of(c: &Value, depth: usize, comps: &[Value], make: &dyn Fn(&Value) -> Option<FxTarget>, out: &mut Vec<FxTarget>) {
        if c["type"] == "CollectionFx" {
            if depth < 4 {
                for t in c["fields"]["Targets"].as_array().into_iter().flatten().filter_map(|r| find_id(comps, r)) {
                    targets_of(t, depth + 1, comps, make, out);
                }
            }
        } else if let Some(t) = make(c) {
            out.push(t);
        }
    }
    let make = |c: &Value| -> Option<FxTarget> {
        let x = &c["fields"];
        match c["type"].as_str()? {
            "MoveInDirectionFx" => {
                let t = &x["TargetTransform"];
                let pm = mat(&t["pm"])?;
                let dir: [f32; 3] = std::array::from_fn(|i| f(&x["moveDirection"][i], 0.0));
                let scale = f(&x["MoveScale"], 1.0);
                let per_value = std::array::from_fn(|r| (pm[r * 4] * dir[0] + pm[r * 4 + 1] * dir[1] + pm[r * 4 + 2] * dir[2]) * scale);
                let (objects, fogs) = under_transform(t);
                Some(FxTarget::Move { per_value, objects, fogs })
            }
            "SwitchGameObjectFx" => Some(FxTarget::Switch { stages: vec![(f32::MIN, part_of(&x["GameObjectA"])), (1e-6, part_of(&x["GameObjectB"]))] }),
            "SwitchGameObjectArrayFx" => Some(FxTarget::Switch {
                stages: x["GameObjects"].as_array()?.iter().map(|g| (f(&g["Threshold"], 0.0), part_of(&g["GameObject"]))).collect(),
            }),
            "LocalScaleFx" => {
                let t = x["TargetTransforms"].as_array()?.last()?;
                let (objects, _) = under_transform(t);
                Some(FxTarget::Scale { m0: mat(&t["m"])?, bounds: [f(&x["ValueBounds"][0], 1.0), f(&x["ValueBounds"][1], 10.0)], objects })
            }
            "ParametricSliceEndWidthFx" => {
                let id = x["SpriteLight"]["id"].as_i64()?;
                let sprite = lights.sprites.iter().position(|s| s.id == id)?;
                Some(FxTarget::EndWidth { sprite, mul: f(&x["ValueMultiplier"], 1.0), bounds: [f(&x["ValueBounds"][0], 0.0), f(&x["ValueBounds"][1], 1.0)] })
            }
            "AlphaFx" => {
                let objects = x["MpbControllers"]
                    .as_array()?
                    .iter()
                    .filter_map(by_id)
                    .flat_map(|m| m["fields"]["Renderers"].as_array().cloned().unwrap_or_default())
                    .filter_map(|r| r["path"].as_str().and_then(|p| env.objects.iter().position(|o| o.path == p)))
                    .collect();
                Some(FxTarget::Alpha { color: v4(&x["StaticColor"]), prop: Prop::of(x["Property"].as_str().unwrap_or("_Color")), objects })
            }
            _ => None,
        }
    };
    let mut out = Vec::new();
    for manager in comps.iter().filter(|c| c["type"] == "FloatFxGroupEffectManager") {
        for entry in manager["fields"]["effectEntries"].as_array().into_iter().flatten() {
            let (Some(group), Some(effect)) = (entry["Group"].as_i64(), by_id(&entry["Effect"])) else { continue };
            for fx in effect["fields"]["fxEntries"].as_array().into_iter().flatten() {
                let mut targets = Vec::new();
                for t in fx["Targets"].as_array().into_iter().flatten().filter_map(by_id) {
                    targets_of(t, 0, comps, &make, &mut targets);
                }
                if !targets.is_empty() {
                    out.push(FxEntry { group, element: fx["ID"].as_i64().unwrap_or(0), targets });
                }
            }
        }
    }
    out
}

/// A light's GameObject that a Position or Aim line can move, with what's drawn under it.
struct Mover {
    path: String,
    keys: Vec<Key>,
    /// World position (environment units).
    rest: [f32; 3],
    /// Normal world rotation (columns).
    rot: [[f32; 3]; 3],
    objects: Vec<usize>,
    fogs: Vec<usize>,
}

/// An environment's light components, loaded once.
#[derive(Default)]
pub struct EnvLights {
    /// What special events and boost do in this environment.
    smooth_steps: Vec<SmoothStep>,
    switches: Vec<Switch>,
    boost_switches: Vec<(Vec<Part>, Vec<Part>)>,
    /// Transforms moved by rotation and translation light groups, children first.
    group_transforms: Vec<GroupTransform>,
    /// Ring id → what's drawn on that ring: object indices and fog light indices.
    ring_members: HashMap<i64, (Vec<usize>, Vec<usize>)>,
    movers: Vec<Mover>,
    /// Lights whose GameObject name isn't unique, so Chroma can't address them alone (rings).
    unmovable: Vec<Key>,
    boxes: Vec<BoxLight>,
    glows: Vec<FakeGlow>,
    sprites: Vec<SpriteLight>,
    material_lights: Vec<MaterialLight>,
    color_arrays: Vec<ColorArray>,
    sprite_lights: Vec<SpriteColorLight>,
    fires: Vec<FireLight>,
    fx: Vec<FxEntry>,
    constraints: Vec<Constraint>,
    /// Transform ID → its parent's (from the exported objects' ancestor chains).
    parent_of: HashMap<i64, i64>,
    fog: Vec<(FogLight, Option<Key>, String)>,
    dir: Vec<DirLightDef>,
    sky: Option<SkyGradient>,
    mirror_plane: Option<[f32; 4]>,
}

impl EnvLights {
    /// From the files next to an environment (`dir`: its folder with components.json and bloomfog.json).
    pub fn load(env: &EnvFile, dir: &Path) -> EnvLights {
        let read = |n: &str| -> Value {
            std::fs::read_to_string(dir.join(n)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
        };
        let comps = read("components.json");
        let bloomfog = read("bloomfog.json");
        let empty = Vec::new();
        let comps = comps.as_array().unwrap_or(&empty);
        // v3 light groups address lights by (group, element), kept in a lane of their own.
        let mut group_keys: HashMap<i64, Key> = HashMap::new();
        for manager in comps.iter().filter(|c| c["type"] == "LightColorGroupEffectManager") {
            for entry in manager["fields"]["effectEntries"].as_array().into_iter().flatten() {
                let Some(group) = entry["Group"].as_i64() else { continue };
                let Some(effect) = comps.iter().find(|c| c["type"] == "LightColorGroupEffect" && c["id"] == entry["Effect"]["id"]) else { continue };
                for light in effect["fields"]["lightEntries"].as_array().into_iter().flatten() {
                    let (Some(id), Some(c)) = (light["id"].as_i64(), comps.iter().find(|c| c["id"] == light["id"])) else { continue };
                    let element = c["fields"]["ID"].as_i64().unwrap_or(0);
                    group_keys.insert(id, (crate::map::group_lane(0, group), vec![element]));
                }
            }
        }
        let key_for = |c: &Value| key_of(&c["light"]).or_else(|| c["id"].as_i64().and_then(|id| group_keys.get(&id).cloned()));
        // Several objects can share a path (rings are copies): the one at the component's position.
        let mut by_path: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, o) in env.objects.iter().enumerate() {
            by_path.entry(o.path.as_str()).or_default().push(i);
        }
        let object_at = |path: &str, c: &Value| -> Option<usize> {
            let at = |o: usize| {
                let m = &env.objects[o].matrix;
                let d = |a: f32, b: f32| (a - b) * (a - b);
                d(m[3], f(&c["m"][3], 0.0)) + d(m[7], f(&c["m"][7], 0.0)) + d(m[11], f(&c["m"][11], 0.0))
            };
            by_path.get(path)?.iter().copied().min_by(|a, b| at(*a).total_cmp(&at(*b)))
        };
        // The light (lane, map IDs) of the ParametricBloomFogLightController that owns each tube, glow
        // sprite and fog object (an object can hold controllers for several lanes).
        let mut owner: HashMap<i64, Key> = HashMap::new();
        for c in comps.iter().filter(|c| c["type"] == "ParametricBloomFogLightController") {
            let Some(k) = key_for(c) else { continue };
            for part in ["BoxLight", "SpriteLight", "BloomFog"] {
                if let Some(id) = c["fields"][part]["id"].as_i64() {
                    owner.insert(id, k.clone());
                }
            }
        }
        let owned = |c: &Value| c["id"].as_i64().and_then(|id| owner.get(&id).cloned()).or_else(|| key_for(c));
        let mut out = EnvLights { sky: crate::refrender::sky_gradient(&Value::Array(comps.clone())), ..Default::default() };
        for c in comps {
            let x = &c["fields"];
            let path = c["path"].as_str().unwrap_or_default();
            match c["type"].as_str().unwrap_or_default() {
                "ParametricBoxLight" => {
                    let rpath = x["Renderer"]["path"].as_str().unwrap_or(path);
                    let Some(object) = object_at(rpath, c) else { continue };
                    out.boxes.push(BoxLight {
                        object,
                        key: owned(c),
                        static_color: v4(&x["color"]),
                        alpha_mul: f(&x["AlphaMultiplier"], 1.0),
                        min_alpha: f(&x["MinAlpha"], 0.0),
                        alpha_width: [f(&x["AlphaStart"], 1.0), f(&x["AlphaEnd"], 1.0), f(&x["WidthStart"], 1.0), f(&x["WidthEnd"], 1.0)],
                    });
                }
                "ParametricSpriteLight" => {
                    let rpath = x["Renderer"]["path"].as_str().unwrap_or(path);
                    let Some(object) = object_at(rpath, c) else { continue };
                    let width = f(&x["Width"], 0.5) * f(&x["WidthMultiplier"], 1.0);
                    out.sprites.push(SpriteLight {
                        id: c["id"].as_i64().unwrap_or(0),
                        object,
                        key: owned(c),
                        static_color: v4(&x["color"]),
                        alpha_mul: f(&x["AlphaMultiplier"], 1.0),
                        min_alpha: f(&x["MinAlpha"], 0.0),
                        alpha_width: [f(&x["AlphaStart"], 1.0), f(&x["AlphaEnd"], 1.0), f(&x["WidthStart"], 1.0), f(&x["WidthEnd"], 1.0)],
                        size_params: [width, f(&x["Length"], 1.0), f(&x["Center"], 0.5), width * 2.0],
                    });
                }
                "InstancedMaterialLightController" | "MaterialLightController" => {
                    let instanced = c["type"] == "InstancedMaterialLightController";
                    let by_id = |r: &Value| r["id"].as_i64().and_then(|id| comps.iter().find(|k| k["id"].as_i64() == Some(id)));
                    // The renderers and the property it writes.
                    let (renderers, prop, setter) = if instanced {
                        let Some(setter) = by_id(&x["MpbColorSetter"]) else { continue };
                        let Some(mpb) = by_id(&setter["fields"]["Controller"]) else { continue };
                        let list: Vec<Value> = mpb["fields"]["Renderers"].as_array().cloned().unwrap_or_default();
                        (list, setter["fields"]["Property"].as_str().unwrap_or("_Color").to_owned(), Some(setter["fields"].clone()))
                    } else {
                        (vec![x["Renderer"].clone()], x["Property"].as_str().unwrap_or("_Color").to_owned(), None)
                    };
                    let objects: Vec<usize> = renderers.iter().filter_map(|r| r["path"].as_str()).filter_map(|p| object_at(p, c)).collect();
                    if objects.is_empty() {
                        continue;
                    }
                    let rule = match setter {
                        Some(sx) => MaterialRule::Instanced {
                            intensity: f(&x["Intensity"], 1.0),
                            hdr: x["HDR"].as_bool().unwrap_or(false),
                            min_alpha: f(&x["MinAlpha"], 0.0),
                            set_color_only: x["SetColorOnly"].as_bool().unwrap_or(false).then(|| f(&x["startAlpha"], 0.0)),
                            multiply: match x["MultiplyColorByAlpha"].as_str() {
                                Some("BeforeApplyingMinAlpha") => 1,
                                Some("AfterApplyingMinAlpha") => 2,
                                _ => 0,
                            },
                            saturate: x["SaturateIntensity"].as_bool().unwrap_or(false),
                            inverse_alpha: sx["InverseAlpha"].as_bool().unwrap_or(false),
                            multiply_with_alpha: sx["MultiplyWithAlpha"].as_bool().unwrap_or(false),
                            disable_on_zero: sx["DisableOnZeroAlpha"].as_bool().unwrap_or(false),
                        },
                        None => MaterialRule::Material {
                            set_alpha_only: x["SetAlphaOnly"].as_bool().unwrap_or(false),
                            alpha_intensity: f(&x["AlphaIntensity"], 1.0),
                            alpha_into_color: x["AlphaIntoColor"].as_bool().unwrap_or(false),
                            set_color_only: x["SetColorOnly"].as_bool().unwrap_or(false).then(|| f(&x["Alpha"], 0.0)),
                            multiply_with_alpha: x["MultiplyColorWithAlpha"].as_bool().unwrap_or(false),
                            color_multiplier: x["MultiplyColor"].as_bool().unwrap_or(false).then(|| f(&x["ColorMultiplier"], 1.0)),
                        },
                    };
                    out.material_lights.push(MaterialLight { key: key_for(c), static_color: v4(&x["Color"]), objects, prop: Prop::of(&prop), rule });
                }
                "SpriteLightController" | "SpriteArrayLightController" | "EnableRendererLightController" => {
                    let list: Vec<Value> = match x["SpriteRenderers"].as_array() {
                        Some(a) => a.clone(),
                        None => vec![x["Renderer"].clone()],
                    };
                    let objects: Vec<usize> = list.iter().filter_map(|r| r["path"].as_str()).filter_map(|p| object_at(p, c)).collect();
                    if objects.is_empty() {
                        continue;
                    }
                    let enable_only = c["type"] == "EnableRendererLightController";
                    let color_rule = (!enable_only).then(|| SpriteRule {
                        intensity: f(&x["Intensity"], 1.0),
                        min_alpha: f(&x["MinAlpha"], 0.0),
                        multiply: match x["MultiplyColorByAlpha"].as_str() {
                            Some("BeforeApplyingMinAlpha") => 1,
                            Some("AfterApplyingMinAlpha") => 2,
                            _ => 0,
                        },
                        set_color_only: x["SetColorOnly"].as_bool().unwrap_or(false),
                        set_alpha_only: x["SetAlphaOnly"].as_bool().unwrap_or(false),
                    });
                    let hide = enable_only || x["HideIfAlphaOutOfRange"].as_bool().unwrap_or(false);
                    out.sprite_lights.push(SpriteColorLight {
                        key: key_for(c),
                        static_color: v4(&x["Color"]),
                        initial: objects.iter().map(|&o| env.objects[o].sprite_color.unwrap_or([1.0; 4])).collect(),
                        objects,
                        color_rule,
                        hide_range: hide.then(|| (f(&x["HideAlphaRangeMin"], 0.001), f(&x["HideAlphaRangeMax"], 1.0))),
                    });
                }
                "BurstFireEffect" | "ContinuousFireEffect" => {
                    let by_id = |r: &Value| r["id"].as_i64().and_then(|id| comps.iter().find(|k| k["id"].as_i64() == Some(id)));
                    let renderers = |r: &Value| -> Vec<usize> {
                        by_id(r)
                            .map(|m| m["fields"]["Renderers"].as_array().cloned().unwrap_or_default())
                            .unwrap_or_default()
                            .iter()
                            .filter_map(|r| r["path"].as_str())
                            .filter_map(|p| object_at(p, c))
                            .collect()
                    };
                    let curve = |v: &Value| -> Vec<[f32; 4]> {
                        v.as_array().into_iter().flatten().map(|k| std::array::from_fn(|i| f(&k[i], 0.0))).collect()
                    };
                    let burst = c["type"] == "BurstFireEffect";
                    let prepass = x["BloomPrePassRenderer"]["path"].as_str().and_then(|p| object_at(p, c)).into_iter().collect();
                    out.fires.push(FireLight {
                        key: key_for(c),
                        burst,
                        fade_in: f(&x["FadeInDuration"], 1.0),
                        fade_out: f(&x["FadeOutDuration"], 1.0),
                        sustain: f(&x["SustainDuration"], 1.0),
                        flip_curve: curve(if burst { &x["FlipbookFadeOutCurve"] } else { &x["FlipbookSustainCurve"] }),
                        bloom_curve: curve(if burst { &x["BloomFadeOutCurve"] } else { &x["BloomSustainCurve"] }),
                        bloom_mul: f(&x["BloomIntensityMultiplier"], 1.0),
                        use_emission: x["UseEmissionColor"].as_bool().unwrap_or(false),
                        point_color: v4(&x["PointLightColor"]),
                        flip_objects: renderers(&x["FlipBookPropertyBlockController"]),
                        bloom_objects: renderers(&x["BloomPropertyBlockController"]),
                        emission_objects: renderers(&x["EmissionTextureColorPropertyBlockController"]),
                        prepass,
                    });
                }
                "ColorArrayLightsController" => {
                    let by_id = |r: &Value| r["id"].as_i64().and_then(|id| comps.iter().find(|k| k["id"].as_i64() == Some(id)));
                    let data: Vec<&Value> = x["ColorArrayData"].as_array().into_iter().flatten().filter_map(by_id).collect();
                    let mpbs: Vec<&Value> = x["MpbControllers"].as_array().into_iter().flatten().filter_map(by_id).collect();
                    let len = x["ColorArrayData"].as_array().map_or(0, Vec::len);
                    let num = if mpbs.is_empty() { 0 } else { len / mpbs.len() };
                    let mut offsets = Vec::new();
                    for (i, m) in mpbs.iter().enumerate() {
                        for r in m["fields"]["Renderers"].as_array().into_iter().flatten() {
                            if let Some(o) = r["path"].as_str().and_then(|p| object_at(p, m)) {
                                offsets.push((o, (i * num) as f32));
                            }
                        }
                    }
                    let material = offsets.first().and_then(|(o, _)| env.objects[*o].materials.first()).and_then(|m| usize::try_from(*m).ok());
                    let entries = data
                        .iter()
                        .filter_map(|d| Some((d["fields"]["Index"].as_u64()? as usize, key_for(d), v4(&d["fields"]["Color"]))))
                        .collect();
                    out.color_arrays.push(ColorArray { id: c["id"].as_i64().unwrap_or(0), material, entries, len, offsets });
                }
                "RectangleFakeGlowLightController" => {
                    let Some(object) = object_at(path, c) else { continue };
                    let size = v4(&x["Size"]);
                    let edge = f(&x["EdgeSize"], 0.0);
                    out.glows.push(FakeGlow {
                        object,
                        key: key_for(c),
                        static_color: v4(&x["Color"]),
                        alpha_mul: f(&x["AlphaMultiplier"], 1.0),
                        min_alpha: f(&x["MinAlpha"], 0.0),
                        size_params: [size[0] / 2.0, size[1] / 2.0, 1.0, edge / 2.0],
                    });
                }
                _ => {}
            }
        }
        // Directional lights, in scene order, with their combining controllers.
        for (i, c) in comps.iter().enumerate().filter(|(_, c)| c["type"] == "DirectionalLight") {
            let x = &c["fields"];
            let m = &c["m"];
            let g = |k: usize| f(&m[k], 0.0);
            let fwd = [g(2), g(6), g(10)];
            let len = (fwd[0] * fwd[0] + fwd[1] * fwd[1] + fwd[2] * fwd[2]).sqrt().max(1e-6);
            // Matched by component instance ID (several lights share a path), else by path.
            let same = |r: &Value, c: &Value| match (r["id"].as_i64(), c["id"].as_i64()) {
                (Some(a), Some(b)) => a == b,
                _ => r["path"] == c["path"],
            };
            let controller = comps.iter().find(|k| k["type"] == "DirectionalLightsController" && same(&k["fields"]["Light"], c)).map(|k| {
                let y = &k["fields"];
                let children = y["LightIntensityData"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|r| comps.iter().find(|d| d["type"] == "LightIntensityData" && same(r, d)))
                    .map(|d| (key_for(d), v4(&d["fields"]["Color"]), f(&d["fields"]["Intensity"], 1.0)))
                    .collect();
                Combined {
                    children,
                    intensity: f(&y["Intensity"], 1.0),
                    max_intensity: f(&y["MaxIntensity"], 1.0),
                    multiply_by_alpha: y["MultiplyColorByAlpha"].as_bool().unwrap_or(true),
                    maximum: y["MixType"] != "FractionAndSum",
                    intensity_only: y["SetIntensityOnly"].as_bool().unwrap_or(false).then(|| v4(&y["DefaultColor"])),
                }
            });
            let _ = i;
            out.dir.push(DirLightDef {
                path: c["path"].as_str().unwrap_or_default().to_owned(),
                pos: [g(3), g(7), g(11), 0.0],
                dir: [-fwd[0] / len, -fwd[1] / len, -fwd[2] / len, 0.0],
                intensity: f(&x["Intensity"], 1.0),
                radius: f(&x["Radius"], 100.0),
                static_color: v4(&x["Color"]),
                controller,
            });
        }
        // Mirrors reflect about PlanarReflection.PlaneTransform (its position nudged 0.001 down).
        out.mirror_plane = comps.iter().find(|c| c["type"] == "PlanarReflection").and_then(|c| {
            let m = c["fields"]["PlaneTransform"]["m"].as_array()?;
            let g = |k: usize| m.get(k).and_then(Value::as_f64).unwrap_or(0.0) as f32;
            let up = [g(1), g(5), g(9)];
            let len = (up[0] * up[0] + up[1] * up[1] + up[2] * up[2]).sqrt().max(1e-6);
            let n = up.map(|c| c / len);
            let p = [g(3) - n[0] * 0.001, g(7) - n[1] * 0.001, g(11) - n[2] * 0.001];
            Some([n[0], n[1], n[2], -(n[0] * p[0] + n[1] * p[1] + n[2] * p[2])])
        });
        let statics = crate::refrender::fog_lights(&bloomfog);
        out.fog = statics
            .into_iter()
            .zip(bloomfog.as_array().unwrap_or(&empty))
            .map(|(l, b)| (l, owned(b), b["path"].as_str().unwrap_or_default().to_owned()))
            .collect();
        let fog_tids: Vec<Vec<i64>> = bloomfog
            .as_array()
            .unwrap_or(&empty)
            .iter()
            .map(|b| b["tids"].as_array().into_iter().flatten().filter_map(Value::as_i64).collect())
            .collect();
        out.group_transforms = group_transforms(comps, env, &out.fog, &fog_tids);
        out.fx = read_fx(comps, env, &out, &fog_tids);
        for o in &env.objects {
            for w in o.tids.windows(2) {
                out.parent_of.insert(w[0], w[1]);
            }
        }
        for t in fog_tids.iter() {
            for w in t.windows(2) {
                out.parent_of.insert(w[0], w[1]);
            }
        }
        // Constrained transforms and their sources may have nothing drawn under them: their own chains.
        for c in comps.iter().filter(|c| c["type"].as_str().is_some_and(|t| t.ends_with("Constraint"))) {
            let chains = std::iter::once(&c["tids"]).chain(c["fields"]["sources"].as_array().into_iter().flatten().map(|s| &s["tids"]));
            for chain in chains {
                let ids: Vec<i64> = chain.as_array().into_iter().flatten().filter_map(Value::as_i64).collect();
                for w in ids.windows(2) {
                    out.parent_of.insert(w[0], w[1]);
                }
            }
        }
        for c in comps {
            let kind = match c["type"].as_str().unwrap_or_default() {
                "PositionConstraint" => ConstraintKind::Position,
                "RotationConstraint" => ConstraintKind::Rotation,
                "ParentConstraint" => ConstraintKind::Parent,
                _ => continue,
            };
            let (Some(tid), true) = (c["tid"].as_i64(), c["active"].as_bool().unwrap_or(false)) else { continue };
            let mat = |v: &Value| -> Option<[f32; 16]> {
                let a = v.as_array()?;
                (a.len() == 16).then(|| std::array::from_fn(|i| f(&a[i], 0.0)))
            };
            let Some(m0) = mat(&c["m"]) else { continue };
            let sources: Vec<(i64, f32, [f32; 16])> = c["fields"]["sources"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|s| Some((s["tid"].as_i64()?, f(&s["weight"], 1.0), mat(&s["m"])?)))
                .collect();
            if sources.is_empty() {
                continue;
            }
            out.constraints.push(Constraint {
                kind,
                tid,
                m0,
                weight: f(&c["fields"]["weight"], 1.0),
                sources,
                objects: env.objects.iter().enumerate().filter(|(_, o)| o.tids.contains(&tid)).map(|(i, _)| i).collect(),
                fogs: fog_tids.iter().enumerate().filter(|(_, t)| t.contains(&tid)).map(|(i, _)| i).collect(),
            });
        }
        out.read_specials(comps, env);
        // Rings share names, so each part belongs to the ring of its name nearest in depth.
        let mut ring_paths: HashMap<&str, Vec<(i64, f32)>> = HashMap::new();
        for c in comps.iter().filter(|c| c["type"] == "TrackLaneRing") {
            if let (Some(p), Some(id)) = (c["path"].as_str(), c["id"].as_i64()) {
                ring_paths.entry(p).or_default().push((id, f(&c["m"][11], 0.0)));
            }
        }
        let owner_ring = |path: &str, z: f32| -> Option<i64> {
            let rings = ring_paths.iter().find(|(p, _)| crate::motion::under(path, p))?.1;
            rings.iter().min_by(|a, b| (a.1 - z).abs().total_cmp(&(b.1 - z).abs())).map(|r| r.0)
        };
        for (i, o) in env.objects.iter().enumerate() {
            if let Some(id) = owner_ring(&o.path, o.matrix[11]) {
                out.ring_members.entry(id).or_default().0.push(i);
            }
        }
        for (i, (l, _, path)) in out.fog.iter().enumerate() {
            if let Some(id) = owner_ring(path, l.matrix[11]) {
                out.ring_members.entry(id).or_default().1.push(i);
            }
        }
        // Movable lights: one GameObject per name (several controllers can share an object).
        let same = |a: &[f32; 16], b: &[f32; 16]| (0..16).all(|i| (a[i] - b[i]).abs() < 1e-3);
        let controllers: Vec<(&str, Key, [f32; 3], [f32; 16])> = comps
            .iter()
            .filter(|c| c["type"] == "ParametricBloomFogLightController")
            .filter_map(|c| {
                let t = |k: usize| f(&c["m"][k], 0.0);
                let matrix: [f32; 16] = std::array::from_fn(t);
                Some((c["path"].as_str()?, key_for(c)?, [t(3), t(7), t(11)], matrix))
            })
            .collect();
        for (path, key, rest, m) in &controllers {
            if controllers.iter().any(|(p, _, _, o)| p == path && !same(o, m)) {
                out.unmovable.push(key.clone());
            } else if let Some(mover) = out.movers.iter_mut().find(|v| v.path == *path) {
                mover.keys.push(key.clone());
            } else {
                out.movers.push(Mover {
                    path: (*path).to_owned(),
                    keys: vec![key.clone()],
                    rest: *rest,
                    rot: rotation_of(m),
                    objects: env.objects.iter().enumerate().filter(|(_, o)| crate::motion::under(&o.path, path)).map(|(i, _)| i).collect(),
                    fogs: out.fog.iter().enumerate().filter(|(_, (_, _, p))| crate::motion::under(p, path)).map(|(i, _)| i).collect(),
                });
            }
        }
        out
    }

    /// The scene state for event colors from `light` ((lane, keys) → sRGB rgb + intensity).
    pub fn state(&self, light: &dyn Fn(&Key) -> [f32; 4]) -> SceneState {
        let mut s = SceneState { sky: self.sky.clone(), mirror_plane: self.mirror_plane, ..Default::default() };
        for b in &self.boxes {
            let c = b.key.as_ref().map_or(b.static_color, |k| light(k));
            let a = (c[3] * b.alpha_mul).max(b.min_alpha);
            s.blocks.insert(
                b.object,
                Block { color: Some(lin([c[0], c[1], c[2], a])), alpha_width: Some(b.alpha_width), ..Default::default() },
            );
            // Tubes off at export (DisableRenderersOnZeroAlpha) come back with light.
            if a > 0.01 {
                s.shown.insert(b.object);
            }
        }
        for g in &self.glows {
            let c = g.key.as_ref().map_or(g.static_color, |k| light(k));
            let a = (c[3] * g.alpha_mul).max(g.min_alpha);
            s.blocks.insert(
                g.object,
                Block { color: Some(lin([c[0], c[1], c[2], a])), size_params: Some(g.size_params), ..Default::default() },
            );
        }
        for sp in &self.sprites {
            let c = sp.key.as_ref().map_or(sp.static_color, |k| light(k));
            let a = (c[3] * sp.alpha_mul).max(sp.min_alpha);
            let b = s.blocks.entry(sp.object).or_default();
            b.color = Some(lin([c[0], c[1], c[2], a]));
            b.alpha_width = Some(sp.alpha_width);
            b.size_params = Some(sp.size_params);
            if a > 0.01 {
                s.shown.insert(sp.object);
            }
        }
        for m in &self.material_lights {
            let (v, shown) = m.value(m.key.as_ref().map_or(m.static_color, |k| light(k)));
            let v = lin(v);
            for &o in &m.objects {
                let b = s.blocks.entry(o).or_default();
                match m.prop {
                    Prop::Color => b.color = Some(v),
                    Prop::Emission => b.emission_color = Some(v),
                    Prop::EmissionTex => b.emission_tex_color = Some(v),
                    Prop::Other => {}
                }
                if !shown {
                    b.enabled = false;
                }
            }
        }
        for sl in &self.sprite_lights {
            let mut c = sl.key.as_ref().map_or(sl.static_color, |k| light(k));
            if let Some(r) = &sl.color_rule {
                if r.multiply == 1 {
                    (0..3).for_each(|k| c[k] *= c[3]);
                }
                if !r.set_color_only {
                    c[3] = c[3].max(r.min_alpha);
                }
                if r.multiply == 2 {
                    (0..3).for_each(|k| c[k] *= c[3]);
                }
                for (&o, &base) in sl.objects.iter().zip(&sl.initial) {
                    let a = if r.set_color_only { base[3] } else { c[3] };
                    let value = if r.set_alpha_only {
                        [base[0], base[1], base[2], a * r.intensity]
                    } else {
                        let rgb = [c[0], c[1], c[2]].map(|v| srgb_to_linear(v * r.intensity));
                        [rgb[0], rgb[1], rgb[2], a * r.intensity]
                    };
                    s.blocks.entry(o).or_default().renderer_color = Some(value);
                }
            }
            if let Some((lo, hi)) = sl.hide_range {
                let shown = c[3] >= lo && c[3] <= hi;
                for &o in &sl.objects {
                    if !shown {
                        s.blocks.entry(o).or_default().enabled = false;
                    } else {
                        s.shown.insert(o);
                    }
                }
            }
        }
        for ca in &self.color_arrays {
            let Some(mi) = ca.material else { continue };
            let mut arr = vec![[0.0f32; 4]; ca.len];
            for (i, key, fallback) in &ca.entries {
                if let Some(slot) = arr.get_mut(*i) {
                    *slot = lin(key.as_ref().map_or(*fallback, |k| light(k)));
                }
            }
            s.color_arrays.insert(mi, arr);
            for &(o, off) in &ca.offsets {
                s.blocks.entry(o).or_default().colors_offset = Some(off);
            }
        }
        s.base_boost = [1.0, 0.0];
        s.fog_lights = self
            .fog
            .iter()
            .map(|(l, key, _)| {
                let mut l = l.clone();
                if let Some(k) = key {
                    l.color = light(k);
                }
                l
            })
            .collect();
        let mut d = DirLights::default();
        for (i, def) in self.dir.iter().take(5).enumerate() {
            let c = def.controller.as_ref().map_or(def.static_color, |c| c.color(light));
            let c = c.map(|v| v * def.intensity);
            d.pos[i] = def.pos;
            d.dir[i] = def.dir;
            d.color[i] = lin(c);
            d.radius[i] = def.radius;
        }
        for i in self.dir.len()..5 {
            d.radius[i] = 100.0;
        }
        s.lights = d;
        s
    }

    /// The scene state with rotating lasers and lights moved (`motion` from the map's events).
    pub fn state_at(&self, env: &EnvFile, light: &dyn Fn(&Key) -> [f32; 4], motion: &crate::motion::Motion, seconds: f32) -> SceneState {
        use crate::motion::{apply, under};
        let mut s = self.state(light);
        s.time = seconds;
        let bps = motion.beats_per_second();
        if bps > 0.0 {
            self.apply_fires(&mut s, motion, seconds as f64 * bps, 1.0 / bps);
            self.apply_fx(env, &mut s, motion, seconds as f64 * bps);
        }
        for (pivot, delta) in motion.deltas(seconds) {
            for (i, o) in env.objects.iter().enumerate() {
                if under(&o.path, pivot) {
                    let base = s.moved.get(&i).copied().unwrap_or(o.matrix);
                    s.moved.insert(i, apply(&delta, &base));
                }
            }
            for (l, (_, _, path)) in s.fog_lights.iter_mut().zip(&self.fog) {
                if under(path, pivot) {
                    l.matrix = apply(&delta, &l.matrix);
                }
            }
            for (i, def) in self.dir.iter().take(5).enumerate() {
                if under(&def.path, pivot) {
                    let (p, d) = (s.lights.pos[i], s.lights.dir[i]);
                    let m = &delta;
                    s.lights.pos[i] = std::array::from_fn(|r| if r < 3 { m[r * 4] * p[0] + m[r * 4 + 1] * p[1] + m[r * 4 + 2] * p[2] + m[r * 4 + 3] } else { 0.0 });
                    let nd: [f32; 3] = std::array::from_fn(|r| m[r * 4] * d[0] + m[r * 4 + 1] * d[1] + m[r * 4 + 2] * d[2]);
                    let len = (nd[0] * nd[0] + nd[1] * nd[1] + nd[2] * nd[2]).sqrt().max(1e-6);
                    s.lights.dir[i] = [nd[0] / len, nd[1] / len, nd[2] / len, 0.0];
                }
            }
        }
        // Beats, for light groups: the song clock at this tempo.
        let beat = f64::from(seconds) * motion.beats_per_second();
        let mut applied: Vec<(Option<i64>, [f32; 16])> = Vec::new();
        for t in &self.group_transforms {
            let delta = if t.motion.limits.is_some() {
                let Some(v) = motion.groups.translation(&t.motion, beat) else { continue };
                crate::motion::local_translation_delta(&t.m0, &t.parent, t.motion.axis, v)
            } else {
                let Some(deg) = motion.groups.rotation(&t.motion, beat) else { continue };
                crate::motion::local_rotation_delta(&t.m0, &t.parent, t.motion.axis, deg)
            };
            applied.push((t.tid, delta));
            for &i in &t.objects {
                let base = s.moved.get(&i).copied().unwrap_or(env.objects[i].matrix);
                s.moved.insert(i, apply(&delta, &base));
            }
            for &i in &t.fogs {
                s.fog_lights[i].matrix = apply(&delta, &s.fog_lights[i].matrix);
            }
        }
        self.apply_constraints(env, &mut s, &mut applied);
        self.apply_specials(env, &mut s, motion, beat);
        for (ring, delta) in motion.ring_deltas(seconds) {
            let Some((objects, fogs)) = self.ring_members.get(&ring) else { continue };
            for &i in objects {
                let base = s.moved.get(&i).copied().unwrap_or(env.objects[i].matrix);
                s.moved.insert(i, apply(&delta, &base));
            }
            for &i in fogs {
                s.fog_lights[i].matrix = apply(&delta, &s.fog_lights[i].matrix);
            }
        }
        s
    }

    /// The objects showing the lights `pick` selects: their tubes, or their glow panels for lights
    /// without tubes (a glow's mesh is its whole halo).
    pub fn light_objects(&self, pick: &dyn Fn(&Key) -> bool) -> Vec<usize> {
        let tubes: Vec<usize> = self.boxes.iter().filter(|b| b.key.as_ref().is_some_and(|k| pick(k))).map(|b| b.object).collect();
        if !tubes.is_empty() {
            return tubes;
        }
        self.glows.iter().filter(|g| g.key.as_ref().is_some_and(|k| pick(k))).map(|g| g.object).collect()
    }

    fn read_specials(&mut self, comps: &[Value], env: &EnvFile) {
        use crate::motion::under;
        let types: HashMap<i64, i64> = comps
            .iter()
            .filter(|c| c["type"] == "BasicEventEffectManager")
            .flat_map(|c| c["fields"]["effectEntries"].as_array().into_iter().flatten())
            .filter_map(|e| Some((e["Manager"]["id"].as_i64()?, e["Type"].as_i64()?)))
            .collect();
        let fog = &self.fog;
        let part = |path: &str| Part {
            objects: env.objects.iter().enumerate().filter(|(_, o)| under(&o.path, path)).map(|(i, _)| i).collect(),
            fogs: fog.iter().enumerate().filter(|(_, (_, _, p))| under(p, path)).map(|(i, _)| i).collect(),
        };
        let parts = |list: &Value| -> Part {
            let mut all = Part::default();
            for g in list.as_array().into_iter().flatten() {
                if let Some(p) = g["gameObject"].as_str() {
                    let one = part(p);
                    all.objects.extend(one.objects);
                    all.fogs.extend(one.fogs);
                }
            }
            all
        };
        let matrix = |v: &Value| -> Option<[f32; 16]> {
            let a = v.as_array()?;
            (a.len() == 16).then(|| std::array::from_fn(|i| f(&a[i], 0.0)))
        };
        let (mut steps, mut switches, mut boosts) = (Vec::new(), Vec::new(), Vec::new());
        for c in comps {
            let x = &c["fields"];
            match c["type"].as_str().unwrap_or_default() {
                "SmoothStepPositionEventEffect" => {
                    let Some(ty) = c["id"].as_i64().and_then(|id| types.get(&id).copied()) else { continue };
                    let Some(path) = c["path"].as_str() else { continue };
                    let Some(parent) = matrix(&x["tr"]["pm"]) else { continue };
                    let v = |k: &str| -> [f32; 3] { std::array::from_fn(|i| f(&x[k][i], 0.0)) };
                    let clamp = (x["ClampValue"].as_bool() == Some(true)).then(|| (x["MinY"].as_i64().unwrap_or(0), x["MaxY"].as_i64().unwrap_or(0)));
                    steps.push(SmoothStep { ty, parent, init: v("initPos"), vector: v("MovementVector"), step: f(&x["StepSize"], 0.0), clamp, part: part(path) });
                }
                "GameObjectIntSwitch" => {
                    let Some(ty) = x["Effect"]["id"].as_i64().and_then(|id| types.get(&id).copied()) else { continue };
                    let containers = x["GameObjectsValueContainers"].as_array().into_iter().flatten().filter_map(|e| Some((e["Value"].as_i64()?, parts(&e["GameObjects"])))).collect();
                    switches.push(Switch { ty, default: x["DefaultValue"].as_i64().unwrap_or(0), containers });
                }
                "GameObjectSwitch" => boosts.push((vec![parts(&x["NormalGameObjects"])], vec![parts(&x["BoostGameObjects"])])),
                _ => {}
            }
        }
        (self.smooth_steps, self.switches, self.boost_switches) = (steps, switches, boosts);
    }

    fn apply_specials(&self, env: &EnvFile, s: &mut SceneState, motion: &crate::motion::Motion, beat: f64) {
        let show = |part: &Part, on: bool, s: &mut SceneState| {
            for &i in &part.objects {
                if on {
                    s.shown.insert(i);
                    s.hidden.remove(&i);
                } else {
                    s.hidden.insert(i);
                    s.shown.remove(&i);
                }
            }
            for &i in &part.fogs {
                s.fog_lights[i].enabled = on;
            }
        };
        for sw in &self.switches {
            // An event with a value no container has leaves the current one active.
            let known = |v: i64| sw.containers.iter().any(|(c, _)| *c == v);
            let events = motion.special_events(sw.ty);
            let upto = events.partition_point(|e| e.0 <= beat);
            let active = events[..upto].iter().rev().map(|e| e.1).find(|v| known(*v)).unwrap_or(sw.default);
            sw.containers.iter().filter(|(v, _)| *v != active).for_each(|(_, p)| show(p, false, s));
            sw.containers.iter().filter(|(v, _)| *v == active).for_each(|(_, p)| show(p, true, s));
        }
        if !self.boost_switches.is_empty() {
            let boost = motion.boosted(beat);
            for (normal, boosted) in &self.boost_switches {
                normal.iter().for_each(|p| show(p, !boost, s));
                boosted.iter().for_each(|p| show(p, boost, s));
            }
        }
        for step in &self.smooth_steps {
            let events = motion.special_events(step.ty);
            let position = |value: i64| -> [f32; 3] {
                let value = step.clamp.map_or(value, |(min, max)| value.clamp(min, max));
                std::array::from_fn(|i| step.init[i] + step.vector[i] * (step.step * (value - 4) as f32))
            };
            let i = events.partition_point(|e| e.0 <= beat);
            let current = if i == 0 { step.init } else { position(events[i - 1].1) };
            let local = match events.get(i) {
                None => current,
                Some(next) if i > 0 => {
                    let t = crate::eval::ease(Some("easeInOutCubic"), ((beat - events[i - 1].0) / (next.0 - events[i - 1].0)) as f32);
                    let to = position(next.1);
                    std::array::from_fn(|k| current[k] + (to[k] - current[k]) * t)
                }
                Some(_) => current,
            };
            let (dx, dy, dz) = (local[0] - step.init[0], local[1] - step.init[1], local[2] - step.init[2]);
            if dx == 0.0 && dy == 0.0 && dz == 0.0 {
                continue;
            }
            // The local offset through the parent's rotation, in world space.
            let p = &step.parent;
            let world = [p[0] * dx + p[1] * dy + p[2] * dz, p[4] * dx + p[5] * dy + p[6] * dz, p[8] * dx + p[9] * dy + p[10] * dz];
            let shift = |m: &mut [f32; 16]| {
                m[3] += world[0];
                m[7] += world[1];
                m[11] += world[2];
            };
            for &i in &step.part.objects {
                let mut m = s.moved.get(&i).copied().unwrap_or(env.objects[i].matrix);
                shift(&mut m);
                s.moved.insert(i, m);
            }
            for &i in &step.part.fogs {
                shift(&mut s.fog_lights[i].matrix);
            }
        }
    }

    /// What a Position line can move in the game (metres), and how many of its lights it can't.
    pub fn move_targets(&self, row: &Row, unit: f32) -> (Vec<MoveTarget>, usize) {
        let targets = self
            .movers
            .iter()
            .filter(|m| m.keys.iter().any(|k| row.reaches(k.0, &k.1)))
            .map(|m| MoveTarget { path: m.path.clone(), rest: m.rest.map(|v| v * unit), rot: m.rot })
            .collect();
        (targets, self.unmovable.iter().filter(|k| row.reaches(k.0, &k.1)).count())
    }

    /// How many of an Aim line's lights spin from laser-speed events between `from` and `to` seconds.
    pub fn spinning(&self, row: &Row, motion: &crate::motion::Motion, from: f32, to: f32) -> usize {
        let reached: Vec<&Mover> = self.movers.iter().filter(|m| m.keys.iter().any(|k| row.reaches(k.0, &k.1))).collect();
        let steps = ((to - from) * 4.0).ceil().clamp(1.0, 4000.0) as usize;
        let mut spun = std::collections::HashSet::new();
        for step in 0..=steps {
            let t = from + (to - from) * step as f32 / steps as f32;
            for (pivot, _) in motion.deltas(t) {
                spun.extend(reached.iter().filter(|m| crate::motion::under(&m.path, pivot)).map(|m| m.path.as_str()));
            }
        }
        spun.len()
    }

    /// Moves and turns lights by the map's Position and Aim lines at `beat`. The first line of each
    /// kind reaching a light wins. An aimed light ignores laser spin: it points where the keys say.
    pub fn apply_positions(&self, env: &EnvFile, s: &mut SceneState, rows: &[Row], beat: f64) {
        let lines: Vec<&Row> = rows.iter().filter(|r| r.is_motion() && !r.keys.is_empty()).collect();
        if lines.is_empty() {
            return;
        }
        for m in &self.movers {
            let line = |kind: RowKind| lines.iter().find(|r| r.kind == kind && m.keys.iter().any(|k| r.reaches(k.0, &k.1)));
            if let Some(q) = line(RowKind::Aim).and_then(|r| r.aim_at(beat)) {
                for &i in &m.objects {
                    s.moved.insert(i, turned(&env.objects[i].matrix, q, m.rest));
                }
                for &i in &m.fogs {
                    s.fog_lights[i].matrix = turned(&self.fog[i].0.matrix, q, m.rest);
                }
            }
            if let Some(r) = line(RowKind::Position) {
                let off = r.position_at(beat).map(|v| v / env.unit);
                let shift = |mat: &mut [f32; 16]| {
                    mat[3] += off[0];
                    mat[7] += off[1];
                    mat[11] += off[2];
                };
                for &i in &m.objects {
                    let mut mat = s.moved.get(&i).copied().unwrap_or(env.objects[i].matrix);
                    shift(&mut mat);
                    s.moved.insert(i, mat);
                }
                for &i in &m.fogs {
                    shift(&mut s.fog_lights[i].matrix);
                }
            }
        }
    }

    /// Fire effects at `beat` (seconds per beat `spb`): flipbook start times, fades and visibility.
    fn apply_fires(&self, s: &mut SceneState, motion: &crate::motion::Motion, beat: f64, spb: f64) {
        for fire in &self.fires {
            let Some((_, Some((0, group)))) = fire.key.as_ref().map(|k| (k, crate::map::group_of(k.0))) else { continue };
            let element = fire.key.as_ref().and_then(|k| k.1.first().copied()).unwrap_or(0);
            let event = motion.groups.color_event(group, element, beat);
            // (flipbook start beat, flipbook alpha, bloom alpha) while the effect shows.
            let shown = event.and_then(|(start, end, brightness)| {
                let t = beat as f32;
                let start = start as f32;
                if fire.burst {
                    if t - start > fire.fade_out || t < start {
                        return None;
                    }
                    let progress = if brightness > 0.0 {
                        0.0
                    } else {
                        let d = fire.fade_out.max(1e-6);
                        ((t - start - start) / d).clamp(0.0, 1.0)
                    };
                    Some((start, eval_curve(&fire.flip_curve, progress), eval_curve(&fire.bloom_curve, progress)))
                } else {
                    if brightness <= 0.0 {
                        return None;
                    }
                    let end = end.min(f32::MAX as f64) as f32;
                    let fade_in_end = (start + fire.fade_in).min(end);
                    let fade_out_start = (end - fire.fade_out).max(start);
                    let inv = |a: f32, b: f32, v: f32| if b > a { ((v - a) / (b - a)).clamp(0.0, 1.0) } else { 1.0 };
                    let a = inv(start, fade_in_end, t);
                    let b = 1.0 - inv(fade_out_start, end, t);
                    let quad_out = 1.0 - (1.0 - a) * (1.0 - a);
                    let quad_in_out = if b < 0.5 { 2.0 * b * b } else { 1.0 - (-2.0 * b + 2.0).powi(2) / 2.0 };
                    let k = quad_out * quad_in_out;
                    let x = (t - start) / fire.sustain.max(1e-6);
                    Some((start, k * eval_curve(&fire.flip_curve, x), k * eval_curve(&fire.bloom_curve, x)))
                }
            });
            let all = fire.flip_objects.iter().chain(&fire.prepass);
            match shown {
                None => {
                    for &o in all {
                        s.blocks.entry(o).or_default().enabled = false;
                    }
                }
                Some((start, flip, bloom)) => {
                    let start_seconds = (start as f64 * spb) as f32;
                    for &o in &fire.flip_objects {
                        let b = s.blocks.entry(o).or_default();
                        b.start_time = Some(start_seconds);
                        b.color = Some([1.0, 1.0, 1.0, flip]);
                        s.shown.insert(o);
                    }
                    for &o in &fire.bloom_objects {
                        s.blocks.entry(o).or_default().color = Some([1.0, 1.0, 1.0, bloom * fire.bloom_mul]);
                        s.shown.insert(o);
                    }
                    if fire.use_emission {
                        for &o in &fire.emission_objects {
                            s.blocks.entry(o).or_default().emission_tex_color = Some(fire.point_color.map(|v| v * bloom));
                        }
                    }
                    s.shown.extend(fire.prepass.iter().copied());
                }
            }
        }
    }

    /// A transform's move so far (the group moves applied to it or an ancestor, in order).
    fn transform_delta(&self, tid: i64, applied: &[(Option<i64>, [f32; 16])]) -> [f32; 16] {
        let mut chain = HashSet::from([tid]);
        let mut t = tid;
        while let Some(&p) = self.parent_of.get(&t) {
            if !chain.insert(p) {
                break;
            }
            t = p;
        }
        let mut d: [f32; 16] = std::array::from_fn(|i| if i % 5 == 0 { 1.0 } else { 0.0 });
        for (id, delta) in applied {
            if id.is_some_and(|id| chain.contains(&id)) {
                d = crate::motion::mul(delta, &d);
            }
        }
        d
    }

    /// Constraints after the group moves: each constrained transform is put where its sources now
    /// say, and that correction moves what's under it.
    fn apply_constraints(&self, env: &EnvFile, s: &mut SceneState, applied: &mut Vec<(Option<i64>, [f32; 16])>) {
        use crate::motion::{apply, invert, mul};
        for c in &self.constraints {
            let current = mul(&self.transform_delta(c.tid, applied), &c.m0);
            let total: f32 = c.sources.iter().map(|s| s.1).sum();
            if total <= 0.0 || c.weight <= 0.0 {
                continue;
            }
            // Where the sources take it: each source's move since export, weighted.
            let moved_by = |src: &(i64, f32, [f32; 16])| -> [f32; 16] {
                let now = mul(&self.transform_delta(src.0, applied), &src.2);
                mul(&now, &invert(&src.2))
            };
            let target = match c.kind {
                ConstraintKind::Position => {
                    // The weighted source position plus the exported offset (not turned with the sources).
                    let mut p = [c.m0[3], c.m0[7], c.m0[11]];
                    for src in &c.sources {
                        let now = mul(&self.transform_delta(src.0, applied), &src.2);
                        for (k, at) in [3, 7, 11].into_iter().enumerate() {
                            p[k] += (now[at] - src.2[at]) * src.1 / total;
                        }
                    }
                    let mut m = current;
                    (m[3], m[7], m[11]) = (p[0], p[1], p[2]);
                    m
                }
                ConstraintKind::Rotation | ConstraintKind::Parent => {
                    let d = moved_by(&c.sources[0]);
                    let mut m = mul(&d, &c.m0);
                    if c.kind == ConstraintKind::Rotation {
                        (m[3], m[7], m[11]) = (current[3], current[7], current[11]);
                    }
                    m
                }
            };
            let mut target = target;
            if c.weight < 1.0 {
                (0..16).for_each(|i| target[i] = current[i] + (target[i] - current[i]) * c.weight);
            }
            let correction = mul(&target, &invert(&current));
            applied.push((Some(c.tid), correction));
            for &i in &c.objects {
                let base = s.moved.get(&i).copied().unwrap_or(env.objects[i].matrix);
                s.moved.insert(i, apply(&correction, &base));
            }
            for &i in &c.fogs {
                s.fog_lights[i].matrix = apply(&correction, &s.fog_lights[i].matrix);
            }
        }
    }

    /// VFX group values on their targets (value 0 is the exported state).
    fn apply_fx(&self, env: &EnvFile, s: &mut SceneState, motion: &crate::motion::Motion, beat: f64) {
        for e in &self.fx {
            let Some(value) = motion.groups.vfx(e.group, e.element, beat) else { continue };
            for t in &e.targets {
                match t {
                    FxTarget::Move { per_value, objects, fogs } => {
                        if value == 0.0 {
                            continue;
                        }
                        let d = per_value.map(|v| v * value);
                        for &o in objects {
                            let m = s.moved.entry(o).or_insert(env.objects[o].matrix);
                            (m[3], m[7], m[11]) = (m[3] + d[0], m[7] + d[1], m[11] + d[2]);
                        }
                        for &i in fogs {
                            if let Some(l) = s.fog_lights.get_mut(i) {
                                (l.matrix[3], l.matrix[7], l.matrix[11]) = (l.matrix[3] + d[0], l.matrix[7] + d[1], l.matrix[11] + d[2]);
                            }
                        }
                    }
                    FxTarget::Switch { stages } => {
                        let active = (0..stages.len()).rev().find(|&i| value >= stages[i].0);
                        for (i, (_, part)) in stages.iter().enumerate() {
                            for &o in &part.objects {
                                if Some(i) == active {
                                    s.hidden.remove(&o);
                                    s.shown.insert(o);
                                } else {
                                    s.shown.remove(&o);
                                    s.hidden.insert(o);
                                }
                            }
                            for &fi in &part.fogs {
                                if let Some(l) = s.fog_lights.get_mut(fi) {
                                    l.enabled = Some(i) == active;
                                }
                            }
                        }
                    }
                    FxTarget::Scale { m0, bounds, objects } => {
                        let k = value.clamp(bounds[0], bounds[1]) / bounds[0].clamp(bounds[0], bounds[1]).max(1e-6);
                        if (k - 1.0).abs() < 1e-6 {
                            continue;
                        }
                        let p = [m0[3], m0[7], m0[11]];
                        for &o in objects {
                            let m = s.moved.entry(o).or_insert(env.objects[o].matrix);
                            for r in 0..3 {
                                for c in 0..3 {
                                    m[r * 4 + c] *= k;
                                }
                                m[r * 4 + 3] = p[r] + (m[r * 4 + 3] - p[r]) * k;
                            }
                        }
                    }
                    FxTarget::EndWidth { sprite, mul, bounds } => {
                        let sp = &self.sprites[*sprite];
                        if let Some(aw) = s.blocks.entry(sp.object).or_default().alpha_width.as_mut() {
                            aw[3] = (value * mul).clamp(bounds[0], bounds[1]);
                        }
                    }
                    FxTarget::Alpha { color, prop, objects } => {
                        let c = lin([color[0], color[1], color[2], value]);
                        for &o in objects {
                            let b = s.blocks.entry(o).or_default();
                            match prop {
                                Prop::Color => b.color = Some(c),
                                Prop::Emission => b.emission_color = Some(c),
                                Prop::EmissionTex => b.emission_tex_color = Some(c),
                                Prop::Other => {}
                            }
                        }
                    }
                }
            }
        }
    }

    /// The material a ColorArrayLightsController (by instance ID) writes.
    pub fn color_array_material(&self, id: i64) -> Option<usize> {
        self.color_arrays.iter().find(|c| c.id == id).and_then(|c| c.material)
    }

    pub fn mirror_plane(&self) -> Option<[f32; 4]> {
        self.mirror_plane
    }

    /// The exported components (for building the map's motion).
    pub fn components(dir: &Path) -> Vec<Value> {
        std::fs::read_to_string(dir.join("components.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
    }
}
