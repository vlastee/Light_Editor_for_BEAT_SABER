//! v3 light groups (event box groups), evaluated like ChroMapper's GLS effects, and Chroma's animated
//! fog. Beat Saber builds every element's timeline from the groups: each box picks elements with a
//! filter, spreads its events across them, and a later group cuts off what an earlier one left.
//! Random filters use .NET's `System.Random` with the filter's seed, as ChroMapper does.

use crate::environment::FogParams;
use crate::map::{self, Beatmap, FOG_LANE, RowKind};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

type Filtered = (i64, i64, i64);

struct Filter {
    start: i64,
    step: i64,
    count: i64,
    group_size: i64,
    chunk: i64,
    visible: i64,
    limits_duration: bool,
    limits_distribution: bool,
    /// RandomType flags (1 keep order, 2 random elements) and seed.
    random: i64,
    seed: i32,
}

/// .NET's `System.Random(seed)` (the legacy subtractive generator ChroMapper's Mono uses).
struct NetRandom {
    seeds: [i32; 56],
    inext: usize,
    inextp: usize,
}

impl NetRandom {
    fn new(seed: i32) -> NetRandom {
        const MBIG: i32 = i32::MAX;
        let mut s = [0i32; 56];
        let sub = if seed == i32::MIN { MBIG } else { seed.abs() };
        let mut mj = 161_803_398 - sub;
        s[55] = mj;
        let mut mk = 1i32;
        for i in 1..55 {
            let ii = (21 * i) % 55;
            s[ii] = mk;
            mk = mj.wrapping_sub(mk);
            if mk < 0 {
                mk = mk.wrapping_add(MBIG);
            }
            mj = s[ii];
        }
        for _ in 1..5 {
            for i in 1..56 {
                s[i] = s[i].wrapping_sub(s[1 + (i + 30) % 55]);
                if s[i] < 0 {
                    s[i] = s[i].wrapping_add(MBIG);
                }
            }
        }
        NetRandom { seeds: s, inext: 0, inextp: 21 }
    }

    fn next(&mut self, max: i64) -> i64 {
        let a = if self.inext + 1 >= 56 { 1 } else { self.inext + 1 };
        let b = if self.inextp + 1 >= 56 { 1 } else { self.inextp + 1 };
        let mut r = self.seeds[a].wrapping_sub(self.seeds[b]);
        if r == i32::MAX {
            r -= 1;
        }
        if r < 0 {
            r = r.wrapping_add(i32::MAX);
        }
        self.seeds[a] = r;
        (self.inext, self.inextp) = (a, b);
        (r as f64 * (1.0 / i32::MAX as f64) * max as f64) as i64
    }
}

/// ChroMapper's `ShuffleExtensions.Shuffle` (inside-out Fisher-Yates).
fn shuffle(items: &[i64], seed: i32) -> Vec<i64> {
    let mut r = NetRandom::new(seed);
    let mut out: Vec<i64> = Vec::with_capacity(items.len());
    for &item in items {
        let j = r.next(out.len() as i64 + 1) as usize;
        if j == out.len() {
            out.push(item);
        } else {
            out.push(out[j]);
            out[j] = item;
        }
    }
    out
}

/// `PickRandomElementsWithTombstone`: `count` of `0..total` by selection sampling, the rest `None`.
fn pick_random(count: i64, total: i64, seed: i32) -> Vec<Option<i64>> {
    let mut r = NetRandom::new(seed);
    let mut left = count;
    (0..total)
        .map(|i| {
            if r.next(total - i) < left {
                left -= 1;
                Some(i)
            } else {
                None
            }
        })
        .collect()
}

fn divide_round_up(value: i64, divisor: i64) -> i64 {
    ((value - 1) / divisor) + 1
}

/// `IndexFilterHelper.Convert`: `None` for filters the game ignores.
fn convert(f: &Value, group_size: i64) -> Option<Filter> {
    let int = |k: &str| f[k].as_f64().unwrap_or(0.0) as i64;
    let chunks = int("c");
    if group_size <= 0 || chunks < 0 {
        return None;
    }
    let chunk = if chunks == 0 { 1 } else { divide_round_up(group_size, chunks) };
    let offset_size = divide_round_up(group_size, chunk);
    let (p0, p1, reverse) = (int("p"), int("t"), int("r") == 1);
    let (start, step, count) = match int("f") {
        1 => {
            if p0 <= 0 || p1 < 0 || p1 >= p0 {
                return None;
            }
            let offset = divide_round_up(offset_size, p0);
            if p1 > (offset_size - 1) / offset {
                return None;
            }
            let section = offset * p1;
            let count = offset.min(offset_size - section);
            if reverse { (offset_size - section - 1, -1, count) } else { (section, 1, count) }
        }
        2 => {
            if p0 < 0 || p0 >= offset_size || p1 < 0 {
                return None;
            }
            let count = if p1 == 0 { 1 } else { divide_round_up(offset_size - p0, p1) };
            if reverse { (offset_size - 1 - p0, -p1, count) } else { (p0, p1, count) }
        }
        _ => return None,
    };
    let limit = f["l"].as_f64().unwrap_or(0.0);
    let visible = if limit == 0.0 || limit == 1.0 { count } else { (count as f64 * limit).ceil() as i64 };
    let affects = int("d");
    Some(Filter {
        start,
        step,
        count,
        group_size,
        chunk,
        visible,
        limits_duration: affects & 1 != 0,
        limits_distribution: affects & 2 != 0,
        random: int("n"),
        seed: f["s"].as_f64().unwrap_or(0.0) as i64 as i32,
    })
}

