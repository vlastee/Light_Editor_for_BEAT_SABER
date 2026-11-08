//! Rotating lasers and lights (ChroMapper's LightPairRotationEffect and LightRotationEffect) from
//! laser-speed events. ChroMapper rolls Unity's Random for start angles and directions; we use a
//! deterministic random per effect and event, so poses match in motion but not in exact angle.

use serde_json::Value;

type M4 = [f32; 16];

fn mul(a: &M4, b: &M4) -> M4 {
    std::array::from_fn(|i| {
        let (r, c) = (i / 4, i % 4);
        (0..4).map(|k| a[r * 4 + k] * b[k * 4 + c]).sum()
    })
}

fn invert(m: &M4) -> M4 {
    let a = |r: usize, c: usize| m[r * 4 + c] as f64;
    let mut inv = [0f64; 16];
    for r in 0..4 {
        for c in 0..4 {
            let rows: Vec<usize> = (0..4).filter(|&x| x != c).collect();
            let cols: Vec<usize> = (0..4).filter(|&x| x != r).collect();
            let s = |i: usize, j: usize| a(rows[i], cols[j]);
            let minor = s(0, 0) * (s(1, 1) * s(2, 2) - s(1, 2) * s(2, 1)) - s(0, 1) * (s(1, 0) * s(2, 2) - s(1, 2) * s(2, 0))
                + s(0, 2) * (s(1, 0) * s(2, 1) - s(1, 1) * s(2, 0));
            inv[r * 4 + c] = if (r + c) % 2 == 0 { minor } else { -minor };
        }
    }
    let det: f64 = (0..4).map(|c| a(0, c) * inv[c * 4]).sum();
    let d = if det.abs() < 1e-12 { 1.0 } else { det };
    std::array::from_fn(|i| (inv[i] / d) as f32)
}

/// Unity's Quaternion.Euler (degrees; Z, then X, then Y) as a rotation matrix.
fn euler(e: [f32; 3]) -> M4 {
    let (x, y, z) = (e[0].to_radians(), e[1].to_radians(), e[2].to_radians());
    let rx: M4 = [1.0, 0.0, 0.0, 0.0, 0.0, x.cos(), -x.sin(), 0.0, 0.0, x.sin(), x.cos(), 0.0, 0.0, 0.0, 0.0, 1.0];
    let ry: M4 = [y.cos(), 0.0, y.sin(), 0.0, 0.0, 1.0, 0.0, 0.0, -y.sin(), 0.0, y.cos(), 0.0, 0.0, 0.0, 0.0, 1.0];
    let rz: M4 = [z.cos(), -z.sin(), 0.0, 0.0, z.sin(), z.cos(), 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0];
    mul(&ry, &mul(&rx, &rz))
}

fn scaled(v: [f32; 3], a: f32) -> [f32; 3] {
    v.map(|c| c * a)
}

/// SplitMix64 of a seed: a deterministic 0..1 value.
fn random(seed: u64) -> f32 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 24) as f32
}

#[derive(Clone, Debug)]
struct Event {
    seconds: f32,
    beat_bits: u32,
    ty: i64,
    value: i64,
    speed: Option<f32>,
    direction: Option<i64>,
    lock: bool,
    has_custom: bool,
}

/// Laser-speed events (and switch events) of the given types, in time order.
fn events(json: &Value, types: &[i64], bpm: f64) -> Vec<Event> {
    let mut out = Vec::new();
    let v3 = json["basicBeatmapEvents"].as_array();
    let list = v3.or(json["_events"].as_array());
    for e in list.into_iter().flatten() {
        let (b, t, v, cd) = if v3.is_some() {
            (e["b"].as_f64().unwrap_or(0.0), e["et"].as_i64().unwrap_or(0), e["i"].as_i64().unwrap_or(0), &e["customData"])
        } else {
            (e["_time"].as_f64().unwrap_or(0.0), e["_type"].as_i64().unwrap_or(0), e["_value"].as_i64().unwrap_or(0), &e["_customData"])
        };
        if !types.contains(&t) {
            continue;
        }
        let num = |k: &str| cd[k].as_f64().map(|x| x as f32);
        out.push(Event {
            seconds: (b as f32 as f64 * 60.0 / bpm) as f32,
            beat_bits: (b as f32).to_bits(),
            ty: t,
            value: v,
            speed: num("speed").or(num("_speed")).or(num("_preciseSpeed")),
            direction: cd["direction"].as_i64().or(cd["_direction"].as_i64()),
            lock: cd["lockRotation"].as_bool().or(cd["_lockPosition"].as_bool()).unwrap_or(false),
            has_custom: cd.as_object().is_some_and(|o| !o.is_empty()),
        });
    }
    out.sort_by(|a, b| a.seconds.total_cmp(&b.seconds));
    out
}

