//! Loading and saving Beat Saber maps (v3 difficulty files) as keyframe rows.

use crate::scene::Quat;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Event types that are light lanes. Everything else in `basicBeatmapEvents` is kept untouched.
pub const LIGHT_LANES: &[(i64, &str)] = &[
    (0, "Back Lasers"),
    (1, "Ring Lights"),
    (2, "Left Lasers"),
    (3, "Right Lasers"),
    (4, "Center Lights"),
    (6, "Extra Left"),
    (7, "Extra Right"),
    (10, "Left Lasers 2"),
    (11, "Right Lasers 2"),
];

/// Other event types that change how the lights look or move. Each is one timeline row.
pub const EFFECT_LANES: &[(i64, &str)] = &[
    (5, "Color Boost"),
    (8, "Ring Spin"),
    (9, "Ring Zoom"),
    (12, "Left Laser Speed"),
    (13, "Right Laser Speed"),
    (16, "Special 16"),
    (17, "Special 17"),
    (18, "Special 18"),
    (19, "Special 19"),
    (40, "Special 40"),
    (41, "Special 41"),
    (42, "Special 42"),
    (43, "Special 43"),
];

/// Effect lanes always offered; the Special ones only appear in maps that use them.
const COMMON_EFFECTS: [i64; 5] = [5, 8, 9, 12, 13];

/// Top-to-bottom order of the lanes on the timeline.
pub const LANE_ORDER: &[i64] = &[5, 0, 1, 8, 9, 2, 12, 3, 13, 4, 6, 7, 10, 11, 16, 17, 18, 19, 40, 41, 42, 43];

pub fn is_effect_lane(et: i64) -> bool {
    EFFECT_LANES.iter().any(|(t, _)| *t == et)
}

pub fn lane_name(et: i64) -> &'static str {
    LIGHT_LANES
        .iter()
        .chain(EFFECT_LANES)
        .find(|(t, _)| *t == et)
        .map_or("Unknown", |(_, n)| n)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Off,
    On,
    Flash,
    Fade,
    Transition,
}

impl Kind {
    pub const ALL: [Kind; 5] = [Kind::Off, Kind::On, Kind::Flash, Kind::Fade, Kind::Transition];

    pub fn label(self) -> &'static str {
        match self {
            Kind::Off => "Off",
            Kind::On => "On",
            Kind::Flash => "Flash",
            Kind::Fade => "Fade",
            Kind::Transition => "Transition",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Base {
    Blue,
    Red,
    White,
}

impl Base {
    pub const ALL: [Base; 3] = [Base::Blue, Base::Red, Base::White];

    pub fn label(self) -> &'static str {
        match self {
            Base::Blue => "Blue",
            Base::Red => "Red",
            Base::White => "White",
        }
    }

    /// Approximate default environment colors, used when an event has no Chroma color.
    /// The environment's color for events without a Chroma color (see [`set_base_colors`]).
    pub fn rgba(self) -> [f32; 4] {
        let c = BASE_COLORS.read().map_or(DEFAULT_BASE_COLORS, |c| *c);
        match self {
            Base::Blue => c[0],
            Base::Red => c[1],
            Base::White => c[2],
        }
    }
}

/// Beat Saber's default scheme: right (blue) and left (red) environment colors, and white.
pub const DEFAULT_BASE_COLORS: [[f32; 4]; 3] = [
    [0.188, 0.675, 1.0, 1.0],
    [0.85, 0.085, 0.085, 1.0],
    [1.0, 1.0, 1.0, 1.0],
];

static BASE_COLORS: std::sync::RwLock<[[f32; 4]; 3]> = std::sync::RwLock::new(DEFAULT_BASE_COLORS);

pub fn set_base_colors(blue: [f32; 4], red: [f32; 4]) {
    if let Ok(mut c) = BASE_COLORS.write() {
        *c = [blue, red, DEFAULT_BASE_COLORS[2]];
    }
}

/// Splits a v3 event value into (kind, base color). Values: 0 off, 1-4 blue, 5-8 red, 9-12 white.
fn decode_value(i: i64) -> (Kind, Base) {
    if i <= 0 || i > 12 {
        return (Kind::Off, Base::Blue);
    }
    let base = match (i - 1) / 4 {
        0 => Base::Blue,
        1 => Base::Red,
        _ => Base::White,
    };
    let kind = match (i - 1) % 4 {
        0 => Kind::On,
        1 => Kind::Flash,
        2 => Kind::Fade,
        _ => Kind::Transition,
    };
    (kind, base)
}

fn encode_value(kind: Kind, base: Base) -> i64 {
    let offset = match base {
        Base::Blue => 0,
        Base::Red => 4,
        Base::White => 8,
    };
    match kind {
        Kind::Off => 0,
        Kind::On => offset + 1,
        Kind::Flash => offset + 2,
        Kind::Fade => offset + 3,
        Kind::Transition => offset + 4,
    }
}

#[derive(Clone, Debug)]
pub struct Key {
    pub id: u64,
    pub beat: f64,
    pub kind: Kind,
    pub base: Base,
    /// Chroma color (RGBA). `None` means the environment's default color for `base`.
    pub color: Option<[f32; 4]>,
    /// The original color had no alpha channel; written back as `[r, g, b]` while alpha stays 1.
    pub color_rgb_only: bool,
    pub brightness: f32,
    /// Original `i` value when it isn't a plain 0-12 light value (e.g. legacy Chroma RGB
    /// packed into the value). Written back unchanged until the type or base color is edited.
    pub raw_value: Option<i64>,
    /// Original JSON for color and brightness, written back as-is so untouched values stay
    /// exact. Cleared when the field is edited.
    pub color_src: Option<Value>,
    pub brightness_src: Option<Value>,
    pub easing: Option<String>,
    pub lerp_hsv: bool,
    /// Fields we don't edit, written back as they were.
    pub extra: Map<String, Value>,
    pub extra_custom: Map<String, Value>,
    /// On a Position line: how far the lights are moved from where they normally are (metres).
    /// On an Aim line: how far they're turned from how they normally point (degrees: X, Y, Z).
    pub pos: Option<[f32; 3]>,
    /// On an effect row (boost, ring, laser speed…); `raw_value` holds its value.
    pub effect: bool,
    /// An effect event as it was in the file, written back unchanged until it's edited.
    pub source: Option<Value>,
}

/// Legacy Chroma stored RGB in the value itself: 2_000_000_000 + 0xRRGGBB.
fn legacy_rgb(raw: i64) -> Option<[f32; 4]> {
    let rgb = raw.checked_sub(2_000_000_000).filter(|v| (0..=0xFF_FFFF).contains(v))?;
    let ch = |shift: i64| ((rgb >> shift) & 0xFF) as f32 / 255.0;
    Some([ch(16), ch(8), ch(0), 1.0])
}

impl Key {
    /// A keyframe on a Position or Aim line.
    pub fn line(kind: RowKind, id: u64, beat: f64, pos: [f32; 3], easing: Option<String>) -> Key {
        Key {
            id,
            beat,
            kind: Kind::Transition,
            base: Base::Blue,
            color: Some(if kind == RowKind::Aim { [1.0, 0.7, 0.3, 1.0] } else { [0.35, 0.85, 0.9, 1.0] }),
            color_rgb_only: false,
            brightness: 1.0,
            raw_value: None,
            color_src: None,
            brightness_src: None,
            easing,
            lerp_hsv: false,
            extra: Map::new(),
            extra_custom: Map::new(),
            pos: Some(pos),
            effect: false,
            source: None,
        }
    }

