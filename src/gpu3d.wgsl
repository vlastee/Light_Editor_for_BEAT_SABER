// ChroMapper BloomFog environment rendering, ported for gpu3d.rs. Formulas follow ChroMapper's shaders
// (BloomfogMesh, Bloom, PostBloom, Lit, ParametricBox*, Mirror, Spectrogram). World units are
// ChroMapper's (not metres), as its fog constants expect.

struct Globals {
    view_proj: mat4x4<f32>,
    cam_pos: vec4<f32>,
    right: vec4<f32>,          // xyz, w = 1 / P_fog.m00
    up: vec4<f32>,             // xyz, w = 1 / P_fog.m11
    fwd: vec4<f32>,
    fog: vec4<f32>,            // offset, attenuation, height-fog startY, height-fog height
    ratio: vec4<f32>,          // fog texture to screen ratio (xy), auto exposure limit, legacy (0/1)
    screen: vec4<f32>,         // w, h, 1/w, 1/h
    light_pos: array<vec4<f32>, 5>,
    light_dir: array<vec4<f32>, 5>,
    light_color: array<vec4<f32>, 5>,
    light_radius: array<vec4<f32>, 2>,
    clip: vec4<f32>,           // mirror plane (normal, offset) for the reflected view
    flags: vec4<f32>,          // x: clip on, y: mirrored winding
    misc: vec4<f32>,           // song seconds, _BaseColorBoost, _BaseColorBoostThreshold
};

struct Object {
    model: mat4x4<f32>,
    normal: mat4x4<f32>,
    info: vec4<u32>,           // kind, flags
    // Per renderer: _Color (linear rgb), _AlphaWidth, _SizeParams, _EmissionTexColor, _EmissionColor,
    // (_StartTime, color array index base + _ColorsArrayOffset, _TimeOffset, 0), SpriteRenderer color.
    b: array<vec4<f32>, 7>,
    p: array<vec4<f32>, 12>,   // material parameters (layout per kind, see gpu3d.rs)
};

@group(0) @binding(0) var<uniform> g: Globals;
@group(0) @binding(1) var<storage, read> objects: array<Object>;
@group(0) @binding(2) var<storage, read> color_arr: array<vec4<f32>>;

const KIND_LIT: u32 = 0u;
const KIND_BOX_OPAQUE: u32 = 1u;
const KIND_BOX_TRANSPARENT: u32 = 2u;
const KIND_FAKE_GLOW: u32 = 3u;
const KIND_MIRROR: u32 = 4u;
const KIND_SPECTROGRAM: u32 = 5u;
const KIND_SLICE: u32 = 7u;

const F_DIFFUSE: u32 = 1u;
const F_SPECULAR: u32 = 2u;
const F_FALLOFF: u32 = 4u;
const F_BOTH_SIDES: u32 = 8u;
const F_FOG: u32 = 16u;
const F_HEIGHT_FOG: u32 = 32u;
const F_VERTEX_COLOR: u32 = 64u;
const F_EMISSION_SIMPLE: u32 = 128u;
const F_ACES_BEFORE: u32 = 256u;
const F_DIRT: u32 = 512u;
const F_MAIN_TEXTURE: u32 = 1024u;
const F_ALPHA_RED: u32 = 2048u;
const F_SQUARE_ALPHA: u32 = 4096u;
const F_FOG_COLOR: u32 = 8192u;
const F_FOG_ALPHA: u32 = 16384u;
const F_FOG_LERP: u32 = 32768u;
const F_WHITEBOOST_MAIN: u32 = 65536u;
const F_USE_FOG_FOR_LIGHTS: u32 = 131072u;
const F_ANGLE_DISAPPEAR: u32 = 262144u;
const F_Y_BILLBOARD: u32 = 524288u;
const F_ALPHA_WIDTH_SCALE: u32 = 1048576u;
const F_WHITEBOOST_ALWAYS: u32 = 2097152u;
const F_VERTEX_EMISSION: u32 = 4194304u;
const F_EMISSION_FLIPBOOK: u32 = 8388608u;
const F_COLOR_ARRAY: u32 = 16777216u;
const F_TEXTURE_FLIPBOOK: u32 = 33554432u;
const F_FLIPBOOK_NOBLEND: u32 = 67108864u;
const F_SPECTROGRAM_COLOR: u32 = 134217728u;

// _ColorsArray[round(uv2.x * 10 + uv2.y + _ColorsArrayOffset)] (b[5].y adds this material's base).
fn array_color(o: Object, uv2: vec2<f32>) -> vec4<f32> {
    let i = i32(round(uv2.x * 10.0 + uv2.y + o.b[5].y));
    if (i < 0 || u32(i) >= arrayLength(&color_arr)) {
        return vec4<f32>(0.0);
    }
    return color_arr[u32(i)];
}

fn has(o: Object, f: u32) -> bool {
    return (o.info.y & f) != 0u;
}

fn sat(x: f32) -> f32 {
    return clamp(x, 0.0, 1.0);
}

fn smooth01(t: f32) -> f32 {
    return t * t * (3.0 - 2.0 * t);
}

