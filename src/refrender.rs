//! `--render-ref <capture dir> [pose]`: renders a ChroMapper capture (test/compare-chromapper.sh with
//! PROBE_FULL=1) with the GPU renderer, using ChroMapper's own recorded light state at each snapshot,
//! so the renderer can be checked against ChroMapper's screenshots independently of the light model.
//! Writes `ours-<beat>.png` next to each `<beat>.png`.

use crate::environment::{EnvFile, FogParams};
use crate::gpu3d::{Block, DirLights, FogLight, Frame, Gpu3d, SceneState, SkyGradient, View};
use serde_json::Value;
use std::path::Path;

/// The camera pose test/cm-run-job.sh gives ChroMapper-PropPreview.
pub const DEFAULT_POSE: &str = "0,2.833,-1.667,-8,0,0,70,0.15,5000";

fn f(v: &Value) -> f32 {
    v.as_f64().unwrap_or(0.0) as f32
}

fn v4(v: &Value) -> [f32; 4] {
    std::array::from_fn(|i| v.get(i).map_or(0.0, f))
}

fn m16(v: &Value) -> Option<[f32; 16]> {
    let a = v.as_array()?;
    (a.len() == 16).then(|| std::array::from_fn(|i| f(&a[i])))
}

fn read_json(path: &Path) -> Value {
    std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn read_lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .map(|t| t.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
        .unwrap_or_default()
}

/// BloomFogObjects' static settings (bloomfog.json).
pub fn fog_lights(bloomfog: &Value) -> Vec<FogLight> {
    bloomfog
        .as_array()
        .into_iter()
        .flatten()
        .map(|b| {
            let x = &b["fields"];
            FogLight {
                enabled: true,
                color: v4(&x["color"]),
                matrix: m16(&x["CachedTransform"]["m"]).unwrap_or([0.0; 16]),
                length: f(&x["Length"]),
                center: f(&x["Center"]),
                start_width: f(&x["StartWidth"]),
                end_width: f(&x["EndWidth"]),
                start_alpha: f(&x["StartAlpha"]),
                end_alpha: f(&x["EndAlpha"]),
                length_alpha_fog_mul: f(&x["MultiplyLengthByAlphaBloomFogMultiplier"]),
                length_alpha_mul: f(&x["MultiplyLengthByAlphaMultiplier"]),
                width_mul: f(&x["LightWidthMultiplier"]),
                intensity_mul: f(&x["IntensityMultiplier"]),
                boost_to_white: f(&x["BoostToWhite"]),
                limit_alpha: x["LimitAlpha"].as_bool().unwrap_or(false),
                min_alpha: f(&x["MinAlpha"]),
                max_alpha: f(&x["MaxAlpha"]),
                disable_on_zero: x["DisableRenderersOnZeroAlpha"].as_bool().unwrap_or(false),
                // Additive only for the Add line materials (BlendOp 0); the default material uses Max.
                blend_max: !(b["material"].is_string() && b["blendOp"].as_f64() == Some(0.0)),
                priority: b["priority"].as_i64().unwrap_or(0),
            }
        })
        .collect()
}

/// The AfterBlur background gradient (components.json).
pub fn sky_gradient(components: &Value) -> Option<SkyGradient> {
    let c = components.as_array()?.iter().find(|c| {
        c["type"] == "BloomPrePassBackgroundColorsGradient" && c["active"].as_bool().unwrap_or(true)
    })?;
    let x = &c["fields"];
    if x["ExecutionTimeType"].as_str().is_some_and(|t| t.contains("Before")) {
        return None;
    }
    let elements = x["Elements"]
        .as_array()?
        .iter()
        .map(|e| (v4(&e["Color"]), f(&e["StartT"]), e["Exp"].as_f64().unwrap_or(1.0) as f32))
        .collect();
    Some(SkyGradient { elements, tint: v4(&x["TintColor"]) })
}

pub fn run(dir: &Path, pose: Option<&str>) -> i32 {
    match render_all(dir, pose) {
        Ok(n) => {
            println!("rendered {n} snapshots in {}", dir.display());
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

fn render_all(dir: &Path, pose: Option<&str>) -> Result<usize, String> {
    let env = EnvFile::load_path(&dir.join("environment.json")).ok_or("no environment.json")?;
    let statics = fog_lights(&read_json(&dir.join("bloomfog.json")));
    let sky = sky_gradient(&read_json(&dir.join("components.json")));
    let snapshots = read_lines(&dir.join("snapshots.jsonl"));
    let lights = read_lines(&dir.join("lights.jsonl"));
    let view = View::from_pose(pose.unwrap_or(DEFAULT_POSE)).ok_or("bad pose")?;
    // BSLT_FROM_LIGHTS=1: derive shader inputs from ChroMapper's light colors with our component logic.
    let from_lights = std::env::var_os("BSLT_FROM_LIGHTS").is_some();
    let env_lights = crate::envscene::EnvLights::load(&env, dir);
    let mut gpu = Gpu3d::headless()?;
    gpu.set_environment(&env, Some(&dir.join("textures")));
    let mut n = 0;
    for s in &snapshots {
        let beat = s["beat"].as_f64().unwrap_or(f64::NAN);
        let name = format!("{}", (beat * 1000.0).round() / 1000.0);
        if !dir.join(format!("{name}.png")).exists() {
            continue;
        }
        let g = &s["globals"];
        let mut state = SceneState { sky: sky.clone(), mirror_plane: env_lights.mirror_plane(), ..Default::default() };
        state.time = s["seconds"].as_f64().unwrap_or(0.0) as f32;
        state.base_boost = [g["_BaseColorBoost"].as_f64().unwrap_or(1.0) as f32, g["_BaseColorBoostThreshold"].as_f64().unwrap_or(0.0) as f32];
        // Color arrays by ColorArrayLightsController instance ID → that controller's material.
        for a in s["colorArrays"].as_array().into_iter().flatten() {
            let (Some(id), Some(colors)) = (a[0].as_i64(), a[1].as_array()) else { continue };
            if let Some(mi) = env_lights.color_array_material(id) {
                state.color_arrays.insert(mi, colors.iter().map(v4).collect());
            }
        }
        let arr = |k: &str| -> [[f32; 4]; 5] { std::array::from_fn(|i| v4(&g[k][i])) };
        state.lights = DirLights {
            pos: arr("_DirectionalLightPositions"),
            dir: arr("_DirectionalLightDirections"),
            color: arr("_DirectionalLightColors"),
            radius: std::array::from_fn(|i| g["_DirectionalLightRadii"].get(i).map_or(100.0, f)),
        };
        // BloomFogObjects: [enabled, color, matrix, length, startAlpha, endAlpha, startWidth, endWidth, mlbabf, mlbam].
        state.fog_lights = statics.clone();
        for (l, d) in state.fog_lights.iter_mut().zip(s["bloomFog"].as_array().into_iter().flatten()) {
            let Some(d) = d.as_array() else {
                l.enabled = false;
                continue;
            };
            l.enabled = d[0].as_i64() == Some(1);
            l.color = v4(&d[1]);
            if let Some(m) = m16(&d[2]) {
                l.matrix = m;
            }
            l.length = f(&d[3]);
            l.start_alpha = f(&d[4]);
            l.end_alpha = f(&d[5]);
            l.start_width = f(&d[6]);
            l.end_width = f(&d[7]);
            l.length_alpha_fog_mul = f(&d[8]);
            l.length_alpha_mul = f(&d[9]);
        }
        for b in s["blocks"].as_array().into_iter().flatten() {
            let (Some(i), Some(props)) = (b[0].as_u64(), b[2].as_object()) else { continue };
            // Renderers switched off at export (lights at zero) can be on in the snapshot.
            if b[1].as_i64() == Some(1) {
                state.shown.insert(i as usize);
            }
            let get = |k: &str| props.get(k).filter(|v| v.is_array()).map(v4);
            let num = |k: &str| props.get(k).and_then(Value::as_f64).map(|v| v as f32);
            state.blocks.insert(
                i as usize,
                Block {
                    color: get("_Color"),
                    alpha_width: get("_AlphaWidth"),
                    size_params: get("_SizeParams"),
                    emission_tex_color: get("_EmissionTexColor"),
                    emission_color: get("_EmissionColor"),
                    start_time: num("_StartTime"),
                    time_offset: num("_TimeOffset"),
                    colors_offset: num("_ColorsArrayOffset"),
                    renderer_color: None,
                    enabled: b[1].as_i64() != Some(0),
                },
            );
        }
        for sp in s["sprites"].as_array().into_iter().flatten() {
            let Some(i) = sp[0].as_u64() else { continue };
            let b = state.blocks.entry(i as usize).or_default();
            b.renderer_color = Some(v4(&sp[2]));
            b.enabled = b.enabled && sp[1].as_i64() != Some(0);
        }
        for (si, ps) in s["particles"].as_array().into_iter().flatten().enumerate() {
            let list = ps.as_array().into_iter().flatten().filter_map(crate::environment::Particle::from_json).collect();
            state.particles.insert(si, list);
        }
        if let Some(l) = lights.iter().find(|l| (l["beat"].as_f64().unwrap_or(f64::NAN) - beat).abs() < 1e-3) {
            for m in l["moved"].as_array().into_iter().flatten() {
                if let (Some(i), Some(mat)) = (m[0].as_u64(), m16(&m[1])) {
                    state.moved.insert(i as usize, mat);
                }
            }
        }
        if from_lights {
            let rec = lights.iter().find(|l| (l["beat"].as_f64().unwrap_or(f64::NAN) - beat).abs() < 1e-3);
            let colors: std::collections::HashMap<(i64, Vec<i64>), [f32; 4]> = rec
                .and_then(|r| r["lights"].as_array())
                .into_iter()
                .flatten()
                .filter_map(|l| Some((crate::envscene::key_of(&Value::Array(vec![l[0].clone(), l[1].clone()]))?, v4(&l[2]))))
                .collect();
            let computed = env_lights.state(&|k| colors.get(k).copied().unwrap_or([0.0; 4]));
            if std::env::var_os("BSLT_DEBUG").is_some() {
                let mut n = 0;
                for (i, b) in &computed.blocks {
                    let rec = state.blocks.get(i).and_then(|r| r.color);
                    if let (Some(r), Some(c)) = (rec, b.color) {
                        if (0..4).any(|k| (r[k] - c[k]).abs() > 0.02) && n < 6 {
                            eprintln!("  block {i} {} rec {r:?} ours {c:?}", env.objects[*i].path);
                            n += 1;
                        }
                    } else if rec.is_none() && n < 6 {
                        eprintln!("  block {i} {} no recorded color, ours {:?}", env.objects[*i].path, b.color);
                        n += 1;
                    }
                }
                let mut m = 0;
                for (l, c) in state.fog_lights.iter().zip(&computed.fog_lights) {
                    if (0..4).any(|k| (l.color[k] - c.color[k]).abs() > 0.02) && m < 6 {
                        eprintln!("  fog rec {:?} ours {:?}", l.color, c.color);
                        m += 1;
                    }
                }
            }
            for (i, b) in computed.blocks {
                let e = state.blocks.entry(i).or_default();
                e.color = b.color.or(e.color);
                e.alpha_width = b.alpha_width.or(e.alpha_width);
                e.size_params = b.size_params.or(e.size_params);
                e.emission_color = b.emission_color.or(e.emission_color);
                e.emission_tex_color = b.emission_tex_color.or(e.emission_tex_color);
                e.colors_offset = b.colors_offset.or(e.colors_offset);
                e.renderer_color = b.renderer_color.or(e.renderer_color);
                e.enabled = b.enabled;
            }
            state.shown.extend(computed.shown);
            state.color_arrays.extend(computed.color_arrays);
            if std::env::var_os("BSLT_DEBUG").is_some() {
                eprintln!("beat {beat}\n  recorded {:?}\n  computed {:?}", state.lights.color, computed.lights.color);
                eprintln!("  rec pos {:?}\n  our pos {:?}", state.lights.pos, computed.lights.pos);
            }
            state.lights = computed.lights;
            for (l, c) in state.fog_lights.iter_mut().zip(computed.fog_lights) {
                l.color = c.color;
            }
        }
        let base = env.fog.unwrap_or(FogParams {
            offset: 0.0,
            height: 25.0,
            start_y: -50.0,
            attenuation: 0.00025,
            auto_exposure_limit: 1000.0,
            legacy_auto_exposure: false,
        });
        let fog = FogParams {
            offset: g["_CustomFogOffset"].as_f64().map_or(base.offset, |v| v as f32),
            attenuation: g["_CustomFogAttenuation"].as_f64().map_or(base.attenuation, |v| v as f32),
            start_y: g["_CustomFogHeightFogStartY"].as_f64().map_or(base.start_y, |v| v as f32),
            height: g["_CustomFogHeightFogHeight"].as_f64().map_or(base.height, |v| v as f32),
            ..base
        };
        let (w, h) = (1280, 420);
        gpu.render(&Frame { view, width: w, height: h, env: &env, state: &state, fog, post_bloom: true });
        let rgb = gpu.read_rgb();
        crate::gpu3d::write_png(&dir.join(format!("ours-{name}.png")), w, h, &rgb)?;
        if std::env::var_os("BSLT_DUMP_FOG").is_some() {
            crate::gpu3d::write_png(&dir.join(format!("ours-fog-{name}.png")), 512, 512, &gpu.read_fog_rgb())?;
            crate::gpu3d::write_png(&dir.join(format!("ours-refl-{name}.png")), 1024, 1024, &gpu.read_reflection_rgb())?;
        }
        n += 1;
    }
    Ok(n)
}