    /// A keyframe on an effect row.
    pub fn effect(id: u64, beat: f64, lane: i64, value: i64) -> Key {
        Key {
            id,
            beat,
            kind: Key::effect_shape(lane, value),
            base: Base::Blue,
            color: Some([0.95, 0.85, 0.45, 1.0]),
            color_rgb_only: false,
            brightness: 1.0,
            raw_value: Some(value),
            color_src: None,
            brightness_src: None,
            easing: None,
            lerp_hsv: false,
            extra: Map::new(),
            extra_custom: Map::new(),
            pos: None,
            effect: true,
            source: None,
        }
    }

    /// Off-shaped when the value switches the effect off; ring events are just triggers.
    pub fn effect_shape(lane: i64, value: i64) -> Kind {
        if value == 0 && matches!(lane, 5 | 12 | 13) { Kind::Off } else { Kind::On }
    }

    pub fn display_color(&self) -> [f32; 4] {
        self.color
            .or_else(|| self.raw_value.and_then(legacy_rgb))
            .unwrap_or_else(|| self.base.rgba())
    }
}

/// Where the editor keeps Position lines in a difficulty's custom data.
const POSITIONS_KEY: &str = "bsltPositions";
/// Chroma tracks the editor generates (replaced on every save).
pub const TRACK_PREFIX: &str = "bslt_pos_";

/// A light's GameObject and where it normally is (world position, metres).
pub struct MoveTarget {
    pub path: String,
    pub rest: [f32; 3],
    /// Its normal world rotation (columns).
    pub rot: [[f32; 3]; 3],
}

/// A Position line's movable lights, and how many of its lights can't be addressed.
pub type Resolve<'a> = dyn Fn(&Row) -> (Vec<MoveTarget>, usize) + 'a;

/// One timeline row: a light lane, optionally limited to a set of Chroma light IDs.
#[derive(Clone, Debug)]
pub struct Row {
    pub lane: i64,
    pub light_ids: Option<Value>,
    pub keys: Vec<Key>,
    pub kind: RowKind,
}

/// What a timeline row does. Position and Aim lines act on the lights of the light row above them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowKind {
    Light,
    Position,
    Aim,
    /// Non-light events: boost, ring spin and zoom, laser speed, special events.
    Effect,
}

impl Row {
    pub fn is_light(&self) -> bool {
        self.kind == RowKind::Light
    }

    pub fn is_motion(&self) -> bool {
        matches!(self.kind, RowKind::Position | RowKind::Aim)
    }

    pub fn is_effect(&self) -> bool {
        self.kind == RowKind::Effect
    }

    pub fn label(&self) -> String {
        match self.kind {
            RowKind::Position => return "Position".to_owned(),
            RowKind::Aim => return "Aim".to_owned(),
            RowKind::Effect => return lane_name(self.lane).to_owned(),
            RowKind::Light => {}
        }
        match &self.light_ids {
            None => "all lights".to_owned(),
            Some(Value::Array(ids)) => {
                let shown: Vec<String> = ids.iter().take(6).map(|v| v.to_string()).collect();
                if ids.len() > 6 {
                    format!("IDs {}… ({})", shown.join(","), ids.len())
                } else {
                    format!("IDs {}", shown.join(","))
                }
            }
            Some(v) => format!("ID {v}"),
        }
    }

    /// Whether this row addresses a light of `lane` reached by the map light IDs `ids`.
    pub fn reaches(&self, lane: i64, ids: &[i64]) -> bool {
        lane == self.lane
            && match &self.light_ids {
                None => true,
                Some(Value::Array(a)) if a.is_empty() => true,
                Some(Value::Array(a)) => a.iter().filter_map(Value::as_i64).any(|i| ids.contains(&i)),
                Some(v) => v.as_i64().is_some_and(|i| ids.contains(&i)),
            }
    }

    /// The keys around `beat` on a Position or Aim line and the eased fraction between them.
    /// `None` before the line starts; no second key after its last key.
    fn segment(&self, beat: f64) -> Option<([f32; 3], Option<[f32; 3]>, f32)> {
        let i = self.keys.partition_point(|k| k.beat <= beat);
        let a = &self.keys[i.checked_sub(1)?];
        let from = a.pos.unwrap_or([0.0; 3]);
        let Some(b) = self.keys.get(i) else {
            return Some((from, None, 0.0));
        };
        let t = crate::eval::ease(a.easing.as_deref(), ((beat - a.beat) / (b.beat - a.beat)) as f32);
        Some((from, Some(b.pos.unwrap_or([0.0; 3])), t))
    }

    /// A line's values at `beat`, blended per axis. The lights stay put before the first key and
    /// hold the last key after it; the earlier key's easing shapes each move.
    pub fn position_at(&self, beat: f64) -> [f32; 3] {
        match self.segment(beat) {
            None => [0.0; 3],
            Some((from, None, _)) => from,
            Some((from, Some(to), t)) => std::array::from_fn(|k| from[k] + (to[k] - from[k]) * t),
        }
    }

    /// An Aim line's turn at `beat` (`None` before it starts). Blended the way the game blends
    /// rotation points: the short way around.
    pub fn aim_at(&self, beat: f64) -> Option<Quat> {
        let (from, to, t) = self.segment(beat)?;
        let a = Quat::euler(from);
        Some(to.map_or(a, |to| a.slerp(Quat::euler(to), t)))
    }