fn aces(x: vec3<f32>) -> vec3<f32> {
    return clamp((x * (2.51 * x + 0.03)) / (x * (2.43 * x + 0.59) + 0.14), vec3<f32>(0.0), vec3<f32>(1.0));
}

// CalculateCustomFogFactor: 0 = no fog, 1 = full fog. d2 = squared distance from the camera.
fn fog_factor(d2: f32, start_offset: f32, scale: f32) -> f32 {
    var r = max(d2 - start_offset, 0.0);
    r = max(r * scale - g.fog.x, 0.0);
    return 1.0 - 1.0 / (r * g.fog.y + 1.0);
}

// CalculateCustomHeightFogFactor: 0 below StartY + Height, 1 above StartY + 2 Height.
fn height_factor(y: f32, h_scale: f32, h_offset: f32) -> f32 {
    var h = g.fog.w;
    if (h == 0.0) {
        h = 1e-5;
    }
    return smooth01(sat(((y * h_scale + h_offset) - (g.fog.w + g.fog.z)) / h));
}

// Parametric distance transmission (light tubes): 1 = fully visible.
fn transmission(p: vec3<f32>, start_offset: f32, scale: f32, divisor: f32) -> f32 {
    let d = p - g.cam_pos.xyz;
    let d2 = dot(d, d);
    return 1.0 / (max(max(d2 - start_offset, 0.0) * (scale / max(divisor, 1.0)) - g.fog.x, 0.0) * g.fog.y + 1.0);
}

@group(1) @binding(0) var fog_tex: texture_2d<f32>;
@group(1) @binding(1) var clamp_samp: sampler;
@group(1) @binding(2) var refl_tex: texture_2d<f32>;

// The blurred fog texture at this pixel (screen mapped into the 130-degree fog frustum).
fn fog_color(frag: vec4<f32>) -> vec3<f32> {
    let uv = vec2<f32>(frag.x * g.screen.z, 1.0 - frag.y * g.screen.w);
    let f = (uv - 0.5) * g.ratio.xy + 0.5;
    return textureSampleLevel(fog_tex, clamp_samp, vec2<f32>(f.x, f.y), 0.0).rgb;
}

@group(2) @binding(0) var tex0: texture_2d<f32>;
@group(2) @binding(1) var tex1: texture_2d<f32>;
@group(2) @binding(2) var repeat_samp: sampler;

// --- Meshes -------------------------------------------------------------------------------------

struct VOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec4<f32>,
    @location(3) color: vec4<f32>,
    @location(4) @interpolate(flat) id: u32,
    @location(5) uv2: vec2<f32>,
};

@vertex
fn vs_mesh(@location(0) pos_in: vec3<f32>, @location(1) normal: vec3<f32>, @location(2) uv: vec2<f32>,
           @location(3) color: vec4<f32>, @location(4) uv2: vec2<f32>, @builtin(instance_index) id: u32) -> VOut {
    let o = objects[id];
    let kind = o.info.x;
    var pos = pos_in;
    var extra = vec2<f32>(0.0, 0.0);
    var uv_out = uv;
    var color_out = color;
    if (kind == KIND_SPECTROGRAM) {
        // No audio: every band is 0, so the bars sink by their full peak offset.
        pos = pos - uv.y * o.p[4].xyz;
    } else if (kind == KIND_BOX_TRANSPARENT) {
        let aw = o.b[1];
        let top = select(0.0, 1.0, pos.y > 0.5);
        let width = mix(aw.z, aw.w, top);
        extra.x = mix(aw.x, aw.y, top);
        pos = vec3<f32>(pos.x * width, pos.y, pos.z * width);
    } else if (kind == KIND_FAKE_GLOW) {
        let sp = o.b[2];
        let s = sign(pos);
        let size = max(abs(sp.xyz), vec3<f32>(1e-5));
        pos = s + (pos - s) * (2.0 * sp.w / size);
    } else if (kind == KIND_SLICE) {
        // ParametricSliceBillboard: caps and body scaled apart, turned about Y to face the camera.
        let c = o.b[0];
        let aw = o.b[1];
        let sp = o.b[2];
        let ew = max((aw.wz - aw.zw) * 1.3333 + aw.zw, aw.wz * 0.01);
        var band = ew.y;
        if (uv.y > 0.9) {
            band = ew.x;
        } else if (uv.y > 0.5) {
            band = aw.w;
        } else if (uv.y > 0.1) {
            band = aw.z;
        }
        var local_x = pos_in.x * sp.x * band;
        if (has(o, F_ALPHA_WIDTH_SCALE)) {
            let wa = sat(max(aw.x, aw.y) * c.a);
            local_x = local_x * smooth01(sat((wa - 0.02) / max(wa, 1e-6)));
        }
        let cap = select(0.0, 1.0, abs(uv.y - 0.5) >= 0.49);
        let local_y = (pos_in.y - sp.z) * sp.y + cap * (pos_in.y - 0.5) * sp.w;
        let cap_dir = select(0.0, 1.0, uv.y < 0.5) - select(0.0, 1.0, uv.y > 0.5);
        let adj_v = uv.y + (1.0 - cap) * (o.p[1].x - 0.25) * cap_dir;
        let inv3 = transpose(mat3x3<f32>(o.normal[0].xyz, o.normal[1].xyz, o.normal[2].xyz));
        let cam = inv3 * (g.cam_pos.xyz - o.model[3].xyz);
        var lp = vec3<f32>(local_x, local_y, pos_in.z);
        if (has(o, F_Y_BILLBOARD)) {
            let cxz = normalize(cam.xz + vec2<f32>(1e-9, 0.0));
            let sel = vec2<f32>(local_x, pos_in.z);
            lp = vec3<f32>(dot(vec2<f32>(-cxz.y, -cxz.x), sel), local_y, dot(vec2<f32>(cxz.x, -cxz.y), sel));
        }
        pos = lp;
        uv_out = vec2<f32>(uv.x * band, adj_v);
        extra = vec2<f32>(band, select(aw.x, aw.y, local_y > (0.5 - sp.z)));
        let ch = cam.y - local_y;
        let dist = max(sqrt(dot(cam.xz, cam.xz) + ch * ch), 1e-6);
        let ang = sat((dot(cam.xz / dist, normalize(cam.xz + vec2<f32>(1e-9, 0.0))) - 0.05) * 2.2222223);
        color_out = vec4<f32>(smooth01(ang), 0.0, 0.0, 0.0);
    }
    let world = (o.model * vec4<f32>(pos, 1.0)).xyz;
    let n = normalize((o.normal * vec4<f32>(normal, 0.0)).xyz + vec3<f32>(0.0, 0.0, 1e-9));
    if (kind == KIND_FAKE_GLOW) {
        extra.y = min(abs(dot(normalize(world - g.cam_pos.xyz), n) * o.p[1].x), 1.0);
    }
    var out: VOut;
    out.clip = g.view_proj * vec4<f32>(world, 1.0);
    out.world = world;
    out.normal = n;
    out.uv = vec4<f32>(uv_out, extra);
    out.color = color_out;
    out.id = id;
    out.uv2 = uv2;
    return out;
}

