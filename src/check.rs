//! `--check <CustomLevels folder>`: loads every map read-only and verifies that saving
//! would reproduce the same light events. Nothing is written to disk.

use crate::audio::Song;
use crate::eval;
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
            [("b", 0.0), ("et", 0.0), ("i", 0.0), ("f", 1.0)]
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
        for row in &bm.rows {
            let mut b = 0.0;
            while b <= end + 1.0 {
                let c = eval::row_color(row, b, 2.0);
                assert!(c.iter().all(|v| v.is_finite()), "non-finite color in {name}");
                b += 0.37;
            }
        }

        let lanes = crate::preview::build(&bm.rows);
        let mut b = 0.0;
        while b <= end + 1.0 {
            for l in &lanes {
                let cols = crate::preview::lane_colors(&bm.rows, l, b, 2.0, 4);
                assert!(cols.iter().flatten().all(|v| v.is_finite()), "bad preview color in {name}");
            }
            b += 1.7;
        }

        let scene = crate::scene::Scene::parse(&bm.json);
        let base = crate::view3d::base_stage(&lanes, &bm.rows, &scene);
        let objs: Vec<(&crate::scene::Obj, &crate::scene::Scene)> =
            base.iter().chain(&scene.objects).map(|o| (o, &scene)).collect();
        for beat in [0.0, end * 0.25, end * 0.5, end] {
            let light = |lane: i64, id: Option<i64>| {
                crate::preview::light_rgb(&bm.rows, &lanes, lane, id, beat, 2.0)
            };
            crate::view3d::triangle_count(&objs, beat, &light);
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

/// `--render <map folder> <difficulty index> <beat> <out.ppm>`: writes a preview snapshot.
pub fn render(folder: &Path, diff: usize, beat: f64, out: &Path) -> i32 {
    let run = || -> Result<(), String> {
        let info = map::load_info(folder)?;
        let d = info.difficulties.get(diff).ok_or("no such difficulty")?;
        let mut id = 0;
        let bm = map::load_beatmap(&folder.join(&d.file), &mut id)?;
        let lanes = crate::preview::build(&bm.rows);
        let scene = crate::scene::Scene::parse(&bm.json);
        let base = if scene.hides_base {
            Vec::new()
        } else {
            crate::view3d::base_stage(&lanes, &bm.rows, &scene)
        };
        let objs: Vec<(&crate::scene::Obj, &crate::scene::Scene)> =
            base.iter().chain(&scene.objects).map(|o| (o, &scene)).collect();
        let fade = info.bpm / 60.0;
        let light = |lane: i64, id: Option<i64>| crate::preview::light_rgb(&bm.rows, &lanes, lane, id, beat, fade);
        let (w, h) = (1280, 420);
        let px = crate::view3d::render_rgb(&objs, beat, &light, w, h);
        let mut data = format!("P6 {w} {h} 255\n").into_bytes();
        data.extend(px);
        std::fs::write(out, data).map_err(|e| e.to_string())?;
        println!("{} · beat {beat}: {} custom objects", d.name, scene.objects.len());
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