    pub fn sort(&mut self) {
        self.keys.sort_by(|a, b| a.beat.total_cmp(&b.beat));
    }
}

pub struct Difficulty {
    pub name: String,
    pub file: String,
    pub environment: String,
    /// Light colors the map overrides (difficulty custom data or an Info color scheme).
    pub colors: MapColors,
    /// `colors.left` / `colors.right`, kept for the timeline's base colors.
    pub env_left: Option<[f32; 4]>,
    pub env_right: Option<[f32; 4]>,
}

/// Environment light colors a map overrides, resolved like ChroMapper's `PopulateColorsFromMapInfo`.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct MapColors {
    pub left: Option<[f32; 4]>,
    pub right: Option<[f32; 4]>,
    pub white: Option<[f32; 4]>,
    pub left_boost: Option<[f32; 4]>,
    pub right_boost: Option<[f32; 4]>,
    pub white_boost: Option<[f32; 4]>,
}

impl MapColors {
    /// `cd`: the difficulty's custom data; `scheme`: the Info color scheme it uses, if that
    /// scheme overrides colors.
    fn resolve(cd: &Value, scheme: Option<&Value>) -> MapColors {
        let c = |k: &str| rgb_obj(&cd[k]);
        let s = |k: &str| scheme.and_then(|s| rgb_obj(&s[k]));
        let (note_l, note_r) = (c("_colorLeft"), c("_colorRight"));
        let (env_l, env_r) = (c("_envColorLeft"), c("_envColorRight"));
        MapColors {
            left: env_l.or(note_l).or(s("environmentColor0")),
            right: env_r.or(note_r).or(s("environmentColor1")),
            white: c("_envColorWhite").or(s("environmentColorW")),
            left_boost: c("_envColorLeftBoost").or(env_l).or(note_l).or(s("environmentColor0Boost")),
            right_boost: c("_envColorRightBoost").or(env_r).or(note_r).or(s("environmentColor1Boost")),
            white_boost: c("_envColorWhiteBoost").or(s("environmentColorWBoost")),
        }
    }
}

fn rgb_obj(v: &Value) -> Option<[f32; 4]> {
    let c = |k: &str, d: f64| v.get(k).and_then(Value::as_f64).unwrap_or(d) as f32;
    v.is_object().then(|| [c("r", 0.0), c("g", 0.0), c("b", 0.0), c("a", 1.0)])
}

pub struct LoadedMap {
    pub folder: PathBuf,
    pub song_name: String,
    pub bpm: f64,
    pub song_file: PathBuf,
    pub difficulties: Vec<Difficulty>,
    pub warnings: Vec<String>,
}

/// JSON field names for the difficulty formats we can edit.
pub struct Names {
    pub version: &'static str,
    pub list: &'static str,
    b: &'static str,
    et: &'static str,
    i: &'static str,
    f: &'static str,
    custom: &'static str,
    light_id: &'static str,
    color: &'static str,
    easing: &'static str,
    lerp: &'static str,
}

pub const V3: Names = Names {
    version: "v3",
    list: "basicBeatmapEvents",
    b: "b",
    et: "et",
    i: "i",
    f: "f",
    custom: "customData",
    light_id: "lightID",
    color: "color",
    easing: "easing",
    lerp: "lerpType",
};

pub const V2: Names = Names {
    version: "v2",
    list: "_events",
    b: "_time",
    et: "_type",
    i: "_value",
    f: "_floatValue",
    custom: "_customData",
    light_id: "_lightID",
    color: "_color",
    easing: "_easing",
    lerp: "_lerpType",
};

pub struct Beatmap {
    pub path: PathBuf,
    pub names: &'static Names,
    pub json: Value,
    /// Non-light basic events, kept verbatim.
    pub other_events: Vec<Value>,
    pub rows: Vec<Row>,
    pub backed_up: bool,
}

fn read_json(path: &Path) -> Result<Value, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("Can't read {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("Bad JSON in {}: {e}", path.display()))
}

pub fn load_info(folder: &Path) -> Result<LoadedMap, String> {
    let info_path = ["Info.dat", "info.dat"]
        .iter()
        .map(|n| folder.join(n))
        .find(|p| p.exists())
        .ok_or_else(|| format!("No Info.dat in {}", folder.display()))?;
    let info = read_json(&info_path)?;
    let mut difficulties = Vec::new();
    let mut warnings = Vec::new();

    let (song_name, bpm, song_file) = if info.get("_version").is_some() {
        // v2 info format
        for set in info["_difficultyBeatmapSets"].as_array().into_iter().flatten() {
            let ch = set["_beatmapCharacteristicName"].as_str().unwrap_or("?");
            for d in set["_difficultyBeatmaps"].as_array().into_iter().flatten() {
                let cd = &d["_customData"];
                // ChroMapper reads a missing index as 0.
                let scheme = info["_colorSchemes"]
                    .get(d["_beatmapColorSchemeIdx"].as_u64().unwrap_or(0) as usize)
                    .filter(|s| s["useOverride"].as_bool() == Some(true))
                    .map(|s| &s["colorScheme"]);
                let colors = MapColors::resolve(cd, scheme);
                difficulties.push(Difficulty {
                    name: format!("{ch} {}", d["_difficulty"].as_str().unwrap_or("?")),
                    file: d["_beatmapFilename"].as_str().unwrap_or_default().to_owned(),
                    environment: info["_environmentName"]
                        .as_str()
                        .unwrap_or("DefaultEnvironment")
                        .to_owned(),
                    colors,
                    env_left: colors.left,
                    env_right: colors.right,
                });
            }
        }
        (
            format!(
                "{} - {}",
                info["_songAuthorName"].as_str().unwrap_or(""),
                info["_songName"].as_str().unwrap_or("")
            ),
            info["_beatsPerMinute"].as_f64().unwrap_or(120.0),
            info["_songFilename"].as_str().unwrap_or("song.egg").to_owned(),
        )
    } else {
        // v4 info format
        for d in info["difficultyBeatmaps"].as_array().into_iter().flatten() {
            difficulties.push(Difficulty {
                name: format!(
                    "{} {}",
                    d["characteristic"].as_str().unwrap_or("?"),
                    d["difficulty"].as_str().unwrap_or("?")
                ),
                file: d["beatmapDataFilename"].as_str().unwrap_or_default().to_owned(),
                environment: d["environmentNameIdx"]
                    .as_u64()
                    .and_then(|i| info["environmentNames"].get(i as usize))
                    .and_then(Value::as_str)
                    .unwrap_or("DefaultEnvironment")
                    .to_owned(),
                colors: MapColors::default(),
                env_left: None,
                env_right: None,
            });
        }
        (
            format!(
                "{} - {}",
                info["song"]["author"].as_str().unwrap_or(""),
                info["song"]["title"].as_str().unwrap_or("")
            ),
            info["audio"]["bpm"].as_f64().unwrap_or(120.0),
            info["audio"]["songFilename"].as_str().unwrap_or("song.egg").to_owned(),
        )
    };

    difficulties.retain(|d| {
        let ok = folder.join(&d.file).exists();
        if !ok {
            warnings.push(format!("{} is listed but {} is missing", d.name, d.file));
        }
        ok
    });

    Ok(LoadedMap {
        folder: folder.to_path_buf(),
        song_name,
        bpm,
        song_file: folder.join(song_file),
        difficulties,
        warnings,
    })
}