/// A rotating transform: its pose at export and its rotation axes.
struct Pivot {
    path: String,
    m0: M4,
    inv0: M4,
    vector: [f32; 3],
    /// The angle the exported pose has.
    rest: f32,
}

struct Pair {
    pivots: [usize; 2],
    types: [i64; 3],
    start: f32,
    override_random: bool,
    seed: u64,
    events: Vec<Event>,
}

struct Single {
    pivot: usize,
    mul: f32,
    seed: u64,
    events: Vec<Event>,
}

#[derive(Default)]
pub struct Motion {
    pivots: Vec<Pivot>,
    pairs: Vec<Pair>,
    singles: Vec<Single>,
}

impl Motion {
    pub fn new(components: &[Value], json: &Value, bpm: f64) -> Motion {
        let mut m = Motion::default();
        let f = |v: &Value, d: f32| v.as_f64().map_or(d, |x| x as f32);
        let vec3 = |v: &Value| -> [f32; 3] { std::array::from_fn(|i| f(&v[i], 0.0)) };
        let mat = |v: &Value| -> Option<M4> {
            let a = v.as_array()?;
            (a.len() == 16).then(|| std::array::from_fn(|i| f(&a[i], 0.0)))
        };
        let find = |r: &Value, ty: &str| components.iter().find(|c| c["type"] == ty && c["id"].as_i64().is_some() && c["id"] == r["id"]);
        let add_pivot = |m: &mut Motion, t: &Value, vector: [f32; 3], rest: f32| -> Option<usize> {
            let m0 = mat(&t["m"])?;
            m.pivots.push(Pivot { path: t["path"].as_str()?.to_owned(), m0, inv0: invert(&m0), vector, rest });
            Some(m.pivots.len() - 1)
        };
        for (k, c) in components.iter().enumerate() {
            let x = &c["fields"];
            match c["type"].as_str().unwrap_or_default() {
                "LightPairRotationEffect" => {
                    let Some(vis) = find(&x["Visual"], "LightPairRotation") else { continue };
                    let y = &vis["fields"];
                    let start = f(&y["StartRotation"], 0.0);
                    let vector = vec3(&y["RotationVector"]);
                    let (Some(l), Some(r)) = (
                        add_pivot(&mut m, &y["Transforms"][0]["Transform"], vector, start),
                        add_pivot(&mut m, &y["Transforms"][1]["Transform"], vector, -start),
                    ) else {
                        continue;
                    };
                    let types = [x["LeftEventType"].as_i64().unwrap_or(12), x["RightEventType"].as_i64().unwrap_or(13), x["SwitchEventType"].as_i64().unwrap_or(-1)];
                    m.pairs.push(Pair {
                        pivots: [l, r],
                        types,
                        start,
                        override_random: y["OverrideRandomValues"].as_bool().unwrap_or(false),
                        seed: k as u64 * 7919,
                        events: events(json, &types, bpm),
                    });
                }
                "LightRotationEffect" => {
                    let Some(vis) = find(&x["Visual"], "LightRotation") else { continue };
                    let y = &vis["fields"];
                    let Some(p) = add_pivot(&mut m, &y["Transform"], vec3(&y["RotationVector"]), 0.0) else { continue };
                    let ty = x["ID"].as_i64().unwrap_or(12);
                    m.singles.push(Single {
                        pivot: p,
                        mul: f(&x["SpeedMultiplier"], 1.0),
                        seed: k as u64 * 104_729,
                        events: events(json, &[ty], bpm),
                    });
                }
                _ => {}
            }
        }
        m
    }

