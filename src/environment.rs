//! Base environments exported from ChroMapper (tools/export-chromapper-envs.sh): world-space
//! meshes, lights on a (lane, map light ID), each lane's light-switch settings and the colors.

use serde_json::Value;
use std::path::PathBuf;

pub struct EnvObject {
    /// The light's lane and the map light IDs that reach it (empty: only whole-lane events).
    pub light: Option<(i64, Vec<i64>)>,
    pub color: [f32; 4],
    /// World-space triangles in metres, 9 floats each.
    pub tris: Vec<f32>,
    /// For long thin lights: the segment along their length, drawn as a glowing line.
    pub beam: Option<([f32; 3], [f32; 3])>,
    /// Version 2 exports: the shared mesh, its materials (one per submesh, -1: none) and the
    /// mesh-to-world matrix (Unity's, row-major, ChroMapper units: scale positions by `EnvFile::unit`).
    pub mesh: Option<usize>,
    pub materials: Vec<i64>,
    pub matrix: [f32; 16],
    pub enabled: bool,
    /// Instance IDs of the object's transform and its ancestors (tells apart same-named objects).
    pub tids: Vec<i64>,
    /// A SpriteRenderer's sprite texture (its `_MainTex`), by texture key.
    pub sprite_tex: Option<String>,
    /// A SpriteRenderer's color at export (linear rgb, as sprite lights set it).
    pub sprite_color: Option<[f32; 4]>,
    /// Unity layer (decides what mirrors reflect), when exported.
    pub layer: Option<u32>,
    pub path: String,
}

/// A mesh in its own space (ChroMapper units).
pub struct EnvMesh {
    pub name: String,
    pub pos: Vec<[f32; 3]>,
    pub normal: Vec<[f32; 3]>,
    pub uv: Vec<[f32; 2]>,
    /// The second UV set (color array indices), when exported.
    pub uv2: Vec<[f32; 2]>,
    pub color: Vec<[f32; 4]>,
    pub idx: Vec<u32>,
}

pub struct EnvMaterial {
    pub name: String,
    pub shader: String,
    pub queue: i64,
    pub keywords: Vec<String>,
    /// Properties declared as colors (sRGB in the material, linear in the shader).
    pub colors: Vec<String>,
    pub props: serde_json::Map<String, Value>,
}

impl EnvMaterial {
    pub fn has(&self, keyword: &str) -> bool {
        self.keywords.iter().any(|k| k == keyword)
    }
    pub fn float(&self, name: &str, default: f32) -> f32 {
        self.props.get(name).and_then(Value::as_f64).map_or(default, |v| v as f32)
    }
    pub fn vec4(&self, name: &str, default: [f32; 4]) -> [f32; 4] {
        match self.props.get(name).and_then(Value::as_array) {
            Some(a) => std::array::from_fn(|i| a.get(i).and_then(Value::as_f64).map_or(default[i], |v| v as f32)),
            None => default,
        }
    }
}

/// A particle as ChroMapper's renderer draws it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Particle {
    pub pos: [f32; 3],
    pub size: [f32; 3],
    /// Rotation in degrees (Unity euler).
    pub rot: [f32; 3],
    pub color: [f32; 4],
}

impl Particle {
    /// From the probe's `[x, y, z, sx, sy, sz, rx, ry, rz, r, g, b, a, …]`.
    pub fn from_json(v: &Value) -> Option<Particle> {
        let a = v.as_array()?;
        let g = |i: usize| a.get(i).and_then(Value::as_f64).unwrap_or(0.0) as f32;
        (a.len() >= 13).then(|| Particle {
            pos: [g(0), g(1), g(2)],
            size: [g(3), g(4), g(5)],
            rot: [g(6), g(7), g(8)],
            color: [g(9), g(10), g(11), g(12)],
        })
    }
}

pub struct EnvParticleSystem {
    pub materials: Vec<i64>,
    pub matrix: [f32; 16],
    /// Positions are in the system's space (true) or the world.
    pub local: bool,
    /// Drawn as `mesh` per particle (else camera-facing quads).
    pub mesh: Option<usize>,
    pub enabled: bool,
    pub particles: Vec<Particle>,
}