pub fn load_beatmap(path: &Path, next_id: &mut u64) -> Result<Beatmap, String> {
    let json = read_json(path)?;
    let n: &'static Names = if json.get("basicBeatmapEvents").is_some() {
        &V3
    } else if json.get("_events").is_some() {
        &V2
    } else {
        return Err(format!(
            "{} is format version '{}'. Only v2 and v3 difficulty files are supported.",
            path.display(),
            json["version"].as_str().unwrap_or("?")
        ));
    };

    let mut other_events = Vec::new();
    let mut rows: Vec<Row> = Vec::new();

    for ev in json[n.list].as_array().into_iter().flatten() {
        let Some(obj) = ev.as_object() else { continue };
        let et = obj.get(n.et).and_then(Value::as_i64).unwrap_or(0);
        if is_effect_lane(et) {
            *next_id += 1;
            add_effect(&mut rows, et, effect_key(n, *next_id, et, ev, obj));
            continue;
        }
        if !LIGHT_LANES.iter().any(|(t, _)| *t == et) {
            other_events.push(ev.clone());
            continue;
        }

        let mut extra = obj.clone();
        let beat = extra.remove(n.b).and_then(|v| v.as_f64()).unwrap_or(0.0);
        extra.remove(n.et);
        let value = extra.remove(n.i).and_then(|v| v.as_i64()).unwrap_or(0);
        let brightness_src = extra.remove(n.f);
        // A missing brightness is 0 in v3 (the game's default) and 1 in v2 (as ChroMapper reads it).
        let default_f = if n.version == "v3" { 0.0 } else { 1.0 };
        let brightness = brightness_src.as_ref().and_then(Value::as_f64).unwrap_or(default_f) as f32;
        let mut custom = match extra.remove(n.custom) {
            Some(Value::Object(m)) => m,
            _ => Map::new(),
        };
        let light_ids = custom.remove(n.light_id);
        let color_rgb_only = custom
            .get(n.color)
            .and_then(Value::as_array)
            .is_some_and(|a| a.len() == 3);
        let color_src = custom.get(n.color).cloned();
        let color = custom.remove(n.color).and_then(|c| {
            let a = c.as_array()?;
            let ch = |i: usize, d: f32| a.get(i).and_then(Value::as_f64).map_or(d, |v| v as f32);
            Some([ch(0, 1.0), ch(1, 1.0), ch(2, 1.0), ch(3, 1.0)])
        });
        let easing = match custom.remove(n.easing) {
            Some(Value::String(e)) => Some(e),
            Some(other) => {
                // Not a string we understand: keep it as it was.
                custom.insert(n.easing.into(), other);
                None
            }
            None => None,
        };
        let lerp_hsv = custom.get(n.lerp).and_then(Value::as_str) == Some("HSV");
        if lerp_hsv {
            custom.remove(n.lerp);
        }
        let (kind, base) = match value {
            0..=12 => decode_value(value),
            // Legacy Chroma color events only set the color; treat them like "On" for display.
            _ if legacy_rgb(value).is_some() => (Kind::On, Base::Blue),
            _ => decode_value(value),
        };
        let raw_value = (!(0..=12).contains(&value)).then_some(value);

        *next_id += 1;
        let key = Key {
            id: *next_id,
            beat,
            kind,
            base,
            color,
            color_rgb_only,
            brightness,
            raw_value,
            color_src,
            brightness_src,
            easing,
            lerp_hsv,
            extra,
            extra_custom: custom,
            pos: None,
            effect: false,
            source: None,
        };

        match rows
            .iter_mut()
            .find(|r| r.lane == et && r.light_ids == light_ids)
        {
            Some(row) => row.keys.push(key),
            None => rows.push(Row {
                lane: et,
                light_ids,
                keys: vec![key],
                kind: RowKind::Light,
            }),
        }
    }

    for ev in json[BOOST_LIST].as_array().into_iter().flatten() {
        let Some(beat) = ev["b"].as_f64() else { continue };
        *next_id += 1;
        let on = i64::from(ev["o"].as_bool().unwrap_or(false));
        let mut key = Key::effect(*next_id, beat, 5, on);
        key.source = Some(ev.clone());
        add_effect(&mut rows, 5, key);
    }
    for lane in COMMON_EFFECTS {
        if !rows.iter().any(|r| r.lane == lane) {
            rows.push(Row { lane, light_ids: None, keys: Vec::new(), kind: RowKind::Effect });
        }
    }

    // Make sure every lane has at least an "all lights" row to add keys to.
    for (lane, _) in LIGHT_LANES {
        if !rows.iter().any(|r| r.lane == *lane && r.light_ids.is_none()) {
            rows.push(Row {
                lane: *lane,
                light_ids: None,
                keys: Vec::new(),
                kind: RowKind::Light,
            });
        }
    }

    let lane_order = |et: i64| LANE_ORDER.iter().position(|t| *t == et).unwrap_or(99);
    rows.sort_by_key(|r| (lane_order(r.lane), r.light_ids.is_some()));
    for r in &mut rows {
        r.sort();
    }
    load_positions(&json[n.custom], &mut rows, next_id);

    Ok(Beatmap {
        path: path.to_path_buf(),
        names: n,
        json,
        other_events,
        rows,
        backed_up: false,
    })
}

const BOOST_LIST: &str = "colorBoostBeatmapEvents";

