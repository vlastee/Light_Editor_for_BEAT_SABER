//! Beat Saber v4 lightshow files. Their events point into shared data tables; on load they're expanded
//! to the v3 shape the rest of the editor reads, and packed back into tables on save.
//!
//! Field meanings follow ChroMapper's `Beatmap/V4` readers.

use serde_json::{Map, Value, json};
use std::collections::HashMap;

pub fn is_lightshow(d: &Value) -> bool {
    d["version"].as_str().is_some_and(|v| v.starts_with('4')) && (d.get("eventBoxGroups").is_some() || d.get("basicEvents").is_some() || d.get("colorBoostEvents").is_some())
}

fn table<'a>(d: &'a Value, name: &str) -> &'a [Value] {
    d[name].as_array().map_or(&[], Vec::as_slice)
}

fn at<'a>(list: &'a [Value], i: &Value) -> &'a Value {
    static NULL: Value = Value::Null;
    i.as_u64().and_then(|i| list.get(i as usize)).unwrap_or(&NULL)
}

fn with_custom(mut v: Value, from: &Value) -> Value {
    if let Some(c) = from.get("customData") {
        v["customData"] = c.clone();
    }
    v
}

/// The lightshow in v3's shape. VFX groups keep their values inline.
pub fn expand(d: &Value) -> Value {
    let (basic, basic_data) = (table(d, "basicEvents"), table(d, "basicEventsData"));
    let events: Vec<Value> = basic
        .iter()
        .map(|e| {
            let x = at(basic_data, &e["i"]);
            with_custom(json!({ "b": e["b"], "et": x["t"], "i": x["i"], "f": x["f"] }), e)
        })
        .collect();
    let boost_data = table(d, "colorBoostEventsData");
    let boosts: Vec<Value> = table(d, "colorBoostEvents")
        .iter()
        .map(|e| json!({ "b": e.get("b").cloned().unwrap_or(json!(0)), "o": at(boost_data, &e["i"])["b"].as_i64() == Some(1) }))
        .collect();
    let filters = table(d, "indexFilters");
    let (color_boxes, color_events) = (table(d, "lightColorEventBoxes"), table(d, "lightColorEvents"));
    let (rot_boxes, rot_events) = (table(d, "lightRotationEventBoxes"), table(d, "lightRotationEvents"));
    let (move_boxes, move_events) = (table(d, "lightTranslationEventBoxes"), table(d, "lightTranslationEvents"));
    let (fx_boxes, fx_events) = (table(d, "fxEventBoxes"), table(d, "floatFxEvents"));
    let mut lists: [Vec<Value>; 4] = Default::default();
    for g in table(d, "eventBoxGroups") {
        let kind = g["t"].as_i64().unwrap_or(0);
        if !(1..=4).contains(&kind) {
            continue;
        }
        let boxes: Vec<Value> = g["e"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|b| {
                let filter = at(filters, &b["f"]).clone();
                let nodes = b["l"].as_array().map_or(&[][..], Vec::as_slice);
                match kind {
                    1 => {
                        let c = at(color_boxes, &b["e"]);
                        let list: Vec<Value> = nodes
                            .iter()
                            .map(|n| {
                                let x = at(color_events, &n["i"]);
                                let transition = if x["p"].as_i64() == Some(1) {
                                    2
                                } else if x["e"].as_i64() == Some(-1) {
                                    0
                                } else {
                                    1
                                };
                                with_custom(json!({ "b": n["b"], "i": transition, "c": x["c"], "s": x["b"], "f": x["f"], "ease": x["e"], "sb": x["sb"], "sf": x["sf"] }), n)
                            })
                            .collect();
                        json!({ "f": filter, "w": c["w"], "d": c["d"], "r": c["s"], "t": c["t"], "b": c["b"], "i": c["e"], "e": list })
                    }
                    2 => {
                        let c = at(rot_boxes, &b["e"]);
                        let list: Vec<Value> = nodes
                            .iter()
                            .map(|n| {
                                let x = at(rot_events, &n["i"]);
                                json!({ "b": n["b"], "p": x["p"], "e": x["e"], "r": x["r"], "o": x["d"], "l": x["l"] })
                            })
                            .collect();
                        json!({ "f": filter, "w": c["w"], "d": c["d"], "s": c["s"], "t": c["t"], "b": c["b"], "i": c["e"], "a": c["a"], "r": c["f"], "l": list })
                    }
                    3 => {
                        let c = at(move_boxes, &b["e"]);
                        let list: Vec<Value> = nodes
                            .iter()
                            .map(|n| {
                                let x = at(move_events, &n["i"]);
                                json!({ "b": n["b"], "p": x["p"], "e": x["e"], "t": x["t"] })
                            })
                            .collect();
                        json!({ "f": filter, "w": c["w"], "d": c["d"], "s": c["s"], "t": c["t"], "b": c["b"], "i": c["e"], "a": c["a"], "r": c["f"], "l": list })
                    }
                    _ => {
                        let c = at(fx_boxes, &b["e"]);
                        let list: Vec<Value> = nodes
                            .iter()
                            .map(|n| {
                                let x = at(fx_events, &n["i"]);
                                json!({ "b": n["b"], "p": x["p"], "e": x["e"], "v": x["v"] })
                            })
                            .collect();
                        json!({ "f": filter, "w": c["w"], "d": c["d"], "s": c["s"], "t": c["t"], "b": c["b"], "i": c["e"], "l": list })
                    }
                }
            })
            .collect();
        lists[(kind - 1) as usize].push(with_custom(json!({ "b": g["b"], "g": g["g"], "e": boxes }), g));
    }
    let [color, rotation, translation, vfx] = lists;
    let (mut events, mut color, mut rotation, mut translation, mut vfx) = (events, color, rotation, translation, vfx);
    for list in [&mut events, &mut color, &mut rotation, &mut translation, &mut vfx] {
        list.iter_mut().for_each(fill_defaults);
    }
    json!({
        "version": d["version"],
        "basicBeatmapEvents": events,
        "colorBoostBeatmapEvents": boosts,
        "lightColorEventBoxGroups": color,
        "lightRotationEventBoxGroups": rotation,
        "lightTranslationEventBoxGroups": translation,
        "vfxEventBoxGroups": vfx,
        "customData": d.get("customData").cloned().unwrap_or_else(|| json!({})),
    })
}

