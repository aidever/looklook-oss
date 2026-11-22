// 一个实例一个矩形：纯色，或者从图集里按像素取字形（1:1，不缩放、不采样插值）
struct U {
    size: vec4<f32>,
};

@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var atlas: texture_2d<f32>;

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) @interpolate(flat) rect: vec4<f32>,
    @location(1) @interpolate(flat) uv: vec2<f32>,
    @location(2) @interpolate(flat) kind: u32,
    @location(3) @interpolate(flat) color: vec4<f32>,
};

@vertex
fn vs(
    @builtin(vertex_index) vi: u32,
    @location(0) rect: vec4<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) kind: u32,
    @location(3) color: vec4<f32>,
) -> VOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    let p = rect.xy + corners[vi] * rect.zw;
    var out: VOut;
    out.pos = vec4<f32>(p.x / u.size.x * 2.0 - 1.0, 1.0 - p.y / u.size.y * 2.0, 0.0, 1.0);
    out.rect = rect;
    out.uv = uv;
    out.kind = kind;
    out.color = color;
    return out;
}

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    if (in.kind == 0u) {
        return in.color;
    }
    let off = floor(in.pos.xy - in.rect.xy);
    let t = textureLoad(atlas, vec2<i32>(in.uv + off), 0);
    if (in.kind == 1u) {
        return vec4<f32>(in.color.rgb, in.color.a * t.a);
    }
    return vec4<f32>(t.rgb, t.a * in.color.a);
}