    /// Each pivot's angle at `seconds`.
    fn angles(&self, seconds: f32) -> Vec<f32> {
        let mut out: Vec<f32> = self.pivots.iter().map(|p| p.rest).collect();
        for pair in &self.pairs {
            let (start_l, start_r) = (pair.start, -pair.start);
            let (mut la, mut ra, mut ls, mut rs, mut le, mut re) = (start_l, start_r, 0.0f32, 0.0f32, false, false);
            let mut over = pair.override_random;
            let mut switch_index = 1;
            let mut last = 0.0f32;
            for e in pair.events.iter().take_while(|e| e.seconds <= seconds) {
                let dt = e.seconds - last;
                la += if le { ls * dt } else { 0.0 };
                ra += if re { rs * dt } else { 0.0 };
                last = e.seconds;
                if e.ty == pair.types[2] {
                    over = switch_index % 2 == 1;
                    switch_index += 1;
                }
                let seed = pair.seed ^ ((e.beat_bits as u64) << 16);
                let (rstart, rdir) = if over {
                    (((e.seconds * 90.0).round() as i64 % 360) as f32, 1.0)
                } else {
                    (random(seed ^ 0xABCD) * 360.0, if random(seed) < 0.5 { 1.0 } else { -1.0 })
                };
                let left = e.ty == pair.types[0];
                if left || e.ty == pair.types[1] {
                    let mut value = e.value as f32;
                    if value > 0.0 {
                        value = e.speed.unwrap_or(value);
                    }
                    let mut dir = if left { rdir } else { -rdir };
                    if let Some(d) = e.direction {
                        dir = if (d == 0) == left { -1.0 } else { 1.0 };
                    }
                    if e.value == 0 {
                        if left {
                            le = false;
                            if !e.lock {
                                la = start_l;
                            }
                        } else {
                            re = false;
                            if !e.lock {
                                ra = start_r;
                            }
                        }
                    } else if e.value > 0 {
                        if left {
                            le = true;
                            if !e.lock {
                                la = rstart + start_l;
                            }
                            ls = value * 20.0 * dir;
                        } else {
                            re = true;
                            if !e.lock {
                                ra = -rstart + start_r;
                            }
                            rs = value * 20.0 * dir;
                        }
                    }
                } else if e.ty == pair.types[2] {
                    la = rstart + start_l;
                    ra = -rstart + start_r;
                    ls = ls.abs();
                    rs = -rs.abs();
                }
            }
            let dt = seconds - last;
            out[pair.pivots[0]] = la + if le { ls * dt } else { 0.0 };
            out[pair.pivots[1]] = ra + if re { rs * dt } else { 0.0 };
        }
        for s in &self.singles {
            let (mut angle, mut speed, mut enabled, mut last) = (0.0f32, 0.0f32, false, 0.0f32);
            for e in s.events.iter().take_while(|e| e.seconds <= seconds) {
                let pre = angle + if enabled { speed * (e.seconds - last) } else { 0.0 };
                last = e.seconds;
                let seed = s.seed ^ ((e.beat_bits as u64) << 16);
                let mut dir = if random(seed) < 0.5 { 1.0 } else { -1.0 };
                let offset = random(seed ^ 0x1234) * 180.0;
                if let Some(d) = e.direction {
                    let left_event = e.ty == 12;
                    dir = if d == 0 { if left_event { -1.0 } else { 1.0 } } else if left_event { 1.0 } else { -1.0 };
                }
                if e.value == 0 {
                    enabled = false;
                    speed = 0.0;
                    angle = if e.lock { pre } else { 0.0 };
                } else if e.value > 0 {
                    enabled = true;
                    angle = if e.lock { pre } else { offset };
                    let value = e.speed.unwrap_or(e.value as f32);
                    let mul = if e.has_custom { 1.0 } else { s.mul };
                    speed = value * mul * 20.0 * dir;
                } else {
                    angle = pre;
                }
            }
            out[s.pivot] = angle + if enabled { speed * (seconds - last) } else { 0.0 };
        }
        out
    }

    /// For each pivot whose angle differs from its exported pose: its path and the matrix that takes the
    /// exported pose of anything under it to the current one.
    pub fn deltas(&self, seconds: f32) -> Vec<(&str, M4)> {
        let angles = self.angles(seconds);
        self.pivots
            .iter()
            .zip(angles)
            .filter(|(p, a)| (a - p.rest).abs() > 1e-4)
            .map(|(p, a)| {
                // local rotation = Start * Euler(v * angle); the exported pose had Euler(v * rest).
                let local = mul(&invert(&euler(scaled(p.vector, p.rest))), &euler(scaled(p.vector, a)));
                (p.path.as_str(), mul(&p.m0, &mul(&local, &p.inv0)))
            })
            .collect()
    }
}

/// Applies a pivot delta to a world matrix.
pub fn apply(delta: &M4, m: &M4) -> M4 {
    mul(delta, m)
}

/// Whether `path` is the pivot or below it.
pub fn under(path: &str, pivot: &str) -> bool {
    path == pivot || (path.starts_with(pivot) && path.as_bytes().get(pivot.len()) == Some(&b'/'))
}