/// v4 files can leave out fields that are 0.
fn fill_defaults(v: &mut Value) {
    match v {
        Value::Null => *v = json!(0),
        Value::Array(a) => a.iter_mut().for_each(fill_defaults),
        Value::Object(m) => m.iter_mut().filter(|(k, _)| k.as_str() != "customData").for_each(|(_, v)| fill_defaults(v)),
        _ => {}
    }
}

/// A shared data table: identical entries are stored once.
struct Table {
    list: Vec<Value>,
    index: HashMap<String, usize>,
}

impl Table {
    /// Starts from the file's own table, so unchanged events keep their indices.
    fn new(d: &Value, name: &str) -> Table {
        let mut t = Table { list: Vec::new(), index: HashMap::new() };
        for v in table(d, name) {
            t.index.entry(key_of(v)).or_insert(t.list.len());
            t.list.push(v.clone());
        }
        t
    }

    fn add(&mut self, v: Value) -> usize {
        *self.index.entry(key_of(&v)).or_insert_with(|| {
            self.list.push(v);
            self.list.len() - 1
        })
    }
}

/// Numbers compare by value, so 1 and 1.0 are the same table entry.
fn key_of(v: &Value) -> String {
    match v {
        Value::Number(n) => n.as_f64().map_or_else(|| n.to_string(), |f| f.to_string()),
        Value::Array(a) => format!("[{}]", a.iter().map(key_of).collect::<Vec<_>>().join(",")),
        Value::Object(m) => {
            let mut parts: Vec<String> = m.iter().map(|(k, v)| format!("{k}:{}", key_of(v))).collect();
            parts.sort();
            format!("{{{}}}", parts.join(","))
        }
        other => other.to_string(),
    }
}

fn node(n: &Value, i: usize) -> Value {
    with_custom(json!({ "b": n["b"], "i": i }), n)
}

