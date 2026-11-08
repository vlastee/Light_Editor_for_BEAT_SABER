//! Light colors as ChroMapper (0.14 dev) computes them, ported from its `BasicLightEffect`,
//! `LightColorTween` and `BasicEventColorLerp`: each event becomes a state that is linked to the
//! next one (transitions, flash/fade timing, off events inheriting the previous color), then the
//! state is tweened at the current beat.

use crate::environment::{EnvFile, LaneParams};
use crate::eval::ease;
use crate::map::{Base, Beatmap, Kind, MapColors, Row};
use serde_json::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// ChroMapper's `BasicEventLightIntensity`: normal (non-white, non-highlight) colors are dimmed.
const NORMAL_ALPHA: f32 = 0.749_019_6;
const BOOST_NORMAL_ALPHA: f32 = 0.8;
const FLASH_SECONDS: f64 = 0.6;
const FADE_SECONDS: f64 = 1.5;

#[derive(Clone, Copy, PartialEq)]
enum Lerp {
    Rgb,
    LegacyHsv,
    TrueHsv,
}

#[derive(Clone)]
enum Easing {
    Linear,
    CubicOut,
    ExpoOut,
    Named(String),
}

impl Easing {
    fn apply(&self, t: f32) -> f32 {
        match self {
            Easing::Linear => t,
            Easing::CubicOut => 1.0 - (1.0 - t).powi(3),
            Easing::ExpoOut => {
                if t >= 1.0 {
                    1.0
                } else {
                    1.0 - 2f32.powf(-10.0 * t)
                }
            }
            Easing::Named(n) => ease(Some(n), t),
        }
    }
}

/// One basic light event as ChroMapper sees it.
#[derive(Clone)]
struct Ev {
    beat: f64,
    order: u64,
    kind: Kind,
    color: Base,
    chroma: Option<[f32; 4]>,
    f: f32,
    easing: Option<String>,
    lerp: Lerp,
    /// `None`: all lights of the lane.
    ids: Option<Vec<i64>>,
    /// The event has its own Chroma color (a later legacy RGB event at the same beat keeps it).
    has_custom: bool,
}

/// A legacy Chroma `_lightGradient`: every non-off state of the lane that starts inside it takes
/// its colors on a separate color clock.
#[derive(Clone)]
struct Gradient {
    start: f64,
    end: f64,
    from: [f32; 4],
    to: [f32; 4],
    easing: String,
}

/// A legacy Chroma RGB event (value 2_000_000_000 + 0xRRGGBB, or the reset value): no state of
/// its own, it colors the lane's following events.
struct Lite {
    beat: f64,
    order: u64,
    color: Option<[f32; 4]>,
}

const RGB_OFFSET: i64 = 2_000_000_000;
const RGB_RESET: i64 = 1_900_000_001;

fn color_of(v: &Value) -> Option<[f32; 4]> {
    let a = v.as_array()?;
    let ch = |i: usize| a.get(i).and_then(Value::as_f64).map(|v| v as f32);
    Some([ch(0)?, ch(1)?, ch(2)?, ch(3).unwrap_or(1.0)])
}

#[derive(Clone)]
struct St {
    start: f64,
    /// Start of the color clock: `start`, or a gradient's start.
    start_color_t: f64,
    end_alpha_t: f64,
    end_color_t: f64,
    /// A gradient's easing for the color (the brightness keeps `easing`).
    color_easing: Option<Easing>,
    gradient: bool,
    start_color: Base,
    end_color: Base,
    start_chroma: Option<[f32; 4]>,
    end_chroma: Option<[f32; 4]>,
    start_alpha: f32,
    end_alpha: f32,
    easing: Easing,
    lerp: Lerp,
    kind: Kind,
}

#[derive(Clone, Copy)]
pub struct Palette {
    pub red: [f32; 4],
    pub blue: [f32; 4],
    pub white: [f32; 4],
    pub red_boost: [f32; 4],
    pub blue_boost: [f32; 4],
    pub white_boost: [f32; 4],
}