struct Lighting {
    diffuse: vec3<f32>,
    specular: vec3<f32>,
};

// ChroMapper's five custom directional lights (Common/Lighting.hlsl).
fn lights(p: vec3<f32>, n: vec3<f32>, smoothness: f32, both_sides: f32, falloff: bool, both: bool) -> Lighting {
    var out: Lighting;
    out.diffuse = vec3<f32>(0.0);
    out.specular = vec3<f32>(0.0);
    let r = reflect(normalize(p - g.cam_pos.xyz), n);
    let s2 = smoothness * smoothness;
    let s_scale = s2 * s2 * 500.0;
    for (var i = 0u; i < 5u; i = i + 1u) {
        let l = g.light_dir[i].xyz;
        let c = g.light_color[i].rgb;
        let nd = dot(n, l);
        let lr = l - r;
        let lobe = pow(sat(1.0 - dot(lr, lr) * s_scale * 0.5), 8.0);
        if (falloff) {
            let rad = g.light_radius[i / 4u][i % 4u];
            let d = p - g.light_pos[i].xyz;
            let fall = 1.0 / (dot(d, d) / (rad * rad) * 25.0 + 1.0);
            out.diffuse += max(nd, 0.0) * c * fall;
            out.specular += lobe * c * s_scale * fall;
        } else {
            var diff = max(nd, 0.0);
            if (both) {
                diff += min(nd, 0.0) * (-both_sides);
            }
            out.diffuse += diff * c;
            out.specular += lobe * c * s_scale;
        }
    }
    return out;
}