impl Filter {
    /// (element, duration order, distribution order).
    fn items(&self) -> Vec<Filtered> {
        let mut out = Vec::new();
        let mut values: Vec<i64> = (0..self.count).map(|i| self.start + self.step * i).collect();
        if self.random != 0 && self.random & 1 == 0 {
            values = shuffle(&values, self.seed);
        }
        let ids: Vec<Option<i64>> = if self.visible <= 0 {
            (0..self.count).map(Some).collect()
        } else if self.random & 2 != 0 {
            pick_random(self.visible, self.count, self.seed)
        } else {
            (0..self.count).map(|i| (i < self.visible).then_some(i)).collect()
        };
        let pairs = values.iter().zip(&ids).filter_map(|(v, id)| id.map(|id| (*v, id)));
        for (limited, (value, index)) in pairs.enumerate() {
            for local in 0..self.chunk {
                let element = value * self.chunk + local;
                if element >= self.group_size {
                    break;
                }
                let order = |limits: bool| if limits { limited as i64 } else { index };
                out.push((element, order(self.limits_duration), order(self.limits_distribution)));
            }
        }
        out
    }

    fn duration_count(&self) -> i64 {
        if self.limits_duration { self.visible } else { self.count }
    }

    fn distribution_count(&self) -> i64 {
        if self.limits_distribution { self.visible } else { self.count }
    }
}

/// Beat Saber's easing ids, as the editor's easing names. Id -1 holds the start value.
fn ease(id: i32, t: f32) -> f32 {
    const NAMES: [&str; 31] = [
        "easeLinear", "easeInQuad", "easeOutQuad", "easeInOutQuad", "easeInSine", "easeOutSine", "easeInOutSine", "easeInCubic", "easeOutCubic",
        "easeInOutCubic", "easeInQuart", "easeOutQuart", "easeInOutQuart", "easeInQuint", "easeOutQuint", "easeInOutQuint", "easeInExpo",
        "easeOutExpo", "easeInOutExpo", "easeInCirc", "easeOutCirc", "easeInOutCirc", "easeInBack", "easeOutBack", "easeInOutBack",
        "easeInElastic", "easeOutElastic", "easeInOutElastic", "easeInBounce", "easeOutBounce", "easeInOutBounce",
    ];
    match id {
        -1 => if t >= 1.0 { 1.0 } else { 0.0 },
        0..=30 => crate::eval::ease(Some(NAMES[id as usize]), t),
        100 => crate::eval::ease(Some("easeInOutBack"), t),
        101 => crate::eval::ease(Some("easeInOutElastic"), t),
        102 => crate::eval::ease(Some("easeInOutBounce"), t),
        _ => t,
    }
}

/// `DistributionHelper.GetValueStep`.
fn value_step(index: i64, count: i64, wave: bool, spread: f32, easing: i32) -> f32 {
    if wave {
        spread * ease(easing, index as f32 / (count - 1).max(1) as f32)
    } else {
        spread * ease(easing, index as f32 / count as f32) * count as f32
    }
}

#[derive(Clone, Copy)]
struct Node<T> {
    t: f64,
    use_previous: bool,
    easing: i32,
    value: T,
}

/// A box field.
fn num(v: &Value, k: &str) -> f32 {
    v[k].as_f64().unwrap_or(0.0) as f32
}