struct Lane {
    params: LaneParams,
    events: Vec<Ev>,
    /// Sorted by start time.
    gradients: Vec<Gradient>,
}

pub struct LightModel {
    lanes: HashMap<i64, Lane>,
    palette: Palette,
    flash_beats: f64,
    fade_beats: f64,
    boost: Vec<(f64, bool)>,
    cache: RefCell<HashMap<(i64, Vec<i64>), Rc<Vec<St>>>>,
}

fn ids_of(v: &Value) -> Vec<i64> {
    match v {
        Value::Array(a) => a.iter().filter_map(Value::as_i64).collect(),
        Value::Number(n) => n.as_i64().into_iter().collect(),
        _ => Vec::new(),
    }
}

fn inverse_lerp(a: f64, b: f64, v: f64) -> f32 {
    if (b - a).abs() < 1e-12 || !b.is_finite() {
        return 0.0;
    }
    ((v - a) / (b - a)).clamp(0.0, 1.0) as f32
}

fn rgb_to_hsv(c: [f32; 4]) -> [f32; 3] {
    let (r, g, b) = (c[0], c[1], c[2]);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let h = if d <= 0.0 {
        0.0
    } else if max == r {
        ((g - b) / d).rem_euclid(6.0) / 6.0
    } else if max == g {
        ((b - r) / d + 2.0) / 6.0
    } else {
        ((r - g) / d + 4.0) / 6.0
    };
    [h, if max <= 0.0 { 0.0 } else { d / max }, max]
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [f32; 3] {
    let h6 = h.rem_euclid(1.0) * 6.0;
    let c = v * s;
    let x = c * (1.0 - (h6 % 2.0 - 1.0).abs());
    let (r, g, b) = match h6 as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    [r + m, g + m, b + m]
}

fn interpolate(a: [f32; 4], b: [f32; 4], t: f32, lerp: Lerp) -> [f32; 4] {
    let alpha = a[3] + (b[3] - a[3]) * t;
    match lerp {
        Lerp::Rgb => std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t),
        Lerp::LegacyHsv | Lerp::TrueHsv => {
            let (ha, hb) = (rgb_to_hsv(a), rgb_to_hsv(b));
            let mut h0 = ha[0];
            if lerp == Lerp::TrueHsv {
                // The short way around the hue circle.
                if hb[0] - h0 > 0.5 {
                    h0 += 1.0;
                } else if h0 - hb[0] > 0.5 {
                    h0 -= 1.0;
                }
            }
            let rgb = hsv_to_rgb(
                h0 + (hb[0] - h0) * t,
                ha[1] + (hb[1] - ha[1]) * t,
                ha[2] + (hb[2] - ha[2]) * t,
            );
            [rgb[0], rgb[1], rgb[2], alpha]
        }
    }
}

fn base_state(e: &Ev, off: f32, flash: f64, fade: f64) -> St {
    let mut s = St {
        start: e.beat,
        start_color_t: e.beat,
        end_alpha_t: f64::MAX,
        end_color_t: f64::MAX,
        color_easing: None,
        gradient: false,
        start_color: e.color,
        end_color: e.color,
        start_chroma: e.chroma,
        end_chroma: e.chroma,
        start_alpha: e.f,
        end_alpha: e.f,
        easing: Easing::Linear,
        lerp: Lerp::Rgb,
        kind: e.kind,
    };
    match e.kind {
        Kind::Off => s.start_alpha = e.f * off,
        Kind::Flash => {
            s.end_alpha_t = e.beat + flash;
            s.end_color_t = s.end_alpha_t;
            s.easing = Easing::CubicOut;
        }
        Kind::Fade => {
            s.end_alpha_t = e.beat + fade;
            s.end_color_t = s.end_alpha_t;
            s.end_alpha = e.f * off;
            s.easing = Easing::ExpoOut;
        }
        _ => {}
    }
    if e.kind == Kind::Off {
        s.end_alpha = s.start_alpha;
    }
    s
}

