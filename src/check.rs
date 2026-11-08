//! `--check <CustomLevels folder>`: loads every map read-only and verifies that saving
//! would reproduce the same light events. Nothing is written to disk.

use crate::audio::Song;
use crate::map;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Default)]
struct Report {
    maps: usize,
    diffs_ok: usize,
    not_v3: usize,
    errors: Vec<String>,
    mismatches: Vec<String>,
    audio_failed: Vec<String>,
    events: usize,
    rows: usize,
    formats: BTreeMap<&'static str, usize>,
    custom_objects: usize,
    maps_with_geometry: usize,
    with_env: usize,
    no_env: BTreeMap<String, usize>,
}

/// Canonical text form of an event: sorted keys, numbers rounded, v3 defaults filled in.
fn canonical(v: &Value) -> String {
    fn norm(v: &Value) -> Value {
        match v {
            Value::Number(n) => {
                let f = n.as_f64().unwrap_or(0.0);
                serde_json::json!((f * 10_000.0).round() / 10_000.0)
            }
            Value::Array(a) => Value::Array(a.iter().map(norm).collect()),
            Value::Object(o) => {
                let sorted: BTreeMap<_, _> = o.iter().map(|(k, v)| (k.clone(), norm(v))).collect();
                Value::Object(sorted.into_iter().collect())
            }
            other => other.clone(),
        }
    }
    let mut v = v.clone();
    if let Some(o) = v.as_object_mut() {
        let v2 = o.contains_key("_time") || o.contains_key("_type");
        let defaults = if v2 {
            [("_time", 0.0), ("_type", 0.0), ("_value", 0.0), ("_floatValue", 1.0)]
        } else {
            [("b", 0.0), ("et", 0.0), ("i", 0.0), ("f", 0.0)]
        };
        for (k, d) in defaults {
            o.entry(k).or_insert(serde_json::json!(d));
        }
        for key in ["customData", "_customData"] {
            if o.get(key).and_then(Value::as_object).is_some_and(|c| c.is_empty()) {
                o.remove(key);
            }
        }
    }
    let n = norm(&v);
    // Round-trip through a BTreeMap so nested object keys are ordered too.
    let ordered: BTreeMap<String, Value> = n
        .as_object()
        .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    serde_json::to_string(&ordered).unwrap_or_default()
}

