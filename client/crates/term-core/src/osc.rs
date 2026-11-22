//! Incremental OSC scanner for shell integration (OSC 7 cwd, OSC 9;4 progress, OSC 133 marks).

use serde::{Deserialize, Serialize};

use crate::types::{CoreEvent, ProgressState, PromptMarkKind};

const MAX_OSC_LEN: usize = 8192;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
enum ScanState {
    #[default]
    Normal,
    Esc,
    OscBody,
    OscEsc,
}

/// Byte-stream scanner extracting OSC sequences that `vte::ansi::Performer` leaves unhandled.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OscScanner {
    state: ScanState,
    buf: Vec<u8>,
}

impl OscScanner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.state = ScanState::Normal;
        self.buf.clear();
    }

    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<CoreEvent>) {
        for &b in bytes {
            match self.state {
                ScanState::Normal => {
                    if b == 0x1b {
                        self.state = ScanState::Esc;
                    }
                }
                ScanState::Esc => {
                    if b == b']' {
                        self.buf.clear();
                        self.state = ScanState::OscBody;
                    } else if b == 0x1b {
                        self.state = ScanState::Esc;
                    } else {
                        self.state = ScanState::Normal;
                    }
                }
                ScanState::OscBody => match b {
                    0x07 => {
                        self.dispatch(out);
                        self.buf.clear();
                        self.state = ScanState::Normal;
                    }
                    0x1b => {
                        self.state = ScanState::OscEsc;
                    }
                    0x18 | 0x1a => {
                        // CAN / SUB abort escape sequence.
                        self.buf.clear();
                        self.state = ScanState::Normal;
                    }
                    _ => {
                        if self.buf.len() < MAX_OSC_LEN {
                            self.buf.push(b);
                        } else {
                            self.buf.clear();
                            self.state = ScanState::Normal;
                        }
                    }
                },
                ScanState::OscEsc => {
                    if b == b'\\' {
                        self.dispatch(out);
                        self.buf.clear();
                        self.state = ScanState::Normal;
                    } else if b == b']' {
                        // New OSC aborted the previous unterminated one.
                        self.buf.clear();
                        self.state = ScanState::OscBody;
                    } else if b == 0x1b {
                        self.state = ScanState::Esc;
                        self.buf.clear();
                    } else {
                        self.buf.clear();
                        self.state = ScanState::Normal;
                    }
                }
            }
        }
    }

    fn dispatch(&self, out: &mut Vec<CoreEvent>) {
        let Ok(text) = std::str::from_utf8(&self.buf) else {
            return;
        };
        if let Some(rest) = text.strip_prefix("7;") {
            if let Some(path) = parse_osc7_cwd(rest) {
                out.push(CoreEvent::CwdChanged(path));
            }
        } else if let Some(rest) = text.strip_prefix("9;4;") {
            if let Some(progress) = parse_osc9_4(rest) {
                out.push(CoreEvent::Progress(progress));
            }
        } else if let Some(rest) = text.strip_prefix("133;") {
            if let Some(mark) = parse_osc133(rest) {
                out.push(CoreEvent::PromptMark(mark));
            }
        }
    }
}

fn parse_osc7_cwd(arg: &str) -> Option<String> {
    let path_raw = if let Some(after_scheme) = arg.strip_prefix("file://") {
        // file://host/path -> find first '/' separating host from path.
        let slash = after_scheme.find('/')?;
        &after_scheme[slash..]
    } else if arg.starts_with('/') {
        arg
    } else {
        return None;
    };
    let path_no_query = path_raw.split(['?', '#']).next().unwrap_or(path_raw);
    let decoded = percent_decode_utf8(path_no_query)?;
    if decoded.is_empty() {
        None
    } else {
        Some(decoded)
    }
}

fn percent_decode_utf8(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn parse_osc9_4(arg: &str) -> Option<ProgressState> {
    let mut parts = arg.split(';');
    let state = parts.next()?.parse::<u8>().ok()?;
    let pct = parts.next().and_then(|s| s.parse::<u8>().ok()).map(|v| v.min(100));
    match state {
        0 => Some(ProgressState::Hidden),
        1 => Some(ProgressState::Normal(pct.unwrap_or(0))),
        2 => Some(ProgressState::Error(pct.unwrap_or(0))),
        3 => Some(ProgressState::Indeterminate),
        4 => Some(ProgressState::Paused(pct.unwrap_or(0))),
        _ => None,
    }
}

fn parse_osc133(arg: &str) -> Option<PromptMarkKind> {
    let mut parts = arg.split(';');
    match parts.next()? {
        "A" => Some(PromptMarkKind::PromptStart),
        "B" => Some(PromptMarkKind::CommandStart),
        "C" => Some(PromptMarkKind::CommandExecuted),
        "D" => {
            let code = parts.next().and_then(|s| s.parse::<i32>().ok());
            Some(PromptMarkKind::CommandFinished(code))
        }
        _ => None,
    }
}