fn effect_key(n: &Names, id: u64, lane: i64, ev: &Value, obj: &Map<String, Value>) -> Key {
    let value = obj.get(n.i).and_then(Value::as_i64).unwrap_or(0);
    let mut key = Key::effect(id, obj.get(n.b).and_then(Value::as_f64).unwrap_or(0.0), lane, value);
    key.brightness_src = obj.get(n.f).cloned();
    key.extra = obj.clone();
    for field in [n.b, n.et, n.i, n.f, n.custom] {
        key.extra.remove(field);
    }
    if let Some(Value::Object(m)) = obj.get(n.custom) {
        key.extra_custom = m.clone();
    }
    key.source = Some(ev.clone());
    key
}

fn add_effect(rows: &mut Vec<Row>, lane: i64, key: Key) {
    match rows.iter_mut().find(|r| r.lane == lane) {
        Some(row) => row.keys.push(key),
        None => rows.push(Row { lane, light_ids: None, keys: vec![key], kind: RowKind::Effect }),
    }
}

/// Position lines saved by the editor (`POSITIONS_KEY` in the difficulty's custom data), each placed
/// under its light row.
fn load_positions(custom: &Value, rows: &mut Vec<Row>, next_id: &mut u64) {
    for p in custom[POSITIONS_KEY].as_array().into_iter().flatten() {
        let Some(lane) = p["lane"].as_i64() else { continue };
        let light_ids = p.get("lightIds").cloned();
        let kind = if p["kind"] == "aim" { RowKind::Aim } else { RowKind::Position };
        let keys = p["keys"].as_array().into_iter().flatten().filter_map(|k| {
            let a = k["p"].as_array()?;
            let ch = |i: usize| a.get(i).and_then(Value::as_f64).map(|v| v as f32);
            let pos = [ch(0)?, ch(1)?, ch(2)?];
            *next_id += 1;
            Some(Key::line(kind, *next_id, k["b"].as_f64()?, pos, k["easing"].as_str().map(str::to_owned)))
        });
        let mut line = Row { lane, light_ids, keys: keys.collect(), kind };
        line.sort();
        let parent = rows.iter().position(|r| r.is_light() && r.lane == lane && r.light_ids == line.light_ids);
        let at = parent.unwrap_or_else(|| {
            // The light row lost all its keys: bring it back, last in its lane.
            let at = rows.iter().rposition(|r| r.lane == lane).map_or(rows.len(), |i| i + 1);
            rows.insert(at, Row { lane, light_ids: line.light_ids.clone(), keys: Vec::new(), kind: RowKind::Light });
            at
        });
        rows.insert(line_slot(rows, at, kind), line);
    }
}

/// Where a new line goes under the light row `at`: Position first, then Aim.
pub fn line_slot(rows: &[Row], at: usize, kind: RowKind) -> usize {
    if kind == RowKind::Position {
        return at + 1;
    }
    let mut i = at + 1;
    while rows.get(i).is_some_and(|r| r.is_motion()) {
        i += 1;
    }
    i
}

/// Unity's `Quaternion.eulerAngles` for a rotation given by its columns (degrees).
fn euler_of(c: [[f32; 3]; 3]) -> [f32; 3] {
    let (r02, r12, r22) = (c[2][0], c[2][1], c[2][2]);
    let x = (-r12).clamp(-1.0, 1.0).asin();
    let (y, z) = if r12.abs() < 0.9999 {
        (r02.atan2(r22), c[0][1].atan2(c[1][1]))
    } else {
        ((-c[0][2]).atan2(c[0][0]), 0.0)
    };
    [x, y, z].map(f32::to_degrees)
}

/// The world rotation (Euler degrees) of a light normally at `rest`, turned by `turn` (Euler degrees).
fn aimed(turn: [f32; 3], rest: [[f32; 3]; 3]) -> [f32; 3] {
    let q = Quat::euler(turn);
    euler_of(rest.map(|c| q.rotate(c)))
}

/// Regex for a light's GameObject in Chroma's ID format (`Environment.[12]Name`).
fn chroma_id(path: &str) -> String {
    let mut out = String::new();
    for (i, seg) in path.split('/').enumerate() {
        if i > 0 {
            out.push_str(r"\.\[\d+\]");
        }
        for c in seg.chars() {
            if "\\.+*?()|[]{}^$".contains(c) {
                out.push('\\');
            }
            out.push(c);
        }
    }
    out.push('$');
    out
}

/// Shortest decimal that reads back as the same f32, so 0.17025 stays 0.17025 in the file.
fn clean(x: f32) -> f64 {
    format!("{x}").parse().unwrap_or(f64::from(x))
}

/// In v3, boost events live in their own list (unless the file had them as basic events).
fn in_boost_list(n: &Names, row: &Row, key: &Key) -> bool {
    n.version == "v3" && row.lane == 5 && key.source.as_ref().is_none_or(|s| s.get(n.et).is_none())
}

/// The effect event as it was, while it hasn't been changed.
fn unchanged<'a>(n: &Names, key: &'a Key) -> Option<&'a Value> {
    let s = key.source.as_ref()?;
    (s.get(n.b).and_then(Value::as_f64).unwrap_or(0.0) == key.beat).then_some(s)
}

fn effect_to_event(n: &Names, row: &Row, key: &Key) -> Value {
    if let Some(s) = unchanged(n, key) {
        return s.clone();
    }
    let mut obj = Map::new();
    obj.insert(n.b.into(), json!(key.beat));
    obj.insert(n.et.into(), json!(row.lane));
    obj.insert(n.i.into(), json!(key.raw_value.unwrap_or(0)));
    if let Some(f) = &key.brightness_src {
        obj.insert(n.f.into(), f.clone());
    }
    for (k, v) in &key.extra {
        obj.insert(k.clone(), v.clone());
    }
    if !key.extra_custom.is_empty() {
        obj.insert(n.custom.into(), Value::Object(key.extra_custom.clone()));
    }
    Value::Object(obj)
}

fn boost_to_event(n: &Names, key: &Key) -> Value {
    match (unchanged(n, key), key.source.as_ref()) {
        (Some(s), _) => s.clone(),
        _ => json!({ "b": key.beat, "o": key.raw_value.unwrap_or(0) != 0 }),
    }
}