fn check_map(folder: &Path, decode_audio: bool, report: &Mutex<Report>) {
    let name = folder.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let info = match map::load_info(folder) {
        Ok(i) => i,
        Err(e) => {
            report.lock().unwrap().errors.push(format!("{name}: {e}"));
            return;
        }
    };
    report.lock().unwrap().maps += 1;

    if decode_audio {
        match Song::load(&info.song_file) {
            Ok(s) if s.peaks.is_empty() => report
                .lock()
                .unwrap()
                .audio_failed
                .push(format!("{name}: decoded but empty")),
            Ok(_) => {}
            Err(e) => report.lock().unwrap().audio_failed.push(format!("{name}: {e}")),
        }
    }

    for d in &info.difficulties {
        let path = folder.join(&d.file);
        let mut next_id = 0;
        let bm = match map::load_beatmap(&path, &mut next_id) {
            Ok(bm) => bm,
            Err(e) => {
                let mut r = report.lock().unwrap();
                if e.contains("Only v2 and v3") {
                    r.not_v3 += 1;
                } else {
                    r.errors.push(format!("{name} / {}: {e}", d.name));
                }
                continue;
            }
        };

        // Exercise the preview evaluation on every row so a panic would show up here.
        let end = bm
            .rows
            .iter()
            .filter_map(|r| r.keys.last())
            .map(|k| k.beat)
            .fold(0.0, f64::max);
        let env = crate::environment::EnvFile::load(&d.environment);
        let model = crate::cmlight::LightModel::build(&bm, env.as_ref(), &d.colors, info.bpm);
        for row in &bm.rows {
            let mut b = 0.0;
            while b <= end + 1.0 {
                let c = model.row_rgb(row, b);
                assert!(c.iter().all(|v| v.is_finite()), "non-finite color in {name}");
                b += 0.37;
            }
        }
        let lanes = crate::preview::build(&bm.rows);
        let scene = crate::scene::Scene::parse(&bm.json);
        let base = crate::view3d::base_stage(&lanes, &bm.rows, &scene);
        let objs: Vec<(&crate::scene::Obj, &crate::scene::Scene)> =
            base.iter().chain(&scene.objects).map(|o| (o, &scene)).collect();
        for beat in [0.0, end * 0.25, end * 0.5, end] {
            let light = |lane: i64, ids: &[i64]| model.rgb_keys(lane, ids, beat);
            crate::view3d::triangle_count(&objs, env.as_ref(), beat, &light);
        }
        if env.is_some() {
            report.lock().unwrap().with_env += 1;
        } else {
            *report.lock().unwrap().no_env.entry(d.environment.clone()).or_default() += 1;
        }
        {
            let mut r = report.lock().unwrap();
            r.custom_objects += scene.objects.len();
            if !scene.objects.is_empty() {
                r.maps_with_geometry += 1;
            }
        }

        let mut before: Vec<String> = bm.json[bm.names.list]
            .as_array()
            .map(|a| a.iter().map(canonical).collect())
            .unwrap_or_default();
        let mut after: Vec<String> = bm.build_events().iter().map(canonical).collect();
        before.sort();
        after.sort();

        let mut r = report.lock().unwrap();
        r.events += before.len();
        r.rows += bm.rows.len();
        *r.formats.entry(bm.names.version).or_default() += 1;
        if before == after {
            r.diffs_ok += 1;
        } else {
            let missing = before.iter().find(|e| after.binary_search(e).is_err());
            let added = after.iter().find(|e| before.binary_search(e).is_err());
            r.mismatches.push(format!(
                "{name} / {}: {} events before, {} after\n      original: {}\n      rebuilt:  {}",
                d.name,
                before.len(),
                after.len(),
                missing.map_or("-", String::as_str),
                added.map_or("-", String::as_str),
            ));
        }
    }
}

pub fn run(dir: &Path, decode_audio: bool) -> i32 {
    let mut folders: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect(),
        Err(e) => {
            eprintln!("Can't read {}: {e}", dir.display());
            return 2;
        }
    };
    folders.sort();
    println!(
        "Checking {} map folders in {} (read-only{})",
        folders.len(),
        dir.display(),
        if decode_audio { ", decoding audio" } else { "" }
    );

    let report = Mutex::new(Report::default());
    let next = Mutex::new(0usize);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let i = {
                        let mut n = next.lock().unwrap();
                        *n += 1;
                        *n - 1
                    };
                    let Some(f) = folders.get(i) else { break };
                    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        check_map(f, decode_audio, &report);
                    }));
                    if res.is_err() {
                        report
                            .lock()
                            .unwrap()
                            .errors
                            .push(format!("{}: PANIC", f.display()));
                    }
                }
            });
        }
    });

    let r = report.into_inner().unwrap();
    println!(
        "\nMaps loaded: {}\ndifficulties round-tripped OK: {} (loaded per format: {:?})\nDifficulties skipped (unsupported format): {}\nLight rows: {}  ·  events compared: {}",
        r.maps, r.diffs_ok, r.formats, r.not_v3, r.rows, r.events
    );
    println!(
        "3D preview: {} difficulties with custom geometry, {} objects rendered",
        r.maps_with_geometry, r.custom_objects
    );
    println!(
        "Imported environment used for {} difficulties; none for: {:?}",
        r.with_env, r.no_env
    );
    for (title, list) in [
        ("Errors", &r.errors),
        ("Round-trip mismatches", &r.mismatches),
        ("Audio problems", &r.audio_failed),
    ] {
        println!("\n{title}: {}", list.len());
        for e in list.iter().take(25) {
            println!("  - {e}");
        }
        if list.len() > 25 {
            println!("  … and {} more", list.len() - 25);
        }
    }
    i32::from(!r.mismatches.is_empty() || !r.errors.is_empty())
}