fn shade_lit(v: VOut, o: Object, front: bool) -> vec4<f32> {
    let n = select(-v.normal, v.normal, front);
    var bc = o.b[0].rgb;
    if (has(o, F_VERTEX_COLOR)) {
        bc = bc * v.color.rgb;
    }
    let metallic = o.p[1].x;
    let smoothness = o.p[1].y;
    let ambient = bc * max(o.p[2].xyz, vec3<f32>(o.p[2].w));
    let lit = lights(v.world, n, smoothness, o.p[1].w, has(o, F_FALLOFF), has(o, F_BOTH_SIDES));
    var dl = vec3<f32>(0.0);
    if (has(o, F_DIFFUSE)) {
        dl = lit.diffuse;
    }
    let direct_color = dl * bc;
    var direct_diffuse = direct_color * (1.0 - metallic);
    if (has(o, F_DIFFUSE) && has(o, F_SPECULAR)) {
        direct_diffuse = direct_diffuse * 0.96;
    }
    var direct_specular = vec3<f32>(0.0);
    if (has(o, F_SPECULAR)) {
        let base = select(bc, direct_color, has(o, F_DIFFUSE));
        let spec_color = 0.04 + metallic * (base - 0.04);
        direct_specular = lit.specular * spec_color * o.p[1].z;
    }
    var color = vec4<f32>(ambient + direct_diffuse + direct_specular, 0.0);
    if (has(o, F_ACES_BEFORE)) {
        color = vec4<f32>(aces(color.rgb), color.a);
    }
    var supp = 0.0;
    if (has(o, F_EMISSION_SIMPLE)) {
        let st = o.p[5];
        let e = textureSampleLevel(tex0, repeat_samp, v.uv.xy * st.xy + st.zw, 0.0).rg * o.p[4].w;
        var c = o.b[3];
        if (has(o, F_COLOR_ARRAY)) {
            c = array_color(o, v.uv2);
        }
        color = color + vec4<f32>(e.x * c.rgb * c.a, e.y * e.y * c.a * 3.5 * o.p[6].x);
        supp = e.x * c.a;
    }
    if (has(o, F_EMISSION_FLIPBOOK)) {
        // Frames packed four to a texel (RGBA), _FlipbookColumns x _FlipbookRows, from _StartTime.
        let fb = o.p[8];
        let total = trunc(fb.x * fb.y);
        let looping = trunc(total - fb.z);
        let ft = (g.misc.x + o.b[5].z - o.b[5].x) * fb.w;
        let lt = ft - fb.z;
        let signed_count = select(-looping, looping, looping * lt >= -(looping * lt));
        let frame = select(fract(lt / signed_count) * signed_count + fb.z, ft, ft < total);
        let cq = floor(frame) / fb.x;
        let column = fract(abs(cq)) * select(-1.0, 1.0, cq >= -cq) * fb.x;
        let row = fb.y - 1.0 - floor(frame / fb.x);
        let fuv = (vec2<f32>(column, row) + v.uv.xy) / vec2<f32>(fb.x, fb.y);
        let ff = fract(frame);
        var sel = vec4<f32>(0.0, 0.0, 0.0, 1.0);
        if (ff < 0.25) {
            sel = vec4<f32>(1.0, 0.0, 0.0, 0.0);
        } else if (ff < 0.5) {
            sel = vec4<f32>(0.0, 1.0, 0.0, 0.0);
        } else if (ff < 0.75) {
            sel = vec4<f32>(0.0, 0.0, 1.0, 0.0);
        }
        if (v.uv.x + v.uv.y <= 0.01) {
            sel = vec4<f32>(0.0);
        }
        let st = o.p[5];
        let e = dot(textureSampleLevel(tex0, repeat_samp, fuv * st.xy + st.zw, 0.0), sel) * o.p[4].w;
        let c = o.b[3];
        supp = supp + e * c.a;
        color = vec4<f32>(color.rgb + e * c.rgb * c.a, e * e * c.a * 3.5 * o.p[6].x);
    }
    if (has(o, F_VERTEX_EMISSION)) {
        // ResolveVertexEmission: vertex green past _EmissionThreshold, times _EmissionColor.
        let ec = o.b[4];
        let t = sat((v.color.g - o.p[7].x) / max(1.0 - o.p[7].x, 1e-5));
        let k = smooth01(t) * o.p[7].y;
        supp = supp + k * ec.a * ec.a;
        color = color + vec4<f32>(ec.rgb * ec.a * k, v.color.a * v.color.a * ec.a * o.p[7].z);
    }
    supp = supp * o.p[6].y;
    if (!has(o, F_ACES_BEFORE)) {
        color = vec4<f32>(aces(color.rgb), color.a);
    }
    if (has(o, F_FOG)) {
        let d = v.world - g.cam_pos.xyz;
        let f = fog_factor(dot(d, d), o.p[3].x, o.p[3].y * (1.0 - supp));
        var blend = f;
        if (has(o, F_HEIGHT_FOG)) {
            blend = 1.0 - height_factor(v.world.y, o.p[3].z, o.p[3].w) * (1.0 - f);
        }
        color = mix(color, vec4<f32>(fog_color(v.clip), 0.0), blend);
    }
    // Alpha (the bloom amount) through the material's source alpha factor (0 Zero, 1 One, 5 SrcAlpha).
    let src_a = o.p[4].x;
    if (src_a < 0.5) {
        color.a = 0.0;
    } else if (src_a > 4.5) {
        color.a = color.a * color.a;
    }
    return color;
}

fn shade_spectrogram(v: VOut, o: Object, front: bool) -> vec4<f32> {
    let n = select(-v.normal, v.normal, front);
    let metallic = o.p[1].x;
    let lit = lights(v.world, n, o.p[1].y, 1.0, has(o, F_FALLOFF), false);
    var lighting = vec3<f32>(0.0);
    if (has(o, F_DIFFUSE)) {
        let dc = lit.diffuse * o.p[0].rgb;
        if (has(o, F_SPECULAR)) {
            let sc = metallic * (dc - 0.04) + 0.04;
            lighting = dc * 0.96 * (1.0 - metallic) + lit.specular * sc * o.p[1].z;
        } else {
            lighting = dc * (1.0 - metallic);
        }
    }
    var c = vec4<f32>(aces(lighting), 0.0);
    let d = v.world - g.cam_pos.xyz;
    let h = height_factor(v.world.y, 1.0, 0.0);
    c = mix(c, vec4<f32>(fog_color(v.clip), 0.0), 1.0 - h * (1.0 - fog_factor(dot(d, d), o.p[3].x, o.p[3].y)));
    return vec4<f32>(c.rgb, 0.0);
}

