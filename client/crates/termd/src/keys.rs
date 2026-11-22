//! Key-name translation (tmux `send-keys` syntax) and mouse escape encoding.

use std::fmt::Write as _;

use term_proto::{MouseEncoding, MouseEventKind, MouseProtocol};

/// Translate tmux-style key names (e.g. `"Enter"`, `"C-c"`, `"Escape"`, `"Up"`, `"BTab"`)
/// into terminal input bytes, honoring `app_cursor` mode (`DECCKM`).
pub fn translate_tmux_keys(keys: &[String], app_cursor: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for key in keys {
        translate_single_key(key, app_cursor, &mut out);
    }
    out
}

fn translate_single_key(key: &str, app_cursor: bool, out: &mut Vec<u8>) {
    match key {
        "Enter" | "Return" | "KP Enter" | "KPEnter" | "C-m" => out.push(b'\r'),
        "C-j" => out.push(b'\n'),
        "Escape" | "Esc" | "C-[" => out.push(0x1b),
        "Tab" | "C-i" => out.push(b'\t'),
        "BTab" | "S-Tab" => out.extend_from_slice(b"\x1b[Z"),
        "BSpace" | "Backspace" => out.push(0x7f),
        "Space" => out.push(b' '),
        "Up" => out.extend_from_slice(if app_cursor { b"\x1bOA" } else { b"\x1b[A" }),
        "Down" => out.extend_from_slice(if app_cursor { b"\x1bOB" } else { b"\x1b[B" }),
        "Right" => out.extend_from_slice(if app_cursor { b"\x1bOC" } else { b"\x1b[C" }),
        "Left" => out.extend_from_slice(if app_cursor { b"\x1bOD" } else { b"\x1b[D" }),
        "Home" => out.extend_from_slice(if app_cursor { b"\x1bOH" } else { b"\x1b[H" }),
        "End" => out.extend_from_slice(if app_cursor { b"\x1bOF" } else { b"\x1b[F" }),
        "IC" | "Insert" => out.extend_from_slice(b"\x1b[2~"),
        "DC" | "Delete" => out.extend_from_slice(b"\x1b[3~"),
        "PPage" | "PageUp" | "PP" => out.extend_from_slice(b"\x1b[5~"),
        "NPage" | "PageDown" | "NP" => out.extend_from_slice(b"\x1b[6~"),
        "F1" => out.extend_from_slice(b"\x1bOP"),
        "F2" => out.extend_from_slice(b"\x1bOQ"),
        "F3" => out.extend_from_slice(b"\x1bOR"),
        "F4" => out.extend_from_slice(b"\x1bOS"),
        "F5" => out.extend_from_slice(b"\x1b[15~"),
        "F6" => out.extend_from_slice(b"\x1b[17~"),
        "F7" => out.extend_from_slice(b"\x1b[18~"),
        "F8" => out.extend_from_slice(b"\x1b[19~"),
        "F9" => out.extend_from_slice(b"\x1b[20~"),
        "F10" => out.extend_from_slice(b"\x1b[21~"),
        "F11" => out.extend_from_slice(b"\x1b[23~"),
        "F12" => out.extend_from_slice(b"\x1b[24~"),
        "C-@" | "C-Space" => out.push(0x00),
        "C-\\" => out.push(0x1c),
        "C-]" => out.push(0x1d),
        "C-^" => out.push(0x1e),
        "C-_" => out.push(0x1f),
        other => {
            if let Some(ctrl) = other.strip_prefix("C-").or_else(|| other.strip_prefix('^')) {
                if ctrl.len() == 1 {
                    let b = ctrl.as_bytes()[0];
                    if b.is_ascii_alphabetic() {
                        out.push(b.to_ascii_lowercase() - b'a' + 1);
                        return;
                    }
                }
            }
            if let Some(meta) = other.strip_prefix("M-") {
                out.push(0x1b);
                translate_single_key(meta, app_cursor, out);
                return;
            }
            out.extend_from_slice(other.as_bytes());
        }
    }
}

/// Encode structured mouse pointer events according to the terminal's active mouse modes.
pub fn encode_mouse_report(
    kind: MouseEventKind,
    button: u8,
    col: u16,
    row: u16,
    modifiers: u8,
    protocol: MouseProtocol,
    encoding: MouseEncoding,
) -> Option<Vec<u8>> {
    if protocol == MouseProtocol::None {
        return None;
    }
    if matches!(kind, MouseEventKind::Move) && protocol != MouseProtocol::AnyMotion {
        return None;
    }
    if matches!(kind, MouseEventKind::Drag) && protocol == MouseProtocol::Normal {
        return None;
    }

    let base_btn = match kind {
        MouseEventKind::Down | MouseEventKind::Drag => button & 0x03,
        MouseEventKind::Up => {
            if encoding == MouseEncoding::Sgr {
                button & 0x03
            } else {
                3
            }
        }
        MouseEventKind::Move => 35,
        MouseEventKind::ScrollUp => 64,
        MouseEventKind::ScrollDown => 65,
        MouseEventKind::ScrollLeft => 66,
        MouseEventKind::ScrollRight => 67,
    };
    let motion_bit = if matches!(kind, MouseEventKind::Drag | MouseEventKind::Move) {
        32
    } else {
        0
    };
    let cb = base_btn | motion_bit | (modifiers & 0x1c);
    let x = col.saturating_add(1);
    let y = row.saturating_add(1);

    match encoding {
        MouseEncoding::Sgr => {
            let suffix = if matches!(kind, MouseEventKind::Up) {
                'm'
            } else {
                'M'
            };
            let mut s = String::with_capacity(16);
            let _ = write!(s, "\x1b[<{cb};{x};{y}{suffix}");
            Some(s.into_bytes())
        }
        MouseEncoding::Utf8 => {
            let mut bytes = vec![0x1b, b'[', b'M'];
            push_utf8_coord(cb as u32 + 32, &mut bytes);
            push_utf8_coord(x as u32 + 32, &mut bytes);
            push_utf8_coord(y as u32 + 32, &mut bytes);
            Some(bytes)
        }
        MouseEncoding::X10 => {
            if x > 223 || y > 223 {
                return None;
            }
            Some(vec![
                0x1b,
                b'[',
                b'M',
                cb.saturating_add(32),
                (x as u8).saturating_add(32),
                (y as u8).saturating_add(32),
            ])
        }
    }
}

fn push_utf8_coord(val: u32, out: &mut Vec<u8>) {
    if let Some(ch) = char::from_u32(val) {
        let mut buf = [0u8; 4];
        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
    }
}