/// `--save-test <map folder> <work dir>`: copies the map's files into `work dir`, then edits,
/// saves and reloads the copy to make sure a real save works. The original is never touched.
pub fn save_test(src: &Path, work: &Path) -> i32 {
    let run = || -> Result<String, String> {
        std::fs::create_dir_all(work).map_err(|e| e.to_string())?;
        for entry in std::fs::read_dir(src).map_err(|e| e.to_string())? {
            let p = entry.map_err(|e| e.to_string())?.path();
            if p.extension().is_some_and(|x| x == "dat") {
                std::fs::copy(&p, work.join(p.file_name().unwrap())).map_err(|e| e.to_string())?;
            }
        }
        let info = map::load_info(work)?;
        let d = info.difficulties.first().ok_or("no difficulties")?;
        let path = work.join(&d.file);
        let mut id = 0;
        let mut bm = map::load_beatmap(&path, &mut id)?;
        let before_total = bm.json[bm.names.list].as_array().map_or(0, Vec::len);
        let before_other = bm.other_events.clone();

        // Edit: recolor the first key, delete the last key of that row, add a new key.
        let row = bm.rows.iter_mut().find(|r| r.keys.len() >= 2).ok_or("no row with 2 keys")?;
        let first_beat = row.keys[0].beat;
        row.keys[0].color = Some([0.25, 0.5, 0.75, 1.0]);
        row.keys[0].color_src = None;
        row.keys.pop();
        let mut added = row.keys[0].clone();
        added.beat = 9999.5;
        added.kind = map::Kind::Transition;
        added.easing = Some("easeInOutSine".into());
        added.raw_value = None;
        row.keys.push(added);
        row.sort();
        bm.save()?;

        let mut id2 = 0;
        let re = map::load_beatmap(&path, &mut id2)?;
        let after_total = re.json[re.names.list].as_array().map_or(0, Vec::len);
        if after_total != before_total {
            return Err(format!("event count {before_total} -> {after_total}"));
        }
        if re.other_events != before_other {
            return Err("non-light events changed".into());
        }
        let keys: Vec<&map::Key> = re.rows.iter().flat_map(|r| &r.keys).collect();
        let recolored = keys.iter().any(|k| {
            k.beat == first_beat
                && k.color.is_some_and(|c| (c[0] - 0.25).abs() < 1e-6 && (c[2] - 0.75).abs() < 1e-6)
        });
        let new_key = keys.iter().any(|k| {
            k.beat == 9999.5
                && k.kind == map::Kind::Transition
                && k.easing.as_deref() == Some("easeInOutSine")
        });
        if !recolored || !new_key {
            return Err(format!("edits missing after reload (recolor {recolored}, new key {new_key})"));
        }
        let backups = std::fs::read_dir(work)
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".timeline-backup-"))
            .count();
        if backups != 1 {
            return Err(format!("expected 1 backup file, found {backups}"));
        }
        Ok(format!(
            "{} ({}): saved, reloaded, {after_total} events, edits present, backup written",
            d.file, re.names.version
        ))
    };
    match run() {
        Ok(m) => {
            println!("OK  {m}");
            0
        }
        Err(e) => {
            println!("FAIL {e}");
            1
        }
    }
}