fn key_to_event(n: &Names, row: &Row, key: &Key) -> Value {
    if row.is_effect() {
        return effect_to_event(n, row, key);
    }
    let mut obj = Map::new();
    obj.insert(n.b.into(), json!(key.beat));
    obj.insert(n.et.into(), json!(row.lane));
    let value = key
        .raw_value
        .unwrap_or_else(|| encode_value(key.kind, key.base));
    obj.insert(n.i.into(), json!(value));
    let f = key
        .brightness_src
        .clone()
        .unwrap_or_else(|| json!(clean(key.brightness)));
    obj.insert(n.f.into(), f);
    for (k, v) in &key.extra {
        obj.insert(k.clone(), v.clone());
    }

    let mut custom = key.extra_custom.clone();
    if let Some(ids) = &row.light_ids {
        custom.insert(n.light_id.into(), ids.clone());
    }
    if let (Some(src), Some(_)) = (&key.color_src, key.color) {
        custom.insert(n.color.into(), src.clone());
    } else if let Some(c) = key.color {
        let c = c.map(clean);
        if key.color_rgb_only && c[3] == 1.0 {
            custom.insert(n.color.into(), json!(c[..3]));
        } else {
            custom.insert(n.color.into(), json!(c));
        }
    }
    if let Some(e) = &key.easing {
        custom.insert(n.easing.into(), json!(e));
    }
    if key.lerp_hsv {
        custom.insert(n.lerp.into(), json!("HSV"));
    }
    if !custom.is_empty() {
        obj.insert(n.custom.into(), Value::Object(custom));
    }
    Value::Object(obj)
}

impl Beatmap {
    pub fn light_event_count(&self) -> usize {
        self.rows.iter().filter(|r| r.is_light()).map(|r| r.keys.len()).sum()
    }

    /// v3's separate boost list as it would be saved; `None` for v2.
    pub fn build_boosts(&self) -> Option<Vec<Value>> {
        let n = self.names;
        if n.version != "v3" {
            return None;
        }
        let row = self.rows.iter().find(|r| r.lane == 5)?;
        Some(row.keys.iter().filter(|k| in_boost_list(n, row, k)).map(|k| boost_to_event(n, k)).collect())
    }

    /// Puts the effect rows back in the JSON, which the light model and laser motion read.
    pub fn sync_events(&mut self) {
        self.json[self.names.list] = Value::Array(self.build_events());
        if let Some(b) = self.build_boosts() {
            if !b.is_empty() || self.json.get(BOOST_LIST).is_some() {
                self.json[BOOST_LIST] = Value::Array(b);
            }
        }
    }

    /// The full basic event list as it would be saved.
    pub fn build_events(&self) -> Vec<Value> {
        let n = self.names;
        let mut events = self.other_events.clone();
        for row in self.rows.iter().filter(|r| !r.is_motion()) {
            let in_list = row.keys.iter().filter(|k| !in_boost_list(n, row, k));
            events.extend(in_list.map(|k| key_to_event(n, row, k)));
        }
        events.sort_by(|a, b| {
            let fa = a[n.b].as_f64().unwrap_or(0.0);
            let fb = b[n.b].as_f64().unwrap_or(0.0);
            fa.total_cmp(&fb)
        });
        events
    }

    /// Puts the Position lines into the difficulty's custom data: the keys as the editor reads them
    /// back, and, through `resolve` (a line's movable lights and how many it can't reach), the Chroma
    /// tracks and animations that move the lights in the game. Maps without Position lines are left
    /// alone. Returns a note for the user.
    pub fn write_positions(&mut self, resolve: Option<&Resolve>) -> Option<String> {
        let v3 = self.names.version == "v3";
        let name = |a: &'static str, b: &'static str| if v3 { a } else { b };
        let (env_key, events_key, track_key) = (name("environment", "_environment"), name("customEvents", "_customEvents"), name("track", "_track"));
        let data_key = name("d", "_data");
        let lines: Vec<&Row> = self.rows.iter().filter(|r| r.is_motion() && !r.keys.is_empty()).collect();
        let saved: Vec<Value> = lines
            .iter()
            .map(|r| {
                let keys: Vec<Value> = r
                    .keys
                    .iter()
                    .map(|k| {
                        let mut o = json!({ "b": k.beat, "p": k.pos.unwrap_or([0.0; 3]).map(clean) });
                        if let Some(e) = &k.easing {
                            o["easing"] = json!(e);
                        }
                        o
                    })
                    .collect();
                let mut o = json!({ "lane": r.lane, "keys": keys });
                if r.kind == RowKind::Aim {
                    o["kind"] = json!("aim");
                }
                if let Some(ids) = &r.light_ids {
                    o["lightIds"] = ids.clone();
                }
                o
            })
            .collect();
        let custom_key = self.names.custom;
        if lines.is_empty() && self.json[custom_key].get(POSITIONS_KEY).is_none() {
            return None;
        }
        // Generated for the game below, once the old ones are gone.
        let mut env_entries = Vec::new();
        let mut events = Vec::new();
        let mut unreachable = 0;
        // One track per object; the first line of each kind reaching it wins.
        let mut tracks: HashMap<String, String> = HashMap::new();
        let mut seen = std::collections::HashSet::new();
        if let Some(resolve) = resolve {
            let unit = if v3 { 1.0 } else { 0.6 };
            for r in &lines {
                let (targets, missed) = resolve(r);
                unreachable += missed;
                for t in targets {
                    let aim = r.kind == RowKind::Aim;
                    if !seen.insert((t.path.clone(), aim)) {
                        continue;
                    }
                    let n = tracks.len();
                    let track = tracks.entry(t.path.clone()).or_insert_with(|| {
                        let track = format!("{TRACK_PREFIX}{n}");
                        env_entries.push(if v3 {
                            json!({ "id": chroma_id(&t.path), "lookupMethod": "Regex", "track": track })
                        } else {
                            json!({ "_id": chroma_id(&t.path), "_lookupMethod": "Regex", "_track": track })
                        });
                        track
                    });
                    let first = r.keys[0].beat;
                    let span = r.keys[r.keys.len() - 1].beat - first;
                    let mut points = Vec::new();
                    for (i, k) in r.keys.iter().enumerate() {
                        let off = k.pos.unwrap_or([0.0; 3]);
                        let value = if aim { aimed(off, t.rot) } else { std::array::from_fn(|a| (t.rest[a] + off[a]) / unit) };
                        let mut p: Vec<Value> = value.iter().map(|v| json!(clean(*v))).collect();
                        p.push(json!(if span > 0.0 { (k.beat - first) / span } else { 0.0 }));
                        if let Some(e) = i.checked_sub(1).and_then(|i| r.keys[i].easing.as_ref()) {
                            p.push(json!(e));
                        }
                        points.push(Value::Array(p));
                    }
                    events.push(if v3 {
                        let mut d = json!({ "track": track, "duration": span });
                        d[if aim { "rotation" } else { "position" }] = Value::Array(points);
                        json!({ "b": first, "t": "AnimateTrack", "d": d })
                    } else {
                        let mut d = json!({ "_track": track, "_duration": span });
                        d[if aim { "_rotation" } else { "_position" }] = Value::Array(points);
                        json!({ "_time": first, "_type": "AnimateTrack", "_data": d })
                    });
                }
            }
        }
        let lines_n = lines.len();
        let root = &mut self.json;
        if !root[custom_key].is_object() {
            root[custom_key] = json!({});
        }
        let cd = root[custom_key].as_object_mut()?;
        if saved.is_empty() {
            cd.remove(POSITIONS_KEY);
        } else {
            cd.insert(POSITIONS_KEY.into(), Value::Array(saved));
        }
        if resolve.is_none() {
            return (lines_n > 0).then(|| {
                " Position lines kept for the editor, but not written for the game: this environment isn't exported.".to_owned()
            });
        }
        let ours = |t: Option<&Value>| t.and_then(Value::as_str).is_some_and(|s| s.starts_with(TRACK_PREFIX));
        let time = if v3 { "b" } else { "_time" };
        let list = |cd: &mut Map<String, Value>, key: &str, keep: &dyn Fn(&Value) -> bool, add: Vec<Value>| {
            let mut v: Vec<Value> = cd.get(key).and_then(Value::as_array).into_iter().flatten().filter(|e| keep(e)).cloned().collect();
            v.extend(add);
            if v.is_empty() {
                cd.remove(key);
            } else {
                cd.insert(key.to_owned(), Value::Array(v));
            }
        };
        list(cd, env_key, &|e| !ours(e.get(track_key)), env_entries);
        list(cd, events_key, &|e| !ours(e[data_key].get(track_key)), events);
        if let Some(v) = cd.get_mut(events_key).and_then(Value::as_array_mut) {
            v.sort_by(|a, b| a[time].as_f64().unwrap_or(0.0).total_cmp(&b[time].as_f64().unwrap_or(0.0)));
        }
        (unreachable > 0).then(|| format!(" {unreachable} lights share a name with others and can't be moved in the game."))
    }

