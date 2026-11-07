//! Approximates what a light row looks like at a given beat, for the preview strips.

use crate::map::{Key, Kind, Row};
use std::f32::consts::PI;

pub const EASINGS: &[&str] = &[
    "easeLinear",
    "easeStep",
    "easeInQuad",
    "easeOutQuad",
    "easeInOutQuad",
    "easeInCubic",
    "easeOutCubic",
    "easeInOutCubic",
    "easeInQuart",
    "easeOutQuart",
    "easeInOutQuart",
    "easeInQuint",
    "easeOutQuint",
    "easeInOutQuint",
    "easeInSine",
    "easeOutSine",
    "easeInOutSine",
    "easeInExpo",
    "easeOutExpo",
    "easeInOutExpo",
    "easeInCirc",
    "easeOutCirc",
    "easeInOutCirc",
    "easeInBack",
    "easeOutBack",
    "easeInOutBack",
    "easeInElastic",
    "easeOutElastic",
    "easeInOutElastic",
    "easeInBounce",
    "easeOutBounce",
    "easeInOutBounce",
];

fn out_bounce(t: f32) -> f32 {
    let (n, d) = (7.5625, 2.75);
    if t < 1.0 / d {
        n * t * t
    } else if t < 2.0 / d {
        let t = t - 1.5 / d;
        n * t * t + 0.75
    } else if t < 2.5 / d {
        let t = t - 2.25 / d;
        n * t * t + 0.9375
    } else {
        let t = t - 2.625 / d;
        n * t * t + 0.984375
    }
}

pub fn ease(name: Option<&str>, t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    let in_out = |f: fn(f32) -> f32| {
        if t < 0.5 {
            f(2.0 * t) / 2.0
        } else {
            1.0 - f(2.0 - 2.0 * t) / 2.0
        }
    };
    let pow_in = |p: i32| t.powi(p);
    let pow_out = |p: i32| 1.0 - (1.0 - t).powi(p);
    const C1: f32 = 1.70158;
    match name.unwrap_or("easeLinear") {
        "easeStep" => {
            if t < 1.0 {
                0.0
            } else {
                1.0
            }
        }
        "easeInQuad" => pow_in(2),
        "easeOutQuad" => pow_out(2),
        "easeInOutQuad" => in_out(|x| x * x),
        "easeInCubic" => pow_in(3),
        "easeOutCubic" => pow_out(3),
        "easeInOutCubic" => in_out(|x| x.powi(3)),
        "easeInQuart" => pow_in(4),
        "easeOutQuart" => pow_out(4),
        "easeInOutQuart" => in_out(|x| x.powi(4)),
        "easeInQuint" => pow_in(5),
        "easeOutQuint" => pow_out(5),
        "easeInOutQuint" => in_out(|x| x.powi(5)),
        "easeInSine" => 1.0 - (t * PI / 2.0).cos(),
        "easeOutSine" => (t * PI / 2.0).sin(),
        "easeInOutSine" => -((PI * t).cos() - 1.0) / 2.0,
        "easeInExpo" => in_expo(t),
        "easeOutExpo" => 1.0 - in_expo(1.0 - t),
        "easeInOutExpo" => in_out(in_expo),
        "easeInCirc" => in_circ(t),
        "easeOutCirc" => 1.0 - in_circ(1.0 - t),
        "easeInOutCirc" => in_out(in_circ),
        "easeInBack" => in_back(t),
        "easeOutBack" => 1.0 - in_back(1.0 - t),
        "easeInOutBack" => in_out(|x| x * x * ((C1 * 1.525 + 1.0) * x - C1 * 1.525)),
        "easeInElastic" => in_elastic(t),
        "easeOutElastic" => 1.0 - in_elastic(1.0 - t),
        "easeInOutElastic" => in_out(in_elastic),
        "easeInBounce" => 1.0 - out_bounce(1.0 - t),
        "easeOutBounce" => out_bounce(t),
        "easeInOutBounce" => in_out(|x| 1.0 - out_bounce(1.0 - x)),
        _ => t,
    }
}