/// ChroMapper's ChromaLite as it inserts events in time order (`InsertData` and
/// `UpdateExistingWithChromaLite`), including its `SortedBucketArray.EnumerateFrom` bug: with no state at
/// exactly the lite's beat it enumerates from the start of the lite's 10-beat bucket.
fn cm_apply_lites(events: &mut [Ev], lites: &[Lite]) {
    const BUCKET: f64 = 10.0;
    let (mut i, mut last): (usize, Option<&Lite>) = (0, None);
    for l in lites {
        // Events before the lite are inserted first and take the latest lite so far.
        while i < events.len() && (events[i].beat, events[i].order) < (l.beat, l.order) {
            if let Some(p) = last {
                events[i].chroma = p.color;
            }
            i += 1;
        }
        let at = events[..i].partition_point(|e| e.beat < l.beat);
        let exact = events[at..i].first().is_some_and(|e| e.beat == l.beat);
        let from = if exact { l.beat } else { (l.beat / BUCKET).floor() * BUCKET };
        let start = events[..i].partition_point(|e| e.beat < from);
        for e in events[start..i].iter_mut().filter(|e| !e.has_custom) {
            e.chroma = l.color;
        }
        last = Some(l);
    }
    if let Some(p) = last {
        for e in &mut events[i..] {
            e.chroma = p.color;
        }
    }
}

fn valid_transition_source(k: Kind) -> bool {
    matches!(k, Kind::On | Kind::Off | Kind::Transition)
}

/// ChroMapper's `OnInsertUpdateToPreviousState` as events are inserted in time order.
fn link(prev: &mut St, prev_ev: Option<&Ev>, new: &mut St, off: f32) {
    let prev_f = prev_ev.map_or(1.0, |e| e.f);
    if new.kind == Kind::Transition && valid_transition_source(prev.kind) {
        if prev.kind == Kind::Off {
            prev.start_color = new.start_color;
        }
        prev.end_alpha_t = new.start;
        prev.end_color_t = new.start;
        prev.end_color = new.start_color;
        prev.end_chroma = new.start_chroma;
        prev.end_alpha = new.start_alpha;
        // The transition's interpolation is serialized on the preceding event.
        prev.easing = Easing::Named(prev_ev.and_then(|e| e.easing.clone()).unwrap_or_else(|| "easeLinear".into()));
        prev.lerp = prev_ev.map_or(Lerp::Rgb, |e| e.lerp);
        return;
    }
    let highlight = matches!(prev.kind, Kind::Fade | Kind::Flash);
    if !highlight && !prev.gradient {
        prev.end_color_t = new.start_color_t;
        prev.end_chroma = prev.start_chroma;
    }
    prev.end_color = prev.start_color;
    if !highlight {
        prev.end_alpha_t = new.start;
        prev.end_alpha = prev.start_alpha;
    }
    if prev.kind == Kind::Off {
        prev.start_alpha = prev_f * off;
        prev.end_alpha = prev.start_alpha;
    }
    if new.kind == Kind::Off {
        new.start_color = prev.end_color;
    }
}

impl LightModel {
    /// `colors`: the light colors the map overrides.
    pub fn build(bm: &Beatmap, env: Option<&EnvFile>, colors: &MapColors, bpm: f64) -> LightModel {
        Self::build_with(bm, env, colors, bpm, false)
    }