/// `original` with its event tables replaced from the expanded lightshow `v`.
pub fn collapse(v: &Value, original: &Value) -> Value {
    let mut out = original.clone();
    let mut basic_data = Table::new(original, "basicEventsData");
    let basic: Vec<Value> = v["basicBeatmapEvents"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            let i = basic_data.add(json!({ "t": e["et"], "i": e["i"], "f": e["f"] }));
            with_custom(json!({ "b": e["b"], "i": i }), e)
        })
        .collect();
    let mut boost_data = Table::new(original, "colorBoostEventsData");
    let boosts: Vec<Value> = v["colorBoostBeatmapEvents"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| json!({ "b": e["b"], "i": boost_data.add(json!({ "b": i64::from(e["o"].as_bool() == Some(true)) })) }))
        .collect();
    let mut filters = Table::new(original, "indexFilters");
    let mut color_boxes = Table::new(original, "lightColorEventBoxes");
    let mut color_events = Table::new(original, "lightColorEvents");
    let mut rot_boxes = Table::new(original, "lightRotationEventBoxes");
    let mut rot_events = Table::new(original, "lightRotationEvents");
    let mut move_boxes = Table::new(original, "lightTranslationEventBoxes");
    let mut move_events = Table::new(original, "lightTranslationEvents");
    let mut fx_boxes = Table::new(original, "fxEventBoxes");
    let mut fx_events = Table::new(original, "floatFxEvents");
    let mut groups = Vec::new();
    for (kind, name) in [(1, "lightColorEventBoxGroups"), (2, "lightRotationEventBoxGroups"), (3, "lightTranslationEventBoxGroups"), (4, "vfxEventBoxGroups")] {
        for g in v[name].as_array().into_iter().flatten() {
            let boxes: Vec<Value> = g["e"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|b| {
                    let f = filters.add(b["f"].clone());
                    let (e, nodes): (usize, Vec<Value>) = match kind {
                        1 => {
                            let e = color_boxes.add(json!({ "w": b["w"], "d": b["d"], "s": b["r"], "t": b["t"], "b": b["b"], "e": b["i"] }));
                            let list = b["e"].as_array().into_iter().flatten().map(|n| {
                                let transition = n["i"].as_i64().unwrap_or(0);
                                let ease = if n["ease"].is_null() { json!(if transition == 1 { 0 } else { -1 }) } else { n["ease"].clone() };
                                let zero = |x: &Value| if x.is_null() { json!(0) } else { x.clone() };
                                let i = color_events.add(json!({ "e": ease, "p": i64::from(transition == 2), "c": n["c"], "b": n["s"], "f": n["f"], "sb": zero(&n["sb"]), "sf": zero(&n["sf"]) }));
                                node(n, i)
                            });
                            (e, list.collect())
                        }
                        2 => {
                            let e = rot_boxes.add(json!({ "w": b["w"], "d": b["d"], "s": b["s"], "t": b["t"], "b": b["b"], "e": b["i"], "a": b["a"], "f": b["r"] }));
                            let list = b["l"].as_array().into_iter().flatten().map(|n| {
                                let i = rot_events.add(json!({ "p": n["p"], "e": n["e"], "r": n["r"], "d": n["o"], "l": n["l"] }));
                                node(n, i)
                            });
                            (e, list.collect())
                        }
                        3 => {
                            let e = move_boxes.add(json!({ "w": b["w"], "d": b["d"], "s": b["s"], "t": b["t"], "b": b["b"], "e": b["i"], "a": b["a"], "f": b["r"] }));
                            let list = b["l"].as_array().into_iter().flatten().map(|n| {
                                let i = move_events.add(json!({ "p": n["p"], "e": n["e"], "t": n["t"] }));
                                node(n, i)
                            });
                            (e, list.collect())
                        }
                        _ => {
                            let e = fx_boxes.add(json!({ "w": b["w"], "d": b["d"], "s": b["s"], "t": b["t"], "b": b["b"], "e": b["i"] }));
                            let list = b["l"].as_array().into_iter().flatten().map(|n| {
                                let i = fx_events.add(json!({ "p": n["p"], "e": n["e"], "v": n["v"] }));
                                node(n, i)
                            });
                            (e, list.collect())
                        }
                    };
                    json!({ "f": f, "e": e, "l": nodes })
                })
                .collect();
            groups.push(with_custom(json!({ "b": g["b"], "g": g["g"], "t": kind, "e": boxes }), g));
        }
    }
    groups.sort_by(|a, b| a["b"].as_f64().unwrap_or(0.0).total_cmp(&b["b"].as_f64().unwrap_or(0.0)));
    let tables: [(&str, Vec<Value>); 14] = [
        ("basicEvents", basic),
        ("basicEventsData", basic_data.list),
        ("colorBoostEvents", boosts),
        ("colorBoostEventsData", boost_data.list),
        ("eventBoxGroups", groups),
        ("indexFilters", filters.list),
        ("lightColorEventBoxes", color_boxes.list),
        ("lightColorEvents", color_events.list),
        ("lightRotationEventBoxes", rot_boxes.list),
        ("lightRotationEvents", rot_events.list),
        ("lightTranslationEventBoxes", move_boxes.list),
        ("lightTranslationEvents", move_events.list),
        ("fxEventBoxes", fx_boxes.list),
        ("floatFxEvents", fx_events.list),
    ];
    let map: &mut Map<String, Value> = out.as_object_mut().expect("lightshow is an object");
    for (name, list) in tables {
        if !list.is_empty() || map.contains_key(name) {
            map.insert(name.to_owned(), Value::Array(list));
        }
    }
    if let Some(c) = v.get("customData").filter(|c| c.as_object().is_some_and(|m| !m.is_empty())) {
        map.insert("customData".to_owned(), c.clone());
    }
    out
}