/// Each (axis, element)'s nodes. `make(event, box, order within the box, spread value)`.
fn build<T: Copy>(
    groups: &[&Value],
    count: i64,
    event_key: &str,
    spread_key: &str,
    axis_of: &dyn Fn(&Value) -> i64,
    make: &dyn Fn(&Value, &Value, usize, f32) -> Option<(bool, i32, T)>,
) -> HashMap<(i64, i64), Vec<Node<T>>> {
    // For each element, in group order: the box that drives it and when it starts.
    struct State<'a> {
        group_beat: f64,
        local: f64,
        beat_step: f64,
        boxed: &'a Value,
        duration: i64,
        distribution: i64,
        filter: std::rc::Rc<Filter>,
    }
    let mut states: HashMap<(i64, i64), Vec<State>> = HashMap::new();
    for g in groups {
        let beat = g["b"].as_f64().unwrap_or(0.0);
        let mut taken = HashSet::new();
        for b in g["e"].as_array().into_iter().flatten() {
            let events = b[event_key].as_array().map_or(&[][..], Vec::as_slice);
            let Some(last) = events.last() else { continue };
            let Some(filter) = convert(&b["f"], count) else { continue };
            let filter = std::rc::Rc::new(filter);
            let wave = b["d"].as_i64() == Some(1);
            let spread = b["w"].as_f64().unwrap_or(0.0);
            let spread = if wave { (spread - last["b"].as_f64().unwrap_or(0.0)).max(0.0) } else { spread };
            let beat_step = if wave { spread / (filter.duration_count() - 1).max(1) as f64 } else { spread };
            let axis = axis_of(b);
            for (element, duration, distribution) in filter.items() {
                if !taken.insert((axis, element)) {
                    continue;
                }
                states.entry((axis, element)).or_default().push(State {
                    group_beat: beat,
                    local: beat + beat_step * duration as f64,
                    beat_step,
                    boxed: b,
                    duration,
                    distribution,
                    filter: filter.clone(),
                });
            }
        }
    }
    let mut out = HashMap::new();
    for (key, list) in states {
        let mut nodes = Vec::new();
        for (i, s) in list.iter().enumerate() {
            let limit = list.get(i + 1).map_or(f64::INFINITY, |n| n.local);
            let spread = value_step(
                s.distribution,
                s.filter.distribution_count(),
                s.boxed["t"].as_i64() == Some(1),
                num(s.boxed, spread_key),
                s.boxed["i"].as_i64().unwrap_or(0) as i32,
            );
            for (order, e) in s.boxed[event_key].as_array().into_iter().flatten().enumerate() {
                let t = s.group_beat + e["b"].as_f64().unwrap_or(0.0) + s.duration as f64 * s.beat_step;
                if t > limit {
                    continue;
                }
                if let Some((use_previous, easing, value)) = make(e, s.boxed, order, spread) {
                    nodes.push(Node { t, use_previous, easing, value });
                }
            }
        }
        nodes.sort_by(|a, b| a.t.total_cmp(&b.t));
        out.insert(key, nodes);
    }
    out
}

/// The nodes around `beat`: start value, end value, eased-curve id and raw fraction.
fn span<T: Copy>(nodes: &[Node<T>], beat: f64, default: T) -> (T, T, i32, f32, Option<usize>) {
    let i = nodes.partition_point(|n| n.t <= beat);
    let current = i.checked_sub(1);
    let start_index = current.and_then(|c| {
        if !nodes[c].use_previous {
            return Some(c);
        }
        nodes[..c].iter().rposition(|n| !n.use_previous)
    });
    let start = start_index.map_or(default, |s| nodes[s].value);
    let next = nodes.get(i);
    let (end, easing) = match next.filter(|n| !n.use_previous) {
        Some(n) => (n.value, n.easing),
        None => (start, start_index.map_or(0, |s| nodes[s].easing)),
    };
    let fraction = match (current, next) {
        (Some(c), Some(n)) if n.t > nodes[c].t => ((beat - nodes[c].t) / (n.t - nodes[c].t)) as f32,
        _ => 0.0,
    };
    (start, end, easing, fraction, current)
}

#[derive(Clone, Copy, Default)]
struct ColorValue {
    color: i64,
    custom: Option<[f32; 4]>,
    brightness: f32,
    frequency: f32,
    strobe: f32,
    fade: bool,
}

#[derive(Clone, Copy, Default)]
struct Rotation {
    angle: f32,
    direction: i64,
    loops: i64,
}

#[derive(Clone, Copy, Default)]
struct Translation {
    value: f32,
    spread: f32,
}

/// How one transform of a rotation or translation group moves.
pub struct GroupMove {
    pub group: i64,
    pub element: i64,
    pub axis: i64,
    pub mirrored: bool,
    /// Translation, with its (value, spread) limits; `None` for rotation.
    pub limits: Option<([f32; 2], [f32; 2])>,
}

#[derive(Default)]
pub struct GroupLights {
    colors: HashMap<(i64, i64), Vec<Node<ColorValue>>>,
    rotations: HashMap<(i64, i64, i64), Vec<Node<Rotation>>>,
    translations: HashMap<(i64, i64, i64), Vec<Node<Translation>>>,
    /// VFX groups: (group, element) → float values.
    vfx: HashMap<(i64, i64), Vec<Node<f32>>>,
    fog_tracks: HashSet<String>,
    fog_base: HashMap<&'static str, f32>,
}

const FOG_FIELDS: [(&str, &str); 4] = [("attenuation", "_attenuation"), ("offset", "_offset"), ("startY", "_startY"), ("height", "_height")];

/// `Mathf.Repeat(v, 360)`.
fn wrap(v: f32) -> f32 {
    v - (v / 360.0).floor() * 360.0
}

fn delta_angle(from: f32, to: f32) -> f32 {
    let d = wrap(to - from);
    if d > 180.0 { d - 360.0 } else { d }
}