    /// `cm_quirks`: reproduce ChroMapper's own bugs (only for comparing against it; the game doesn't have
    /// them): a legacy RGB event with no event at exactly its beat recolors from the start of its 10-beat
    /// state bucket, so it also colors earlier events in that bucket.
    pub fn build_with(bm: &Beatmap, env: Option<&EnvFile>, colors: &MapColors, bpm: f64, cm_quirks: bool) -> LightModel {
        let defaults = crate::map::DEFAULT_BASE_COLORS;
        let red = colors.left.or(env.map(|e| e.red)).unwrap_or(defaults[1]);
        let blue = colors.right.or(env.map(|e| e.blue)).unwrap_or(defaults[0]);
        let white = colors.white.or(env.map(|e| e.white)).unwrap_or(defaults[2]);
        let palette = Palette {
            red,
            blue,
            white,
            red_boost: colors.left_boost.or(env.and_then(|e| e.red_boost)).unwrap_or(red),
            blue_boost: colors.right_boost.or(env.and_then(|e| e.blue_boost)).unwrap_or(blue),
            white_boost: colors.white_boost.or(env.and_then(|e| e.white_boost)).unwrap_or(white),
        };
        let mut lanes: HashMap<i64, Lane> = HashMap::new();
        let mut lites: HashMap<i64, Vec<Lite>> = HashMap::new();
        for row in &bm.rows {
            let ids = row.light_ids.as_ref().map(ids_of);
            for k in &row.keys {
                let lerp = if k.lerp_hsv {
                    Lerp::LegacyHsv
                } else if k.extra_custom.get("lerpType").or(k.extra_custom.get("_lerpType")).and_then(Value::as_str)
                    == Some("trueHSV")
                {
                    Lerp::TrueHsv
                } else {
                    Lerp::Rgb
                };
                let lane = lanes.entry(row.lane).or_insert_with(|| Lane {
                    params: env
                        .and_then(|e| e.lanes.iter().find(|l| l.lane == row.lane).copied())
                        .unwrap_or(LaneParams { lane: row.lane, ..LaneParams::default() }),
                    events: Vec::new(),
                    gradients: Vec::new(),
                });
                // ChroMapper and the game keep beats as f32.
                let beat = k.beat as f32 as f64;
                match k.raw_value {
                    Some(v) if v >= RGB_OFFSET || v == RGB_RESET => {
                        let color = (v >= RGB_OFFSET).then(|| {
                            let rgb = v - RGB_OFFSET;
                            let ch = |s: i64| ((rgb >> s) & 0xFF) as f32 / 255.0;
                            [ch(16), ch(8), ch(0), 1.0]
                        });
                        lites.entry(row.lane).or_default().push(Lite { beat, order: k.id, color });
                        continue;
                    }
                    _ => {}
                }
                if let Some(g) = k.extra_custom.get("_lightGradient").filter(|g| g.is_object()) {
                    if let (Some(from), Some(to)) = (color_of(&g["_startColor"]), color_of(&g["_endColor"])) {
                        lane.gradients.push(Gradient {
                            start: beat,
                            end: beat + g["_duration"].as_f64().unwrap_or(0.0) as f32 as f64,
                            from,
                            to,
                            easing: g["_easing"].as_str().unwrap_or("easeLinear").to_owned(),
                        });
                    }
                }
                lane.events.push(Ev {
                    beat,
                    order: k.id,
                    kind: k.kind,
                    // ChroMapper: blue for 1-4, red for 5-8, white otherwise (including off).
                    color: if k.kind == Kind::Off { Base::White } else { k.base },
                    // White events ignore their Chroma color.
                    chroma: if k.kind != Kind::Off && k.base == Base::White { None } else { k.color },
                    f: k.brightness,
                    easing: k.easing.clone(),
                    lerp,
                    ids: ids.clone(),
                    has_custom: k.color.is_some(),
                });
            }
        }
        for (lane_type, lane) in lanes.iter_mut() {
            lane.events.sort_by(|a, b| a.beat.total_cmp(&b.beat).then(a.order.cmp(&b.order)));
            lane.gradients.sort_by(|a, b| a.start.total_cmp(&b.start));
            // A legacy RGB event colors the events after it (replacing their own color), and
            // same-beat events before it that have no color of their own.
            if let Some(ls) = lites.get_mut(lane_type) {
                ls.sort_by(|a, b| a.beat.total_cmp(&b.beat).then(a.order.cmp(&b.order)));
                if cm_quirks {
                    cm_apply_lites(&mut lane.events, ls);
                    continue;
                }
                for e in &mut lane.events {
                    let applies = |l: &&Lite| l.beat < e.beat || (l.beat == e.beat && (l.order < e.order || !e.has_custom));
                    if let Some(l) = ls.iter().rev().find(applies) {
                        e.chroma = l.color;
                    }
                }
            }
        }
        let mut boost: Vec<(f64, bool)> = Vec::new();
        for e in bm.json["colorBoostBeatmapEvents"].as_array().into_iter().flatten() {
            boost.push((e["b"].as_f64().unwrap_or(0.0) as f32 as f64, e["o"].as_bool().unwrap_or(false)));
        }
        for e in bm.json["_events"].as_array().into_iter().flatten() {
            if e["_type"].as_i64() == Some(5) {
                boost.push((e["_time"].as_f64().unwrap_or(0.0) as f32 as f64, e["_value"].as_i64() == Some(1)));
            }
        }
        boost.sort_by(|a, b| a.0.total_cmp(&b.0));
        LightModel {
            lanes,
            palette,
            flash_beats: FLASH_SECONDS * bpm / 60.0,
            fade_beats: FADE_SECONDS * bpm / 60.0,
            boost,
            cache: RefCell::new(HashMap::new()),
        }
    }

