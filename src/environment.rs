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
}

pub fn dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/bs-light-timeline/environments"))
}

impl EnvFile {
    fn from_json(v: &Value) -> Option<EnvFile> {
        let c4 = |v: &Value| -> [f32; 4] {
            let a = v.as_array();
            std::array::from_fn(|i| a.and_then(|a| a.get(i)).and_then(Value::as_f64).unwrap_or(1.0) as f32)
        };
        let opt = |k: &str| v.get(k).filter(|c| c.is_array()).map(c4);
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
                    let tris: Vec<f32> = o["tris"]
                        .as_array()
                        .map(|t| t.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect())
                        .unwrap_or_default();
                    EnvObject {
                        beam: light.as_ref().and_then(|_| beam_of(&tris)),
                        light,
                        color: c4(&o["color"]),
                        tris,
                    }
                })
                .collect(),
        })
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
