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
    rings: Vec<RingSystem>,
}

/// Beat Saber's fixed physics step, which ring motion follows.
const FIXED_DT: f32 = 0.02;
/// Rings keep settling after the last event for at most this long.
const RING_SETTLE_SECONDS: f32 = 30.0;

/// One ring's exported pose, in its parent's space.
struct RingPose {
    id: i64,
    parent: M4,
    inv_parent: M4,
    x: f32,
    y: f32,
    z0: f32,
    rot0: f32,
}

/// The rings of one manager, with their simulated angle and depth per physics frame (from frame -1).
struct RingSystem {
    poses: Vec<RingPose>,
    rot: Vec<f32>,
    z: Vec<f32>,
}

#[derive(Clone, Copy, Default)]
struct RingState {
    rot: f32,
    dest: f32,
    speed: f32,
}

#[derive(Clone, Copy)]
struct RingWave {
    creation: i32,
    next: i32,
    progress: f32,
    first: f32,
    delta: f32,
    step: f32,
    propagation: f32,
    speed: f32,
    created: bool,
}

/// The physics frame on which an event at `seconds` first takes effect (the callback runs on a
/// 90 Hz render, then Chroma's fixed tick picks it up).
fn first_assignment_frame(seconds: f32) -> i32 {
    let callback = (seconds * 90.0 - 0.00001).ceil() / 90.0;
    ((callback - FIXED_DT * 0.7) / FIXED_DT).floor() as i32 + 1
}

fn lerp_clamped(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t.clamp(0.0, 1.0)
}

/// ChroMapper's TrackLaneRingsRotationEffect run forward: waves assign each ring's destination in
/// turn, and rings ease toward it. Returns the angles per frame from frame -1 through `last`.
fn simulate_ring_spin(count: usize, startup: RingWave, mut pending: std::collections::VecDeque<RingWave>, last: i32) -> Vec<f32> {
    let mut rings = vec![RingState::default(); count];
    let mut waves = vec![startup];
    let mut out = Vec::with_capacity((last + 2).max(0) as usize * count);
    for frame in (startup.creation + 1)..=last {
        for w in waves.iter_mut().rev().filter(|w| w.created && w.next == frame) {
            let mut ring = w.progress as i64;
            w.progress += w.propagation;
            while (ring as f32) < w.progress && (ring as usize) < count {
                rings[ring as usize].dest = w.first + ring as f32 * w.step;
                rings[ring as usize].speed = w.speed;
                ring += 1;
            }
            w.next += 1;
        }
        waves.retain(|w| !w.created || w.progress < count as f32);
        for r in &mut rings {
            r.rot = lerp_clamped(r.rot, r.dest, FIXED_DT * r.speed);
        }
        while pending.front().is_some_and(|w| w.creation <= frame) {
            if let Some(mut w) = pending.pop_front() {
                // Everything dispatched by one callback sees the same first-ring destination.
                w.first = rings[0].dest + w.delta;
                w.next = w.creation + 1;
                w.created = true;
                waves.push(w);
            }
        }
        if frame >= -1 {
            out.extend(rings.iter().map(|r| r.rot));
        }
    }
    out
}

/// ChroMapper's TrackLaneRingsPositionEffect run forward. `events`: (first frame, step, speed).
fn simulate_ring_zoom(start: &[f32], offsets: &[f32], events: &[(i32, f32, f32)], last: i32) -> Vec<f32> {
    let mut z = start.to_vec();
    let mut out = Vec::with_capacity((last + 2).max(0) as usize * z.len());
    out.extend_from_slice(&z);
    let mut next = 0;
    let (mut step, mut speed) = (0.0f32, 0.0f32);
    for frame in 0..=last {
        while next < events.len() && events[next].0 <= frame {
            (_, step, speed) = events[next];
            next += 1;
        }
        for (i, v) in z.iter_mut().enumerate() {
            *v = lerp_clamped(*v, offsets[i] + i as f32 * step, FIXED_DT * speed);
        }
        out.extend_from_slice(&z);
    }
    out
}