    /// `keys`: the map light IDs that reach the light (empty: only events for the whole lane).
    fn states(&self, lane: i64, keys: &[i64]) -> Option<Rc<Vec<St>>> {
        if let Some(s) = self.cache.borrow().get(&(lane, keys.to_vec())) {
            return Some(s.clone());
        }
        let l = self.lanes.get(&lane)?;
        let off = l.params.off;
        let evs: Vec<&Ev> = l
            .events
            .iter()
            .filter(|e| match &e.ids {
                None => true,
                Some(ids) => keys.iter().any(|k| ids.contains(k)),
            })
            .collect();
        // Before the first event: off, or at the off brightness for LightOnStart lanes.
        let mut states = vec![St {
            start: f64::MIN,
            start_color_t: f64::MIN,
            end_alpha_t: f64::MAX,
            end_color_t: f64::MAX,
            color_easing: None,
            gradient: false,
            start_color: Base::Red,
            end_color: Base::Red,
            start_chroma: None,
            end_chroma: None,
            start_alpha: if l.params.on_start { off } else { 0.0 },
            end_alpha: if l.params.on_start { off } else { 0.0 },
            easing: Easing::Linear,
            lerp: Lerp::Rgb,
            kind: Kind::Off,
        }];
        let mut prev_ev: Option<&Ev> = None;
        for e in evs {
            let mut new = base_state(e, off, self.flash_beats, self.fade_beats);
            if e.kind != Kind::Off {
                if let Some(g) = l.gradients.iter().rev().find(|g| g.start <= e.beat && e.beat <= g.end) {
                    new.start_color_t = g.start;
                    new.end_color_t = g.end;
                    new.start_chroma = Some(g.from);
                    new.end_chroma = Some(g.to);
                    new.color_easing = Some(Easing::Named(g.easing.clone()));
                    new.lerp = Lerp::Rgb;
                    new.gradient = true;
                }
            }
            let last = states.len() - 1;
            link(&mut states[last], prev_ev, &mut new, off);
            if prev_ev.is_none() && states[last].kind == Kind::Off {
                // The initial state keeps its own brightness.
                let a = if l.params.on_start { off } else { 0.0 };
                if new.kind != Kind::Transition {
                    states[last].start_alpha = a;
                    states[last].end_alpha = a;
                } else {
                    states[last].start_alpha = a;
                }
            }
            states.push(new);
            prev_ev = Some(e);
        }
        let rc = Rc::new(states);
        self.cache.borrow_mut().insert((lane, keys.to_vec()), rc.clone());
        Some(rc)
    }