fn in_expo(t: f32) -> f32 {
    if t <= 0.0 { 0.0 } else { 2f32.powf(10.0 * t - 10.0) }
}

fn in_circ(t: f32) -> f32 {
    1.0 - (1.0 - t * t).max(0.0).sqrt()
}

fn in_back(t: f32) -> f32 {
    const C1: f32 = 1.70158;
    (C1 + 1.0) * t * t * t - C1 * t * t
}

fn in_elastic(t: f32) -> f32 {
    if t <= 0.0 || t >= 1.0 {
        return t;
    }
    -(2f32.powf(10.0 * t - 10.0)) * ((t * 10.0 - 10.75) * (2.0 * PI / 3.0)).sin()
}

fn rgb_to_hsv([r, g, b, a]: [f32; 4]) -> [f32; 4] {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let h = if d == 0.0 {
        0.0
    } else if max == r {
        ((g - b) / d).rem_euclid(6.0) / 6.0
    } else if max == g {
        ((b - r) / d + 2.0) / 6.0
    } else {
        ((r - g) / d + 4.0) / 6.0
    };
    let s = if max == 0.0 { 0.0 } else { d / max };
    [h, s, max, a]
}

fn hsv_to_rgb([h, s, v, a]: [f32; 4]) -> [f32; 4] {
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
    [r + m, g + m, b + m, a]
}

/// Color times brightness, with alpha folded in. Returns linear-ish RGB in 0..=1.
fn lit(color: [f32; 4], brightness: f32) -> [f32; 3] {
    let k = (brightness * color[3]).clamp(0.0, 1.5);
    [color[0] * k, color[1] * k, color[2] * k].map(|c| c.clamp(0.0, 1.0))
}

fn lerp4(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t)
}

fn mix(from: &Key, to: &Key, t: f32) -> [f32; 3] {
    let t = ease(to.easing.as_deref(), t);
    let (a, b) = (from.display_color(), to.display_color());
    let color = if to.lerp_hsv {
        let (mut ha, hb) = (rgb_to_hsv(a), rgb_to_hsv(b));
        // Take the short way around the hue circle.
        if hb[0] - ha[0] > 0.5 {
            ha[0] += 1.0;
        } else if ha[0] - hb[0] > 0.5 {
            ha[0] -= 1.0;
        }
        hsv_to_rgb(lerp4(ha, hb, t))
    } else {
        lerp4(a, b, t)
    };
    let brightness = from.brightness + (to.brightness - from.brightness) * t;
    lit(color, brightness)
}

/// Light color of a row at `beat`. `fade_beats` is how long a Fade event takes to go dark.
pub fn row_color(row: &Row, beat: f64, fade_beats: f64) -> [f32; 3] {
    let keys = &row.keys;
    let idx = keys.partition_point(|k| k.beat <= beat);
    if idx == 0 {
        return [0.0; 3];
    }
    let cur = &keys[idx - 1];
    if let Some(next) = keys.get(idx) {
        if next.kind == Kind::Transition && cur.kind != Kind::Off && next.beat > cur.beat {
            let t = ((beat - cur.beat) / (next.beat - cur.beat)) as f32;
            return mix(cur, next, t);
        }
    }
    let since = beat - cur.beat;
    match cur.kind {
        Kind::Off => [0.0; 3],
        Kind::On | Kind::Transition => lit(cur.display_color(), cur.brightness),
        Kind::Flash => {
            // Bright pop that settles to normal brightness.
            let boost = 1.0 + 0.5 * (1.0 - (since / 0.5).min(1.0)) as f32;
            lit(cur.display_color(), cur.brightness * boost)
        }
        Kind::Fade => {
            let k = 1.0 - (since / fade_beats).min(1.0) as f32;
            lit(cur.display_color(), cur.brightness * k)
        }
    }
}