/// Ring events (type 8 or 9) as (seconds, custom data), in time order.
fn ring_events(json: &Value, ty: i64, bpm: f64) -> Vec<(f32, Value)> {
    let v3 = json["basicBeatmapEvents"].as_array();
    let list = v3.or(json["_events"].as_array());
    let spb = 60.0 / bpm.max(1.0);
    let mut out: Vec<(f32, Value)> = list
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let (b, t, cd) = if v3.is_some() {
                (e["b"].as_f64()?, e["et"].as_i64().unwrap_or(0), &e["customData"])
            } else {
                (e["_time"].as_f64()?, e["_type"].as_i64().unwrap_or(0), &e["_customData"])
            };
            (t == ty).then(|| ((b * spb) as f32, cd.clone()))
        })
        .collect();
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    out
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
        m.rings = Self::build_rings(components, json, bpm);
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
    fn build_rings(components: &[Value], json: &Value, bpm: f64) -> Vec<RingSystem> {
        let num = |v: &Value, d: f32| v.as_f64().map_or(d, |x| x as f32);
        let find = |r: &Value, ty: &str| components.iter().find(|c| c["type"] == ty && c["id"].as_i64().is_some() && c["id"] == r["id"]);
        let mat = |v: &Value| -> Option<M4> {
            let a = v.as_array()?;
            (a.len() == 16).then(|| std::array::from_fn(|i| num(&a[i], 0.0)))
        };
        let identity: M4 = std::array::from_fn(|i| if i % 5 == 0 { 1.0 } else { 0.0 });
        // A Chroma number under its v3 or v2 name.
        let custom = |cd: &Value, v3: &str, v2: &str| cd[v3].as_f64().or(cd[v2].as_f64()).map(|x| x as f32);
        let spin_events = ring_events(json, 8, bpm);
        let zoom_events = ring_events(json, 9, bpm);
        let root = if json.get("basicBeatmapEvents").is_some() { &json["customData"] } else { &json["_customData"] };
        let old_propagation = [&root["mappedForOldRingPropagationSpeed"], &root["_mappedForOldRingPropagationSpeed"]]
            .iter()
            .any(|v| v.as_bool() == Some(true));
        let last_event = spin_events.iter().chain(&zoom_events).map(|e| e.0).fold(0.0f32, f32::max);
        let last = ((last_event + RING_SETTLE_SECONDS) / FIXED_DT).ceil() as i32;

        let mut out = Vec::new();
        for manager in components.iter().filter(|c| c["type"] == "TrackLaneRingsManager") {
            let f = &manager["fields"];
            let rings: Vec<&Value> = f["Rings"].as_array().into_iter().flatten().filter_map(|r| find(r, "TrackLaneRing")).collect();
            if rings.is_empty() {
                continue;
            }
            let parent = if f["SpawnAsChildren"].as_bool() == Some(true) { mat(&manager["m"]).unwrap_or(identity) } else { identity };
            let inv_parent = invert(&parent);
            let mut poses = Vec::new();
            let mut starts = Vec::new();
            let mut offsets = Vec::new();
            for r in &rings {
                let Some(m0) = mat(&r["m"]) else { continue };
                let l = mul(&inv_parent, &m0);
                poses.push(RingPose {
                    id: r["id"].as_i64().unwrap_or(0),
                    parent,
                    inv_parent,
                    x: l[3],
                    y: l[7],
                    z0: l[11],
                    rot0: l[4].atan2(l[0]).to_degrees(),
                });
                starts.push(num(&r["fields"]["PositionZ"], l[11]));
                offsets.push(num(&r["fields"]["PositionOffset"][2], 0.0));
            }
            let count = poses.len();
            if count != rings.len() {
                continue;
            }
            let same_manager = |v: &Value| v["id"].is_i64() && v["id"] == manager["id"];

            // Spin: one effect per manager.
            let mut rot = Vec::new();
            let spin = components.iter().filter(|c| c["type"] == "TrackLaneRingsRotationEffect").find_map(|e| {
                let vis = find(&e["fields"]["Visual"], "TrackLaneRingsRotation")?;
                same_manager(&vis["fields"]["Manager"]).then_some((e, vis))
            });
            if let Some((e, vis)) = spin {
                let x = &e["fields"];
                let y = &vis["fields"];
                let name = e["path"].as_str().and_then(|p| p.rsplit('/').next()).unwrap_or_default();
                let counter_allowed = !name.contains("Big");
                let (rotation, step, propagation, flexy) =
                    (num(&x["Rotation"], 0.0), num(&x["Step"], 0.0), num(&x["PropagationSpeed"], 1.0), num(&x["FlexySpeed"], 1.0));
                let step_type = x["StepType"].as_str().unwrap_or("Range");
                let seed = e["id"].as_i64().unwrap_or(0) as u64;
                let startup = RingWave {
                    creation: -20,
                    next: -19,
                    progress: 0.0,
                    first: num(&y["StartupRotationAngle"], 0.0),
                    delta: 0.0,
                    step: num(&y["StartupRotationStep"], 0.0),
                    propagation: num(&y["StartupRotationPropagationSpeed"], 1.0),
                    speed: num(&y["StartupRotationFlexySpeed"], 1.0),
                    created: true,
                };
                let mut pending = std::collections::VecDeque::new();
                for (k, (seconds, cd)) in spin_events.iter().enumerate() {
                    let r = |salt: u64| random(seed.wrapping_mul(31).wrapping_add((k as u64) << 8).wrapping_add(salt));
                    let mut wave_step = match step_type {
                        "Range0ToMax" => r(1) * step,
                        "Range" => (r(1) * 2.0 - 1.0) * step,
                        "MaxOr0" => if r(1) < 0.5 { step } else { 0.0 },
                        _ => 0.0,
                    };
                    let filter = cd["nameFilter"].as_str().or(cd["_nameFilter"].as_str()).unwrap_or_default();
                    if !filter.is_empty() && !filter.eq_ignore_ascii_case(name) {
                        continue;
                    }
                    if let Some(v) = custom(cd, "step", "_step") {
                        wave_step = v;
                    }
                    if let Some(v) = cd["_stepMult"].as_f64() {
                        wave_step *= v as f32;
                    }
                    let mut clockwise = r(2) >= 0.5;
                    if let Some(v) = custom(cd, "direction", "_direction") {
                        clockwise = v == 0.0;
                    }
                    let explicit = custom(cd, "prop", "_prop");
                    let mut prop = explicit.unwrap_or(propagation);
                    if explicit.is_some() && old_propagation {
                        prop *= 0.6;
                    }
                    if let Some(v) = cd["_propMult"].as_f64() {
                        prop *= v as f32;
                    }
                    let mut speed = custom(cd, "speed", "_speed").or(custom(cd, "preciseSpeed", "_preciseSpeed")).unwrap_or(flexy);
                    if let Some(v) = cd["_speedMult"].as_f64() {
                        speed *= v as f32;
                    }
                    let mut turn = custom(cd, "rotation", "_rotation").unwrap_or(rotation);
                    if cd["_reset"].as_bool() == Some(true) {
                        turn = rotation;
                        wave_step = 0.0;
                        prop = 50.0;
                        speed = 50.0;
                    }
                    let mut signed = turn * if clockwise { 1.0 } else { -1.0 };
                    // Heck only reads counterSpin in v2 maps.
                    if counter_allowed && cd["_counterSpin"].as_bool() == Some(true) {
                        signed = -signed;
                    }
                    if prop <= 0.0 {
                        continue;
                    }
                    let assignment = first_assignment_frame(*seconds);
                    pending.push_back(RingWave {
                        creation: assignment - 1,
                        next: assignment,
                        progress: 0.0,
                        first: 0.0,
                        delta: signed,
                        step: wave_step,
                        propagation: prop,
                        speed,
                        created: false,
                    });
                }
                rot = simulate_ring_spin(count, startup, pending, last);
            }

            // Zoom: when this manager has one.
            let mut z = Vec::new();
            let zoom = components.iter().filter(|c| c["type"] == "TrackLaneRingsPositionEffect").find_map(|e| {
                let vis = find(&e["fields"]["Visual"], "TrackLaneRingsPositionSpawner")?;
                same_manager(&vis["fields"]["RingManager"]).then_some(vis)
            });
            if let Some(vis) = zoom {
                let y = &vis["fields"];
                let (min, max, move_speed) = (num(&y["MinPositionStep"], 0.0), num(&y["MaxPositionStep"], 0.0), num(&y["MoveSpeed"], 0.0));
                let events: Vec<(i32, f32, f32)> = zoom_events
                    .iter()
                    .enumerate()
                    .map(|(k, (seconds, cd))| {
                        let default_step = if (k + 1) % 2 == 0 { max } else { min };
                        let speed = custom(cd, "speed", "_speed").or(custom(cd, "preciseSpeed", "_preciseSpeed")).unwrap_or(move_speed);
                        (first_assignment_frame(*seconds), custom(cd, "step", "_step").unwrap_or(default_step), speed)
                    })
                    .collect();
                z = simulate_ring_zoom(&starts, &offsets, &events, last);
            }
            out.push(RingSystem { poses, rot, z });
        }
        out
    }

    /// Ring id → how that ring moved from its exported pose at `seconds`.
    pub fn ring_deltas(&self, seconds: f32) -> Vec<(i64, M4)> {
        let position = f64::from(seconds) / f64::from(FIXED_DT);
        let frame = position.floor().max(0.0);
        let t = (position - frame) as f32;
        let mut out = Vec::new();
        for sys in &self.rings {
            let count = sys.poses.len();
            let sample = |values: &[f32], i: usize, fallback: f32| -> f32 {
                let frames = values.len() / count.max(1);
                if frames == 0 {
                    return fallback;
                }
                let cur = (frame as usize + 1).min(frames - 1);
                let (a, b) = (values[(cur - 1) * count + i], values[cur * count + i]);
                a + (b - a) * t
            };
            for (i, p) in sys.poses.iter().enumerate() {
                let rot = sample(&sys.rot, i, p.rot0);
                let z = sample(&sys.z, i, p.z0);
                let translate = |x: f32, y: f32, z: f32| -> M4 {
                    let mut m: M4 = std::array::from_fn(|k| if k % 5 == 0 { 1.0 } else { 0.0 });
                    (m[3], m[7], m[11]) = (x, y, z);
                    m
                };
                let local = mul(&translate(p.x, p.y, z), &mul(&euler([0.0, 0.0, rot - p.rot0]), &translate(-p.x, -p.y, -p.z0)));
                out.push((p.id, mul(&p.parent, &mul(&local, &p.inv_parent))));
            }
        }
        out
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    fn wave(creation: i32, delta: f32, step: f32, propagation: f32, speed: f32) -> RingWave {
        RingWave { creation, next: creation + 1, progress: 0.0, first: 0.0, delta, step, propagation, speed, created: false }
    }

    #[test]
    fn spin_waves_reach_rings_in_turn_and_add_up() {
        let startup = RingWave { created: true, first: 45.0, next: -19, ..wave(-20, 0.0, 0.0, 1.0, 10.0) };
        let later = std::collections::VecDeque::from([wave(100, 90.0, 5.0, 1.0, 50.0)]);
        let out = simulate_ring_spin(3, startup, later, 400);
        let at = |frame: i32, ring: usize| out[(frame + 1) as usize * 3 + ring];
        // Before the event every ring is easing toward the startup angle.
        assert!((at(90, 0) - 45.0).abs() < 0.01);
        // One ring per tick: the first has started moving before the third.
        assert!(at(101, 0) > at(101, 2));
        // The step fans the rings out from the new first-ring destination.
        for ring in 0..3 {
            assert!((at(400, ring) - (135.0 + 5.0 * ring as f32)).abs() < 0.01, "{}", at(400, ring));
        }
    }

    #[test]
    fn zoom_moves_each_ring_by_its_index() {
        let out = simulate_ring_zoom(&[20.0, 28.0], &[20.0, 20.0], &[(10, 4.0, 50.0)], 200);
        let last = out.len() - 2;
        assert_eq!((out[last], out[last + 1]), (20.0, 24.0));
        // Nothing moves before the event's frame.
        assert_eq!(out[2 * 5 + 1], 28.0);
    }

    #[test]
    fn events_wait_for_the_physics_tick_after_their_render() {
        // 1.0 s is exactly on the 90 Hz grid and 50 Hz frame 50; 0.7 of a tick puts it on frame 50.
        assert_eq!(first_assignment_frame(1.0), 50);
    }
}