fn shade_box_opaque(v: VOut, o: Object) -> vec4<f32> {
    let c = o.b[0];
    var a = c.a * c.a;
    var h = 1.0;
    if (has(o, F_HEIGHT_FOG)) {
        h = height_factor(v.world.y, o.p[3].z, o.p[3].w);
    }
    a = a * h * transmission(v.world, o.p[3].x, o.p[3].y, c.a);
    let ft = transmission(v.world, o.p[3].x, o.p[3].y, a);
    let fog_blend = 1.0 - h * ft;
    var rgb = c.rgb * a;
    // Verbatim from ChroMapper's ParametricBoxOpaque: rgb + rgb + fogBlend * (target - rgb).
    rgb = rgb + rgb + fog_blend * (fog_color(v.clip) - rgb);
    return vec4<f32>(rgb, a);
}

fn shade_box_transparent(v: VOut, o: Object) -> vec4<f32> {
    let c = o.b[0];
    let af = v.uv.z;
    let a0 = af * af * af * c.a;
    var a = a0 * a0;
    if (has(o, F_HEIGHT_FOG)) {
        a = a * height_factor(v.world.y, o.p[3].z, o.p[3].w);
    }
    a = a * transmission(v.world, o.p[3].x, o.p[3].y, a0);
    return vec4<f32>(c.rgb * a, a);
}

fn shade_fake_glow(v: VOut, o: Object) -> vec4<f32> {
    let c = o.b[0];
    let t = textureSampleLevel(tex0, clamp_samp, v.uv.xy, 0.0).a;
    var a = t * t;
    var hs = 1.0;
    var ho = 0.0;
    if (has(o, F_HEIGHT_FOG)) {
        hs = o.p[3].z;
        ho = o.p[3].w;
    }
    let h = height_factor(v.world.y, hs, ho);
    a = a * sat(h * transmission(v.world, o.p[3].x, o.p[3].y, 1.0) * v.uv.w * c.a);
    return vec4<f32>(c.rgb * a, a);
}

fn shade_mirror(v: VOut, o: Object) -> vec4<f32> {
    // Normal map wobble on the reflected view (sampled at the screen position).
    let nst = o.p[5];
    var n = textureSampleLevel(tex0, repeat_samp, v.uv.xy * nst.xy + nst.zw, 0.0);
    n.x = n.x * n.w;
    let nxy = n.xy * 2.0 - 1.0;
    let to = v.world - g.cam_pos.xyz;
    let view_y = to.y / max(length(to), 1e-6);
    let suv = vec2<f32>(v.clip.x * g.screen.z, 1.0 - v.clip.y * g.screen.w) - nxy * o.p[1].x * view_y;
    let refl = textureSampleLevel(refl_tex, clamp_samp, vec2<f32>(suv.x, 1.0 - suv.y), 0.0);
    var c = refl * o.p[1].y * o.p[1].y;
    if (has(o, F_DIRT)) {
        let st = o.p[6];
        let dirt = textureSampleLevel(tex1, repeat_samp, v.uv.xy * st.xy + st.zw, 0.0);
        c = c * (1.0 + o.p[1].w * (dirt - 1.0));
    }
    let d = v.world - g.cam_pos.xyz;
    c = mix(c, vec4<f32>(fog_color(v.clip), 0.0), fog_factor(dot(d, d), o.p[3].x, o.p[3].y));
    return c;
}

// ChroMapper/Parametric Slice Billboard (glow sprites along lasers).
fn shade_slice(v: VOut, o: Object) -> vec4<f32> {
    let c = o.b[0];
    let auv = vec2<f32>(v.uv.x / max(abs(v.uv.z), 1e-5), v.uv.y);
    var a = v.uv.w * v.uv.w * v.uv.w * c.a;
    if (has(o, F_ANGLE_DISAPPEAR)) {
        a = a * v.color.x;
    }
    var fogf = 1.0;
    if (has(o, F_FOG)) {
        var hf = 1.0;
        if (has(o, F_HEIGHT_FOG)) {
            hf = height_factor(v.world.y, o.p[3].z, o.p[3].w);
        }
        fogf = hf * transmission(v.world, o.p[3].x, o.p[3].y, a);
    }
    if (has(o, F_USE_FOG_FOR_LIGHTS)) {
        a = a * fogf;
    }
    if (has(o, F_SQUARE_ALPHA)) {
        a = a * a;
    }
    let ta = textureSampleLevel(tex0, clamp_samp, auv, 0.0).a;
    a = a * ta * ta;
    if (!has(o, F_USE_FOG_FOR_LIGHTS)) {
        a = a * fogf;
    }
    var rgb = c.rgb * a;
    if (has(o, F_WHITEBOOST_ALWAYS)) {
        let wb = a * a * o.p[1].z;
        rgb = clamp(c.rgb * a + vec3<f32>(wb * wb * g.misc.y - g.misc.z), vec3<f32>(0.0), vec3<f32>(1.0));
    }
    return vec4<f32>(rgb, a * o.p[1].y);
}