/// The environment's BloomFog settings.
#[derive(Clone, Copy, Debug)]
pub struct FogParams {
    pub offset: f32,
    pub height: f32,
    pub start_y: f32,
    pub attenuation: f32,
    pub auto_exposure_limit: f32,
    pub legacy_auto_exposure: bool,
}

/// Applies Unity's row-major matrix to a point.
pub fn transform_point(m: &[f32; 16], p: [f32; 3]) -> [f32; 3] {
    std::array::from_fn(|r| m[r * 4] * p[0] + m[r * 4 + 1] * p[1] + m[r * 4 + 2] * p[2] + m[r * 4 + 3])
}

/// The long axis of a mesh when it is at least 6× longer than it is thick.
fn beam_of(tris: &[f32]) -> Option<([f32; 3], [f32; 3])> {
    let mut verts: Vec<[f32; 3]> = tris.chunks(3).map(|v| [v[0], v[1], v[2]]).collect();
    verts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    verts.dedup();
    if verts.len() < 2 || verts.len() > 400 {
        return None;
    }
    let d2 = |a: [f32; 3], b: [f32; 3]| (0..3).map(|k| (a[k] - b[k]).powi(2)).sum::<f32>();
    let (mut a, mut b, mut best) = (verts[0], verts[0], 0.0);
    for (i, p) in verts.iter().enumerate() {
        for q in &verts[i + 1..] {
            let d = d2(*p, *q);
            if d > best {
                (a, b, best) = (*p, *q, d);
            }
        }
    }
    let len = best.sqrt();
    if len < 1e-4 {
        return None;
    }
    let dir: [f32; 3] = std::array::from_fn(|k| (b[k] - a[k]) / len);
    let n = verts.len() as f32;
    let c: [f32; 3] = std::array::from_fn(|k| verts.iter().map(|v| v[k]).sum::<f32>() / n);
    let (mut lo, mut hi, mut thick) = (f32::MAX, f32::MIN, 0.0f32);
    for v in &verts {
        let r: [f32; 3] = std::array::from_fn(|k| v[k] - c[k]);
        let t = r[0] * dir[0] + r[1] * dir[1] + r[2] * dir[2];
        lo = lo.min(t);
        hi = hi.max(t);
        let perp = (0..3).map(|k| (r[k] - t * dir[k]).powi(2)).sum::<f32>().sqrt();
        thick = thick.max(perp);
    }
    ((hi - lo) > 6.0 * 2.0 * thick).then(|| {
        (
            std::array::from_fn(|k| c[k] + dir[k] * lo),
            std::array::from_fn(|k| c[k] + dir[k] * hi),
        )
    })
}

/// A lane's light-switch settings in ChroMapper's environment (its `BasicLightEffect`).
#[derive(Clone, Copy, Default, Debug)]
pub struct LaneParams {
    pub lane: i64,
    /// Brightness multiplier for off events.
    pub off: f32,
    /// Lights start at the off brightness instead of dark.
    pub on_start: bool,
    pub invert: bool,
    pub highlight_matches: bool,
}

/// The folder with an imported environment's extras (textures, light components), if any.
pub fn extras_dir(env_name: &str) -> Option<PathBuf> {
    let d = dir()?.join(full_name(env_name));
    d.is_dir().then_some(d)
}

/// Some maps write "Origins" instead of "OriginsEnvironment".
fn full_name(env_name: &str) -> String {
    if env_name.ends_with("Environment") { env_name.to_owned() } else { format!("{env_name}Environment") }
}

pub struct EnvFile {
    #[allow(dead_code)]
    pub name: String,
    pub red: [f32; 4],
    pub blue: [f32; 4],
    pub white: [f32; 4],
    pub red_boost: Option<[f32; 4]>,
    pub blue_boost: Option<[f32; 4]>,
    pub white_boost: Option<[f32; 4]>,
    pub lanes: Vec<LaneParams>,
    pub objects: Vec<EnvObject>,
    /// Metres per ChroMapper unit (version 2).
    pub unit: f32,
    pub meshes: Vec<EnvMesh>,
    pub materials: Vec<EnvMaterial>,
    pub fog: Option<FogParams>,
    pub particle_systems: Vec<EnvParticleSystem>,
}

