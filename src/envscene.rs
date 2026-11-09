//! Drives the GPU renderer from the light model: what ChroMapper's light components do with an event
//! color (sRGB rgb, alpha = intensity) before it reaches the shaders. Uses the environment's exported
//! component settings (components.json) and fog lights (bloomfog.json).

use crate::environment::EnvFile;
use crate::gpu3d::{srgb_to_linear, Block, DirLights, FogLight, SceneState, SkyGradient};
use crate::map::{MoveTarget, Row, RowKind};
use crate::scene::Quat;
use serde_json::Value;
use std::collections::HashMap;
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
    movers: Vec<Mover>,
    /// Lights whose GameObject name isn't unique, so Chroma can't address them alone (rings).
    unmovable: Vec<Key>,
    boxes: Vec<BoxLight>,
    glows: Vec<FakeGlow>,
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
        let by_path: HashMap<&str, usize> = env.objects.iter().enumerate().map(|(i, o)| (o.path.as_str(), i)).collect();
        // The light (lane, map IDs) of the ParametricBloomFogLightController that owns each tube, glow
        // sprite and fog object (an object can hold controllers for several lanes).
        let mut owner: HashMap<i64, Key> = HashMap::new();
        for c in comps.iter().filter(|c| c["type"] == "ParametricBloomFogLightController") {
            let Some(k) = key_of(&c["light"]) else { continue };
            for part in ["BoxLight", "SpriteLight", "BloomFog"] {
                if let Some(id) = c["fields"][part]["id"].as_i64() {
                    owner.insert(id, k.clone());
                }
            }
        }
        let owned = |c: &Value| c["id"].as_i64().and_then(|id| owner.get(&id).cloned()).or_else(|| key_of(&c["light"]));
        let mut out = EnvLights { sky: crate::refrender::sky_gradient(&Value::Array(comps.clone())), ..Default::default() };
        for c in comps {
            let x = &c["fields"];
            let path = c["path"].as_str().unwrap_or_default();
            match c["type"].as_str().unwrap_or_default() {
                "ParametricBoxLight" => {
                    let rpath = x["Renderer"]["path"].as_str().unwrap_or(path);
                    let Some(&object) = by_path.get(rpath) else { continue };
                    out.boxes.push(BoxLight {
                        object,
                        key: owned(c),
                        static_color: v4(&x["color"]),
                        alpha_mul: f(&x["AlphaMultiplier"], 1.0),
                        min_alpha: f(&x["MinAlpha"], 0.0),
                        alpha_width: [f(&x["AlphaStart"], 1.0), f(&x["AlphaEnd"], 1.0), f(&x["WidthStart"], 1.0), f(&x["WidthEnd"], 1.0)],
                    });
                }
                "RectangleFakeGlowLightController" => {
                    let Some(&object) = by_path.get(path) else { continue };
                    let size = v4(&x["Size"]);
                    let edge = f(&x["EdgeSize"], 0.0);
                    out.glows.push(FakeGlow {
                        object,
                        key: key_of(&c["light"]),
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
                    .map(|d| (key_of(&d["light"]), v4(&d["fields"]["Color"]), f(&d["fields"]["Intensity"], 1.0)))
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
        // Movable lights: one GameObject per name (several controllers can share an object).
        let same = |a: &[f32; 16], b: &[f32; 16]| (0..16).all(|i| (a[i] - b[i]).abs() < 1e-3);
        let controllers: Vec<(&str, Key, [f32; 3], [f32; 16])> = comps
            .iter()
            .filter(|c| c["type"] == "ParametricBloomFogLightController")
            .filter_map(|c| {
                let t = |k: usize| f(&c["m"][k], 0.0);
                let matrix: [f32; 16] = std::array::from_fn(t);
                Some((c["path"].as_str()?, key_of(&c["light"])?, [t(3), t(7), t(11)], matrix))
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
        }
        for g in &self.glows {
            let c = g.key.as_ref().map_or(g.static_color, |k| light(k));
            let a = (c[3] * g.alpha_mul).max(g.min_alpha);
            s.blocks.insert(
                g.object,
                Block { color: Some(lin([c[0], c[1], c[2], a])), size_params: Some(g.size_params), ..Default::default() },
            );
        }
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
        let lines: Vec<&Row> = rows.iter().filter(|r| !r.is_light() && !r.keys.is_empty()).collect();
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