impl GroupLights {
    pub fn new(components: &[Value], json: &Value) -> GroupLights {
        let mut out = GroupLights::default();
        out.read_fog(json);
        let group_of = |manager: &str, effect: &Value| -> Option<i64> {
            components
                .iter()
                .filter(|c| c["type"] == manager)
                .flat_map(|c| c["fields"]["effectEntries"].as_array().into_iter().flatten())
                .find(|e| e["Effect"]["id"] == effect["id"])
                .and_then(|e| e["Group"].as_i64())
        };
        let list = |name: &str| -> HashMap<i64, Vec<&Value>> {
            let mut by_group: HashMap<i64, Vec<&Value>> = HashMap::new();
            for g in json[name].as_array().into_iter().flatten() {
                by_group.entry(g["g"].as_i64().unwrap_or(0)).or_default().push(g);
            }
            by_group.values_mut().for_each(|v| v.sort_by(|a, b| a["b"].as_f64().unwrap_or(0.0).total_cmp(&b["b"].as_f64().unwrap_or(0.0))));
            by_group
        };
        let color_groups = list(map::GROUP_LISTS[0]);
        let rotation_groups = list(map::GROUP_LISTS[1]);
        let translation_groups = list(map::GROUP_LISTS[2]);
        // VFX boxes list indices into _fxEventsCollection; give each box its events inline ("fx").
        let fl = json["_fxEventsCollection"]["_fl"].as_array().cloned().unwrap_or_default();
        let resolved: Vec<Value> = json[map::GROUP_LISTS[3]]
            .as_array()
            .into_iter()
            .flatten()
            .map(|g| {
                let mut g = g.clone();
                for b in g["e"].as_array_mut().into_iter().flatten() {
                    let events: Vec<Value> = b["l"].as_array().into_iter().flatten().filter_map(|i| i.as_u64().and_then(|i| fl.get(i as usize)).cloned()).collect();
                    b["fx"] = Value::Array(events);
                }
                g
            })
            .collect();
        let mut vfx_groups: HashMap<i64, Vec<&Value>> = HashMap::new();
        for g in &resolved {
            vfx_groups.entry(g["g"].as_i64().unwrap_or(0)).or_default().push(g);
        }
        vfx_groups.values_mut().for_each(|v| v.sort_by(|a, b| a["b"].as_f64().unwrap_or(0.0).total_cmp(&b["b"].as_f64().unwrap_or(0.0))));
        for c in components {
            let count = c["fields"]["Count"].as_i64().unwrap_or(0);
            match c["type"].as_str().unwrap_or_default() {
                "LightColorGroupEffect" => {
                    let Some(g) = group_of("LightColorGroupEffectManager", c) else { continue };
                    let Some(groups) = color_groups.get(&g) else { continue };
                    let nodes = build(groups, count, "e", "r", &|_| 0, &|e, b, order, spread| {
                        let transition = e["i"].as_i64().unwrap_or(0);
                        let affected = order != 0 || b["b"].as_i64() == Some(1);
                        let easing = e["ease"].as_i64().map_or(if transition == 0 { -1 } else { 0 }, |v| v as i32);
                        let custom = e["customData"]["color"].as_array().map(|a| std::array::from_fn(|i| a.get(i).and_then(Value::as_f64).map_or(1.0, |v| v as f32)));
                        Some((
                            transition == 2,
                            easing,
                            ColorValue {
                                color: e["c"].as_i64().unwrap_or(0),
                                custom,
                                brightness: num(e, "s") + if affected { spread } else { 0.0 },
                                frequency: num(e, "f"),
                                strobe: num(e, "sb"),
                                fade: e["sf"].as_i64() == Some(1),
                            },
                        ))
                    });
                    out.colors.extend(nodes.into_iter().map(|((_, element), n)| ((g, element), n)));
                }
                "LightRotationGroupEffect" => {
                    let Some(g) = group_of("LightRotationGroupEffectManager", c) else { continue };
                    let Some(groups) = rotation_groups.get(&g) else { continue };
                    let nodes = build(groups, count, "l", "s", &|b| b["a"].as_i64().unwrap_or(0), &|e, b, order, spread| {
                        let spread = if order == 0 && b["b"].as_i64() != Some(1) { 0.0 } else { spread };
                        let flip = if b["r"].as_i64() == Some(1) { -1.0 } else { 1.0 };
                        let extra = (spread.abs() / 360.0).floor() as i64;
                        let wrapped = (spread.abs() % 360.0) * spread.signum();
                        Some((
                            e["p"].as_i64() == Some(1),
                            e["e"].as_i64().unwrap_or(0) as i32,
                            Rotation { angle: (wrapped + num(e, "r")) * flip, direction: e["o"].as_i64().unwrap_or(0), loops: e["l"].as_i64().unwrap_or(0) + extra },
                        ))
                    });
                    out.rotations.extend(nodes.into_iter().map(|((axis, element), n)| ((g, axis, element), n)));
                }
                "FloatFxGroupEffect" => {
                    let Some(g) = group_of("FloatFxGroupEffectManager", c) else { continue };
                    let Some(groups) = vfx_groups.get(&g) else { continue };
                    let nodes = build(groups, count, "fx", "s", &|_| 0, &|e, b, order, spread| {
                        let affected = order != 0 || b["b"].as_i64() == Some(1);
                        Some((e["p"].as_i64() == Some(1), e["i"].as_i64().unwrap_or(0) as i32, num(e, "v") + if affected { spread } else { 0.0 }))
                    });
                    out.vfx.extend(nodes.into_iter().map(|((_, element), n)| ((g, element), n)));
                }
                "LightTranslationGroupEffect" => {
                    let Some(g) = group_of("LightTranslationGroupEffectManager", c) else { continue };
                    let Some(groups) = translation_groups.get(&g) else { continue };
                    let nodes = build(groups, count, "l", "s", &|b| b["a"].as_i64().unwrap_or(0), &|e, b, order, spread| {
                        let spread = if order == 0 && b["b"].as_i64() != Some(1) { 0.0 } else { spread };
                        let flip = if b["r"].as_i64() == Some(1) { -1.0 } else { 1.0 };
                        Some((e["p"].as_i64() == Some(1), e["e"].as_i64().unwrap_or(0) as i32, Translation { value: num(e, "t") * flip, spread: spread * flip }))
                    });
                    out.translations.extend(nodes.into_iter().map(|((axis, element), n)| ((g, axis, element), n)));
                }
                _ => {}
            }
        }
        out
    }

