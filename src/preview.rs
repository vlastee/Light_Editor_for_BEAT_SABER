//! Which light IDs each lane uses, for laying out the generic stand-in stage.

use crate::map::Row;
use serde_json::Value;

/// Lights of one lane: each light ID with the rows that can set it.
pub struct LaneLights {
    pub lane: i64,
    /// (light ID, rows that target it). Empty when the map never addresses single IDs here.
    pub ids: Vec<(i64, Vec<usize>)>,
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
            LaneLights { lane: *lane, ids }
        })
        .collect()
}