    /// Writes the rows back into the difficulty file. Backs up the original file once per session.
    pub fn save(&mut self) -> Result<String, String> {
        self.sync_events();

        let mut msg = String::new();
        if !self.backed_up {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let mut backup = self.path.clone().into_os_string();
            backup.push(format!(".timeline-backup-{secs}"));
            std::fs::copy(&self.path, &backup).map_err(|e| format!("Backup failed: {e}"))?;
            self.backed_up = true;
            msg = format!(" (original backed up to {})", PathBuf::from(backup).display());
        }

        let text = serde_json::to_string(&self.json).map_err(|e| e.to_string())?;
        std::fs::write(&self.path, text).map_err(|e| format!("Write failed: {e}"))?;
        Ok(format!(
            "Saved {} light events to {}{msg}",
            self.light_event_count(),
            self.path.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDENTITY: [[f32; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

    fn close(a: [f32; 3], b: [f32; 3]) -> bool {
        (0..3).all(|i| (a[i] - b[i]).abs() < 1e-2)
    }

    #[test]
    fn euler_matches_unity_order() {
        for e in [[20.0, 30.0, 40.0], [-35.0, 120.0, -10.0], [0.0, 0.0, 75.0]] {
            assert!(close(aimed(e, IDENTITY), e), "{e:?} -> {:?}", aimed(e, IDENTITY));
        }
    }

    #[test]
    fn turns_are_in_world_axes_on_top_of_the_rest_rotation() {
        let q = Quat::euler([0.0, 90.0, 0.0]);
        // Turning right by 90 degrees sends forward to right; tilting by 90 sends forward down.
        assert!(close(q.rotate([0.0, 0.0, 1.0]), [1.0, 0.0, 0.0]));
        assert!(close(Quat::euler([90.0, 0.0, 0.0]).rotate([0.0, 0.0, 1.0]), [0.0, -1.0, 0.0]));
        // A light already pointing right, turned 90 more, points backward.
        let rest = [q.rotate([1.0, 0.0, 0.0]), q.rotate([0.0, 1.0, 0.0]), q.rotate([0.0, 0.0, 1.0])];
        let e = aimed([0.0, 90.0, 0.0], rest);
        assert!(close(Quat::euler(e).rotate([0.0, 0.0, 1.0]), [0.0, 0.0, -1.0]), "{e:?}");
    }

    #[test]
    fn aim_lines_share_a_track_with_position_and_load_back() {
        let mut bm = map(&V3, json!({}));
        let keys = vec![Key::line(RowKind::Aim, 3, 1.0, [0.0, 45.0, 0.0], None)];
        bm.rows.push(Row { lane: 2, light_ids: Some(json!([7, 8])), keys, kind: RowKind::Aim });
        bm.write_positions(Some(&resolve));
        let cd = &bm.json["customData"];
        assert_eq!(cd["environment"].as_array().unwrap().len(), 1);
        let events = cd["customEvents"].as_array().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["d"]["track"], events[1]["d"]["track"]);
        // Sorted by beat: the aim line starts first.
        assert!(close(
            std::array::from_fn(|i| events[0]["d"]["rotation"][0][i].as_f64().unwrap() as f32),
            [0.0, 45.0, 0.0]
        ));
        let mut rows = vec![Row { lane: 2, light_ids: Some(json!([7, 8])), keys: Vec::new(), kind: RowKind::Light }];
        let mut id = 10;
        load_positions(cd, &mut rows, &mut id);
        assert_eq!(rows.iter().map(|r| r.kind).collect::<Vec<_>>(), [RowKind::Light, RowKind::Position, RowKind::Aim]);
        assert_eq!(rows[2].aim_at(0.5).map(|_| ()), None);
    }

    fn map(names: &'static Names, json: Value) -> Beatmap {
        let key = Key::line(RowKind::Position, 1, 4.0, [0.0, 2.0, 0.0], Some("easeInQuad".to_owned()));
        let end = Key::line(RowKind::Position, 2, 8.0, [1.0, 2.0, 0.0], None);
        let rows = vec![Row { lane: 2, light_ids: Some(json!([7, 8])), keys: vec![key, end], kind: RowKind::Position }];
        Beatmap { path: PathBuf::new(), names, json, other_events: Vec::new(), rows, backed_up: false }
    }

    fn resolve(_: &Row) -> (Vec<MoveTarget>, usize) {
        (vec![MoveTarget { path: "Environment/Laser (1)".to_owned(), rest: [3.0, 0.6, 6.0], rot: IDENTITY }], 2)
    }

    #[test]
    fn v3_writes_tracks_and_keeps_other_entries() {
        let other = json!({ "id": "Foo", "lookupMethod": "Exact", "active": false });
        let mut bm = map(&V3, json!({ "customData": { "environment": [other.clone()] } }));
        let note = bm.write_positions(Some(&resolve)).unwrap();
        assert!(note.contains("2 lights"));
        let cd = &bm.json["customData"];
        assert_eq!(cd["environment"][0], other);
        assert_eq!(cd["environment"][1]["id"], r"Environment\.\[\d+\]Laser \(1\)$");
        let ev = &cd["customEvents"][0];
        assert_eq!((ev["b"].as_f64(), ev["t"].as_str()), (Some(4.0), Some("AnimateTrack")));
        assert_eq!(ev["d"]["duration"].as_f64(), Some(4.0));
        // The first key's easing shapes the move into the second point.
        assert_eq!(ev["d"]["position"], json!([[3.0, 2.6, 6.0, 0.0], [4.0, 2.6, 6.0, 1.0, "easeInQuad"]]));
        // Saving again replaces the generated entries.
        bm.write_positions(Some(&resolve));
        assert_eq!(bm.json["customData"]["environment"].as_array().unwrap().len(), 2);
        assert_eq!(bm.json["customData"]["customEvents"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn v2_uses_lane_units_and_underscored_names() {
        let mut bm = map(&V2, json!({}));
        bm.write_positions(Some(&|_: &Row| (vec![MoveTarget { path: "Environment/L".to_owned(), rest: [0.6, 0.0, 1.2], rot: IDENTITY }], 0)));
        let cd = &bm.json["_customData"];
        assert_eq!(cd["_environment"][0]["_lookupMethod"], "Regex");
        let d = &cd["_customEvents"][0]["_data"];
        let first = d["_position"][0].as_array().unwrap();
        assert!((first[1].as_f64().unwrap() - 2.0 / 0.6).abs() < 1e-3);
        assert!((first[0].as_f64().unwrap() - 1.0).abs() < 1e-3);
    }

    #[test]
    fn saved_lines_load_back_under_their_light_row() {
        let mut bm = map(&V3, json!({}));
        bm.write_positions(Some(&resolve));
        let mut rows = vec![Row { lane: 2, light_ids: Some(json!([7, 8])), keys: Vec::new(), kind: RowKind::Light }];
        let mut id = 10;
        load_positions(&bm.json["customData"], &mut rows, &mut id);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].kind, RowKind::Position);
        assert_eq!(rows[1].keys[0].pos, Some([0.0, 2.0, 0.0]));
        assert_eq!(rows[1].keys[0].easing.as_deref(), Some("easeInQuad"));
        assert_eq!(rows[1].keys[1].beat, 8.0);
    }

    #[test]
    fn maps_without_position_lines_are_untouched() {
        let json = json!({ "customData": { "environment": [] } });
        let mut bm = map(&V3, json.clone());
        bm.rows.clear();
        assert!(bm.write_positions(Some(&resolve)).is_none());
        assert_eq!(bm.json, json);
    }

    #[test]
    fn position_interpolates_and_holds() {
        let bm = map(&V3, json!({}));
        let r = &bm.rows[0];
        assert_eq!(r.position_at(3.0), [0.0; 3]);
        assert_eq!(r.position_at(4.0), [0.0, 2.0, 0.0]);
        assert_eq!(r.position_at(6.0)[0], 0.25); // easeInQuad at the halfway point
        assert_eq!(r.position_at(20.0), [1.0, 2.0, 0.0]);
    }

    #[test]
    fn effect_events_round_trip_and_edit() {
        let n = &V3;
        let ev = json!({ "b": 4, "et": 12, "i": 1, "customData": { "direction": 1, "lockRotation": false } });
        let obj = ev.as_object().unwrap().clone();
        let mut key = effect_key(n, 1, 12, &ev, &obj);
        let row = Row { lane: 12, light_ids: None, keys: vec![key.clone()], kind: RowKind::Effect };
        // Untouched: exactly as it was, including no "f".
        assert_eq!(effect_to_event(n, &row, &key), ev);
        // Moved: rebuilt with its custom fields.
        key.beat = 6.0;
        let moved = effect_to_event(n, &row, &key);
        assert_eq!((moved["b"].as_f64(), moved["i"].as_i64()), (Some(6.0), Some(1)));
        assert_eq!(moved["customData"]["direction"], 1);
        assert!(moved.get("f").is_none());
    }

    #[test]
    fn v3_boosts_go_to_their_own_list() {
        let mut key = Key::effect(1, 2.0, 5, 1);
        key.source = Some(json!({ "b": 2.0, "o": true }));
        let new = Key::effect(2, 8.0, 5, 0);
        let rows = vec![Row { lane: 5, light_ids: None, keys: vec![key, new], kind: RowKind::Effect }];
        let mut bm = Beatmap { path: PathBuf::new(), names: &V3, json: json!({}), other_events: Vec::new(), rows, backed_up: false };
        assert_eq!(bm.build_boosts().unwrap(), [json!({ "b": 2.0, "o": true }), json!({ "b": 8.0, "o": false })]);
        assert!(bm.build_events().is_empty());
        bm.sync_events();
        assert_eq!(bm.json[BOOST_LIST].as_array().unwrap().len(), 2);
    }

    #[test]
    fn v2_boosts_stay_basic_events() {
        let rows = vec![Row { lane: 5, light_ids: None, keys: vec![Key::effect(1, 2.0, 5, 1)], kind: RowKind::Effect }];
        let bm = Beatmap { path: PathBuf::new(), names: &V2, json: json!({}), other_events: Vec::new(), rows, backed_up: false };
        assert!(bm.build_boosts().is_none());
        assert_eq!(bm.build_events()[0]["_type"], 5);
    }

    #[test]
    fn row_reaches_its_lights_only() {
        let all = Row { lane: 2, light_ids: None, keys: Vec::new(), kind: RowKind::Position };
        let some = Row { lane: 2, light_ids: Some(json!([7, 8])), keys: Vec::new(), kind: RowKind::Position };
        assert!(all.reaches(2, &[]) && all.reaches(2, &[99]) && !all.reaches(3, &[7]));
        assert!(some.reaches(2, &[8, 40]) && !some.reaches(2, &[9]) && !some.reaches(2, &[]));
    }
}
