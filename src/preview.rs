//! Which rows light which (lane, light ID), and the resulting light colors at a beat.

use crate::eval;
use crate::map::Row;
use serde_json::Value;

/// Lights of one lane: each light ID with the rows that can set it.
pub struct LaneLights {
    pub lane: i64,
    /// (light ID, rows that target it, including "all lights" rows). Empty when the map
    /// never addresses single IDs on this lane.
    pub ids: Vec<(i64, Vec<usize>)>,
    pub all_rows: Vec<usize>,
}

fn ids_of(v: &Value) -> Vec<i64> {
    match v {
        Value::Array(a) => a.iter().filter_map(Value::as_i64).collect(),
        Value::Number(n) => n.as_i64().into_iter().collect(),
        _ => Vec::new(),
    }
}

pub fn build(rows: &[Row]) -> Vec<LaneLights> {
    crate::map::LIGHT_LANES
        .iter()
        .map(|(lane, _)| {
            let all_rows: Vec<usize> = rows
                .iter()
                .enumerate()
                .filter(|(_, r)| r.lane == *lane && r.light_ids.is_none())
                .map(|(i, _)| i)
                .collect();
            let mut ids: Vec<(i64, Vec<usize>)> = Vec::new();
            for (i, r) in rows.iter().enumerate().filter(|(_, r)| r.lane == *lane) {
                for id in r.light_ids.as_ref().map(ids_of).unwrap_or_default() {
                    match ids.iter_mut().find(|(x, _)| *x == id) {
                        Some((_, v)) => v.push(i),
                        None => ids.push((id, vec![i])),
                    }
                }
            }
            ids.sort_by_key(|(id, _)| *id);
            for (_, v) in &mut ids {
                v.extend(&all_rows);
            }
            LaneLights {
                lane: *lane,
                ids,
                all_rows,
            }
        })
        .collect()
}

/// Color of a light: the row whose most recent event is latest wins, like events overriding
/// each other in game.
fn light_color(rows: &[Row], candidates: &[usize], beat: f64, fade: f64) -> [f32; 3] {
    let mut best: Option<(f64, usize)> = None;
    for &ri in candidates {
        let keys = &rows[ri].keys;
        let idx = keys.partition_point(|k| k.beat <= beat);
        if idx == 0 {
            continue;
        }
        let start = keys[idx - 1].beat;
        if best.is_none_or(|(b, _)| start >= b) {
            best = Some((start, ri));
        }
    }
    best.map_or([0.0; 3], |(_, ri)| eval::row_color(&rows[ri], beat, fade))
}

/// Colors for each light of a lane, or `default_count` copies of the lane color when the map
/// doesn't use light IDs there.
pub fn lane_colors(rows: &[Row], lane: &LaneLights, beat: f64, fade: f64, default_count: usize) -> Vec<[f32; 3]> {
    if lane.ids.is_empty() {
        vec![light_color(rows, &lane.all_rows, beat, fade); default_count]
    } else {
        lane.ids
            .iter()
            .map(|(_, cands)| light_color(rows, cands, beat, fade))
            .collect()
    }
}

/// Color of one light: `lane` -1 means "whichever lane uses this ID" (v2 geometry lights).
pub fn light_rgb(rows: &[Row], lanes: &[LaneLights], lane: i64, id: Option<i64>, beat: f64, fade: f64) -> [f32; 3] {
    let lane = if lane == -1 {
        let Some(l) = lanes.iter().find(|l| l.ids.iter().any(|(x, _)| Some(*x) == id)) else {
            return [0.0; 3];
        };
        l
    } else {
        let Some(l) = lanes.iter().find(|l| l.lane == lane) else {
            return [0.0; 3];
        };
        l
    };
    match id.and_then(|id| lane.ids.iter().find(|(x, _)| *x == id)) {
        Some((_, cands)) => light_color(rows, cands, beat, fade),
        None => light_color(rows, &lane.all_rows, beat, fade),
    }
}