/// `--render <map folder> <difficulty> <beats> <out dir> [ChroMapper pose]`: writes
/// `<beat>.ppm` snapshots of the preview. `difficulty` is an index or a name ("ExpertPlus");
/// `beats` is comma-separated.
pub fn render(folder: &Path, diff: &str, beats: &str, out: &Path, pose: Option<&str>) -> i32 {
    let run = || -> Result<(), String> {
        let info = map::load_info(folder)?;
        let d = match diff.parse::<usize>() {
            Ok(i) => info.difficulties.get(i),
            Err(_) => info
                .difficulties
                .iter()
                .filter(|d| d.name.starts_with("Standard"))
                .find(|d| d.name.ends_with(&format!(" {diff}"))),
        }
        .ok_or_else(|| format!("no difficulty {diff}"))?;
        let mut id = 0;
        let bm = map::load_beatmap(&folder.join(&d.file), &mut id)?;
        let lanes = crate::preview::build(&bm.rows);
        let scene = crate::scene::Scene::parse(&bm.json);
        let env = crate::environment::EnvFile::load(&d.environment).filter(|_| !scene.hides_base);
        crate::map::set_base_colors(
            d.env_right.or(env.as_ref().map(|e| e.blue)).unwrap_or(crate::map::DEFAULT_BASE_COLORS[0]),
            d.env_left.or(env.as_ref().map(|e| e.red)).unwrap_or(crate::map::DEFAULT_BASE_COLORS[1]),
        );
        let base = if scene.hides_base || env.is_some() {
            Vec::new()
        } else {
            crate::view3d::base_stage(&lanes, &bm.rows, &scene)
        };
        let objs: Vec<(&crate::scene::Obj, &crate::scene::Scene)> =
            base.iter().chain(&scene.objects).map(|o| (o, &scene)).collect();
        let cam = match pose {
            Some(p) => crate::view3d::Camera::from_chromapper_pose(p).ok_or("bad pose")?,
            None => crate::view3d::Camera::default(),
        };
        let model = crate::cmlight::LightModel::build(&bm, env.as_ref(), &d.colors, info.bpm);
        std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
        let (w, h) = (1280, 420);
        // The GPU renderer when the environment was exported with its renderer data (as the editor).
        let gpu_env = env.as_ref().filter(|e| !e.meshes.is_empty()).zip(crate::environment::extras_dir(&d.environment));
        if let Some((e, dir)) = gpu_env.filter(|_| std::env::var_os("BSLT_SOFTWARE").is_none()) {
            let lights = crate::envscene::EnvLights::load(e, &dir);
            let motion = crate::motion::Motion::new(&crate::envscene::EnvLights::components(&dir), &bm.json, info.bpm);
            let mut gpu = crate::gpu3d::Gpu3d::headless()?;
            gpu.set_environment(e, Some(&dir.join("textures")));
            let view = match pose {
                Some(p) => crate::gpu3d::View::from_pose(p).ok_or("bad pose")?,
                None => crate::gpu3d::View::from_camera(&cam, e.unit),
            };
            let fog = e.fog.unwrap_or(crate::environment::FogParams {
                offset: 0.0,
                height: 25.0,
                start_y: -50.0,
                attenuation: 0.002,
                auto_exposure_limit: 1000.0,
                legacy_auto_exposure: false,
            });
            for b in beats.split(',').filter_map(|b| b.trim().parse::<f64>().ok()) {
                let seconds = (b * 60.0 / info.bpm) as f32;
                let state = lights.state_at(e, &|k| model.color_keys(k.0, &k.1, b), &motion, seconds);
                gpu.render(&crate::gpu3d::Frame { view, width: w as u32, height: h as u32, env: e, state: &state, fog, post_bloom: true });
                crate::gpu3d::write_png(&out.join(format!("{b}.png")), w as u32, h as u32, &gpu.read_rgb())?;
            }
            println!("{} · {} · env {} (GPU)", d.name, beats, d.environment);
            return Ok(());
        }
        for b in beats.split(',').filter_map(|b| b.trim().parse::<f64>().ok()) {
            let light = |lane: i64, ids: &[i64]| model.rgb_keys(lane, ids, b);
            let px = crate::view3d::render_rgb(&objs, env.as_ref(), b, &light, &cam, w, h);
            let mut data = format!("P6 {w} {h} 255\n").into_bytes();
            data.extend(px);
            std::fs::write(out.join(format!("{b}.ppm")), data).map_err(|e| e.to_string())?;
        }
        println!(
            "{} · {} · env {}{}",
            d.name,
            beats,
            d.environment,
            if env.is_some() { " (imported)" } else { " (stand-in)" }
        );
        Ok(())
    };
    match run() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// `--plan-compare <CustomLevels> <jobs.tsv>`: one ChroMapper comparison job per map: its hardest
/// Standard difficulty and four beats spread over its light events.
pub fn plan_compare(dir: &Path, jobs: &Path) -> i32 {
    let mut folders: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.is_dir()).collect())
        .unwrap_or_default();
    folders.sort();
    let order = ["ExpertPlus", "Expert", "Hard", "Normal", "Easy"];
    let mut lines = Vec::new();
    for (i, f) in folders.iter().enumerate() {
        let Ok(info) = map::load_info(f) else { continue };
        let pick = order.iter().find_map(|o| {
            let d = info.difficulties.iter().find(|d| d.name == format!("Standard {o}"))?;
            let mut id = 0;
            let bm = map::load_beatmap(&f.join(&d.file), &mut id).ok()?;
            let beats: Vec<f64> = bm.rows.iter().flat_map(|r| r.keys.iter().map(|k| k.beat)).collect();
            let lo = beats.iter().copied().fold(f64::MAX, f64::min);
            let hi = beats.iter().copied().fold(f64::MIN, f64::max);
            (hi > lo).then(|| (o.to_string(), lo, hi))
        });
        let Some((diff, lo, hi)) = pick else { continue };
        let beats: Vec<String> = [0.2, 0.4, 0.6, 0.8]
            .iter()
            .map(|q| format!("{:.3}", lo + (hi - lo) * q))
            .collect();
        let rel = f.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        lines.push(format!("m{i:04}\t/maps/{rel}\t{diff}\t{}", beats.join(",")));
    }
    match std::fs::write(jobs, lines.join("\n") + "\n") {
        Ok(()) => {
            println!("{} jobs written to {}", lines.len(), jobs.display());
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// The Info.dat `_customData` of a Standard difficulty.
fn info_difficulty_custom(folder: &Path, diff: &str) -> Value {
    let info = ["Info.dat", "info.dat"]
        .iter()
        .find_map(|n| std::fs::read_to_string(folder.join(n)).ok())
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .unwrap_or(Value::Null);
    for set in info["_difficultyBeatmapSets"].as_array().into_iter().flatten() {
        if set["_beatmapCharacteristicName"] != "Standard" {
            continue;
        }
        for d in set["_difficultyBeatmaps"].as_array().into_iter().flatten() {
            if d["_difficulty"] == diff {
                return d["_customData"].clone();
            }
        }
    }
    Value::Null
}

/// ChroMapper's `ConvertCustomBpmToOfficial`, which it runs on every map it opens: legacy custom
/// BPM changes (and a positive `_editorOffset`) become official BPM events, and every object's beat
/// is moved so its real time stays the same. Returns the map beat (as the game reads it) for
/// ChroMapper's beat `t`; `t` unchanged when the map has nothing to convert.
struct CmBpmConversion {
    song_bpm: f32,
    /// (beat, bpm), in file order, duplicates of a beat dropped.
    changes: Vec<(f32, f32)>,
}

impl CmBpmConversion {
    fn new(map: &Value, diff_custom: &Value, song_bpm: f64) -> CmBpmConversion {
        let song_bpm = song_bpm as f32;
        let mut changes: Vec<(f32, f32)> = Vec::new();
        let lists = [
            (&map["_customData"]["_BPMChanges"], "_time", "_BPM"),
            (&map["_customData"]["_bpmChanges"], "_time", "_BPM"),
            (&map["_BPMChanges"], "_time", "_BPM"),
            (&map["_bpmChanges"], "_time", "_BPM"),
            (&map["customData"]["BPMChanges"], "b", "m"),
        ];
        for (list, tk, bk) in lists {
            for c in list.as_array().into_iter().flatten() {
                let (Some(t), Some(b)) = (c[tk].as_f64(), c[bk].as_f64()) else { continue };
                if !changes.iter().any(|x| x.0 == t as f32) {
                    changes.push((t as f32, b as f32));
                }
            }
        }
        if let Some(off) = diff_custom["_editorOffset"].as_f64().filter(|o| *o > 0.0) {
            changes.insert(0, (song_bpm / 60.0 * (off as f32 / 1000.0), song_bpm));
        }
        CmBpmConversion { song_bpm, changes }
    }

    /// Where ChroMapper moves an object at map beat `x`.
    fn forward(&self, mut x: f32) -> f32 {
        let mut ch = self.changes.clone();
        for i in 0..ch.len() {
            let t = ch[i].0;
            if (t - t.round()).abs() > 0.01 {
                let off = 1.0 - t % 1.0;
                if x >= t {
                    x += off;
                }
                for c in &mut ch[i..] {
                    c.0 += off;
                }
            }
            let (t, bpm) = ch[i];
            let next = ch.get(i + 1).map(|c| c.0);
            let span = next.map_or(0.0, |n| n - t);
            let scale = bpm / self.song_bpm - 1.0;
            if t < x {
                x += if next.is_none_or(|n| x < n) { (x - t) * scale } else { span * scale };
            }
            for c in &mut ch[i + 1..] {
                c.0 += span * scale;
            }
        }
        x
    }

    fn to_map_beat(&self, t: f64) -> f64 {
        if self.changes.is_empty() {
            return t;
        }
        // `forward` only ever increases with x: find the map beat it sends to t.
        let t = t as f32;
        let (mut lo, mut hi) = (-1.0e5f32, 1.0e5f32);
        for _ in 0..100 {
            let mid = (lo + hi) / 2.0;
            if self.forward(mid) < t { lo = mid } else { hi = mid }
        }
        hi as f64
    }
}

/// `--compare-lights <CustomLevels> <jobs.tsv> <ChroMapper out dir>`: compares every light's color
/// recorded in ChroMapper (lights.jsonl) with the preview's color for the same lane, ID and beat.
pub fn compare_lights(dir: &Path, jobs: &Path, out: &Path) -> i32 {
    let Ok(text) = std::fs::read_to_string(jobs) else {
        eprintln!("can't read {}", jobs.display());
        return 2;
    };
    #[derive(Default)]
    struct Tally {
        compared: usize,
        matched: usize,
        on_off_wrong: usize,
        maps: usize,
    }
    let mut by_env: BTreeMap<String, Tally> = BTreeMap::new();
    let mut worst: Vec<(f32, String)> = Vec::new();
    let mut per_map: Vec<(f32, String)> = Vec::new();
    let mut missing = 0;
    for line in text.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        let [name, map_path, diff, _beats] = cols[..] else { continue };
        let folder = dir.join(map_path.trim_start_matches("/maps/"));
        let Ok(lj) = std::fs::read_to_string(out.join(name).join("lights.jsonl")) else {
            missing += 1;
            continue;
        };
        let Ok(info) = map::load_info(&folder) else { continue };
        let Some(d) = info.difficulties.iter().find(|d| d.name == format!("Standard {diff}")) else { continue };
        let mut id = 0;
        let Ok(bm) = map::load_beatmap(&folder.join(&d.file), &mut id) else { continue };
        let env = crate::environment::EnvFile::load(&d.environment);
        crate::map::set_base_colors(
            d.env_right.or(env.as_ref().map(|e| e.blue)).unwrap_or(crate::map::DEFAULT_BASE_COLORS[0]),
            d.env_left.or(env.as_ref().map(|e| e.red)).unwrap_or(crate::map::DEFAULT_BASE_COLORS[1]),
        );
        let model = crate::cmlight::LightModel::build_with(&bm, env.as_ref(), &d.colors, info.bpm, true);
        let diff_custom = info_difficulty_custom(&folder, diff);
        let conv = CmBpmConversion::new(&bm.json, &diff_custom, info.bpm);
        let t = by_env.entry(d.environment.clone()).or_default();
        t.maps += 1;
        let (mut mc, mut mm) = (0usize, 0usize);
        for rec in lj.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
            // ChroMapper compares in f32: an event at exactly the recorded time has fired.
            let beat = conv.to_map_beat(rec["beat"].as_f64().unwrap_or(0.0)) as f32 as f64;
            for l in rec["lights"].as_array().into_iter().flatten() {
                let Some(lane) = l[0].as_i64() else { continue };
                // One map light ID, or the list of IDs that reach the light (empty: none).
                let keys: Vec<i64> = match &l[1] {
                    Value::Array(a) => a.iter().filter_map(Value::as_i64).collect(),
                    v => v.as_i64().into_iter().collect(),
                };
                let lid = keys.first().copied().unwrap_or(-1);
                if !crate::map::LIGHT_LANES.iter().any(|(t, _)| *t == lane) {
                    continue;
                }
                let c = &l[2];
                let ch = |i: usize| c[i].as_f64().unwrap_or(0.0) as f32;
                // Same as LightModel::rgb: brightness above 1 (Chroma alpha > 1) brightens.
                let a = ch(3).max(0.0);
                let cm = [ch(0) * a, ch(1) * a, ch(2) * a].map(|v| v.clamp(0.0, 1.0));
                let ours = model.rgb_keys(lane, &keys, beat);
                let diffv = (0..3).map(|k| (cm[k] - ours[k]).abs()).fold(0.0, f32::max);
                let lit = |c: [f32; 3]| c.iter().any(|v| *v > 0.05);
                t.compared += 1;
                mc += 1;
                if diffv <= 0.1 {
                    t.matched += 1;
                    mm += 1;
                } else if lit(cm) != lit(ours) {
                    t.on_off_wrong += 1;
                }
                if diffv > 0.1 {
                    worst.push((
                        diffv,
                        format!(
                            "{name} {} beat {beat}: lane {lane} id {lid}: ChroMapper {:.2?} vs ours {:.2?}",
                            d.environment, cm, ours
                        ),
                    ));
                }
            }
        }
        if mc > 0 {
            per_map.push((
                mm as f32 / mc as f32,
                format!("{name} {} [{}] {}/{} lights match", d.environment, map_path, mm, mc),
            ));
        }
    }
    let (tc, tm): (usize, usize) = by_env.values().fold((0, 0), |a, t| (a.0 + t.compared, a.1 + t.matched));
    println!(
        "Light colors compared: {tc}, within 0.1 of ChroMapper: {tm} ({:.1}%); jobs without ChroMapper output: {missing}\n",
        100.0 * tm as f64 / tc.max(1) as f64
    );
    println!("{:30} {:>5} {:>9} {:>7} {:>9}", "environment", "maps", "compared", "match%", "on/off✗");
    for (env, t) in &by_env {
        println!(
            "{env:30} {:>5} {:>9} {:>6.1}% {:>9}",
            t.maps,
            t.compared,
            100.0 * t.matched as f64 / t.compared.max(1) as f64,
            t.on_off_wrong
        );
    }
    per_map.sort_by(|a, b| a.0.total_cmp(&b.0));
    println!("\nLowest-matching maps:");
    for (_, m) in per_map.iter().take(15) {
        println!("  {m}");
    }
    worst.sort_by(|a, b| b.0.total_cmp(&a.0));
    let all: String = worst.iter().map(|(d, w)| format!("{d:.3}\t{w}\n")).collect();
    let _ = std::fs::write(out.join("mismatches.txt"), all);
    println!("\nLargest differences:");
    for (_, w) in worst.iter().take(15) {
        println!("  {w}");
    }
    println!("(all differences: {})", out.join("mismatches.txt").display());
    0
}
