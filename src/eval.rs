//! Heck / Chroma easing functions by name.

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
