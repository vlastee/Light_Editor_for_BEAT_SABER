//! Custom environment geometry from the map (Chroma `environment` + `materials`) and the
//! Heck `AnimateTrack` events that move it. Base-game environment objects aren't in the map,
//! so edits that target them are only counted, not shown.

use crate::eval;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug)]
pub struct Quat {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub w: f32,
}

impl Quat {
    fn axis(axis: [f32; 3], deg: f32) -> Quat {
        let h = deg.to_radians() / 2.0;
        let s = h.sin();
        Quat { x: axis[0] * s, y: axis[1] * s, z: axis[2] * s, w: h.cos() }
    }

    /// Unity's `Quaternion.Euler`: rotate around Z, then X, then Y.
    pub fn euler(e: [f32; 3]) -> Quat {
        Quat::axis([0.0, 1.0, 0.0], e[1])
            .mul(Quat::axis([1.0, 0.0, 0.0], e[0]))
            .mul(Quat::axis([0.0, 0.0, 1.0], e[2]))
    }

    pub fn mul(self, b: Quat) -> Quat {
        let a = self;
        Quat {
            w: a.w * b.w - a.x * b.x - a.y * b.y - a.z * b.z,
            x: a.w * b.x + a.x * b.w + a.y * b.z - a.z * b.y,
            y: a.w * b.y - a.x * b.z + a.y * b.w + a.z * b.x,
            z: a.w * b.z + a.x * b.y - a.y * b.x + a.z * b.w,
        }
    }