// ChroMapper/Particles (the features environment particles use).
fn shade_particles(v: VOut, o: Object) -> vec4<f32> {
    // SPECTROGRAM_COLOR scales by the audio band; without audio every band is 0.
    if (has(o, F_SPECTROGRAM_COLOR)) {
        return vec4<f32>(0.0);
    }
    var color = o.b[0];
    if (has(o, F_COLOR_ARRAY)) {
        color = array_color(o, v.uv2);
    }
    color = color * o.b[6];
    if (has(o, F_VERTEX_COLOR)) {
        color = vec4<f32>(color.rgb * v.color.rgb, color.a * v.color.a);
    }
    color = vec4<f32>(color.rgb * o.p[1].y, color.a);
    var albedo = color;
    if (has(o, F_MAIN_TEXTURE) && has(o, F_TEXTURE_FLIPBOOK)) {
        // Packed RGBA frames from _StartTime (raw UVs, no _MainTex_ST).
        let fb = o.p[6];
        let ft = (g.misc.x + o.b[5].z - o.b[5].x) * fb.w;
        let total = trunc(fb.x * fb.y);
        let looping = trunc(total - fb.z);
        var frame = ft;
        if (ft >= total) {
            frame = fract((ft - fb.z) / looping) * looping + fb.z;
        }
        let cell = floor(frame);
        let fr = fract(frame);
        let column = cell - fb.x * floor(cell / fb.x);
        let row = floor(cell / fb.x);
        let uv = vec2<f32>((v.uv.x + column) / fb.x, (v.uv.y + (fb.y - 1.0 - row)) / fb.y);
        var w: vec4<f32>;
        if (has(o, F_FLIPBOOK_NOBLEND)) {
            let ch = min(floor(fr * 4.0), 3.0);
            w = vec4<f32>(select(0.0, 1.0, ch == 0.0), select(0.0, 1.0, ch == 1.0), select(0.0, 1.0, ch == 2.0), select(0.0, 1.0, ch == 3.0));
        } else {
            w = vec4<f32>(max(1.0 - fr * 3.0, 0.0), max(1.0 - abs(fr * 3.0 - 1.0), 0.0), max(1.0 - abs(fr * 3.0 - 2.0), 0.0), max(fr * 3.0 - 2.0, 0.0));
        }
        albedo.a = albedo.a * dot(textureSampleLevel(tex0, repeat_samp, uv, 0.0), w);
    } else if (has(o, F_MAIN_TEXTURE)) {
        let st = o.p[5];
        let t = textureSampleLevel(tex0, repeat_samp, v.uv.xy * st.xy + st.zw, 0.0);
        albedo.a = albedo.a * select(t.a, t.r, has(o, F_ALPHA_RED));
    }
    albedo.a = albedo.a * o.p[1].x;
    if (has(o, F_SQUARE_ALPHA)) {
        albedo.a = albedo.a * sat(albedo.a);
    }
    if (has(o, F_FOG_COLOR) || has(o, F_FOG_ALPHA) || has(o, F_FOG_LERP)) {
        let d = v.world - g.cam_pos.xyz;
        var amount = 1.0 - fog_factor(dot(d, d), o.p[3].x, o.p[3].y);
        if (has(o, F_HEIGHT_FOG)) {
            amount = amount * height_factor(v.world.y, o.p[3].z, o.p[3].w);
        }
        if (has(o, F_FOG_ALPHA)) {
            albedo.a = albedo.a * amount;
        } else if (has(o, F_FOG_COLOR)) {
            albedo = vec4<f32>(albedo.rgb * fog_color(v.clip), albedo.a * amount);
        } else {
            albedo = vec4<f32>(mix(albedo.rgb, fog_color(v.clip), 1.0 - amount), albedo.a);
        }
    }
    let bloom = albedo.a;
    var out = vec4<f32>(albedo.rgb * bloom, bloom);
    if (has(o, F_WHITEBOOST_MAIN)) {
        out.a = bloom * o.p[1].z;
    }
    let src_a = o.p[4].x;
    if (src_a < 0.5) {
        out.a = 0.0;
    } else if (src_a > 4.5) {
        out.a = out.a * out.a;
    }
    return out;
}

@fragment
fn fs_mesh(v: VOut, @builtin(front_facing) front_in: bool) -> @location(0) vec4<f32> {
    let o = objects[v.id];
    // The reflected view: nothing below the mirror plane, and its winding is mirrored.
    if (g.flags.x > 0.5 && dot(v.world, g.clip.xyz) + g.clip.w < 0.0) {
        discard;
    }
    let front = select(front_in, !front_in, g.flags.y > 0.5);
    switch o.info.x {
        case 1u: { return shade_box_opaque(v, o); }
        case 2u: { return shade_box_transparent(v, o); }
        case 3u: { return shade_fake_glow(v, o); }
        case 4u: { return shade_mirror(v, o); }
        case 5u: { return shade_spectrogram(v, o, front); }
        case 6u: { return shade_particles(v, o); }
        case 7u: { return shade_slice(v, o); }
        default: { return shade_lit(v, o, front); }
    }
}

// --- Fog prepass light quads (BloomfogMesh) -----------------------------------------------------

struct QOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) uv: vec3<f32>,
    @location(2) t: vec4<f32>,
};

@group(1) @binding(0) var mask_tex: texture_2d<f32>;
@group(1) @binding(1) var mask_samp: sampler;