/// `BSLT_ENV_DIR` overrides the folder (for tests).
pub fn dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("BSLT_ENV_DIR") {
        return Some(PathBuf::from(d));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/bs-light-timeline/environments"))
}

impl EnvFile {
    fn from_json(v: &Value) -> Option<EnvFile> {
        let c4 = |v: &Value| -> [f32; 4] {
            let a = v.as_array();
            std::array::from_fn(|i| a.and_then(|a| a.get(i)).and_then(Value::as_f64).unwrap_or(1.0) as f32)
        };
        let opt = |k: &str| v.get(k).filter(|c| c.is_array()).map(c4);
        let floats = |v: &Value| -> Vec<f32> {
            v.as_array().map(|t| t.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect()).unwrap_or_default()
        };
        let unit = v["unit"].as_f64().unwrap_or(0.6) as f32;
        let meshes: Vec<EnvMesh> = v["meshes"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|m| {
                let p = floats(&m["v"]);
                let n = floats(&m["n"]);
                let uv = floats(&m["uv"]);
                let uv2 = floats(&m["uv2"]);
                let c = floats(&m["c"]);
                EnvMesh {
                    color: c.chunks_exact(4).map(|c| [c[0], c[1], c[2], c[3]]).collect(),
                    name: m["name"].as_str().unwrap_or_default().to_owned(),
                    pos: p.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect(),
                    normal: n.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect(),
                    uv: uv.chunks_exact(2).map(|c| [c[0], c[1]]).collect(),
                    uv2: uv2.chunks_exact(2).map(|c| [c[0], c[1]]).collect(),
                    idx: m["i"].as_array().into_iter().flatten().filter_map(|i| i.as_u64().map(|i| i as u32)).collect(),
                }
            })
            .collect();
        let materials: Vec<EnvMaterial> = v["materialList"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|m| EnvMaterial {
                name: m["name"].as_str().unwrap_or_default().to_owned(),
                shader: m["shader"].as_str().unwrap_or_default().to_owned(),
                queue: m["queue"].as_i64().unwrap_or(2000),
                keywords: m["keywords"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect(),
                colors: m["colors"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect(),
                props: m["props"].as_object().cloned().unwrap_or_default(),
            })
            .collect();
        // The environment's own settings (the descriptor's), when several objects hold some.
        let fog = v["fog"].as_array().and_then(|f| {
            let f = f.iter().find(|x| x["owner"] == "EnvironmentDescriptor").or(f.first())?;
            let g = |k: &str| f[k].as_f64().unwrap_or(0.0) as f32;
            Some(FogParams {
                offset: g("offset"),
                height: g("height"),
                start_y: g("startY"),
                attenuation: g("attenuation"),
                auto_exposure_limit: g("autoExposureLimit"),
                legacy_auto_exposure: f["legacyAutoExposure"].as_bool().unwrap_or(false),
            })
        });
        let mats_ref = &materials;
        let meshes_ref = &meshes;
        Some(EnvFile {
            name: v["name"].as_str()?.to_owned(),
            red: c4(&v["red"]),
            blue: c4(&v["blue"]),
            white: opt("white").unwrap_or([1.0; 4]),
            red_boost: opt("redBoost"),
            blue_boost: opt("blueBoost"),
            white_boost: opt("whiteBoost"),
            lanes: v["lanes"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|l| LaneParams {
                    lane: l["type"].as_i64().unwrap_or(0),
                    off: l["off"].as_f64().unwrap_or(0.0) as f32,
                    on_start: l["onStart"].as_bool().unwrap_or(false),
                    invert: l["invert"].as_bool().unwrap_or(false),
                    highlight_matches: l["highlightMatches"].as_bool().unwrap_or(false),
                })
                .collect(),
            objects: v["objects"]
                .as_array()?
                .iter()
                .map(|o| {
                    // [lane, id] or [lane, [ids...]].
                    let light = o["light"].as_array().and_then(|a| {
                        let ids = match a.get(1)? {
                            Value::Array(ids) => ids.iter().filter_map(Value::as_i64).collect(),
                            v => vec![v.as_i64()?],
                        };
                        Some((a.first()?.as_i64()?, ids))
                    });
                    let mesh = o["mesh"].as_u64().map(|m| m as usize).filter(|m| *m < meshes_ref.len());
                    let m = floats(&o["m"]);
                    let matrix: [f32; 16] = if m.len() == 16 {
                        std::array::from_fn(|i| m[i])
                    } else {
                        std::array::from_fn(|i| if i % 5 == 0 { 1.0 } else { 0.0 })
                    };
                    let materials: Vec<i64> =
                        o["materials"].as_array().into_iter().flatten().filter_map(Value::as_i64).collect();
                    // World triangles: given (version 1) or from the mesh (version 2).
                    let tris: Vec<f32> = match mesh {
                        Some(mi) => {
                            let me = &meshes_ref[mi];
                            me.idx
                                .iter()
                                .filter_map(|i| me.pos.get(*i as usize))
                                .flat_map(|p| transform_point(&matrix, *p).map(|c| c * unit))
                                .collect()
                        }
                        None => floats(&o["tris"]),
                    };
                    // The main material's color, for the simple renderer.
                    let color = match materials.first().and_then(|i| mats_ref.get(*i as usize)) {
                        Some(mat) => mat.vec4("_Color", [0.5, 0.5, 0.5, 1.0]),
                        None => c4(&o["color"]),
                    };
                    let enabled = o["enabled"].as_bool().unwrap_or(true);
                    EnvObject {
                        beam: light.as_ref().and_then(|_| beam_of(&tris)),
                        light,
                        color,
                        tris: if enabled { tris } else { Vec::new() },
                        mesh,
                        materials,
                        matrix,
                        enabled,
                        layer: o["layer"].as_u64().map(|l| l as u32),
                        tids: o["tids"].as_array().into_iter().flatten().filter_map(Value::as_i64).collect(),
                        sprite_tex: o["spriteTex"].as_str().map(str::to_owned),
                        sprite_color: o["spriteColor"].as_array().map(|a| std::array::from_fn(|i| a.get(i).and_then(Value::as_f64).unwrap_or(1.0) as f32)),
                        path: o["path"].as_str().unwrap_or_default().to_owned(),
                    }
                })
                .collect(),
            particle_systems: v["particleSystems"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|p| {
                    let m = floats(&p["m"]);
                    EnvParticleSystem {
                        materials: p["materials"].as_array().into_iter().flatten().filter_map(Value::as_i64).collect(),
                        matrix: if m.len() == 16 { std::array::from_fn(|i| m[i]) } else { std::array::from_fn(|i| if i % 5 == 0 { 1.0 } else { 0.0 }) },
                        local: p["space"] == "Local",
                        mesh: p["mesh"].as_i64().and_then(|i| usize::try_from(i).ok()).filter(|_| p["renderMode"] == "Mesh"),
                        enabled: p["enabled"].as_bool().unwrap_or(true),
                        particles: p["particles"].as_array().into_iter().flatten().filter_map(Particle::from_json).collect(),
                    }
                })
                .collect(),
            unit,
            meshes,
            materials,
            fog,
        })
    }

    /// Loads an environment file.
    pub fn load_path(path: &std::path::Path) -> Option<EnvFile> {
        let text = std::fs::read_to_string(path).ok()?;
        Self::from_json(&serde_json::from_str(&text).ok()?)
    }

    /// Loads the imported environment for a game environment name, if it was imported.
    pub fn load(env_name: &str) -> Option<EnvFile> {
        // Some maps write "Origins" instead of "OriginsEnvironment".
        let name = if env_name.ends_with("Environment") {
            env_name.to_owned()
        } else {
            format!("{env_name}Environment")
        };
        let path = dir()?.join(format!("{name}.json"));
        let text = std::fs::read_to_string(path).ok()?;
        Self::from_json(&serde_json::from_str(&text).ok()?)
    }
}