/// Canonical text of every event list in an expanded lightshow, for comparing two of them.
pub fn fingerprint(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = ["basicBeatmapEvents", "colorBoostBeatmapEvents", "lightColorEventBoxGroups", "lightRotationEventBoxGroups", "lightTranslationEventBoxGroups", "vfxEventBoxGroups"]
        .iter()
        .flat_map(|n| v[*n].as_array().into_iter().flatten().map(key_of))
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Value {
        json!({
            "version": "4.0.0",
            "basicEvents": [{ "b": 1, "i": 0 }, { "b": 2, "i": 1 }, { "b": 3, "i": 0 }],
            "basicEventsData": [{ "t": 2, "i": 1, "f": 1 }, { "t": 12, "i": 4, "f": 0 }],
            "colorBoostEvents": [{ "b": 0, "i": 1 }],
            "colorBoostEventsData": [{ "b": 0 }, { "b": 1 }],
            "indexFilters": [{ "f": 1, "p": 1, "t": 0, "r": 0, "c": 0, "n": 0, "s": 0, "l": 0, "d": 0 }],
            "lightColorEventBoxes": [{ "w": 0.5, "d": 1, "s": 0.2, "t": 2, "b": 1, "e": 3 }],
            "lightColorEvents": [{ "e": -1, "p": 0, "c": 1, "b": 0.5, "f": 2, "sb": 0, "sf": 0 }, { "e": 4, "p": 0, "c": 2, "b": 1, "f": 0, "sb": 0.5, "sf": 1 }, { "e": 0, "p": 1, "c": 0, "b": 1, "f": 0, "sb": 0, "sf": 0 }],
            "lightRotationEventBoxes": [{ "w": 0, "d": 1, "s": 10, "t": 1, "b": 0, "e": 0, "a": 1, "f": 1 }],
            "lightRotationEvents": [{ "p": 0, "e": 2, "r": 45, "d": 1, "l": 2 }],
            "lightTranslationEventBoxes": [{ "w": 0, "d": 1, "s": 0, "t": 1, "b": 0, "e": 0, "a": 2, "f": 0 }],
            "lightTranslationEvents": [{ "p": 0, "e": -1, "t": 0.5 }],
            "fxEventBoxes": [{ "w": 0, "d": 1, "s": 0, "t": 1, "b": 0, "e": 0 }],
            "floatFxEvents": [{ "p": 0, "e": -1, "v": 1 }],
            "eventBoxGroups": [
                { "b": 4, "g": 1, "t": 1, "e": [{ "f": 0, "e": 0, "l": [{ "b": 0, "i": 0 }, { "b": 1, "i": 1 }, { "b": 2, "i": 2 }] }] },
                { "b": 5, "g": 2, "t": 2, "e": [{ "f": 0, "e": 0, "l": [{ "b": 0, "i": 0 }] }] },
                { "b": 6, "g": 3, "t": 3, "e": [{ "f": 0, "e": 0, "l": [{ "b": 0, "i": 0 }] }] },
                { "b": 7, "g": 4, "t": 4, "e": [{ "f": 0, "e": 0, "l": [{ "b": 0, "i": 0 }] }] }
            ],
            "useNormalEventsAsCompatibleEvents": true
        })
    }

    #[test]
    fn expands_tables_to_v3_shape() {
        let v = expand(&sample());
        assert_eq!(v["basicBeatmapEvents"][1], json!({ "b": 2, "et": 12, "i": 4, "f": 0 }));
        assert_eq!(v["colorBoostBeatmapEvents"][0], json!({ "b": 0, "o": true }));
        let c = &v["lightColorEventBoxGroups"][0]["e"][0];
        assert_eq!((c["w"].as_f64(), c["r"].as_f64(), c["i"].as_i64()), (Some(0.5), Some(0.2), Some(3)));
        assert_eq!(c["e"][0]["i"], 0);
        assert_eq!(c["e"][1]["i"], 1);
        assert_eq!(c["e"][2]["i"], 2);
        assert_eq!(v["lightRotationEventBoxGroups"][0]["e"][0]["l"][0]["o"], 1);
        assert_eq!(v["lightTranslationEventBoxGroups"][0]["e"][0]["a"], 2);
    }

    #[test]
    fn unchanged_lightshow_packs_back_to_the_same_tables() {
        let d = sample();
        let packed = collapse(&expand(&d), &d);
        assert_eq!(key_of(&packed), key_of(&d));
    }

    #[test]
    fn edits_add_table_entries_and_expand_back() {
        let d = sample();
        let mut v = expand(&d);
        v["basicBeatmapEvents"][0]["i"] = json!(7);
        v["lightRotationEventBoxGroups"][0]["e"][0]["l"][0]["r"] = json!(90);
        let packed = collapse(&v, &d);
        assert_eq!(packed["basicEventsData"].as_array().unwrap().len(), 3);
        assert_eq!(fingerprint(&expand(&packed)), fingerprint(&v));
    }

    #[test]
    fn beat_files_are_not_lightshows() {
        assert!(!is_lightshow(&json!({ "version": "4.1.0", "colorNotes": [] })));
        assert!(!is_lightshow(&json!({ "version": "3.3.0", "basicBeatmapEvents": [] })));
    }
}
