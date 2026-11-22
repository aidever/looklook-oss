//! 框线、方块、盲文、Powerline 符号自己画（设计稿 §7.2）：不靠字体，按设备像素对齐，格子之间严丝合缝。
//! 这里只算出“画什么”（矩形、路径），真正画在字形图集的画布上（atlas.rs）。

#[derive(Clone, Debug, PartialEq)]
pub enum PathOp {
    Move(f32, f32),
    Line(f32, f32),
    /// 圆心、半径、起止角（弧度；屏幕坐标 y 朝下，角度增大是顺时针）、是否逆时针画
    Arc(f32, f32, f32, f32, f32, bool),
    Close,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Prim {
    /// x, y, w, h, 不透明度
    Rect(f32, f32, f32, f32, f32),
    /// 路径：`fill` 填充，否则按 `lw` 描边
    Path { ops: Vec<PathOp>, fill: bool, lw: f32 },
}

/// 线的粗细：0 无，1 细，2 粗，3 双线
type Arms = (u8, u8, u8, u8); // 上 右 下 左

fn arms(c: char) -> Option<Arms> {
    let v = c as u32;
    // U+250C..U+254B：四个方向细 / 粗的各种组合（按 Unicode 的顺序）
    const JOINTS: [Arms; 64] = [
        (0, 1, 1, 0), (0, 2, 1, 0), (0, 1, 2, 0), (0, 2, 2, 0), // ┌┍┎┏
        (0, 0, 1, 1), (0, 0, 1, 2), (0, 0, 2, 1), (0, 0, 2, 2), // ┐┑┒┓
        (1, 1, 0, 0), (1, 2, 0, 0), (2, 1, 0, 0), (2, 2, 0, 0), // └┕┖┗
        (1, 0, 0, 1), (1, 0, 0, 2), (2, 0, 0, 1), (2, 0, 0, 2), // ┘┙┚┛
        (1, 1, 1, 0), (1, 2, 1, 0), (2, 1, 1, 0), (1, 1, 2, 0), (2, 1, 2, 0), (2, 2, 1, 0), (1, 2, 2, 0), (2, 2, 2, 0), // ├…┣
        (1, 0, 1, 1), (1, 0, 1, 2), (2, 0, 1, 1), (1, 0, 2, 1), (2, 0, 2, 1), (2, 0, 1, 2), (1, 0, 2, 2), (2, 0, 2, 2), // ┤…┫
        (0, 1, 1, 1), (0, 1, 1, 2), (0, 2, 1, 1), (0, 2, 1, 2), (0, 1, 2, 1), (0, 1, 2, 2), (0, 2, 2, 1), (0, 2, 2, 2), // ┬…┳
        (1, 1, 0, 1), (1, 1, 0, 2), (1, 2, 0, 1), (1, 2, 0, 2), (2, 1, 0, 1), (2, 1, 0, 2), (2, 2, 0, 1), (2, 2, 0, 2), // ┴…┻
        (1, 1, 1, 1), (1, 1, 1, 2), (1, 2, 1, 1), (1, 2, 1, 2), (2, 1, 1, 1), (1, 1, 2, 1), (2, 1, 2, 1), (2, 1, 1, 2), // ┼…╃
        (2, 2, 1, 1), (1, 1, 2, 2), (1, 2, 2, 1), (2, 2, 1, 2), (1, 2, 2, 2), (2, 1, 2, 2), (2, 2, 2, 1), (2, 2, 2, 2), // ╄…╋
    ];
    Some(match v {
        0x2500 => (0, 1, 0, 1),
        0x2501 => (0, 2, 0, 2),
        0x2502 => (1, 0, 1, 0),
        0x2503 => (2, 0, 2, 0),
        0x250C..=0x254B => JOINTS[(v - 0x250C) as usize],
        0x2550 => (0, 3, 0, 3),
        0x2551 => (3, 0, 3, 0),
        0x2574 => (0, 0, 0, 1),
        0x2575 => (1, 0, 0, 0),
        0x2576 => (0, 1, 0, 0),
        0x2577 => (0, 0, 1, 0),
        0x2578 => (0, 0, 0, 2),
        0x2579 => (2, 0, 0, 0),
        0x257A => (0, 2, 0, 0),
        0x257B => (0, 0, 2, 0),
        0x257C => (0, 2, 0, 1),
        0x257D => (1, 0, 2, 0),
        0x257E => (0, 1, 0, 2),
        0x257F => (2, 0, 1, 0),
        _ => return None,
    })
}

/// 细线的粗细（设备像素）
pub fn thin_line(cell_h: u32) -> f32 {
    ((cell_h as f32) / 17.0).round().max(1.0)
}

fn weight(w: u8, thin: f32) -> f32 {
    match w {
        0 => 0.0,
        1 => thin,
        2 => (thin * 2.0).max(thin + 1.0),
        _ => thin,
    }
}

fn lines(a: Arms, w: f32, h: f32, thin: f32) -> Vec<Prim> {
    let (u, r, d, l) = a;
    let mut out = Vec::new();
    if u == 3 || r == 3 {
        // 双线（只有 ═ ║）：两条细线，中间隔一条细线宽
        let gap = thin;
        if r == 3 {
            let y0 = ((h - thin * 3.0) / 2.0).floor();
            out.push(Prim::Rect(0.0, y0, w, thin, 1.0));
            out.push(Prim::Rect(0.0, y0 + thin + gap, w, thin, 1.0));
        } else {
            let x0 = ((w - thin * 3.0) / 2.0).floor();
            out.push(Prim::Rect(x0, 0.0, thin, h, 1.0));
            out.push(Prim::Rect(x0 + thin + gap, 0.0, thin, h, 1.0));
        }
        return out;
    }
    let vx = |wt: f32| ((w - wt) / 2.0).floor();
    let hy = |wt: f32| ((h - wt) / 2.0).floor();
    // 竖线和横线交在中间：竖线伸到横线的另一边（取两边更粗的那条），拐角不缺角
    let hmax = weight(l, thin).max(weight(r, thin));
    let vmax = weight(u, thin).max(weight(d, thin));
    let (vy0, vy1) = if hmax > 0.0 { (hy(hmax), hy(hmax) + hmax) } else { (hy(vmax), hy(vmax) + vmax) };
    let (hx0, hx1) = if vmax > 0.0 { (vx(vmax), vx(vmax) + vmax) } else { (vx(hmax), vx(hmax) + hmax) };
    if u > 0 {
        let t = weight(u, thin);
        out.push(Prim::Rect(vx(t), 0.0, t, vy1, 1.0));
    }
    if d > 0 {
        let t = weight(d, thin);
        out.push(Prim::Rect(vx(t), vy0, t, h - vy0, 1.0));
    }
    if l > 0 {
        let t = weight(l, thin);
        out.push(Prim::Rect(0.0, hy(t), hx1, t, 1.0));
    }
    if r > 0 {
        let t = weight(r, thin);
        out.push(Prim::Rect(hx0, hy(t), w - hx0, t, 1.0));
    }
    out
}

fn rounded(c: char, w: f32, h: f32, thin: f32) -> Vec<Prim> {
    use std::f32::consts::PI;
    // 线的中心
    let cx = ((w - thin) / 2.0).floor() + thin / 2.0;
    let cy = ((h - thin) / 2.0).floor() + thin / 2.0;
    let r = (w / 2.0).min(h / 2.0);
    let ops = match c {
        // ╭ 往下、往右
        '╭' => vec![PathOp::Move(cx, h), PathOp::Line(cx, cy + r), PathOp::Arc(cx + r, cy + r, r, PI, 1.5 * PI, false), PathOp::Line(w, cy)],
        // ╮ 往下、往左
        '╮' => vec![PathOp::Move(cx, h), PathOp::Line(cx, cy + r), PathOp::Arc(cx - r, cy + r, r, 0.0, -0.5 * PI, true), PathOp::Line(0.0, cy)],
        // ╯ 往上、往左
        '╯' => vec![PathOp::Move(cx, 0.0), PathOp::Line(cx, cy - r), PathOp::Arc(cx - r, cy - r, r, 0.0, 0.5 * PI, false), PathOp::Line(0.0, cy)],
        // ╰ 往上、往右
        _ => vec![PathOp::Move(cx, 0.0), PathOp::Line(cx, cy - r), PathOp::Arc(cx + r, cy - r, r, PI, 0.5 * PI, true), PathOp::Line(w, cy)],
    };
    vec![Prim::Path { ops, fill: false, lw: thin }]
}

fn blocks(c: char, w: f32, h: f32) -> Option<Vec<Prim>> {
    let v = c as u32;
    let rect = |x0: f32, y0: f32, x1: f32, y1: f32| Prim::Rect((w * x0).round(), (h * y0).round(), (w * x1).round() - (w * x0).round(), (h * y1).round() - (h * y0).round(), 1.0);
    Some(match v {
        0x2580 => vec![rect(0.0, 0.0, 1.0, 0.5)],
        0x2581..=0x2588 => {
            let k = (v - 0x2580) as f32 / 8.0;
            vec![rect(0.0, 1.0 - k, 1.0, 1.0)]
        }
        0x2589..=0x258F => {
            let k = (0x2590 - v) as f32 / 8.0;
            vec![rect(0.0, 0.0, k, 1.0)]
        }
        0x2590 => vec![rect(0.5, 0.0, 1.0, 1.0)],
        0x2591..=0x2593 => vec![Prim::Rect(0.0, 0.0, w, h, (v - 0x2590) as f32 * 0.25)],
        0x2594 => vec![rect(0.0, 0.0, 1.0, 0.125)],
        0x2595 => vec![rect(0.875, 0.0, 1.0, 1.0)],
        0x2596..=0x259F => {
            // 四分块：左上 右上 左下 右下
            const Q: [u8; 10] = [0b0010, 0b0001, 0b1000, 0b1011, 0b1001, 0b1110, 0b1101, 0b0100, 0b0110, 0b0111];
            let q = Q[(v - 0x2596) as usize];
            let mut out = Vec::new();
            if q & 0b1000 != 0 {
                out.push(rect(0.0, 0.0, 0.5, 0.5));
            }
            if q & 0b0100 != 0 {
                out.push(rect(0.5, 0.0, 1.0, 0.5));
            }
            if q & 0b0010 != 0 {
                out.push(rect(0.0, 0.5, 0.5, 1.0));
            }
            if q & 0b0001 != 0 {
                out.push(rect(0.5, 0.5, 1.0, 1.0));
            }
            out
        }
        _ => return None,
    })
}

fn braille(c: char, w: f32, h: f32) -> Option<Vec<Prim>> {
    let v = c as u32;
    if !(0x2801..=0x28FF).contains(&v) {
        return None;
    }
    let bits = v - 0x2800;
    // 点的编号 → (列, 行)
    const DOTS: [(u8, u8); 8] = [(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2), (0, 3), (1, 3)];
    let d = (w / 4.0).round().max(1.0);
    let mut out = Vec::new();
    for (i, &(cx, cy)) in DOTS.iter().enumerate() {
        if bits & (1 << i) == 0 {
            continue;
        }
        let x = (w * (cx as f32 * 2.0 + 1.0) / 4.0 - d / 2.0).round();
        let y = (h * (cy as f32 * 2.0 + 1.0) / 8.0 - d / 2.0).round();
        out.push(Prim::Rect(x, y, d, d, 1.0));
    }
    Some(out)
}

fn powerline(c: char, w: f32, h: f32, thin: f32) -> Option<Vec<Prim>> {
    use std::f32::consts::PI;
    let m = h / 2.0;
    Some(match c as u32 {
        0xE0B0 => vec![Prim::Path { ops: vec![PathOp::Move(0.0, 0.0), PathOp::Line(w, m), PathOp::Line(0.0, h), PathOp::Close], fill: true, lw: 0.0 }],
        0xE0B2 => vec![Prim::Path { ops: vec![PathOp::Move(w, 0.0), PathOp::Line(0.0, m), PathOp::Line(w, h), PathOp::Close], fill: true, lw: 0.0 }],
        0xE0B1 => vec![Prim::Path { ops: vec![PathOp::Move(0.0, 0.0), PathOp::Line(w - thin / 2.0, m), PathOp::Line(0.0, h)], fill: false, lw: thin }],
        0xE0B3 => vec![Prim::Path { ops: vec![PathOp::Move(w, 0.0), PathOp::Line(thin / 2.0, m), PathOp::Line(w, h)], fill: false, lw: thin }],
        0xE0B4 => vec![Prim::Path { ops: vec![PathOp::Move(0.0, 0.0), PathOp::Arc(0.0, m, m.min(w), -0.5 * PI, 0.5 * PI, false), PathOp::Close], fill: true, lw: 0.0 }],
        0xE0B6 => vec![Prim::Path { ops: vec![PathOp::Move(w, 0.0), PathOp::Arc(w, m, m.min(w), 1.5 * PI, 0.5 * PI, true), PathOp::Close], fill: true, lw: 0.0 }],
        _ => return None,
    })
}

/// 这个字自己画：返回要画的东西；None = 交给字体
pub fn prims(c: char, w: u32, h: u32) -> Option<Vec<Prim>> {
    let (wf, hf) = (w as f32, h as f32);
    let thin = thin_line(h);
    if let Some(a) = arms(c) {
        return Some(lines(a, wf, hf, thin));
    }
    if matches!(c, '╭' | '╮' | '╯' | '╰') {
        return Some(rounded(c, wf, hf, thin));
    }
    blocks(c, wf, hf).or_else(|| braille(c, wf, hf)).or_else(|| powerline(c, wf, hf, thin))
}

/// 波浪下划线（一个格子宽）
pub fn undercurl(w: u32, h: u32, y: f32, thin: f32) -> Vec<Prim> {
    let amp = (h as f32 / 10.0).max(1.0);
    let mut ops = vec![PathOp::Move(0.0, y)];
    let steps = 8;
    for i in 1..=steps {
        let x = w as f32 * i as f32 / steps as f32;
        let yy = y - amp * (std::f32::consts::TAU * i as f32 / steps as f32).sin();
        ops.push(PathOp::Line(x, yy));
    }
    vec![Prim::Path { ops, fill: false, lw: thin }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rects(p: &[Prim]) -> Vec<(f32, f32, f32, f32)> {
        p.iter()
            .filter_map(|x| match x {
                Prim::Rect(a, b, c, d, _) => Some((*a, *b, *c, *d)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn horizontal_and_vertical_lines_span_the_cell() {
        // ─ 两段拼起来横贯整格，竖线位置在两个格子里一样（对得上）
        let r = rects(&prims('─', 9, 18).unwrap());
        let left = r.iter().map(|x| x.0).fold(f32::MAX, f32::min);
        let right = r.iter().map(|x| x.0 + x.2).fold(0.0, f32::max);
        assert_eq!((left, right), (0.0, 9.0));
        let v = rects(&prims('│', 9, 18).unwrap());
        let top = v.iter().map(|x| x.1).fold(f32::MAX, f32::min);
        let bottom = v.iter().map(|x| x.1 + x.3).fold(0.0, f32::max);
        assert_eq!((top, bottom), (0.0, 18.0));
        // ┼ 的竖线和 │ 的竖线在同一列
        let cross = rects(&prims('┼', 9, 18).unwrap());
        assert!(cross.iter().any(|x| x.0 == v[0].0 && x.2 == v[0].2));
    }

    #[test]
    fn corners_join_without_gaps() {
        // ┌：横线从竖线的左边开始，竖线从横线的上边开始
        let r = rects(&prims('┌', 10, 20).unwrap());
        let down = r.iter().find(|x| x.3 > x.2).unwrap();
        let right = r.iter().find(|x| x.2 > x.3).unwrap();
        assert_eq!(right.0, down.0);
        assert_eq!(down.1, right.1);
        assert_eq!(right.0 + right.2, 10.0);
        assert_eq!(down.1 + down.3, 20.0);
    }

    #[test]
    fn blocks_shades_braille_and_fallback() {
        assert_eq!(rects(&prims('█', 8, 16).unwrap()), vec![(0.0, 0.0, 8.0, 16.0)]);
        assert_eq!(rects(&prims('▄', 8, 16).unwrap()), vec![(0.0, 8.0, 8.0, 8.0)]);
        assert_eq!(rects(&prims('▌', 8, 16).unwrap()), vec![(0.0, 0.0, 4.0, 16.0)]);
        assert!(matches!(prims('▒', 8, 16).unwrap()[0], Prim::Rect(_, _, _, _, a) if a == 0.5));
        assert_eq!(prims('⣿', 8, 16).unwrap().len(), 8);
        assert_eq!(prims('⠁', 8, 16).unwrap().len(), 1);
        assert!(prims('a', 8, 16).is_none());
        assert!(prims('╔', 8, 16).is_none()); // 双线拐角交给字体
        assert!(prims('\u{E0B0}', 8, 16).is_some());
    }
}