    /// RGBA of an element at `beat`; `scheme(color index)` is the environment color. `LightColorTween`.
    pub fn color(&self, group: i64, element: i64, beat: f64, scheme: &dyn Fn(i64) -> [f32; 4]) -> [f32; 4] {
        let Some(nodes) = self.colors.get(&(group, element)) else { return [0.0; 4] };
        let (start, end, easing, n, current) = span(nodes, beat, ColorValue::default());
        let rgb = |v: &ColorValue| v.custom.unwrap_or_else(|| scheme(v.color));
        let (a, b) = (rgb(&start), rgb(&end));
        let eased = ease(easing, n);
        let mix = |x: f32, y: f32, t: f32| x + (y - x) * t;
        let mut color: [f32; 4] = std::array::from_fn(|i| mix(a[i], b[i], eased));
        color[3] = mix(a[3], b[3], eased) * mix(start.brightness, end.brightness, eased);
        let frequency = |v: &ColorValue| if v.brightness <= 0.0 && v.strobe <= 0.0 { 0.0 } else { v.frequency };
        let (f0, f1, s1, end_a, fade) = if easing == -1 {
            (frequency(&start), frequency(&start), start.strobe, a, start.fade)
        } else {
            (frequency(&start), frequency(&end), end.strobe, b, end.fade)
        };
        if (f0 > 0.0 || f1 > 0.0) && let (Some(c), Some(next)) = (current, nodes.get(nodes.partition_point(|x| x.t <= beat))) {
            let duration = (next.t - nodes[c].t) as f32;
            let elapsed = n * duration;
            let half = elapsed * elapsed / (2.0 * duration);
            let phase = (-f0 * half + f0 * elapsed + f1 * half) % 1.0;
            let mut strobe: [f32; 4] = std::array::from_fn(|i| mix(a[i], end_a[i], eased));
            strobe[3] = mix(a[3] * start.strobe, end_a[3] * s1, eased);
            if fade {
                let t = 1.0 - (phase * 2.0 - 1.0).abs();
                let t = crate::eval::ease(Some("easeInOutCubic"), t);
                color = std::array::from_fn(|i| mix(color[i], strobe[i], t));
            } else if phase >= 0.5 {
                color = strobe;
            }
        }
        color
    }

    /// The color event an element is in at `beat`: its start and end beat (end: the next event) and
    /// brightness (fire effects start and stop on these).
    pub fn color_event(&self, group: i64, element: i64, beat: f64) -> Option<(f64, f64, f32)> {
        let nodes = self.colors.get(&(group, element))?;
        let i = nodes.partition_point(|n| n.t <= beat);
        let c = i.checked_sub(1)?;
        Some((nodes[c].t, nodes.get(i).map_or(f64::MAX, |n| n.t), nodes[c].value.brightness))
    }

    /// A VFX element's value at `beat` (0 before its events, as ChroMapper starts it).
    pub fn vfx(&self, group: i64, element: i64, beat: f64) -> Option<f32> {
        let nodes = self.vfx.get(&(group, element))?;
        let (start, end, easing, n, _) = span(nodes, beat, 0.0);
        Some(start + (end - start) * ease(easing, n))
    }

    /// The angle (degrees) a rotation element has at `beat`.
    pub fn rotation(&self, m: &GroupMove, beat: f64) -> Option<f32> {
        let nodes = self.rotations.get(&(m.group, m.axis, m.element))?;
        let (start, end, easing, n, current) = span(nodes, beat, Rotation::default());
        current?;
        let from = wrap(start.angle);
        let to = wrap(end.angle);
        let next_is_previous = nodes.get(nodes.partition_point(|x| x.t <= beat)).is_none_or(|x| x.use_previous);
        let loops = if next_is_previous { 0 } else { end.loops } as f32;
        let delta = delta_angle(from, to);
        let target = match end.direction {
            1 => (if delta >= 0.0 { from + delta } else { from + delta + 360.0 }) + loops * 360.0,
            2 => (if delta <= 0.0 { from + delta } else { from + delta - 360.0 }) - loops * 360.0,
            _ => from + delta + delta.signum() * loops * 360.0,
        };
        let v = from + (target - from) * ease(easing, n);
        Some(if m.mirrored { -v } else { v })
    }