@vertex
fn vs_quad(@location(0) s: vec2<f32>, @location(1) color: vec4<f32>, @location(2) uv: vec3<f32>,
           @location(3) view_pos: vec3<f32>) -> QOut {
    var out: QOut;
    out.clip = vec4<f32>(s * 2.0 - 1.0, 0.0, 1.0);
    let c = color.rgb;
    out.color = vec4<f32>(c * (c * (c * 0.305306011 + 0.682171111) + 0.012522878), color.a);
    out.uv = uv;
    let z = select(view_pos.z, 1e-6, abs(view_pos.z) < 1e-6);
    out.t = vec4<f32>(view_pos / z, 1.0 / z);
    return out;
}

@fragment
fn fs_quad(q: QOut) -> @location(0) vec4<f32> {
    let v = q.t.xyz / q.t.w;
    let u0 = 1.0 / (max(dot(v, v) / max(q.color.a, 1.0) - g.fog.x, 0.0) * g.fog.y + 1.0);
    let uv = vec2<f32>(q.uv.x / max(q.uv.z, 1e-6), q.uv.y);
    let m = textureSampleLevel(mask_tex, mask_samp, uv, 0.0);
    let a = u0 * q.color.a * q.color.a * m.a;
    return vec4<f32>(q.color.rgb * m.rgb * a, 0.0);
}

// --- Fullscreen passes --------------------------------------------------------------------------

struct PassParams {
    texel: vec4<f32>,      // source texel size (xy)
    combine: vec4<f32>,    // _CombineParams.xy, sample scale, gate
};

struct FOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@group(1) @binding(0) var src_a: texture_2d<f32>;
@group(1) @binding(1) var src_b: texture_2d<f32>;
@group(1) @binding(2) var src_c: texture_2d<f32>;
@group(1) @binding(3) var samp: sampler;
@group(1) @binding(4) var<uniform> pp: PassParams;

@vertex
fn vs_full(@builtin(vertex_index) i: u32) -> FOut {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var out: FOut;
    out.clip = vec4<f32>(p * 2.0 - 1.0, 1.0, 1.0);
    out.uv = vec2<f32>(p.x, 1.0 - p.y);
    return out;
}

fn tap(t: texture_2d<f32>, uv: vec2<f32>) -> vec4<f32> {
    return textureSampleLevel(t, samp, clamp(uv, vec2<f32>(0.0), vec2<f32>(1.0)), 0.0);
}

fn down4(t: texture_2d<f32>, uv: vec2<f32>, ts: vec2<f32>) -> vec4<f32> {
    return (tap(t, uv + ts * vec2<f32>(-1.0, -1.0)) + tap(t, uv + ts * vec2<f32>(1.0, -1.0))
          + tap(t, uv + ts * vec2<f32>(-1.0, 1.0)) + tap(t, uv + ts * vec2<f32>(1.0, 1.0))) * 0.25;
}

fn down13(t: texture_2d<f32>, uv: vec2<f32>, ts: vec2<f32>) -> vec4<f32> {
    let a = tap(t, uv + ts * vec2<f32>(0.5, 0.5)) + tap(t, uv + ts * vec2<f32>(-0.5, 0.5))
          + tap(t, uv + ts * vec2<f32>(0.5, -0.5)) + tap(t, uv + ts * vec2<f32>(-0.5, -0.5));
    let c = tap(t, uv + ts * vec2<f32>(-1.0, -1.0)) + tap(t, uv + ts * vec2<f32>(1.0, -1.0))
          + tap(t, uv + ts * vec2<f32>(-1.0, 1.0)) + tap(t, uv + ts * vec2<f32>(1.0, 1.0));
    let e = tap(t, uv + ts * vec2<f32>(0.0, -1.0)) + tap(t, uv + ts * vec2<f32>(-1.0, 0.0))
          + tap(t, uv + ts * vec2<f32>(1.0, 0.0)) + tap(t, uv + ts * vec2<f32>(0.0, 1.0));
    return a * 0.125 + c * 0.03125 + e * 0.0625 + tap(t, uv) * 0.125;
}

fn tent(t: texture_2d<f32>, uv: vec2<f32>, ts: vec2<f32>, s: f32) -> vec4<f32> {
    let d = vec4<f32>(1.0, 1.0, -1.0, 0.0) * ts.xyxy * s;
    var c = tap(t, uv - d.xy);
    c += tap(t, uv - d.wy) * 2.0;
    c += tap(t, uv - d.zy);
    c += tap(t, uv + d.zw) * 2.0;
    c += tap(t, uv) * 4.0;
    c += tap(t, uv + d.xw) * 2.0;
    c += tap(t, uv + d.zy);
    c += tap(t, uv + d.wy) * 2.0;
    c += tap(t, uv + d.xy);
    return c * (1.0 / 16.0);
}

// Pass 3: 4-tap box downsample.
@fragment
fn fs_down4(v: FOut) -> @location(0) vec4<f32> {
    return down4(src_a, v.uv, pp.texel.xy);
}

// Passes 0/2: 13-tap downsample, optionally gating rgb by the bloom alpha.
@fragment
fn fs_down13(v: FOut) -> @location(0) vec4<f32> {
    var c = down13(src_a, v.uv, pp.texel.xy);
    if (pp.combine.w > 0.0) {
        c = vec4<f32>(c.rgb * sat(c.a * pp.combine.w), c.a);
    }
    return c;
}

