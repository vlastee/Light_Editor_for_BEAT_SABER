//! The timeline window: dope-sheet style rows of light keyframes over the song waveform.

use crate::audio::{PEAK_BLOCK, Player, Song};
use crate::eval;
use crate::preview::{self, LaneLights};
use crate::cmlight::LightModel;
use crate::environment::EnvFile;
use crate::scene::{Obj, Scene};
use crate::view3d::{self, Camera};
use crate::map::{self, Base, Beatmap, Key, Kind, LoadedMap, Row, RowKind};
use eframe::egui::{
    self, Align2, Color32, FontId, Painter, PointerButton, Pos2, Rect, Sense, Shape, Stroke,
    StrokeKind, Vec2,
};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

const LABEL_W: f32 = 230.0;
const RULER_H: f32 = 22.0;
const WAVE_H: f32 = 56.0;
const LANE_H: f32 = 24.0;
const ROW_H: f32 = 22.0;
const KEY_R: f32 = 6.0;
const SELECTED: Color32 = Color32::from_rgb(255, 190, 60);
const PLAYHEAD: Color32 = Color32::from_rgb(80, 150, 255);

#[derive(Clone, Copy, PartialEq)]
enum Line {
    Lane(i64),
    Row(usize),
}

#[derive(Clone, Copy)]
struct Laid {
    line: Line,
    y: f32,
    h: f32,
}

enum Drag {
    None,
    Move {
        start_beat: f64,
        orig: HashMap<u64, f64>,
        moved: bool,
    },
    Select {
        start: Pos2,
    },
    Pan {
        last: Pos2,
    },
    Scrub,
}

enum Change {
    Beat(f64),
    Kind(Kind),
    Base(Base),
    Color(Option<[f32; 4]>),
    Brightness(f32),
    Easing(Option<String>),
    Hsv(bool),
    /// A Position key's offset along one axis.
    Axis(usize, f32),
    /// An effect key's value, and its lane for the key shape.
    Value(i64, i64),
    /// An effect key's Chroma field: set, or removed.
    Custom(String, Option<Value>),
}

impl Change {
    fn apply(&self, k: &mut Key) {
        // Each kind of key only takes its own fields (a selection can mix them).
        let takes = match self {
            Change::Beat(_) => true,
            Change::Easing(_) => !k.effect,
            Change::Axis(..) => k.pos.is_some(),
            Change::Value(..) | Change::Custom(..) => k.effect,
            _ => !k.effect && k.pos.is_none(),
        };
        if !takes {
            return;
        }
        match self {
            Change::Axis(a, v) => {
                if let Some(p) = &mut k.pos {
                    p[*a] = *v;
                }
            }
            Change::Value(v, lane) => {
                k.raw_value = Some(*v);
                k.kind = Key::effect_shape(*lane, *v);
                k.source = None;
            }
            Change::Custom(name, v) => {
                match v {
                    Some(v) => k.extra_custom.insert(name.clone(), v.clone()),
                    None => k.extra_custom.remove(name),
                };
                k.source = None;
            }
            Change::Beat(b) => k.beat = b.max(0.0),
            Change::Kind(v) => {
                k.kind = *v;
                k.raw_value = None;
            }
            Change::Base(v) => {
                k.base = *v;
                k.raw_value = None;
            }
            Change::Color(v) => {
                k.color = *v;
                k.color_src = None;
            }
            Change::Brightness(v) => {
                k.brightness = *v;
                k.brightness_src = None;
            }
            Change::Easing(v) => k.easing.clone_from(v),
            Change::Hsv(v) => k.lerp_hsv = *v,
        }
    }
}

pub struct App {
    path_input: String,
    map: Option<LoadedMap>,
    diff_idx: usize,
    beatmap: Option<Beatmap>,
    song: Option<Song>,
    player: Option<Player>,
    status: String,
    next_id: u64,
    dirty: bool,
    close_armed: bool,

    view_start: f64,
    px_per_beat: f32,
    scroll_y: f32,
    tl_left: f32,
    tl_width: f32,
    playhead: f64,
    snap: u32,
    collapsed: HashSet<i64>,

    selected: HashSet<u64>,
    active_row: Option<usize>,
    drag: Drag,
    box_rect: Option<Rect>,
    clipboard: Vec<(usize, Key)>,
    undo: Vec<Vec<Row>>,
    redo: Vec<Vec<Row>>,
    last_panel_edit: f64,
    new_kind: Kind,
    palette: Vec<[f32; 4]>,
    show_preview: bool,
    highlight_lights: bool,
    preview_lights: Vec<LaneLights>,
    scene: Scene,
    base_objs: Vec<Obj>,
    show_base: bool,
    camera: Camera,
    env: Option<EnvFile>,
    env_name: String,
    /// ChroMapper-faithful light colors; rebuilt when `rows_gen` moves past `model_gen`.
    model: Option<LightModel>,
    rows_gen: u64,
    model_gen: u64,
    env_colors: map::MapColors,
    /// Widgets that take typed text; shortcuts are off only while one of these has focus.
    text_inputs: Vec<egui::Id>,
    /// GPU renderer (ChroMapper's BloomFog port) for environments exported with meshes.
    gpu: Option<crate::gpu3d::Gpu3d>,
    wgpu: Option<eframe::egui_wgpu::RenderState>,
    gpu_texture: Option<(egui::TextureId, (u32, u32))>,
    env_lights: Option<crate::envscene::EnvLights>,
    env_components: Vec<serde_json::Value>,
    /// Rotating lasers and lights from the map's events.
    motion: crate::motion::Motion,
    /// Draw the environment with the GPU renderer (else the simple one).
    use_gpu: bool,
}

fn last_path_file() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".config/bs-light-timeline/last_map.txt"))
}

fn rgb32(c: [f32; 3]) -> Color32 {
    let [r, g, b] = c.map(|v| (v.clamp(0.0, 1.0) * 255.0) as u8);
    Color32::from_rgb(r, g, b)
}

fn rgba32(c: [f32; 4]) -> Color32 {
    rgb32([c[0], c[1], c[2]])
}

fn hex(c: [f32; 4]) -> String {
    let [r, g, b, a] = c.map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8);
    format!("#{r:02X}{g:02X}{b:02X}{a:02X}")
}

fn fmt_time(secs: f64) -> String {
    let secs = secs.max(0.0);
    format!("{}:{:06.3}", (secs / 60.0) as u32, secs % 60.0)
}

fn key_range(row: &Row, b0: f64, b1: f64) -> &[Key] {
    let s = row.keys.partition_point(|k| k.beat < b0);
    let e = row.keys.partition_point(|k| k.beat <= b1).max(s);
    &row.keys[s..e]
}

/// A laser-speed key's speed: Chroma's precise speed, else the event value.
fn effect_speed(k: &Key) -> f32 {
    ["speed", "_speed", "_preciseSpeed", "preciseSpeed"]
        .iter()
        .find_map(|n| k.extra_custom.get(*n).and_then(Value::as_f64))
        .unwrap_or(k.raw_value.unwrap_or(0) as f64) as f32
}

fn effect_active(k: &Key) -> bool {
    k.raw_value.unwrap_or(0) != 0 || effect_speed(k) > 0.0
}

/// A Chroma number on an effect key: the box adds or removes it.
fn optional_number(ui: &mut egui::Ui, label: &str, name: String, t: &Key, inputs: &mut Vec<egui::Id>, changes: &mut Vec<Change>) {
    ui.label(label);
    ui.horizontal(|ui| {
        let current = t.extra_custom.get(&name).and_then(Value::as_f64);
        let mut on = current.is_some();
        if ui.checkbox(&mut on, "").changed() {
            changes.push(Change::Custom(name.clone(), on.then(|| json!(0))));
        }
        if let Some(mut v) = current {
            let r = ui.add(egui::DragValue::new(&mut v).speed(0.05));
            inputs.push(r.id);
            if r.changed() {
                changes.push(Change::Custom(name, Some(json!(v))));
            }
        }
    });
    ui.end_row();
}

/// The Keyframe panel for an effect key, after its Beat row.
fn effect_fields(ui: &mut egui::Ui, lane: i64, t: &Key, v3: bool, inputs: &mut Vec<egui::Id>, changes: &mut Vec<Change>) {
    let value = t.raw_value.unwrap_or(0);
    let name = |n: &str| if v3 { n.to_owned() } else { format!("_{n}") };
    match lane {
        5 => {
            ui.label("Color boost");
            let mut on = value != 0;
            if ui.checkbox(&mut on, "On").changed() {
                changes.push(Change::Value(i64::from(on), lane));
            }
            ui.end_row();
        }
        12 | 13 => {
            let precise = if v3 { "speed" } else { "_preciseSpeed" };
            let existing = ["speed", "_speed", "_preciseSpeed", "preciseSpeed"].into_iter().find(|n| t.extra_custom.contains_key(*n));
            ui.label("Speed");
            let mut v = effect_speed(t);
            let r = ui.add(egui::DragValue::new(&mut v).speed(0.1).range(0.0..=1000.0));
            inputs.push(r.id);
            if r.changed() {
                if existing.is_none() && v.fract() == 0.0 {
                    changes.push(Change::Value(v as i64, lane));
                } else {
                    // A fractional speed needs Chroma's field; the value keeps the lasers on.
                    changes.push(Change::Custom(existing.unwrap_or(precise).to_owned(), Some(json!(v))));
                    changes.push(Change::Value(i64::from(v > 0.0), lane));
                }
            }
            ui.end_row();

            let dir = name("direction");
            ui.label("Direction");
            let current = t.extra_custom.get(&dir).and_then(Value::as_i64);
            let text = |d: Option<i64>| match d {
                Some(0) => "Counter-clockwise",
                Some(_) => "Clockwise",
                None => "Random",
            };
            egui::ComboBox::from_id_salt("laser direction").selected_text(text(current)).show_ui(ui, |ui| {
                for d in [None, Some(0), Some(1)] {
                    if ui.selectable_label(current == d, text(d)).clicked() {
                        changes.push(Change::Custom(dir.clone(), d.map(|v| json!(v))));
                    }
                }
            });
            ui.end_row();

            let lock = if v3 { "lockRotation" } else { "_lockPosition" };
            ui.label("Keep angle");
            let mut on = t.extra_custom.get(lock).and_then(Value::as_bool).unwrap_or(false);
            if ui.checkbox(&mut on, "Don't jump to a new angle").changed() {
                changes.push(Change::Custom(lock.to_owned(), on.then(|| json!(true))));
            }
            ui.end_row();
        }
        8 | 9 => {
            ui.label("Chroma fields").on_hover_text("Tick a box to override the game's default.");
            ui.label("");
            ui.end_row();
            let fields: &[(&str, &str)] = if lane == 8 {
                &[("Rotation", "rotation"), ("Step", "step"), ("Propagation", "prop"), ("Speed", "speed"), ("Direction", "direction")]
            } else {
                &[("Step", "step"), ("Speed", "speed")]
            };
            for (label, field) in fields {
                optional_number(ui, label, name(field), t, inputs, changes);
            }
        }
        _ => {
            ui.label("Value");
            let mut v = value;
            let r = ui.add(egui::DragValue::new(&mut v).speed(0.1));
            inputs.push(r.id);
            if r.changed() {
                changes.push(Change::Value(v, lane));
            }
            ui.end_row();
        }
    }
}