    /// The local position along the axis a translation element has at `beat`.
    pub fn translation(&self, m: &GroupMove, beat: f64) -> Option<f32> {
        let (values, spreads) = m.limits?;
        let nodes = self.translations.get(&(m.group, m.axis, m.element))?;
        let (start, end, easing, n, current) = span(nodes, beat, Translation::default());
        current?;
        let sign = if m.mirrored { -1.0 } else { 1.0 };
        let compute = |t: Translation| {
            let a = (t.value * sign + 1.0) * 0.5;
            let d = (t.spread * sign + 1.0) * 0.5;
            values[0] + (values[1] - values[0]) * a + spreads[0] + (spreads[1] - spreads[0]) * d
        };
        let (a, b) = (compute(start), compute(end));
        Some(a + (b - a) * ease(easing, n))
    }

    fn read_fog(&mut self, json: &Value) {
        let root = if json.get("basicBeatmapEvents").is_some() { &json["customData"] } else { &json["_customData"] };
        for e in root["environment"].as_array().into_iter().flatten() {
            let fog = &e["components"]["BloomFogEnvironment"];
            if let Some(t) = fog["track"].as_str() {
                self.fog_tracks.insert(t.to_owned());
            }
            // Maps usually put the environment's root object on a track, which also holds the fog.
            if e["id"].as_str().is_some_and(|id| id.trim_end_matches('$').ends_with("Environment") && !id.contains(".[")) {
                for t in e["track"].as_str().into_iter().chain(e["track"].as_array().into_iter().flatten().filter_map(Value::as_str)) {
                    self.fog_tracks.insert(t.to_owned());
                }
            }
            for (name, _) in FOG_FIELDS {
                if let Some(v) = fog[name].as_f64() {
                    self.fog_base.insert(name, v as f32);
                }
            }
        }
    }

    /// The fog at `beat`: the environment's, then the map's static values, then its animations.
    pub fn fog(&self, bm: &Beatmap, beat: f64, base: FogParams) -> FogParams {
        let root = &bm.json[bm.names.custom];
        let current = |field: &'static str, legacy: &str, fallback: f32| -> f32 {
            let mut value = self.fog_base.get(field).copied().unwrap_or(fallback);
            for k in bm.rows.iter().filter(|r| r.kind == RowKind::Component && r.lane == FOG_LANE).flat_map(|r| &r.keys) {
                if k.beat > beat {
                    break;
                }
                let Some(doc) = &k.source else { continue };
                let (points, duration, easing, track) = if doc.get("d").is_some() {
                    let d = &doc["d"];
                    (&d["BloomFogEnvironment"][field], &d["duration"], &d["easing"], &d["track"])
                } else {
                    let d = &doc["_data"];
                    (&d[legacy], &d["_duration"], &d["_easing"], &d["_track"])
                };
                let owned = doc.get("_data").is_some() || track.as_str().is_some_and(|t| t == map::NEW_FOG_TRACK || self.fog_tracks.contains(t));
                if !owned || points.is_null() {
                    continue;
                }
                let span = duration.as_f64().unwrap_or(0.0);
                let t = if span <= 0.0 { 1.0 } else { ((beat - k.beat) / span).clamp(0.0, 1.0) as f32 };
                if let Some(v) = sample(points, crate::eval::ease(easing.as_str(), t), root, bm.names.version == "v3") {
                    value = v;
                }
            }
            value
        };
        FogParams {
            attenuation: current("attenuation", "_attenuation", base.attenuation),
            offset: current("offset", "_offset", base.offset),
            start_y: current("startY", "_startY", base.start_y),
            height: current("height", "_height", base.height),
            ..base
        }
    }
}