// Pass 5: tent upsample of src_a combined with src_b (_BloomTex).
@fragment
fn fs_up(v: FOut) -> @location(0) vec4<f32> {
    return tap(src_b, v.uv) * pp.combine.x + tent(src_a, v.uv, pp.texel.xy, pp.combine.z) * pp.combine.y;
}

// Pass 6: box upsample of src_a combined with src_b (mirror reflections).
@fragment
fn fs_up_box(v: FOut) -> @location(0) vec4<f32> {
    let o = pp.texel.xy * (pp.combine.z * 0.5);
    let b = (tap(src_a, v.uv + vec2<f32>(-o.x, -o.y)) + tap(src_a, v.uv + vec2<f32>(o.x, -o.y))
           + tap(src_a, v.uv + vec2<f32>(-o.x, o.y)) + tap(src_a, v.uv + o)) * 0.25;
    return tap(src_b, v.uv) * pp.combine.x + b * pp.combine.y;
}

// Pass 13: tent combine, auto-exposure knee from the 2x2 level (src_c), then ACES.
@fragment
fn fs_fog_final(v: FOut) -> @location(0) vec4<f32> {
    var c = tap(src_b, v.uv) * pp.combine.x + tent(src_a, v.uv, pp.texel.xy, pp.combine.z) * pp.combine.y;
    let gi = tap(src_c, vec2<f32>(0.5, 0.5)).rgb;
    let lum = dot(gi, vec3<f32>(0.3, 0.59, 0.11));
    let cap = select(0.1 / sqrt(lum), 1e30, lum <= 0.0);
    var k = min(0.004 * g.ratio.z, cap);
    if (g.ratio.w > 0.5) {
        k = min(lum * g.ratio.z, cap);
    }
    c = c * k;
    return vec4<f32>(aces(c.rgb), c.a);
}

// Sky gradient (AfterBlur, tone-mapped), added into the fog texture. src_a = 128x1 gradient.
@fragment
fn fs_sky_gradient(v: FOut) -> @location(0) vec4<f32> {
    let ndc = vec2<f32>(v.uv.x * 2.0 - 1.0, (1.0 - v.uv.y) * 2.0 - 1.0);
    let dir = g.right.xyz * ndc.x * g.right.w + g.up.xyz * ndc.y * g.up.w + g.fwd.xyz;
    let t = normalize(dir).y * 0.5 + 0.5;
    let c = tap(src_a, vec2<f32>(t, 0.5)) * pp.combine;
    return vec4<f32>(aces(c.rgb), 0.0);
}

// Sky: the fog texture behind everything (drawn at the far plane).
@fragment
fn fs_sky(v: FOut) -> @location(0) vec4<f32> {
    let f = (vec2<f32>(v.uv.x, 1.0 - v.uv.y) - 0.5) * g.ratio.xy + 0.5;
    return vec4<f32>(tap(src_a, vec2<f32>(f.x, f.y)).rgb, 0.0);
}

// Copy of the camera target as UNorm16: clamps colour and bloom alpha to [0, 1].
@fragment
fn fs_clamp(v: FOut) -> @location(0) vec4<f32> {
    return clamp(tap(src_a, v.uv), vec4<f32>(0.0), vec4<f32>(1.0));
}

// PostBloom: scene + its bloom alpha squared (white boost), plus 0.3 x bloom.
// Selected lights' silhouettes.
@fragment
fn fs_outline_mask() -> @location(0) vec4<f32> {
    return vec4<f32>(1.0);
}

// White just outside the mask (src_a), pp.combine.x pixels wide; the light itself keeps its color.
@fragment
fn fs_outline(v: FOut) -> @location(0) vec4<f32> {
    if (tap(src_a, v.uv).r > 0.5) {
        discard;
    }
    let r = pp.texel.xy * pp.combine.x;
    var cover = 0.0;
    for (var i = 0; i < 16; i++) {
        let a = f32(i) * 0.39269908;
        let d = vec2<f32>(cos(a), sin(a)) * r;
        cover = max(cover, max(tap(src_a, v.uv + d).r, tap(src_a, v.uv + d * 0.5).r));
    }
    return vec4<f32>(1.0, 1.0, 1.0, cover);
}

@fragment
fn fs_composite(v: FOut) -> @location(0) vec4<f32> {
    let t = pp.texel.xy;
    let center = tap(src_a, v.uv);
    let alpha = 0.25 * (tap(src_a, v.uv + t * vec2<f32>(-0.5, 0.5)).a + tap(src_a, v.uv + t * vec2<f32>(0.0, -0.5)).a
                      + tap(src_a, v.uv + t * vec2<f32>(0.5, 0.5)).a + center.a);
    let scene = clamp(center.rgb + alpha * alpha, vec3<f32>(0.0), vec3<f32>(1.0));
    var bloom = vec3<f32>(0.0);
    if (pp.combine.x > 0.0) {
        bloom = tap(src_b, v.uv).rgb * pp.combine.x;
    }
    return vec4<f32>(bloom + scene, 1.0);
}