    pub fn rotate(self, v: [f32; 3]) -> [f32; 3] {
        let q = [self.x, self.y, self.z];
        let cross = |a: [f32; 3], b: [f32; 3]| {
            [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
        };
        let t = cross(q, v).map(|c| c * 2.0);
        let u = cross(q, t);
        [
            v[0] + self.w * t[0] + u[0],
            v[1] + self.w * t[1] + u[1],
            v[2] + self.w * t[2] + u[2],
        ]
    }

    /// Like Unity's `Quaternion.Slerp`: always the short way around.
    pub fn slerp(self, mut b: Quat, t: f32) -> Quat {
        let mut dot = self.x * b.x + self.y * b.y + self.z * b.z + self.w * b.w;
        if dot < 0.0 {
            b = Quat { x: -b.x, y: -b.y, z: -b.z, w: -b.w };
            dot = -dot;
        }
        let (wa, wb) = if dot > 0.9995 {
            (1.0 - t, t)
        } else {
            let th = dot.clamp(-1.0, 1.0).acos();
            let s = th.sin();
            (((1.0 - t) * th).sin() / s, (t * th).sin() / s)
        };
        let q = Quat {
            x: self.x * wa + b.x * wb,
            y: self.y * wa + b.y * wb,
            z: self.z * wa + b.z * wb,
            w: self.w * wa + b.w * wb,
        };
        let n = (q.x * q.x + q.y * q.y + q.z * q.z + q.w * q.w).sqrt().max(1e-9);
        Quat { x: q.x / n, y: q.y / n, z: q.z / n, w: q.w / n }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Prim {
    Cube,
    Sphere,
    Cylinder,
    Plane,
    Quad,
    Triangle,
}

#[derive(Clone, Debug)]
pub struct Obj {
    pub prim: Prim,
    pub pos: [f32; 3],
    pub rot: Quat,
    pub scale: [f32; 3],
    pub tracks: Vec<String>,
    /// (lane type, light ID). Lane -1 means "unknown lane" (v2 geometry lights).
    pub light: Option<(i64, Option<i64>)>,
    pub color: [f32; 4],
    pub emissive: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Prop {
    Position,
    Rotation,
    Scale,
}

#[derive(Clone, Debug)]
struct Pt {
    v: [f32; 3],
    t: f32,
    ease: Option<String>,
}

#[derive(Clone, Debug)]
struct Anim {
    start: f64,
    dur: f64,
    easing: Option<String>,
    repeat: u32,
    /// `None` resets the property to the object's own value.
    points: Option<Vec<Pt>>,
}

#[derive(Default)]
pub struct Scene {
    pub objects: Vec<Obj>,
    anims: HashMap<(String, Prop), Vec<Anim>>,
    pub base_edits: usize,
    pub hides_base: bool,
    pub animated_tracks: usize,
    pub ignored_events: HashMap<String, usize>,
}

fn f(v: &Value) -> Option<f32> {
    v.as_f64().map(|x| x as f32)
}

fn vec3(v: Option<&Value>, scale: f32) -> Option<[f32; 3]> {
    let a = v?.as_array()?;
    Some([
        f(a.first()?)? * scale,
        f(a.get(1)?)? * scale,
        f(a.get(2)?)? * scale,
    ])
}

/// Parses a Heck point list: `[[x,y,z,t,"easeX"],…]`, a single `[x,y,z]`, or a named point
/// definition. Spline and modifier entries are ignored (linear between points).
fn points(v: &Value, defs: &HashMap<String, Value>, scale: f32, depth: u8) -> Option<Vec<Pt>> {
    match v {
        Value::String(name) if depth < 4 => points(defs.get(name)?, defs, scale, depth + 1),
        Value::Array(a) if a.first().is_some_and(Value::is_number) => {
            // Single point, possibly with a time.
            let nums: Vec<f32> = a.iter().filter_map(f).collect();
            (nums.len() >= 3).then(|| {
                vec![Pt {
                    v: [nums[0] * scale, nums[1] * scale, nums[2] * scale],
                    t: 0.0,
                    ease: None,
                }]
            })
        }
        Value::Array(a) => {
            let mut out = Vec::new();
            for p in a {
                let Some(p) = p.as_array() else { continue };
                let nums: Vec<f32> = p.iter().filter_map(f).collect();
                if nums.len() < 3 {
                    continue;
                }
                let ease = p
                    .iter()
                    .filter_map(Value::as_str)
                    .find(|s| s.starts_with("ease"))
                    .map(str::to_owned);
                out.push(Pt {
                    v: [nums[0] * scale, nums[1] * scale, nums[2] * scale],
                    t: nums.get(3).copied().unwrap_or(0.0),
                    ease,
                });
            }
            out.sort_by(|a, b| a.t.total_cmp(&b.t));
            (!out.is_empty()).then_some(out)
        }
        _ => None,
    }
}

fn prim(name: &str) -> Option<Prim> {
    Some(match name {
        "Cube" => Prim::Cube,
        "Sphere" => Prim::Sphere,
        "Cylinder" | "Capsule" => Prim::Cylinder,
        "Plane" => Prim::Plane,
        "Quad" => Prim::Quad,
        "Triangle" => Prim::Triangle,
        _ => return None,
    })
}

fn tracks_of(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a.iter().filter_map(|t| t.as_str().map(str::to_owned)).collect(),
        _ => Vec::new(),
    }
}

impl Scene {
    pub fn parse(json: &Value) -> Scene {
        let v3 = json.get("basicBeatmapEvents").is_some() || json.get("customData").is_some();
        let cd = if v3 { &json["customData"] } else { &json["_customData"] };
        let k = |v3name: &'static str, v2name: &'static str| if v3 { v3name } else { v2name };
        // v2 Chroma positions are in lane units (0.6 m).
        let pos_scale = if v3 { 1.0 } else { 0.6 };

        let mut defs: HashMap<String, Value> = HashMap::new();
        match &cd[k("pointDefinitions", "_pointDefinitions")] {
            Value::Object(o) => defs.extend(o.iter().map(|(n, v)| (n.clone(), v.clone()))),
            Value::Array(a) => {
                for d in a {
                    if let (Some(n), Some(p)) = (d["_name"].as_str(), d.get("_points")) {
                        defs.insert(n.to_owned(), p.clone());
                    }
                }
            }
            _ => {}
        }

        let materials = cd[k("materials", "_materials")].as_object().cloned().unwrap_or_default();
        let material_look = |m: &Value| -> ([f32; 4], bool) {
            let m = match m {
                Value::String(name) => materials.get(name).unwrap_or(&Value::Null),
                other => other,
            };
            let c = m[k("color", "_color")].as_array().map_or([1.0; 4], |a| {
                let ch = |i: usize, d: f32| a.get(i).and_then(f).unwrap_or(d);
                [ch(0, 1.0), ch(1, 1.0), ch(2, 1.0), ch(3, 1.0)]
            });
            let shader = m[k("shader", "_shader")].as_str().unwrap_or("Standard");
            (c, shader.contains("Light"))
        };

        let mut scene = Scene::default();
        for e in cd[k("environment", "_environment")].as_array().into_iter().flatten() {
            let geo = &e[k("geometry", "_geometry")];
            if !geo.is_object() {
                // The editor's own Position lines are previewed.
                if tracks_of(e.get(k("track", "_track"))).iter().any(|t| t.starts_with(crate::map::TRACK_PREFIX)) {
                    continue;
                }
                scene.base_edits += 1;
                if e[k("active", "_active")] == Value::Bool(false) {
                    scene.hides_base = true;
                }
                continue;
            }
            let Some(p) = geo[k("type", "_type")].as_str().and_then(prim) else {
                continue;
            };
            let (color, emissive) = material_look(&geo[k("material", "_material")]);
            let light = if v3 {
                let c = &e["components"]["ILightWithId"];
                c.is_object().then(|| {
                    (c["type"].as_i64().unwrap_or(0), c["lightID"].as_i64())
                })
            } else {
                e["_lightID"].as_i64().map(|id| (-1, Some(id)))
            };
            let pos = vec3(e.get(k("localPosition", "_localPosition")), pos_scale)
                .or_else(|| vec3(e.get(k("position", "_position")), pos_scale))
                .unwrap_or([0.0; 3]);
            let rot = vec3(e.get(k("localRotation", "_localRotation")), 1.0)
                .or_else(|| vec3(e.get(k("rotation", "_rotation")), 1.0))
                .unwrap_or([0.0; 3]);
            scene.objects.push(Obj {
                prim: p,
                pos,
                rot: Quat::euler(rot),
                scale: vec3(e.get(k("scale", "_scale")), 1.0).unwrap_or([1.0; 3]),
                tracks: tracks_of(e.get(k("track", "_track"))),
                emissive: emissive || light.is_some(),
                light,
                color,
            });
        }

        let used: HashSet<&str> = scene
            .objects
            .iter()
            .flat_map(|o| o.tracks.iter().map(String::as_str))
            .collect();
        let mut animated = HashSet::new();
        for ev in cd[k("customEvents", "_customEvents")].as_array().into_iter().flatten() {
            let ty = ev[k("t", "_type")].as_str().unwrap_or("");
            if ty != "AnimateTrack" {
                *scene.ignored_events.entry(ty.to_owned()).or_default() += 1;
                continue;
            }
            let d = &ev[k("d", "_data")];
            let start = ev[k("b", "_time")].as_f64().unwrap_or(0.0);
            let dur = d[k("duration", "_duration")].as_f64().unwrap_or(0.0);
            let easing = d[k("easing", "_easing")].as_str().map(str::to_owned);
            let repeat = d[k("repeat", "_repeat")].as_u64().unwrap_or(0) as u32;
            for track in tracks_of(d.get(k("track", "_track"))) {
                if !used.contains(track.as_str()) {
                    continue;
                }
                for (names, prop, scale) in [
                    ([k("localPosition", "_localPosition"), k("position", "_position")], Prop::Position, pos_scale),
                    ([k("localRotation", "_localRotation"), k("rotation", "_rotation")], Prop::Rotation, 1.0),
                    ([k("scale", "_scale"), k("scale", "_scale")], Prop::Scale, 1.0),
                ] {
                    let Some(v) = names.iter().find_map(|n| d.get(*n)) else { continue };
                    scene.anims.entry((track.clone(), prop)).or_default().push(Anim {
                        start,
                        dur,
                        easing: easing.clone(),
                        repeat,
                        points: points(v, &defs, scale, 0),
                    });
                    animated.insert(track.clone());
                }
            }
        }
        for list in scene.anims.values_mut() {
            list.sort_by(|a, b| a.start.total_cmp(&b.start));
        }
        scene.animated_tracks = animated.len();
        scene
    }

    /// The animated value of a property at `beat`, if any animation has set it.
    fn anim(&self, track: &str, prop: Prop, beat: f64) -> Option<(Vec<Pt>, f32)> {
        let list = self.anims.get(&(track.to_owned(), prop))?;
        let i = list.partition_point(|a| a.start <= beat);
        let a = list.get(i.checked_sub(1)?)?;
        let pts = a.points.clone()?;
        let mut t = if a.dur <= 0.0 { 1.0 } else { (beat - a.start) / a.dur };
        if a.repeat > 0 && t < f64::from(a.repeat + 1) {
            t = t.fract();
        }
        let t = eval::ease(a.easing.as_deref(), t.clamp(0.0, 1.0) as f32);
        Some((pts, t))
    }

    fn sample<T>(pts: &[Pt], t: f32, conv: impl Fn([f32; 3]) -> T, mix: impl Fn(T, T, f32) -> T) -> T {
        let j = pts.partition_point(|p| p.t <= t);
        if j == 0 {
            return conv(pts[0].v);
        }
        if j >= pts.len() {
            return conv(pts[pts.len() - 1].v);
        }
        let (a, b) = (&pts[j - 1], &pts[j]);
        let span = (b.t - a.t).max(1e-6);
        let local = eval::ease(b.ease.as_deref(), (t - a.t) / span);
        mix(conv(a.v), conv(b.v), local)
    }

    /// World transform of an object at `beat`: (position, rotation, scale).
    pub fn transform(&self, o: &Obj, beat: f64) -> ([f32; 3], Quat, [f32; 3]) {
        let lerp = |a: [f32; 3], b: [f32; 3], t: f32| std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t);
        let mut pos = o.pos;
        let mut rot = o.rot;
        let mut scale = o.scale;
        for track in &o.tracks {
            if let Some((p, t)) = self.anim(track, Prop::Position, beat) {
                pos = Self::sample(&p, t, |v| v, lerp);
            }
            if let Some((p, t)) = self.anim(track, Prop::Rotation, beat) {
                rot = Self::sample(&p, t, Quat::euler, |a: Quat, b, t| a.slerp(b, t));
            }
            if let Some((p, t)) = self.anim(track, Prop::Scale, beat) {
                scale = Self::sample(&p, t, |v| v, lerp);
            }
        }
        (pos, rot, scale)
    }
}