/// Whether the map's Info.dat names Chroma (as a requirement or suggestion).
fn lists_chroma(m: &LoadedMap) -> bool {
    ["Info.dat", "info.dat"]
        .iter()
        .filter_map(|n| std::fs::read_to_string(m.folder.join(n)).ok())
        .any(|t| t.contains("\"Chroma\""))
}

fn compute_palette(rows: &[Row]) -> Vec<[f32; 4]> {
    let mut counts: Vec<([f32; 4], usize)> = Vec::new();
    for c in rows.iter().filter(|r| r.is_light()).flat_map(|r| &r.keys).filter_map(|k| k.color) {
        let q = c.map(|v| (v * 50.0).round() / 50.0);
        match counts.iter_mut().find(|(x, _)| *x == q) {
            Some(e) => e.1 += 1,
            None => counts.push((q, 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1));
    counts.into_iter().take(18).map(|(c, _)| c).collect()
}

/// Lights picked out in the preview: per lane, either every light (`None`) or those reached by
/// these map light IDs.
#[derive(Default)]
struct Highlight {
    lanes: HashMap<i64, Option<HashSet<i64>>>,
}

impl Highlight {
    fn add(&mut self, row: &Row) {
        let ids: Option<HashSet<i64>> = match &row.light_ids {
            Some(Value::Array(a)) if !a.is_empty() => Some(a.iter().filter_map(Value::as_i64).collect()),
            Some(v) => v.as_i64().map(|i| HashSet::from([i])),
            None => None,
        };
        match (self.lanes.entry(row.lane).or_insert_with(|| Some(HashSet::new())), ids) {
            (Some(have), Some(ids)) => have.extend(ids),
            (entry, _) => *entry = None,
        }
    }

    fn matches(&self, lane: i64, ids: &[i64]) -> bool {
        match self.lanes.get(&lane) {
            None => false,
            Some(None) => true,
            Some(Some(set)) => ids.iter().any(|i| set.contains(i)),
        }
    }

    /// For the simple renderer (custom geometry, the stand-in stage), which can't outline: solid white.
    fn rgb(&self, lane: i64, ids: &[i64], c: [f32; 3]) -> [f32; 3] {
        if self.matches(lane, ids) { [1.0; 3] } else { c }
    }
}

fn draw_key(p: &Painter, c: Pos2, k: &Key, selected: bool, r: f32) {
    let fill = if k.kind == Kind::Off {
        Color32::from_gray(70)
    } else {
        rgba32(k.display_color())
    };
    let stroke = if selected {
        Stroke::new(2.0, SELECTED)
    } else {
        Stroke::new(1.0, Color32::BLACK)
    };
    let pts = |v: &[(f32, f32)]| v.iter().map(|(x, y)| c + Vec2::new(x * r, y * r)).collect();
    match k.kind {
        Kind::On => {
            p.add(Shape::convex_polygon(
                pts(&[(0.0, -1.0), (1.0, 0.0), (0.0, 1.0), (-1.0, 0.0)]),
                fill,
                stroke,
            ));
        }
        Kind::Transition => {
            p.circle_filled(c, r * 0.85, fill);
            p.circle_stroke(c, r * 0.85, stroke);
        }
        Kind::Flash => {
            p.add(Shape::convex_polygon(
                pts(&[(0.0, -1.1), (1.0, 0.8), (-1.0, 0.8)]),
                fill,
                stroke,
            ));
        }
        Kind::Fade => {
            p.add(Shape::convex_polygon(
                pts(&[(-1.0, -0.8), (1.0, -0.8), (0.0, 1.1)]),
                fill,
                stroke,
            ));
        }
        Kind::Off => {
            let rect = Rect::from_center_size(c, Vec2::splat(r * 1.4));
            p.rect_filled(rect, 1.0, fill);
            p.rect_stroke(rect, 1.0, stroke, StrokeKind::Outside);
        }
    }
}

impl App {
    pub fn new(path: Option<String>, wgpu: Option<eframe::egui_wgpu::RenderState>) -> Self {
        let remembered = last_path_file()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .map(|s| s.trim().to_owned());
        let (player, status) = match Player::new() {
            Ok(p) => (Some(p), String::new()),
            Err(e) => (None, e),
        };
        let mut app = Self {
            path_input: path.clone().or(remembered).unwrap_or_default(),
            map: None,
            diff_idx: 0,
            beatmap: None,
            song: None,
            player,
            status,
            next_id: 0,
            dirty: false,
            close_armed: false,
            view_start: 0.0,
            px_per_beat: 40.0,
            scroll_y: 0.0,
            tl_left: LABEL_W,
            tl_width: 1000.0,
            playhead: 0.0,
            snap: 4,
            collapsed: HashSet::new(),
            selected: HashSet::new(),
            active_row: None,
            drag: Drag::None,
            box_rect: None,
            clipboard: Vec::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            last_panel_edit: 0.0,
            new_kind: Kind::Transition,
            palette: Vec::new(),
            show_preview: true,
            highlight_lights: false,
            preview_lights: Vec::new(),
            scene: Scene::default(),
            base_objs: Vec::new(),
            show_base: true,
            camera: Camera::default(),
            env: None,
            env_name: String::new(),
            model: None,
            rows_gen: 1,
            model_gen: 0,
            env_colors: map::MapColors::default(),
            gpu: wgpu.as_ref().map(|w| crate::gpu3d::Gpu3d::new(w.device.clone(), w.queue.clone())),
            wgpu,
            gpu_texture: None,
            env_lights: None,
            env_components: Vec::new(),
            motion: Default::default(),
            use_gpu: true,
            text_inputs: Vec::new(),
        };
        if !app.path_input.is_empty() {
            app.load_folder();
        }
        app
    }

    // ---- loading / saving ----

    fn load_folder(&mut self) {
        self.stop();
        let folder = PathBuf::from(self.path_input.trim());
        match map::load_info(&folder) {
            Ok(m) => {
                let mut notes = m.warnings.clone();
                self.song = match Song::load(&m.song_file) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        notes.push(e);
                        None
                    }
                };
                let first = m.difficulties.len().saturating_sub(1);
                self.map = Some(m);
                self.load_difficulty(first);
                if !notes.is_empty() {
                    self.status = format!("{} | {}", self.status, notes.join(" | "));
                }
                if let Some(f) = last_path_file() {
                    let _ = std::fs::create_dir_all(f.parent().unwrap_or(&f));
                    let _ = std::fs::write(f, self.path_input.trim());
                }
            }
            Err(e) => self.status = e,
        }
    }

    fn load_difficulty(&mut self, idx: usize) {
        let Some(m) = &self.map else { return };
        let Some(d) = m.difficulties.get(idx) else {
            self.beatmap = None;
            self.status = "This map has no difficulty files".into();
            return;
        };
        let env = EnvFile::load(&d.environment);
        map::set_base_colors(
            d.env_right.or(env.as_ref().map(|e| e.blue)).unwrap_or(map::DEFAULT_BASE_COLORS[0]),
            d.env_left.or(env.as_ref().map(|e| e.red)).unwrap_or(map::DEFAULT_BASE_COLORS[1]),
        );
        let env_name = d.environment.clone();
        match map::load_beatmap(&m.folder.join(&d.file), &mut self.next_id) {
            Ok(bm) => {
                self.collapsed = map::LIGHT_LANES
                    .iter()
                    .map(|(t, _)| *t)
                    .filter(|t| bm.rows.iter().filter(|r| r.lane == *t).count() > 6)
                    .collect();
                self.palette = compute_palette(&bm.rows);
                self.preview_lights = preview::build(&bm.rows);
                self.scene = Scene::parse(&bm.json);
                self.base_objs = if env.is_some() {
                    Vec::new()
                } else {
                    view3d::base_stage(&self.preview_lights, &bm.rows, &self.scene)
                };
                // The renderer's data: textures and light components next to the environment file.
                self.env_lights = None;
                self.env_components.clear();
                if let (Some(e), Some(dir)) = (&env, crate::environment::extras_dir(&env_name)) {
                    if !e.meshes.is_empty() {
                        self.env_lights = Some(crate::envscene::EnvLights::load(e, &dir));
                        self.env_components = crate::envscene::EnvLights::components(&dir);
                        if let Some(g) = &mut self.gpu {
                            g.set_environment(e, Some(&dir.join("textures")));
                        }
                    }
                }
                self.env = env;
                self.env_name = env_name;
                self.env_colors = d.colors;
                self.rows_gen += 1;
                self.show_base = !self.scene.hides_base;
                self.status = format!(
                    "Loaded {} ({} light events in {} rows)",
                    d.name,
                    bm.light_event_count(),
                    bm.rows.len()
                );
                self.beatmap = Some(bm);
                self.diff_idx = idx;
                self.selected.clear();
                self.active_row = None;
                self.undo.clear();
                self.redo.clear();
                self.dirty = false;
            }
            Err(e) => {
                self.beatmap = None;
                self.status = e;
            }
        }
    }

    fn save(&mut self) {
        if let Some(bm) = &mut self.beatmap {
            let note = {
                let targets = self.env_lights.as_ref().zip(self.env.as_ref()).map(|(el, e)| {
                    let unit = e.unit;
                    move |r: &Row| el.move_targets(r, unit)
                });
                let resolve = targets.as_ref().map(|f| f as &map::Resolve);
                bm.write_positions(resolve)
            };
            let moves = bm.rows.iter().any(|r| r.is_motion() && !r.keys.is_empty());
            // Aim lines can fight with laser-speed events that spin the same lasers.
            let spinning = self.env_lights.as_ref().map_or(0, |el| {
                let spb = 60.0 / self.map.as_ref().map_or(120.0, |m| m.bpm);
                let at = |k: &Key| (k.beat * spb) as f32;
                bm.rows
                    .iter()
                    .filter(|r| r.kind == RowKind::Aim && !r.keys.is_empty())
                    .map(|r| el.spinning(r, &self.motion, at(&r.keys[0]), at(&r.keys[r.keys.len() - 1])))
                    .sum::<usize>()
            });
            match bm.save() {
                Ok(msg) => {
                    // Warnings first: the backup path can run off the status bar.
                    let mut warn = note.unwrap_or_default();
                    if spinning > 0 {
                        warn += &format!(" {spinning} aimed lasers also spin from laser-speed events: turn those off or the game may fight the aim.");
                    }
                    if moves && !self.map.as_ref().is_some_and(lists_chroma) {
                        warn += " Add Chroma to the map's requirements or the lights won't move in the game.";
                    }
                    self.status = if warn.is_empty() { msg } else { format!("{}  ·  {msg}", warn.trim_start()) };
                    self.dirty = false;
                }
                Err(e) => self.status = e,
            }
        }
    }

    // ---- light model ----

    fn refresh_model(&mut self) {
        if self.model_gen == self.rows_gen {
            return;
        }
        self.model_gen = self.rows_gen;
        let bpm = self.bpm();
        if let Some(bm) = &mut self.beatmap {
            bm.sync_events();
        }
        self.model = self.beatmap.as_ref().map(|bm| {
            LightModel::build(bm, self.env.as_ref(), &self.env_colors, bpm)
        });
        self.motion = self
            .beatmap
            .as_ref()
            .map(|bm| crate::motion::Motion::new(&self.env_components, &bm.json, bpm))
            .unwrap_or_default();
    }

    fn light_rgb(&self, row: &Row, beat: f64) -> [f32; 3] {
        self.model.as_ref().map_or([0.0; 3], |m| m.row_rgb(row, beat))
    }

    // ---- time / view helpers ----

    fn bpm(&self) -> f64 {
        self.map.as_ref().map_or(120.0, |m| m.bpm)
    }

    fn beat_to_secs(&self, b: f64) -> f64 {
        b * 60.0 / self.bpm()
    }

    fn x_to_beat(&self, x: f32) -> f64 {
        self.view_start + f64::from(x - self.tl_left) / f64::from(self.px_per_beat)
    }

    fn beat_to_x(&self, b: f64) -> f32 {
        self.tl_left + ((b - self.view_start) * f64::from(self.px_per_beat)) as f32
    }

    fn snap_beat(&self, b: f64) -> f64 {
        if self.snap == 0 {
            b
        } else {
            let s = f64::from(self.snap);
            (b * s).round() / s
        }
    }

    fn song_beats(&self) -> f64 {
        self.song
            .as_ref()
            .map_or(400.0, |s| s.duration_secs() * self.bpm() / 60.0)
    }

    // ---- playback ----

    fn is_playing(&self) -> bool {
        self.player.as_ref().and_then(Player::position).is_some()
    }

    fn stop(&mut self) {
        if let Some(p) = &mut self.player {
            p.stop();
        }
    }

    fn play_from_playhead(&mut self) {
        let secs = self.beat_to_secs(self.playhead);
        if let (Some(p), Some(s)) = (&mut self.player, &self.song) {
            p.play(s, secs);
        }
    }

    fn toggle_play(&mut self) {
        if self.is_playing() {
            self.stop();
        } else {
            self.play_from_playhead();
        }
    }

    // ---- editing ----

    fn push_undo(&mut self) {
        if let Some(bm) = &self.beatmap {
            self.undo.push(bm.rows.clone());
            if self.undo.len() > 200 {
                self.undo.remove(0);
            }
            self.redo.clear();
        }
    }

    fn undo(&mut self) {
        let Some(bm) = &mut self.beatmap else { return };
        match self.undo.pop() {
            Some(prev) => {
                self.redo.push(std::mem::replace(&mut bm.rows, prev));
                self.dirty = true;
        self.rows_gen += 1;
                self.status = format!("Undo ({} more)", self.undo.len());
            }
            None => self.status = "Nothing to undo".into(),
        }
    }

    fn redo(&mut self) {
        let Some(bm) = &mut self.beatmap else { return };
        match self.redo.pop() {
            Some(next) => {
                self.undo.push(std::mem::replace(&mut bm.rows, next));
                self.dirty = true;
        self.rows_gen += 1;
                self.status = format!("Redo ({} more)", self.redo.len());
            }
            None => self.status = "Nothing to redo".into(),
        }
    }

    fn insert_key(&mut self, row_idx: usize, beat: f64) {
        let beat = self.snap_beat(beat).max(0.0);
        let preferred = self.new_kind;
        if self.beatmap.as_ref().is_none_or(|bm| row_idx >= bm.rows.len()) {
            return;
        }
        self.push_undo();
        self.next_id += 1;
        let id = self.next_id;
        let Some(bm) = &mut self.beatmap else { return };
        let v3 = bm.names.version == "v3";
        let row = &mut bm.rows[row_idx];
        let key = if row.is_motion() {
            // Starts where the lights are at that moment, so adding a key doesn't move them.
            let easing = row.keys.iter().rev().find(|k| k.beat <= beat).and_then(|k| k.easing.clone());
            Key::line(row.kind, id, beat, row.position_at(beat), easing)
        } else if row.is_effect() {
            let lane = row.lane;
            // Like the key before it; a first key turns the effect on.
            match row.keys.iter().rev().find(|k| k.beat <= beat) {
                Some(prev) => Key { id, beat, source: None, extra: Map::new(), ..prev.clone() },
                None => {
                    let mut k = Key::effect(id, beat, lane, i64::from(!matches!(lane, 8 | 9)));
                    k.brightness_src = v3.then(|| json!(1));
                    k
                }
            }
        } else { match row.keys.iter().rev().find(|k| k.beat <= beat) {
            Some(prev) => Key {
                id,
                beat,
                kind: if preferred == Kind::Transition && prev.kind == Kind::Off {
                    Kind::On
                } else {
                    preferred
                },
                extra: Map::new(),
                ..prev.clone()
            },
            None => Key {
                id,
                beat,
                kind: Kind::On,
                base: Base::Blue,
                color: None,
                color_rgb_only: false,
                brightness: 1.0,
                raw_value: None,
                color_src: None,
                brightness_src: None,
                easing: None,
                lerp_hsv: false,
                extra: Map::new(),
                extra_custom: Map::new(),
                pos: None,
                effect: false,
                source: None,
            },
        } };
        row.keys.push(key);
        row.sort();
        self.selected = HashSet::from([id]);
        self.active_row = Some(row_idx);
        self.dirty = true;
        self.rows_gen += 1;
    }

    /// The active light row, if it doesn't have a line of this kind yet.
    fn line_target(&self, kind: RowKind) -> Option<usize> {
        let rows = &self.beatmap.as_ref()?.rows;
        let i = self.active_row?;
        if !rows.get(i)?.is_light() {
            return None;
        }
        (!rows[i + 1..].iter().take_while(|r| r.is_motion()).any(|r| r.kind == kind)).then_some(i)
    }

    fn add_line(&mut self, i: usize, kind: RowKind) {
        self.push_undo();
        let Some(bm) = &mut self.beatmap else { return };
        let line = Row { lane: bm.rows[i].lane, light_ids: bm.rows[i].light_ids.clone(), keys: Vec::new(), kind };
        let unit = self.env.as_ref().map_or(1.0, |e| e.unit);
        let reach = self.env_lights.as_ref().map(|el| el.move_targets(&line, unit));
        let at = map::line_slot(&bm.rows, i, kind);
        let name = line.label();
        bm.rows.insert(at, line);
        self.active_row = Some(at);
        // Row indices moved.
        self.clipboard.clear();
        self.rows_gen += 1;
        self.status = match reach {
            None => format!("{name} line added. Export this environment to preview it and write it for the game."),
            Some((t, 0)) => format!("{name} line added: controls {} objects", t.len()),
            Some((t, n)) => format!("{name} line added: controls {} objects; {n} lights share a name with others and can't be moved", t.len()),
        };
    }

    fn delete_selected(&mut self) {
        if self.selected.is_empty() {
            return;
        }
        self.push_undo();
        if let Some(bm) = &mut self.beatmap {
            for row in &mut bm.rows {
                row.keys.retain(|k| !self.selected.contains(&k.id));
            }
        }
        self.selected.clear();
        self.dirty = true;
        self.rows_gen += 1;
    }

    fn copy_selected(&mut self) {
        let Some(bm) = &self.beatmap else { return };
        let picked: Vec<(usize, Key)> = bm
            .rows
            .iter()
            .enumerate()
            .flat_map(|(i, r)| {
                r.keys
                    .iter()
                    .filter(|k| self.selected.contains(&k.id))
                    .map(move |k| (i, k.clone()))
            })
            .collect();
        let Some(min) = picked.iter().map(|(_, k)| k.beat).min_by(f64::total_cmp) else {
            return;
        };
        self.clipboard = picked
            .into_iter()
            .map(|(i, mut k)| {
                k.beat -= min;
                (i, k)
            })
            .collect();
        self.status = format!("Copied {} keys", self.clipboard.len());
    }

    fn paste(&mut self) {
        if self.clipboard.is_empty() || self.beatmap.is_none() {
            return;
        }
        self.push_undo();
        let at = self.snap_beat(self.playhead);
        let mut new_ids = HashSet::new();
        let clip = self.clipboard.clone();
        for (row_idx, mut key) in clip {
            self.next_id += 1;
            key.id = self.next_id;
            key.beat += at;
            new_ids.insert(key.id);
            if let Some(row) = self
                .beatmap
                .as_mut()
                .and_then(|bm| bm.rows.get_mut(row_idx))
                .filter(|row| row.is_motion() == key.pos.is_some())
            {
                row.keys.push(key);
            }
        }
        if let Some(bm) = &mut self.beatmap {
            bm.rows.iter_mut().for_each(Row::sort);
        }
        self.status = format!("Pasted {} keys at beat {at:.3}", new_ids.len());
        self.selected = new_ids;
        self.dirty = true;
        self.rows_gen += 1;
    }

    fn selected_beats(&self) -> HashMap<u64, f64> {
        self.beatmap
            .iter()
            .flat_map(|bm| bm.rows.iter().flat_map(|r| &r.keys))
            .filter(|k| self.selected.contains(&k.id))
            .map(|k| (k.id, k.beat))
            .collect()
    }

    fn set_beats(&mut self, orig: &HashMap<u64, f64>, delta: f64) {
        self.rows_gen += 1;
        if let Some(bm) = &mut self.beatmap {
            for k in bm.rows.iter_mut().flat_map(|r| r.keys.iter_mut()) {
                if let Some(b) = orig.get(&k.id) {
                    k.beat = (b + delta).max(0.0);
                }
            }
        }
    }

    // ---- timeline layout & hit testing ----

    fn layout_lines(&self, top: f32) -> (Vec<Laid>, f32) {
        let mut out = Vec::new();
        let mut y = top - self.scroll_y;
        let Some(bm) = &self.beatmap else {
            return (out, 0.0);
        };
        for lane in map::LANE_ORDER {
            if map::is_effect_lane(*lane) {
                // One line each, no lane header.
                for (i, _) in bm.rows.iter().enumerate().filter(|(_, r)| r.lane == *lane) {
                    out.push(Laid { line: Line::Row(i), y, h: ROW_H });
                    y += ROW_H;
                }
                continue;
            }
            out.push(Laid {
                line: Line::Lane(*lane),
                y,
                h: LANE_H,
            });
            y += LANE_H;
            if !self.collapsed.contains(lane) {
                for (i, _) in bm.rows.iter().enumerate().filter(|(_, r)| r.lane == *lane) {
                    out.push(Laid {
                        line: Line::Row(i),
                        y,
                        h: ROW_H,
                    });
                    y += ROW_H;
                }
            }
        }
        (out, y + self.scroll_y - top)
    }

    fn line_rows(&self, line: Line) -> Vec<&Row> {
        let Some(bm) = &self.beatmap else {
            return Vec::new();
        };
        match line {
            Line::Row(i) => bm.rows.get(i).into_iter().collect(),
            Line::Lane(lane) => bm.rows.iter().filter(|r| r.lane == lane).collect(),
        }
    }

    /// Keys under the pointer: the nearest key in that line, plus any others at the same beat.
    fn keys_at(&self, lines: &[Laid], p: Pos2) -> Vec<u64> {
        let Some(l) = lines.iter().find(|l| p.y >= l.y && p.y < l.y + l.h) else {
            return Vec::new();
        };
        let rows = self.line_rows(l.line);
        let reach = f64::from((KEY_R + 2.0) / self.px_per_beat);
        let target = self.x_to_beat(p.x);
        let nearest = rows
            .iter()
            .flat_map(|r| key_range(r, target - reach, target + reach))
            .min_by(|a, b| (a.beat - target).abs().total_cmp(&(b.beat - target).abs()));
        let Some(n) = nearest else {
            return Vec::new();
        };
        rows.iter()
            .flat_map(|r| key_range(r, n.beat - 1e-6, n.beat + 1e-6))
            .map(|k| k.id)
            .collect()
    }

    fn find_key(&self, id: u64) -> Option<&Key> {
        self.beatmap
            .as_ref()?
            .rows
            .iter()
            .flat_map(|r| &r.keys)
            .find(|k| k.id == id)
    }

    // ---- input ----

    fn typing(&self, ctx: &egui::Context) -> bool {
        ctx.memory(|m| m.focused())
            .is_some_and(|id| self.text_inputs.contains(&id))
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        // Sliders, combo boxes and buttons keep keyboard focus after a click, which would
        // swallow the shortcuts (and let Space re-press a focused button). Only text fields
        // get to keep it.
        ctx.memory_mut(|m| {
            if let Some(id) = m.focused().filter(|id| !self.text_inputs.contains(id)) {
                m.surrender_focus(id);
            }
        });
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::S)) {
            self.save();
        }
        if self.typing(ctx) {
            return;
        }
        struct K {
            space: bool,
            insert: bool,
            delete: bool,
            undo: bool,
            redo: bool,
            home: bool,
            copy: bool,
            paste: bool,
            left: bool,
            right: bool,
        }
        let k = ctx.input(|i| {
            let cmd = i.modifiers.command;
            K {
                space: i.key_pressed(egui::Key::Space),
                insert: !cmd && i.key_pressed(egui::Key::I),
                delete: i.key_pressed(egui::Key::Delete)
                    || (!cmd && i.key_pressed(egui::Key::X)),
                undo: cmd && !i.modifiers.shift && i.key_pressed(egui::Key::Z),
                redo: cmd
                    && ((i.modifiers.shift && i.key_pressed(egui::Key::Z))
                        || i.key_pressed(egui::Key::Y)),
                home: i.key_pressed(egui::Key::Home),
                copy: i.events.iter().any(|e| matches!(e, egui::Event::Copy))
                    || (cmd && i.key_pressed(egui::Key::C)),
                paste: i.events.iter().any(|e| matches!(e, egui::Event::Paste(_)))
                    || (cmd && i.key_pressed(egui::Key::V)),
                left: i.key_pressed(egui::Key::ArrowLeft),
                right: i.key_pressed(egui::Key::ArrowRight),
            }
        });
        if k.space {
            self.toggle_play();
        }
        if k.insert {
            let row = self.active_row.unwrap_or(0);
            self.insert_key(row, self.playhead);
        }
        if k.delete {
            self.delete_selected();
        }
        if k.undo {
            self.undo();
        }
        if k.redo {
            self.redo();
        }
        if k.copy {
            self.copy_selected();
        }
        if k.paste {
            self.paste();
        }
        if k.home {
            self.playhead = 0.0;
            self.view_start = -1.0;
        }
        if k.left || k.right {
            let step = if self.snap == 0 {
                0.25
            } else {
                1.0 / f64::from(self.snap)
            };
            let dir = if k.left { -1.0 } else { 1.0 };
            self.playhead = self.snap_beat(self.playhead + dir * step).max(0.0);
            if self.is_playing() {
                self.play_from_playhead();
            }
        }
    }

    fn handle_timeline_input(
        &mut self,
        ui: &egui::Ui,
        resp: &egui::Response,
        lines: &[Laid],
        full: Rect,
        rows_top: f32,
        content_h: f32,
    ) {
        let (hover, scroll, zoom, mods, press) = ui.input(|i| {
            (
                i.pointer.hover_pos(),
                i.smooth_scroll_delta,
                i.zoom_delta(),
                i.modifiers,
                i.pointer.press_origin(),
            )
        });
        if let Some(h) = hover.filter(|h| full.contains(*h)) {
            if zoom != 1.0 {
                let ax = h.x.max(self.tl_left);
                let anchor = self.x_to_beat(ax);
                self.px_per_beat = (self.px_per_beat * zoom).clamp(1.0, 3000.0);
                self.view_start =
                    anchor - f64::from(ax - self.tl_left) / f64::from(self.px_per_beat);
            }
            self.view_start -= f64::from(scroll.x / self.px_per_beat);
            if h.y > rows_top {
                self.scroll_y -= scroll.y;
            } else {
                self.view_start -= f64::from(scroll.y / self.px_per_beat);
            }
        }

        let pointer = resp.interact_pointer_pos().or(hover);

        if resp.drag_started_by(PointerButton::Middle) {
            if let Some(p) = pointer {
                self.drag = Drag::Pan { last: p };
            }
        }
        if resp.drag_started_by(PointerButton::Primary) {
            if let Some(p) = press {
                if p.x < self.tl_left {
                    self.drag = Drag::None;
                } else if p.y < rows_top {
                    self.drag = Drag::Scrub;
                } else {
                    let hit = self.keys_at(lines, p);
                    if hit.is_empty() {
                        if !mods.shift {
                            self.selected.clear();
                        }
                        self.drag = Drag::Select { start: p };
                    } else {
                        if mods.shift {
                            self.selected.extend(hit);
                        } else if !hit.iter().any(|id| self.selected.contains(id)) {
                            self.selected = hit.into_iter().collect();
                        }
                        self.push_undo();
                        self.drag = Drag::Move {
                            start_beat: self.x_to_beat(p.x),
                            orig: self.selected_beats(),
                            moved: false,
                        };
                    }
                }
            }
        }

        let mut drag = std::mem::replace(&mut self.drag, Drag::None);
        if resp.dragged() {
            if let Some(p) = pointer {
                match &mut drag {
                    Drag::Move {
                        start_beat,
                        orig,
                        moved,
                    } => {
                        let mut delta = self.x_to_beat(p.x) - *start_beat;
                        if !mods.ctrl && self.snap > 0 {
                            let s = f64::from(self.snap);
                            delta = (delta * s).round() / s;
                        }
                        if delta != 0.0 {
                            *moved = true;
                        }
                        self.set_beats(orig, delta);
                    }
                    Drag::Select { start } => {
                        self.box_rect = Some(Rect::from_two_pos(*start, p));
                    }
                    Drag::Pan { last } => {
                        let d = p - *last;
                        self.view_start -= f64::from(d.x / self.px_per_beat);
                        self.scroll_y -= d.y;
                        *last = p;
                    }
                    Drag::Scrub => self.playhead = self.x_to_beat(p.x).max(0.0),
                    Drag::None => {}
                }
            }
        }
        if resp.drag_stopped() {
            match &drag {
                Drag::Move { moved, .. } => {
                    if *moved {
                        if let Some(bm) = &mut self.beatmap {
                            bm.rows.iter_mut().for_each(Row::sort);
                        }
                        self.dirty = true;
        self.rows_gen += 1;
                    } else {
                        self.undo.pop();
                    }
                }
                Drag::Select { .. } => {
                    if let Some(b) = self.box_rect.take() {
                        let (b0, b1) = (self.x_to_beat(b.left()), self.x_to_beat(b.right()));
                        let ids: Vec<u64> = lines
                            .iter()
                            .filter(|l| l.y + l.h > b.top() && l.y < b.bottom())
                            .flat_map(|l| self.line_rows(l.line))
                            .flat_map(|r| key_range(r, b0, b1))
                            .map(|k| k.id)
                            .collect();
                        self.selected.extend(ids);
                    }
                }
                Drag::Scrub => {
                    if self.is_playing() {
                        self.play_from_playhead();
                    }
                }
                _ => {}
            }
            drag = Drag::None;
        }
        self.drag = drag;

        if resp.clicked() {
            if let Some(p) = pointer {
                let line = lines.iter().find(|l| p.y >= l.y && p.y < l.y + l.h).copied();
                if p.y < rows_top {
                    if p.x >= self.tl_left {
                        self.playhead = self.x_to_beat(p.x).max(0.0);
                        if self.is_playing() {
                            self.play_from_playhead();
                        }
                    }
                } else if let Some(l) = line {
                    match l.line {
                        Line::Lane(lane) if p.x < self.tl_left => {
                            if !self.collapsed.remove(&lane) {
                                self.collapsed.insert(lane);
                            }
                        }
                        Line::Row(i) if p.x < self.tl_left => self.active_row = Some(i),
                        _ => {
                            if let Line::Row(i) = l.line {
                                self.active_row = Some(i);
                            }
                            let hit = self.keys_at(lines, p);
                            if mods.shift {
                                for id in hit {
                                    if !self.selected.remove(&id) {
                                        self.selected.insert(id);
                                    }
                                }
                            } else {
                                self.selected = hit.into_iter().collect();
                            }
                        }
                    }
                }
            }
        }
        if resp.double_clicked() {
            if let Some(p) = pointer {
                let line = lines.iter().find(|l| p.y >= l.y && p.y < l.y + l.h).copied();
                if let Some(Laid {
                    line: Line::Row(i), ..
                }) = line
                {
                    if p.x < self.tl_left {
                        // Double-click a row name: select all its keys.
                        if let Some(row) = self.beatmap.as_ref().and_then(|bm| bm.rows.get(i)) {
                            self.selected = row.keys.iter().map(|k| k.id).collect();
                        }
                    } else if self.keys_at(lines, p).is_empty() {
                        self.insert_key(i, self.x_to_beat(p.x));
                    }
                }
            }
        }

        let max_scroll = (content_h - (full.bottom() - rows_top)).max(0.0);
        self.scroll_y = self.scroll_y.clamp(0.0, max_scroll);
        self.view_start = self.view_start.max(-8.0);
    }

    // ---- painting ----

    fn grid_step(&self) -> f64 {
        [1.0 / 16.0, 0.125, 0.25, 0.5, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0]
            .into_iter()
            .find(|s| s * f64::from(self.px_per_beat) >= 60.0)
            .unwrap_or(256.0)
    }

    fn paint_timeline(
        &self,
        painter: &Painter,
        lines: &[Laid],
        full: Rect,
        rows_top: f32,
        hover: Option<Pos2>,
    ) {
        let Some(bm) = &self.beatmap else { return };
        let tl = Rect::from_min_max(Pos2::new(self.tl_left, full.top()), full.max);
        let rows_area = Rect::from_min_max(Pos2::new(full.left(), rows_top), full.max);
        let tl_rows = Rect::from_min_max(Pos2::new(self.tl_left, rows_top), full.max);
        let rp = painter.with_clip_rect(tl_rows);
        let lp = painter.with_clip_rect(Rect::from_min_max(
            rows_area.min,
            Pos2::new(self.tl_left, full.bottom()),
        ));
        let view_end = self.x_to_beat(tl.right());

        // Row backgrounds.
        for l in lines.iter().filter(|l| l.y + l.h >= rows_top && l.y <= full.bottom()) {
            let r = Rect::from_min_size(Pos2::new(full.left(), l.y), Vec2::new(full.width(), l.h));
            let bg = match l.line {
                Line::Lane(_) => Color32::from_gray(40),
                Line::Row(i) if Some(i) == self.active_row => Color32::from_rgb(42, 46, 62),
                Line::Row(_) => Color32::from_gray(29),
            };
            painter.with_clip_rect(rows_area).rect_filled(r, 0.0, bg);
        }

        // Grid.
        let step = self.grid_step();
        let sub = if step / 4.0 * f64::from(self.px_per_beat) >= 10.0 {
            step / 4.0
        } else {
            step
        };
        let mut b = (self.view_start / sub).floor() * sub;
        while b <= view_end {
            let x = self.beat_to_x(b);
            let col = if (b / 4.0).fract().abs() < 1e-9 {
                Color32::from_gray(70)
            } else if (b / step).fract().abs() < 1e-9 {
                Color32::from_gray(52)
            } else {
                Color32::from_gray(38)
            };
            rp.line_segment(
                [Pos2::new(x, rows_top), Pos2::new(x, full.bottom())],
                Stroke::new(1.0, col),
            );
            b += sub;
        }

        // Light strips and keys.
        let pad = f64::from(KEY_R * 2.0 / self.px_per_beat);
        for l in lines.iter().filter(|l| l.y + l.h >= rows_top && l.y <= full.bottom()) {
            let cy = l.y + l.h / 2.0;
            match l.line {
                Line::Row(i) => {
                    let row = &bm.rows[i];
                    let strip = Rect::from_min_max(
                        Pos2::new(tl.left(), l.y + 5.0),
                        Pos2::new(tl.right(), l.y + l.h - 5.0),
                    );
                    rp.rect_filled(strip, 0.0, Color32::from_gray(14));
                    if row.is_effect() {
                        // Boost on / lasers spinning: a band until the next key.
                        if matches!(row.lane, 5 | 12 | 13) {
                            for (i, k) in row.keys.iter().enumerate() {
                                let x1 = row.keys.get(i + 1).map_or(tl.right(), |n| self.beat_to_x(n.beat));
                                let x0 = self.beat_to_x(k.beat).max(tl.left());
                                if x1 > x0 && effect_active(k) {
                                    let level = if row.lane == 5 { 1.0 } else { (effect_speed(k) / 10.0).clamp(0.25, 1.0) };
                                    let c = Color32::from_rgb((60.0 + 150.0 * level) as u8, (50.0 + 125.0 * level) as u8, 40);
                                    rp.rect_filled(Rect::from_x_y_ranges(x0..=x1.min(tl.right()), strip.y_range()), 0.0, c);
                                }
                            }
                        }
                    } else if row.is_motion() {
                        // Bright while the lights move, dim while they hold.
                        let (bright, dim) = if row.kind == RowKind::Aim {
                            (Color32::from_rgb(200, 140, 60), Color32::from_rgb(85, 60, 30))
                        } else {
                            (Color32::from_rgb(70, 170, 185), Color32::from_rgb(30, 70, 80))
                        };
                        for (i, k) in row.keys.iter().enumerate() {
                            let next = row.keys.get(i + 1);
                            let moving = next.is_some_and(|n| n.pos != k.pos);
                            let x1 = next.map_or(tl.right(), |n| self.beat_to_x(n.beat));
                            let x0 = self.beat_to_x(k.beat).max(tl.left());
                            if x1 > x0 {
                                let c = if moving { bright } else { dim };
                                rp.rect_filled(Rect::from_x_y_ranges(x0..=x1.min(tl.right()), strip.y_range()), 0.0, c);
                            }
                        }
                    } else if !row.keys.is_empty() {
                        let mut x = tl.left();
                        let mut run: Option<(f32, Color32)> = None;
                        while x < tl.right() {
                            let c = rgb32(self.light_rgb(row, self.x_to_beat(x + 1.5)));
                            match run {
                                Some((_, rc)) if rc == c => {}
                                Some((x0, rc)) => {
                                    let r = Rect::from_x_y_ranges(x0..=x, strip.y_range());
                                    rp.rect_filled(r, 0.0, rc);
                                    run = Some((x, c));
                                }
                                None => run = Some((x, c)),
                            }
                            x += 3.0;
                        }
                        if let Some((x0, rc)) = run {
                            let r = Rect::from_x_y_ranges(x0..=tl.right(), strip.y_range());
                            rp.rect_filled(r, 0.0, rc);
                        }
                    }
                    for k in key_range(row, self.view_start - pad, view_end + pad) {
                        let c = Pos2::new(self.beat_to_x(k.beat), cy);
                        draw_key(&rp, c, k, self.selected.contains(&k.id), KEY_R);
                    }
                }
                Line::Lane(lane) => {
                    // Summary row: one small marker per pixel column that has keys.
                    let mut marks: Vec<(i32, bool)> = Vec::new();
                    for row in bm.rows.iter().filter(|r| r.lane == lane) {
                        for k in key_range(row, self.view_start - pad, view_end + pad) {
                            marks.push((
                                self.beat_to_x(k.beat).round() as i32,
                                self.selected.contains(&k.id),
                            ));
                        }
                    }
                    marks.sort_unstable();
                    marks.dedup_by(|a, b| {
                        if a.0 == b.0 {
                            b.1 |= a.1;
                            true
                        } else {
                            false
                        }
                    });
                    for (x, sel) in marks {
                        let c = Pos2::new(x as f32, cy);
                        let r = 4.0;
                        let col = if sel { SELECTED } else { Color32::from_gray(150) };
                        rp.add(Shape::convex_polygon(
                            vec![
                                c + Vec2::new(0.0, -r),
                                c + Vec2::new(r, 0.0),
                                c + Vec2::new(0.0, r),
                                c + Vec2::new(-r, 0.0),
                            ],
                            col,
                            Stroke::NONE,
                        ));
                    }
                }
            }
        }

        // Labels.
        let font = FontId::proportional(13.0);
        for l in lines.iter().filter(|l| l.y + l.h >= rows_top && l.y <= full.bottom()) {
            let cy = l.y + l.h / 2.0;
            match l.line {
                Line::Lane(lane) => {
                    let n = bm.rows.iter().filter(|r| r.lane == lane && r.is_light()).count();
                    let sign = if self.collapsed.contains(&lane) { "+" } else { "−" };
                    lp.text(
                        Pos2::new(full.left() + 8.0, cy),
                        Align2::LEFT_CENTER,
                        format!("{sign}  {}  ({n})", map::lane_name(lane)),
                        FontId::proportional(14.0),
                        Color32::from_gray(225),
                    );
                }
                Line::Row(i) => {
                    let row = &bm.rows[i];
                    if row.is_effect() {
                        lp.text(
                            Pos2::new(full.left() + 8.0, cy),
                            Align2::LEFT_CENTER,
                            format!("{}  [{}]", row.label(), row.keys.len()),
                            font.clone(),
                            Color32::from_rgb(225, 205, 130),
                        );
                        continue;
                    }
                    if row.is_motion() {
                        lp.text(
                            Pos2::new(full.left() + 62.0, cy),
                            Align2::LEFT_CENTER,
                            format!("{}  [{}]", row.label(), row.keys.len()),
                            font.clone(),
                            if row.kind == RowKind::Aim { Color32::from_rgb(235, 185, 110) } else { Color32::from_rgb(120, 200, 210) },
                        );
                        continue;
                    }
                    let sw = Rect::from_center_size(
                        Pos2::new(full.left() + 30.0, cy),
                        Vec2::splat(12.0),
                    );
                    lp.rect_filled(
                        sw,
                        2.0,
                        rgb32(self.light_rgb(row, self.playhead)),
                    );
                    lp.text(
                        Pos2::new(full.left() + 44.0, cy),
                        Align2::LEFT_CENTER,
                        format!("{}  [{}]", row.label(), row.keys.len()),
                        font.clone(),
                        Color32::from_gray(185),
                    );
                }
            }
        }
        painter.line_segment(
            [
                Pos2::new(self.tl_left, full.top()),
                Pos2::new(self.tl_left, full.bottom()),
            ],
            Stroke::new(1.0, Color32::from_gray(60)),
        );

        // Header: ruler and waveform.
        let header = Rect::from_min_max(full.min, Pos2::new(full.right(), rows_top));
        painter.rect_filled(header, 0.0, Color32::from_gray(20));
        let hp = painter.with_clip_rect(Rect::from_min_max(
            Pos2::new(self.tl_left, full.top()),
            Pos2::new(full.right(), rows_top),
        ));
        if let Some(m) = &self.map {
            painter.text(
                full.min + Vec2::new(8.0, 12.0),
                Align2::LEFT_CENTER,
                &m.song_name,
                FontId::proportional(13.0),
                Color32::from_gray(220),
            );
            painter.text(
                full.min + Vec2::new(8.0, 32.0),
                Align2::LEFT_CENTER,
                format!("{:.2} BPM", m.bpm),
                FontId::proportional(12.0),
                Color32::from_gray(150),
            );
        }
        let mut b = (self.view_start / step).floor() * step;
        while b <= view_end {
            let x = self.beat_to_x(b);
            hp.line_segment(
                [
                    Pos2::new(x, full.top() + RULER_H - 6.0),
                    Pos2::new(x, full.top() + RULER_H),
                ],
                Stroke::new(1.0, Color32::from_gray(120)),
            );
            let label = if step < 1.0 {
                format!("{b:.3}")
                    .trim_end_matches('0')
                    .trim_end_matches('.')
                    .to_owned()
            } else {
                format!("{b:.0}")
            };
            hp.text(
                Pos2::new(x + 3.0, full.top() + 8.0),
                Align2::LEFT_CENTER,
                label,
                FontId::monospace(11.0),
                Color32::from_gray(170),
            );
            b += step;
        }
        if let Some(song) = &self.song {
            let mid = full.top() + RULER_H + WAVE_H / 2.0;
            let blocks_per_sec = f64::from(song.sample_rate) / PEAK_BLOCK as f64;
            let mut x = tl.left();
            while x < tl.right() {
                let i0 = (self.beat_to_secs(self.x_to_beat(x)) * blocks_per_sec).max(0.0) as usize;
                let i1 = ((self.beat_to_secs(self.x_to_beat(x + 1.0)) * blocks_per_sec).max(0.0)
                    as usize)
                    .max(i0 + 1);
                if i0 < song.peaks.len() {
                    let (lo, hi) = song.peaks[i0..i1.min(song.peaks.len())]
                        .iter()
                        .fold((0f32, 0f32), |(l, h), (a, b)| (l.min(*a), h.max(*b)));
                    let s = WAVE_H / 2.0 - 2.0;
                    hp.line_segment(
                        [Pos2::new(x, mid - hi * s), Pos2::new(x, mid - lo * s + 1.0)],
                        Stroke::new(1.0, Color32::from_rgb(70, 160, 150)),
                    );
                }
                x += 1.0;
            }
        }

        // Playhead.
        let px = self.beat_to_x(self.playhead);
        if px >= tl.left() && px <= tl.right() {
            painter.line_segment(
                [Pos2::new(px, full.top()), Pos2::new(px, full.bottom())],
                Stroke::new(1.5, PLAYHEAD),
            );
            let tag = Rect::from_min_size(Pos2::new(px - 1.0, full.top()), Vec2::new(64.0, 16.0));
            painter.rect_filled(tag, 2.0, PLAYHEAD);
            painter.text(
                tag.left_center() + Vec2::new(4.0, 0.0),
                Align2::LEFT_CENTER,
                format!("{:.3}", self.playhead),
                FontId::monospace(11.0),
                Color32::WHITE,
            );
        }

        if let Some(b) = self.box_rect {
            rp.rect_filled(b, 0.0, Color32::from_rgba_unmultiplied(255, 190, 60, 25));
            rp.rect_stroke(b, 0.0, Stroke::new(1.0, SELECTED), StrokeKind::Inside);
        }

        // Hover info for the key under the pointer.
        if let Some(h) = hover.filter(|h| tl_rows.contains(*h) && matches!(self.drag, Drag::None)) {
            if let Some(k) = self.keys_at(lines, h).first().and_then(|id| self.find_key(*id)) {
                let mut text = format!(
                    "beat {:.3}  ·  {}  ·  {}  ·  brightness {:.2}",
                    k.beat,
                    k.kind.label(),
                    k.color.map_or_else(|| k.base.label().to_owned(), hex),
                    k.brightness
                );
                if k.kind == Kind::Transition {
                    text += &format!("  ·  {}", k.easing.as_deref().unwrap_or("easeLinear"));
                }
                let galley =
                    painter.layout_no_wrap(text, FontId::proportional(12.0), Color32::WHITE);
                let pos = (h + Vec2::new(14.0, -26.0))
                    .min(full.max - galley.size() - Vec2::splat(8.0));
                let bg = Rect::from_min_size(pos, galley.size()).expand(4.0);
                painter.rect_filled(bg, 3.0, Color32::from_black_alpha(220));
                painter.galley(pos, galley, Color32::WHITE);
            }
        }
    }

    fn timeline_ui(&mut self, ui: &mut egui::Ui) {
        let full = ui.available_rect_before_wrap();
        let painter = ui.painter_at(full);
        painter.rect_filled(full, 0.0, Color32::from_gray(24));
        if self.beatmap.is_none() {
            painter.text(
                full.center(),
                Align2::CENTER_CENTER,
                "Load a map folder (the one with Info.dat) to start",
                FontId::proportional(18.0),
                Color32::GRAY,
            );
            return;
        }
        self.tl_left = full.left() + LABEL_W;
        self.tl_width = full.right() - self.tl_left;
        let rows_top = full.top() + RULER_H + WAVE_H;

        let resp = ui.interact(full, ui.scope_id().with("timeline"), Sense::click_and_drag());
        let (lines, content_h) = self.layout_lines(rows_top);
        self.handle_timeline_input(ui, &resp, &lines, full, rows_top, content_h);

        let (lines, _) = self.layout_lines(rows_top);
        let hover = ui.input(|i| i.pointer.hover_pos());
        self.paint_timeline(&painter, &lines, full, rows_top, hover);
    }

    // ---- side panels ----

    fn properties_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("Keyframe");
        ui.checkbox(&mut self.highlight_lights, "Highlight in preview").on_hover_text(
            "Outlines the lights of the selected keys' rows in white. \
             With no key selected, uses the active row.",
        );
        ui.horizontal(|ui| {
            for (kind, label, hover) in [
                (RowKind::Position, "Add Position line", "Adds a line under the active light row, with keys that move its lights."),
                (RowKind::Aim, "Add Aim line", "Adds a line under the active light row, with keys that turn its lights to point somewhere else."),
            ] {
                let target = self.line_target(kind);
                if ui
                    .add_enabled(target.is_some(), egui::Button::new(label))
                    .on_hover_text(hover)
                    .on_disabled_hover_text("Click a light row that doesn't have this line yet.")
                    .clicked()
                    && let Some(i) = target
                {
                    self.add_line(i, kind);
                }
            }
        });
        ui.add_space(4.0);
        let template = self
            .selected
            .iter()
            .next()
            .and_then(|id| self.find_key(*id))
            .cloned();

        let Some(t) = template else {
            ui.label("Nothing selected.");
            ui.add_space(8.0);
            egui::ComboBox::from_label("New key type")
                .selected_text(self.new_kind.label())
                .show_ui(ui, |ui| {
                    for k in Kind::ALL {
                        ui.selectable_value(&mut self.new_kind, k, k.label());
                    }
                });
            ui.add_space(12.0);
            ui.label(egui::RichText::new("How to use").strong());
            for line in [
                "Double-click a row: add keyframe",
                "I: insert keyframe at playhead (active row)",
                "Click / Shift+click: select keys",
                "Drag empty space: box select",
                "Drag keys: move (hold Ctrl for no snap)",
                "Delete or X: remove keys",
                "Ctrl+C / Ctrl+V: copy, paste at playhead",
                "Ctrl+Z / Ctrl+Shift+Z: undo / redo",
                "Space: play / stop  ·  ←/→: step playhead",
                "Click ruler: move playhead",
                "Wheel: scroll rows  ·  Shift+wheel: pan",
                "Ctrl+wheel: zoom  ·  Middle-drag: pan",
                "Click a lane name: expand / collapse",
                "Double-click a row name: select its keys",
                "Ctrl+S: save",
            ] {
                ui.label(line);
            }
            ui.add_space(12.0);
            ui.label(egui::RichText::new("Shapes").strong());
            ui.label("◆ On   ● Transition   ▲ Flash   ▼ Fade   ■ Off");
            ui.label(
                "A Transition key is blended into from the key before it; that earlier key's \
                 \"Easing to next\" sets the curve. Chain transitions to build curves.",
            );
            return;
        };

        let count = self.selected.len();
        ui.label(format!(
            "{count} key{} selected",
            if count == 1 { "" } else { "s" }
        ));
        if count > 1 {
            ui.label(
                egui::RichText::new("Changes apply to all selected keys.")
                    .small()
                    .weak(),
            );
        }
        ui.add_space(6.0);
        let mut changes = Vec::new();

        egui::Grid::new("props").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
            if count == 1 {
                ui.label("Beat");
                let mut b = t.beat;
                let r = ui.add(egui::DragValue::new(&mut b).speed(0.02).range(0.0..=1.0e6));
                self.text_inputs.push(r.id);
                if r.changed() {
                    changes.push(Change::Beat(b));
                }
                ui.end_row();
            }

            if t.effect {
                let lane = self.beatmap.as_ref().and_then(|bm| {
                    bm.rows.iter().find(|r| r.keys.iter().any(|k| k.id == t.id)).map(|r| r.lane)
                });
                let v3 = self.beatmap.as_ref().is_some_and(|bm| bm.names.version == "v3");
                effect_fields(ui, lane.unwrap_or(0), &t, v3, &mut self.text_inputs, &mut changes);
                return;
            }

            if let Some(p) = t.pos {
                let aim = self.beatmap.as_ref().is_some_and(|bm| {
                    bm.rows.iter().any(|r| r.kind == RowKind::Aim && r.keys.iter().any(|k| k.id == t.id))
                });
                let (names, speed, unit) = if aim {
                    (["X (tilt down)", "Y (turn right)", "Z (roll)"], 0.5, "°")
                } else {
                    (["X (right)", "Y (up)", "Z (forward)"], 0.02, " m")
                };
                for (axis, name) in names.into_iter().enumerate() {
                    ui.label(name);
                    let mut v = p[axis];
                    let r = ui.add(egui::DragValue::new(&mut v).speed(speed).suffix(unit));
                    self.text_inputs.push(r.id);
                    if r.changed() {
                        changes.push(Change::Axis(axis, v));
                    }
                    ui.end_row();
                }
                ui.label("Easing to next");
                egui::ComboBox::from_id_salt("pos easing")
                    .selected_text(t.easing.as_deref().unwrap_or("easeLinear"))
                    .show_ui(ui, |ui| {
                        for e in eval::EASINGS {
                            let current = t.easing.as_deref().unwrap_or("easeLinear") == *e;
                            if ui.selectable_label(current, *e).clicked() {
                                changes.push(Change::Easing((*e != "easeLinear").then(|| (*e).to_owned())));
                            }
                        }
                    });
                ui.end_row();
                return;
            }

            ui.label("Type");
            egui::ComboBox::from_id_salt("kind")
                .selected_text(t.kind.label())
                .show_ui(ui, |ui| {
                    for k in Kind::ALL {
                        if ui.selectable_label(t.kind == k, k.label()).clicked() {
                            changes.push(Change::Kind(k));
                        }
                    }
                });
            ui.end_row();

            ui.label("Base color");
            egui::ComboBox::from_id_salt("base")
                .selected_text(t.base.label())
                .show_ui(ui, |ui| {
                    for b in Base::ALL {
                        if ui.selectable_label(t.base == b, b.label()).clicked() {
                            changes.push(Change::Base(b));
                        }
                    }
                });
            ui.end_row();

            ui.label("Chroma color");
            ui.horizontal(|ui| {
                let mut custom = t.color.is_some();
                if ui.checkbox(&mut custom, "").changed() {
                    changes.push(Change::Color(custom.then(|| t.base.rgba())));
                }
                if let Some(mut c) = t.color {
                    if ui.color_edit_button_rgba_unmultiplied(&mut c).changed() {
                        changes.push(Change::Color(Some(c)));
                    }
                    ui.monospace(hex(c));
                }
            });
            ui.end_row();

            ui.label("Brightness");
            let mut f = t.brightness;
            if ui.add(egui::Slider::new(&mut f, 0.0..=2.0)).changed() {
                changes.push(Change::Brightness(f));
            }
            ui.end_row();

            // The game (and ChroMapper) take a transition's easing and blend from the key before it.
            let starts_transition = matches!(t.kind, Kind::On | Kind::Off | Kind::Transition);
            ui.label("Easing to next")
                .on_hover_text("Used when the next key is a Transition: how this key blends into it.");
            ui.add_enabled_ui(starts_transition, |ui| {
                egui::ComboBox::from_id_salt("easing")
                    .selected_text(t.easing.as_deref().unwrap_or("easeLinear"))
                    .height(400.0)
                    .show_ui(ui, |ui| {
                        for e in eval::EASINGS {
                            let current = t.easing.as_deref().unwrap_or("easeLinear") == *e;
                            if ui.selectable_label(current, *e).clicked() {
                                changes.push(Change::Easing(
                                    (*e != "easeLinear").then(|| (*e).to_owned()),
                                ));
                            }
                        }
                    });
            });
            ui.end_row();

            ui.label("Blend to next");
            ui.add_enabled_ui(starts_transition, |ui| {
                let mut hsv = t.lerp_hsv;
                if ui
                    .checkbox(&mut hsv, "HSV (around the color wheel)")
                    .changed()
                {
                    changes.push(Change::Hsv(hsv));
                }
            });
            ui.end_row();
        });

        if !self.palette.is_empty() && t.pos.is_none() && !t.effect {
            ui.add_space(10.0);
            ui.label(egui::RichText::new("Colors used in this map").strong());
            ui.horizontal_wrapped(|ui| {
                for c in &self.palette {
                    let (rect, r) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::click());
                    ui.painter().rect_filled(rect, 3.0, rgba32(*c));
                    if r.on_hover_text(hex(*c)).clicked() {
                        changes.push(Change::Color(Some(*c)));
                    }
                }
            });
        }

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui.button("Delete").clicked() {
                self.delete_selected();
            }
            if ui.button("Deselect").clicked() {
                self.selected.clear();
            }
        });

        if !changes.is_empty() {
            let now = ui.input(|i| i.time);
            if now - self.last_panel_edit > 0.6 {
                self.push_undo();
            }
            self.last_panel_edit = now;
            if let Some(bm) = &mut self.beatmap {
                for row in &mut bm.rows {
                    let mut touched = false;
                    for k in row.keys.iter_mut().filter(|k| self.selected.contains(&k.id)) {
                        for c in &changes {
                            c.apply(k);
                        }
                        touched = true;
                    }
                    if touched {
                        row.sort();
                    }
                }
            }
            self.dirty = true;
        self.rows_gen += 1;
        }
    }

    /// The lights the "Highlight in preview" checkbox picks out: the selected keys' rows, or the
    /// active row when no key is selected.
    fn highlight(&self) -> Option<Highlight> {
        if !self.highlight_lights {
            return None;
        }
        let rows = &self.beatmap.as_ref()?.rows;
        let mut h = Highlight::default();
        for r in rows.iter().filter(|r| r.keys.iter().any(|k| self.selected.contains(&k.id))) {
            h.add(r);
        }
        if h.lanes.is_empty() {
            h.add(rows.get(self.active_row?)?);
        }
        Some(h)
    }

    fn preview_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let short = self.env_name.trim_end_matches("Environment");
            if self.env.is_some() {
                ui.checkbox(&mut self.show_base, format!("Environment: {short} (ChroMapper)"))
                    .on_hover_text(
                        "Imported from ChroMapper's recreation of this environment, with its light \
                         IDs. Rings and lasers stay in their resting positions.",
                    );
            } else {
                ui.checkbox(&mut self.show_base, "Base stage (approximate)").on_hover_text(format!(
                    "No imported environment for {}. Run --import-chromapper (see README) \
                     to use ChroMapper's environments; until then this is a generic stand-in.",
                    self.env_name
                ));
            }
            if ui.button("Reset camera").clicked() {
                self.camera = Camera::default();
            }
            let s = &self.scene;
            let mut info = format!(
                "{} custom objects · {} animated tracks",
                s.objects.len(),
                s.animated_tracks
            );
            if s.base_edits > 0 {
                info += &format!(" · {} edits to base objects not shown", s.base_edits);
            }
            let ignored: usize = s.ignored_events.values().sum();
            if ignored > 0 {
                let mut kinds: Vec<&str> = s.ignored_events.keys().map(String::as_str).collect();
                kinds.sort_unstable();
                info += &format!(" · {ignored} events not previewed ({})", kinds.join(", "));
            }
            ui.label(egui::RichText::new(info).small().weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new("drag: look · wheel: move · double-click: reset")
                        .small()
                        .weak(),
                );
            });
        });

        let rect = ui.available_rect_before_wrap();
        let resp = ui.allocate_rect(rect, Sense::click_and_drag());
        if resp.dragged() {
            let d = resp.drag_delta();
            self.camera.yaw += d.x * 0.25;
            self.camera.pitch = (self.camera.pitch - d.y * 0.25).clamp(-89.0, 89.0);
        }
        if resp.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                let (y, p) = (self.camera.yaw.to_radians(), self.camera.pitch.to_radians());
                let fwd = [y.sin() * p.cos(), p.sin(), y.cos() * p.cos()];
                for i in 0..3 {
                    self.camera.pos[i] += fwd[i] * scroll * 0.05;
                }
            }
        }
        if resp.double_clicked() {
            self.camera = Camera::default();
        }

        let painter = ui.painter_at(rect);
        let Some(bm) = &self.beatmap else {
            painter.rect_filled(rect, 0.0, Color32::from_rgb(4, 4, 8));
            return;
        };
        let _ = bm;
        let beat = self.playhead;
        let model = self.model.as_ref();
        let highlight = self.highlight();
        let light = |lane: i64, ids: &[i64]| {
            let c = model.map_or([0.0; 3], |m| m.rgb_keys(lane, ids, beat));
            highlight.as_ref().map_or(c, |h| h.rgb(lane, ids, c))
        };
        let mut objs: Vec<(&Obj, &Scene)> = Vec::new();
        if self.show_base {
            objs.extend(self.base_objs.iter().map(|o| (o, &self.scene)));
        }
        objs.extend(self.scene.objects.iter().map(|o| (o, &self.scene)));
        let env = self.env.as_ref().filter(|_| self.show_base);
        let seconds = (beat * 60.0 / self.bpm()) as f32;
        if let (true, Some(e), Some(el), Some(gpu), Some(rs)) = (self.use_gpu, env, &self.env_lights, &mut self.gpu, &self.wgpu) {
            let ppp = ui.ctx().pixels_per_point();
            let (w, h) = (((rect.width() * ppp) as u32).max(1), ((rect.height() * ppp) as u32).max(1));
            let mut state = el.state_at(e, &|k| model.map_or([0.0; 4], |m| m.color_keys(k.0, &k.1, beat)), &self.motion, seconds);
            if let Some(bm) = &self.beatmap {
                el.apply_positions(e, &mut state, &bm.rows, beat);
            }
            if let Some(h) = &highlight {
                state.outline = el.light_objects(&|k| h.matches(k.0, &k.1));
                state.outline_px = 2.0 * ppp;
            }
            let fog = e.fog.unwrap_or(crate::environment::FogParams {
                offset: 0.0,
                height: 25.0,
                start_y: -50.0,
                attenuation: 0.002,
                auto_exposure_limit: 1000.0,
                legacy_auto_exposure: false,
            });
            let view = crate::gpu3d::View::from_camera(&self.camera, e.unit);
            gpu.set_environment(e, None);
            let out = gpu.render(&crate::gpu3d::Frame { view, width: w, height: h, env: e, state: &state, fog, post_bloom: true });
            let tv = out.create_view(&Default::default());
            let mut r = rs.renderer.write();
            let id = match self.gpu_texture {
                Some((id, size)) if size == (w, h) => id,
                Some((id, _)) => {
                    r.update_egui_texture_from_wgpu_texture(&rs.device, &tv, eframe::wgpu::FilterMode::Linear, id);
                    id
                }
                None => r.register_native_texture(&rs.device, &tv, eframe::wgpu::FilterMode::Linear),
            };
            self.gpu_texture = Some((id, (w, h)));
            drop(r);
            painter.image(id, rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
            // Chroma custom geometry: the simple renderer, on top (no depth against the environment yet).
            if !objs.is_empty() {
                view3d::paint_overlay(&painter, rect, &objs, beat, &light, &self.camera);
            }
            ui.ctx().request_repaint();
            return;
        }
        view3d::paint(&painter, rect, &objs, env, beat, &light, &self.camera);
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Map folder");
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.path_input)
                    .desired_width(560.0)
                    .hint_text("…/Beat Saber_Data/CustomLevels/<map>"),
            );
            self.text_inputs.push(r.id);
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.button("Load").clicked() || enter {
                self.load_folder();
            }

            let mut pick = None;
            if let Some(m) = &self.map {
                egui::ComboBox::from_id_salt("difficulty")
                    .selected_text(
                        m.difficulties
                            .get(self.diff_idx)
                            .map_or("-", |d| d.name.as_str()),
                    )
                    .show_ui(ui, |ui| {
                        for (i, d) in m.difficulties.iter().enumerate() {
                            if ui.selectable_label(i == self.diff_idx, &d.name).clicked() {
                                pick = Some(i);
                            }
                        }
                    });
            }
            if let Some(i) = pick.filter(|i| *i != self.diff_idx) {
                if self.dirty {
                    self.status =
                        "Unsaved changes: save or revert before switching difficulty".into();
                } else {
                    self.load_difficulty(i);
                }
            }

            ui.separator();
            let label = if self.dirty { "Save*" } else { "Save" };
            if ui
                .add_enabled(self.beatmap.is_some(), egui::Button::new(label))
                .clicked()
            {
                self.save();
            }
            if ui
                .add_enabled(self.dirty, egui::Button::new("Revert"))
                .clicked()
            {
                self.dirty = false;
                self.load_difficulty(self.diff_idx);
            }
        });

        ui.horizontal(|ui| {
            let playing = self.is_playing();
            if ui
                .add_enabled(
                    self.song.is_some() && self.player.is_some(),
                    egui::Button::new(if playing { "Stop" } else { "Play" }),
                )
                .clicked()
            {
                self.toggle_play();
            }
            ui.monospace(format!(
                "beat {:>9.3}   {}",
                self.playhead,
                fmt_time(self.beat_to_secs(self.playhead))
            ));
            ui.separator();
            ui.label("Snap");
            egui::ComboBox::from_id_salt("snap")
                .width(60.0)
                .selected_text(match self.snap {
                    0 => "off".to_owned(),
                    1 => "1".to_owned(),
                    n => format!("1/{n}"),
                })
                .show_ui(ui, |ui| {
                    for (v, l) in [(0, "off"), (1, "1"), (2, "1/2"), (4, "1/4"), (8, "1/8"), (16, "1/16")] {
                        ui.selectable_value(&mut self.snap, v, l);
                    }
                });
            ui.label("New key");
            egui::ComboBox::from_id_salt("newkind")
                .width(90.0)
                .selected_text(self.new_kind.label())
                .show_ui(ui, |ui| {
                    for k in Kind::ALL {
                        ui.selectable_value(&mut self.new_kind, k, k.label());
                    }
                });
            ui.separator();
            ui.toggle_value(&mut self.show_preview, "Preview");
            if ui.button("Fit song").clicked() {
                self.px_per_beat = (self.tl_width / self.song_beats() as f32).max(1.0);
                self.view_start = 0.0;
            }
            if ui.button("−").clicked() {
                self.px_per_beat = (self.px_per_beat / 1.5).max(1.0);
            }
            if ui.button("+").clicked() {
                self.px_per_beat = (self.px_per_beat * 1.5).min(3000.0);
            }
            if let Some(p) = &mut self.player {
                ui.separator();
                ui.label("Volume");
                let mut v = p.volume;
                if ui.add(egui::Slider::new(&mut v, 0.0..=1.0).show_value(false)).changed() {
                    p.set_volume(v);
                }
            }
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        if ctx.input(|i| i.viewport().close_requested()) && self.dirty && !self.close_armed {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.close_armed = true;
            self.status = "Unsaved changes! Press Ctrl+S to save, or close again to discard.".into();
        }

        if let Some(secs) = self.player.as_ref().and_then(Player::position) {
            self.playhead = secs * self.bpm() / 60.0;
            let view_beats = f64::from(self.tl_width / self.px_per_beat);
            if self.playhead > self.view_start + view_beats * 0.9 || self.playhead < self.view_start
            {
                self.view_start = self.playhead - view_beats * 0.1;
            }
            ctx.request_repaint();
        }

        self.handle_keys(&ctx);
        self.text_inputs.clear();
        self.refresh_model();

        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            self.top_bar(ui);
            ui.add_space(2.0);
        });
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(&self.status);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(bm) = &self.beatmap {
                        ui.label(format!(
                            "{} light events · {} selected{}",
                            bm.light_event_count(),
                            self.selected.len(),
                            if self.dirty { " · unsaved" } else { "" }
                        ));
                    }
                });
            });
        });
        if self.show_preview {
            egui::Panel::bottom("preview")
                .resizable(true)
                .default_size(260.0)
                .show(ui, |ui| self.preview_ui(ui));
        }
        egui::Panel::right("props")
            .default_size(310.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.properties_ui(ui));
            });
        egui::CentralPanel::no_frame().show(ui, |ui| self.timeline_ui(ui));
    }
}
