//! Loading and saving Beat Saber maps (v3 difficulty files) as keyframe rows.

use serde_json::{Map, Value, json};
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

pub fn lane_name(et: i64) -> &'static str {
    LIGHT_LANES
        .iter()
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
}

/// Legacy Chroma stored RGB in the value itself: 2_000_000_000 + 0xRRGGBB.
fn legacy_rgb(raw: i64) -> Option<[f32; 4]> {
    let rgb = raw.checked_sub(2_000_000_000).filter(|v| (0..=0xFF_FFFF).contains(v))?;
    let ch = |shift: i64| ((rgb >> shift) & 0xFF) as f32 / 255.0;
    Some([ch(16), ch(8), ch(0), 1.0])
}

impl Key {
    pub fn display_color(&self) -> [f32; 4] {
        self.color
            .or_else(|| self.raw_value.and_then(legacy_rgb))
            .unwrap_or_else(|| self.base.rgba())
    }
}

/// One timeline row: a light lane, optionally limited to a set of Chroma light IDs.
#[derive(Clone, Debug)]
pub struct Row {
    pub lane: i64,
    pub light_ids: Option<Value>,
    pub keys: Vec<Key>,
}

impl Row {
    pub fn label(&self) -> String {
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
            }),
        }
    }

    // Make sure every lane has at least an "all lights" row to add keys to.
    for (lane, _) in LIGHT_LANES {
        if !rows.iter().any(|r| r.lane == *lane && r.light_ids.is_none()) {
            rows.push(Row {
                lane: *lane,
                light_ids: None,
                keys: Vec::new(),
            });
        }
    }

    let lane_order = |et: i64| LIGHT_LANES.iter().position(|(t, _)| *t == et).unwrap_or(99);
    rows.sort_by_key(|r| (lane_order(r.lane), r.light_ids.is_some()));
    for r in &mut rows {
        r.sort();
    }

    Ok(Beatmap {
        path: path.to_path_buf(),
        names: n,
        json,
        other_events,
        rows,
        backed_up: false,
    })
}

/// Shortest decimal that reads back as the same f32, so 0.17025 stays 0.17025 in the file.
fn clean(x: f32) -> f64 {
    format!("{x}").parse().unwrap_or(f64::from(x))
}

fn key_to_event(n: &Names, row: &Row, key: &Key) -> Value {
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
        self.rows.iter().map(|r| r.keys.len()).sum()
    }

    /// The full basic event list as it would be saved.
    pub fn build_events(&self) -> Vec<Value> {
        let n = self.names;
        let mut events = self.other_events.clone();
        for row in &self.rows {
            events.extend(row.keys.iter().map(|k| key_to_event(n, row, k)));
        }
        events.sort_by(|a, b| {
            let fa = a[n.b].as_f64().unwrap_or(0.0);
            let fb = b[n.b].as_f64().unwrap_or(0.0);
            fa.total_cmp(&fb)
        });
        events
    }

    /// Writes the rows back into the difficulty file. Backs up the original file once per session.
    pub fn save(&mut self) -> Result<String, String> {
        self.json[self.names.list] = Value::Array(self.build_events());

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