/// A 1-D Heck point list at `t`: `[v]`, `[[v, t, "ease"], …]`, or a point definition's name.
fn sample(points: &Value, t: f32, root: &Value, v3: bool) -> Option<f32> {
    if let Some(name) = points.as_str() {
        let defs = if v3 { &root["pointDefinitions"] } else { &root["_pointDefinitions"] };
        let found = defs.get(name).cloned().or_else(|| defs.as_array()?.iter().find(|d| d["_name"] == name).map(|d| d["_points"].clone()))?;
        return sample(&found, t, root, v3);
    }
    let list = points.as_array()?;
    if list.first()?.is_number() {
        return list[0].as_f64().map(|v| v as f32);
    }
    let entries: Vec<(f32, f32, Option<&str>)> = list
        .iter()
        .filter_map(|p| {
            let a = p.as_array()?;
            Some((a.first()?.as_f64()? as f32, a.get(1).and_then(Value::as_f64).unwrap_or(0.0) as f32, a.iter().filter_map(Value::as_str).find(|s| s.starts_with("ease"))))
        })
        .collect();
    let i = entries.partition_point(|e| e.1 <= t);
    if i == 0 {
        return entries.first().map(|e| e.0);
    }
    let a = entries[i - 1];
    let Some(b) = entries.get(i) else { return Some(a.0) };
    let n = crate::eval::ease(b.2, (t - a.1) / (b.1 - a.1).max(1e-6));
    Some(a.0 + (b.0 - a.0) * n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vfx_groups_tween_between_their_events() {
        let comps = vec![
            json!({ "type": "FloatFxGroupEffectManager", "id": 1, "fields": { "effectEntries": [{ "Group": 3, "Effect": { "id": 2 } }] } }),
            json!({ "type": "FloatFxGroupEffect", "id": 2, "fields": { "Count": 2 } }),
        ];
        let filter = json!({ "f": 1, "p": 1, "t": 0, "r": 0, "c": 0, "n": 0, "s": 0, "l": 0, "d": 0 });
        let map = json!({
            "vfxEventBoxGroups": [{ "b": 4.0, "g": 3, "e": [{ "f": filter, "w": 0, "d": 1, "s": 0.5, "t": 1, "b": 1, "i": 0, "l": [0, 1] }] }],
            "_fxEventsCollection": { "_fl": [{ "b": 0, "p": 0, "i": 0, "v": 0 }, { "b": 2, "p": 0, "i": 0, "v": 1 }], "_il": [] }
        });
        let g = GroupLights::new(&comps, &map);
        // Element 0 goes 0 → 1 over beats 4..6; element 1 gets the 0.5 value spread on both events.
        assert_eq!(g.vfx(3, 0, 3.0), Some(0.0));
        assert!((g.vfx(3, 0, 5.0).unwrap() - 0.5).abs() < 1e-5);
        assert!((g.vfx(3, 0, 7.0).unwrap() - 1.0).abs() < 1e-5);
        assert!((g.vfx(3, 1, 7.0).unwrap() - 1.5).abs() < 1e-5);
        assert_eq!(g.vfx(4, 0, 5.0), None);
    }

    #[test]
    fn random_filters_match_dotnet() {
        // Reference values from ChroMapper's ShuffleExtensions with System.Random (test/out/random-vectors.txt).
        let mut r = NetRandom::new(42);
        assert_eq!((0..6).map(|i| r.next(i + 3)).collect::<Vec<_>>(), [2, 0, 0, 3, 1, 2]);
        let mut r = NetRandom::new(-5);
        assert_eq!((0..6).map(|i| r.next(i + 3)).collect::<Vec<_>>(), [1, 1, 1, 3, 3, 7]);
        assert_eq!(shuffle(&(0..8).collect::<Vec<_>>(), 42), [4, 5, 3, 1, 7, 6, 0, 2]);
        assert_eq!(shuffle(&(0..13).collect::<Vec<_>>(), 42), [4, 8, 10, 11, 7, 6, 12, 9, 5, 2, 3, 1, 0]);
        let picked = |k, n, seed| pick_random(k, n, seed).iter().map(|v| v.unwrap_or(-1)).collect::<Vec<_>>();
        assert_eq!(picked(7, 13, 42), [-1, 1, 2, -1, 4, 5, -1, -1, 8, -1, 10, 11, -1]);
        assert_eq!(picked(4, 8, 12345), [0, 1, -1, -1, -1, -1, 6, 7]);
        assert_eq!(picked(1, 13, 7), [-1, -1, -1, 3, -1, -1, -1, -1, -1, -1, -1, -1, -1]);
    }
    use serde_json::json;

    fn filter(p: i64, t: i64, kind: i64) -> Value {
        json!({ "f": kind, "p": p, "t": t, "r": 0, "c": 0, "n": 0, "s": 0, "l": 0, "d": 0 })
    }

    #[test]
    fn division_picks_a_section() {
        let f = convert(&filter(2, 1, 1), 4).unwrap();
        assert_eq!(f.items().iter().map(|i| i.0).collect::<Vec<_>>(), [2, 3]);
    }

    #[test]
    fn step_and_offset_skips_elements() {
        let f = convert(&filter(1, 2, 2), 6).unwrap();
        assert_eq!(f.items().iter().map(|i| i.0).collect::<Vec<_>>(), [1, 3, 5]);
    }

    #[test]
    fn reverse_flips_the_order() {
        let mut v = filter(1, 0, 1);
        v["r"] = json!(1);
        let f = convert(&v, 3).unwrap();
        assert_eq!(f.items().iter().map(|i| i.0).collect::<Vec<_>>(), [2, 1, 0]);
    }

    #[test]
    fn invalid_filters_are_ignored() {
        assert!(convert(&filter(0, 0, 1), 4).is_none());
        assert!(convert(&filter(1, 0, 3), 4).is_none());
        assert!(convert(&filter(1, 0, 1), 0).is_none());
    }

    fn color_group(beat: f64, g: i64, events: Value, w: f64, spread: f64) -> Value {
        json!({ "b": beat, "g": g, "e": [{ "f": filter(1, 0, 1), "w": w, "d": 1, "r": spread, "t": 1, "b": 1, "i": 0, "e": events }] })
    }

    fn components() -> Vec<Value> {
        vec![
            json!({ "type": "LightColorGroupEffect", "id": 5, "fields": { "Count": 2 } }),
            json!({ "type": "LightColorGroupEffectManager", "id": 6, "fields": { "effectEntries": [{ "Group": 3, "Effect": { "id": 5 } }] } }),
        ]
    }

    #[test]
    fn color_fades_and_a_later_group_cuts_it_off() {
        let events = json!([{ "b": 0, "i": 0, "c": 1, "s": 0, "f": 0 }, { "b": 4, "i": 1, "c": 1, "s": 1, "f": 0 }]);
        let later = json!([{ "b": 0, "i": 0, "c": 0, "s": 0.5, "f": 0 }]);
        let json = json!({ "basicBeatmapEvents": [], "lightColorEventBoxGroups": [color_group(0.0, 3, events, 0.0, 0.0), color_group(2.0, 3, later, 0.0, 0.0)] });
        let lights = GroupLights::new(&components(), &json);
        let scheme = |c: i64| [c as f32, 0.0, 0.0, 1.0];
        assert_eq!(lights.color(3, 0, -1.0, &scheme), [0.0, 0.0, 0.0, 0.0]);
        // The fade is cut by the group at beat 2: its beat-4 target never happens.
        assert_eq!(lights.color(3, 0, 1.0, &scheme)[3], 0.0);
        assert_eq!(lights.color(3, 0, 3.0, &scheme), [0.0, 0.0, 0.0, 0.5]);
    }

    #[test]
    fn beat_spread_delays_later_elements() {
        let events = json!([{ "b": 0, "i": 0, "c": 1, "s": 1, "f": 0 }]);
        let json = json!({ "basicBeatmapEvents": [], "lightColorEventBoxGroups": [color_group(0.0, 3, events, 2.0, 0.0)] });
        let lights = GroupLights::new(&components(), &json);
        let scheme = |_: i64| [1.0, 1.0, 1.0, 1.0];
        assert_eq!(lights.color(3, 0, 1.0, &scheme)[3], 1.0);
        assert_eq!(lights.color(3, 1, 1.0, &scheme)[3], 0.0);
        assert_eq!(lights.color(3, 1, 2.0, &scheme)[3], 1.0);
    }

    #[test]
    fn brightness_spread_reaches_the_last_element() {
        let events = json!([{ "b": 0, "i": 0, "c": 1, "s": 0.5, "f": 0 }]);
        let json = json!({ "basicBeatmapEvents": [], "lightColorEventBoxGroups": [color_group(0.0, 3, events, 0.0, 0.25)] });
        let lights = GroupLights::new(&components(), &json);
        let scheme = |_: i64| [1.0, 1.0, 1.0, 1.0];
        assert_eq!(lights.color(3, 0, 1.0, &scheme)[3], 0.5);
        assert_eq!(lights.color(3, 1, 1.0, &scheme)[3], 0.75);
    }

    #[test]
    fn rotation_takes_the_short_way_and_counts_loops() {
        let comps = vec![
            json!({ "type": "LightRotationGroupEffect", "id": 1, "fields": { "Count": 1 } }),
            json!({ "type": "LightRotationGroupEffectManager", "id": 2, "fields": { "effectEntries": [{ "Group": 0, "Effect": { "id": 1 } }] } }),
        ];
        let events = json!([{ "b": 0, "p": 0, "e": -1, "l": 0, "r": 350, "o": 0 }, { "b": 2, "p": 0, "e": 0, "l": 0, "r": 10, "o": 0 }]);
        let box_ = json!({ "f": filter(1, 0, 1), "w": 0, "d": 1, "s": 0, "t": 1, "b": 0, "i": 0, "a": 1, "r": 0, "l": events });
        let json = json!({ "basicBeatmapEvents": [], "lightRotationEventBoxGroups": [{ "b": 0, "g": 0, "e": [box_] }] });
        let lights = GroupLights::new(&comps, &json);
        let m = GroupMove { group: 0, element: 0, axis: 1, mirrored: false, limits: None };
        // Halfway from 350 to 10 the short way is 360.
        assert!((lights.rotation(&m, 1.0).unwrap() - 360.0).abs() < 1e-3);
        assert!(lights.rotation(&m, -1.0).is_none());
    }

    #[test]
    fn fog_points_interpolate() {
        let root = json!({});
        let p = json!([[0.0, 0.0], [10.0, 1.0]]);
        assert_eq!(sample(&p, 0.5, &root, true), Some(5.0));
        assert_eq!(sample(&json!([7.0]), 0.3, &root, true), Some(7.0));
        assert_eq!(sample(&p, 2.0, &root, true), Some(10.0));
    }
}