    fn boosted(&self, beat: f64) -> bool {
        let i = self.boost.partition_point(|b| b.0 <= beat);
        i > 0 && self.boost[i - 1].1
    }

    fn scheme(&self, c: Base, invert: bool, boost: bool) -> [f32; 4] {
        let p = &self.palette;
        let (red, blue, white) = if boost {
            (p.red_boost, p.blue_boost, p.white_boost)
        } else {
            (p.red, p.blue, p.white)
        };
        match (c, invert) {
            (Base::Blue, false) | (Base::Red, true) => blue,
            (Base::Red, false) | (Base::Blue, true) => red,
            (Base::White, _) => white,
        }
    }

    /// ChroMapper's `GetPreviewLightColor`.
    fn preview_color(&self, s: &St, end: bool, params: &LaneParams, boost: bool) -> [f32; 4] {
        let lc = if end { s.end_color } else { s.start_color };
        let mut c = (if end { s.end_chroma } else { s.start_chroma })
            .unwrap_or_else(|| self.scheme(lc, params.invert, boost));
        let is_off = if end { s.kind == Kind::Fade } else { s.kind == Kind::Off };
        let highlight = !end && matches!(s.kind, Kind::Fade | Kind::Flash);
        if is_off {
            c[3] = 1.0;
        } else if !params.highlight_matches && lc != Base::White && !highlight {
            c[3] *= if boost { BOOST_NORMAL_ALPHA } else { NORMAL_ALPHA };
        }
        c
    }

    /// The light's color (RGBA, alpha = brightness) at `beat`. `keys`: the map light IDs that reach
    /// the light (several for remapped lights, none for a light only whole-lane events reach).
    pub fn color_keys(&self, lane: i64, keys: &[i64], beat: f64) -> [f32; 4] {
        let Some(states) = self.states(lane, keys) else {
            return [0.0; 4];
        };
        let Some(params) = self.lanes.get(&lane).map(|l| l.params) else {
            return [0.0; 4];
        };
        let i = states.partition_point(|s| s.start <= beat).max(1) - 1;
        let s = &states[i];
        let boost = self.boosted(beat);
        let n_alpha = inverse_lerp(s.start, s.end_alpha_t, beat);
        let n_color = inverse_lerp(s.start_color_t, s.end_color_t, beat);
        let eased_color = s.color_easing.as_ref().unwrap_or(&s.easing).apply(n_color);
        let compose = !s.gradient
            && (s.start_color_t - s.start).abs() < 1e-6
            && (s.end_color_t - s.end_alpha_t).abs() < 1e-6;
        let a = self.preview_color(s, false, &params, boost);
        let b = self.preview_color(s, true, &params, boost);
        if compose {
            let mut a = a;
            let mut b = b;
            a[3] *= s.start_alpha;
            b[3] *= s.end_alpha;
            interpolate(a, b, eased_color, s.lerp)
        } else {
            let mut c = interpolate(a, b, eased_color, s.lerp);
            c[3] *= s.start_alpha + (s.end_alpha - s.start_alpha) * s.easing.apply(n_alpha);
            c
        }
    }

    /// Displayed color: RGB scaled by brightness, clamped to 0..1.
    pub fn rgb(&self, lane: i64, key: Option<i64>, beat: f64) -> [f32; 3] {
        self.rgb_keys(lane, key.as_slice(), beat)
    }

    pub fn rgb_keys(&self, lane: i64, keys: &[i64], beat: f64) -> [f32; 3] {
        let c = self.color_keys(lane, keys, beat);
        let a = c[3].max(0.0);
        [c[0] * a, c[1] * a, c[2] * a].map(|v| v.clamp(0.0, 1.0))
    }

    /// For a timeline row: its first light ID, or untargeted lights for "all lights" rows.
    pub fn row_rgb(&self, row: &Row, beat: f64) -> [f32; 3] {
        let key = row.light_ids.as_ref().and_then(|v| ids_of(v).first().copied());
        self.rgb(row.lane, key, beat)
    }
}
