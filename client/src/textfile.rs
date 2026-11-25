//! 文字文件的读和写（docs/FILES_REDO_DESIGN_2026-11-24.md §4.4、§4.5）：
//!
//! - 读：认出编码（BOM → UTF-8 / UTF-16；否则严格按 UTF-8，不是再让 chardetng 猜 GB18030、Big5、Shift_JIS…），
//!   转成 UTF-8 给浏览器，同时告诉它原来的编码、换行（lf / crlf / cr / mixed）、有没有 BOM。大文件按段读：
//!   从某个字节、某一行开始取一段。
//! - 写：浏览器里统一用 `\n`，保存时按读到时的换行、编码、BOM 写回，不会悄悄把 CRLF 改成 LF、把 BOM 去掉。
//!   原编码表示不了的字符报 `ENCODING_UNMAPPABLE`（界面问要不要改存成 UTF-8）。

use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use encoding_rs::{Encoding, UTF_16BE, UTF_16LE, UTF_8};
use serde::Serialize;

use crate::error::LocalError;

type R<T> = Result<T, LocalError>;

/// 一次最多给浏览器这么多字节的原文
pub const READ_MAX: u64 = 4 * 1024 * 1024;
/// 默认一段
const READ_DEFAULT: u64 = 1024 * 1024;
/// 能在看看里编辑、保存的上限
pub const EDIT_MAX: u64 = 4 * 1024 * 1024;
/// 猜编码看前面多少
const SNIFF: usize = 64 * 1024;
/// 跳到第 N 行时，往前多给几行做上下文
const LINE_CONTEXT: u64 = 50;

fn io_err(e: std::io::Error, path: &Path) -> LocalError {
    let p = path.display().to_string();
    match e.kind() {
        ErrorKind::NotFound => LocalError::not_found().with("path", p),
        ErrorKind::PermissionDenied => LocalError::new("FORBIDDEN").with("path", p),
        ErrorKind::AlreadyExists => LocalError::new("FILE_EXISTS").with("path", p),
        _ => LocalError::from(e),
    }
}

fn absolute(path: &str) -> R<PathBuf> {
    let p = PathBuf::from(path.trim());
    if path.trim().is_empty() || !p.is_absolute() {
        return Err(LocalError::invalid("path", "absolute"));
    }
    Ok(p)
}

pub fn mtime_ms(m: &std::fs::Metadata) -> i64 {
    m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64)
}

/// 认出来的编码。`None` = 二进制
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Detected {
    pub encoding: &'static Encoding,
    pub bom: bool,
}

/// 按文件开头认编码。`whole`：给的就是整个文件（不是只有开头一段，末尾不会切在字符中间）
pub fn detect(head: &[u8], whole: bool) -> Option<Detected> {
    if let Some((enc, _)) = Encoding::for_bom(head) {
        return Some(Detected { encoding: enc, bom: true });
    }
    let sample = &head[..head.len().min(SNIFF)];
    let cut = whole && head.len() <= SNIFF;
    if sample.contains(&0) {
        return utf16_without_bom(sample).map(|encoding| Detected { encoding, bom: false });
    }
    match std::str::from_utf8(sample) {
        Ok(_) => return Some(Detected { encoding: UTF_8, bom: false }),
        // 只是末尾切在了一个字符中间
        Err(e) if e.error_len().is_none() && !cut => return Some(Detected { encoding: UTF_8, bom: false }),
        Err(_) => {}
    }
    let mut d = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
    d.feed(sample, cut);
    let encoding = d.guess(None, chardetng::Utf8Detection::Allow);
    // 猜出来的也解不通（大量替换字符）：当二进制
    let (text, _) = encoding.decode_without_bom_handling(sample);
    let bad = text.chars().filter(|&c| c == '\u{FFFD}' || (c.is_control() && !matches!(c, '\n' | '\r' | '\t' | '\x0c' | '\x1b'))).count();
    if bad * 20 > text.chars().count().max(1) {
        return None;
    }
    Some(Detected { encoding, bom: false })
}

/// 没有 BOM 的 UTF-16：ASCII 为主的文字每两个字节里有一个 0，且都在同一边
fn utf16_without_bom(s: &[u8]) -> Option<&'static Encoding> {
    let n = s.len() / 2;
    if n < 8 {
        return None;
    }
    let (mut even, mut odd) = (0, 0);
    for pair in s.chunks_exact(2) {
        even += usize::from(pair[0] == 0);
        odd += usize::from(pair[1] == 0);
    }
    if odd * 10 > n * 4 && even * 50 < n {
        Some(UTF_16LE)
    } else if even * 10 > n * 4 && odd * 50 < n {
        Some(UTF_16BE)
    } else {
        None
    }
}

fn is_utf16(e: &'static Encoding) -> bool {
    e == UTF_16LE || e == UTF_16BE
}

/// `lf` / `crlf` / `cr` / `mixed`；没有换行时 `None`
pub fn eol_of(text: &str) -> Option<&'static str> {
    let b = text.as_bytes();
    let (mut lf, mut crlf, mut cr) = (0usize, 0usize, 0usize);
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\r' if b.get(i + 1) == Some(&b'\n') => {
                crlf += 1;
                i += 1;
            }
            b'\r' => cr += 1,
            b'\n' => lf += 1,
            _ => {}
        }
        i += 1;
    }
    match ((lf > 0) as u8 + (crlf > 0) as u8 + (cr > 0) as u8, lf, crlf) {
        (0, ..) => None,
        (1, l, _) if l > 0 => Some("lf"),
        (1, _, c) if c > 0 => Some("crlf"),
        (1, ..) => Some("cr"),
        _ => Some("mixed"),
    }
}

/// 混合换行时多数派是哪种（保存时按它写）
pub fn eol_major(text: &str) -> &'static str {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count() - crlf;
    if crlf > lf {
        "crlf"
    } else {
        "lf"
    }
}

#[derive(Debug, Serialize)]
pub struct ReadOut {
    /// 转成 UTF-8 的文字；二进制时为 None
    pub text: Option<String>,
    /// 编码名（WHATWG 名字，例如 `UTF-8`、`GBK`、`UTF-16LE`）
    pub encoding: Option<&'static str>,
    pub bom: bool,
    /// 这一段里的换行
    pub eol: Option<&'static str>,
    /// 混合换行时的多数派
    pub eol_major: Option<&'static str>,
    pub size: u64,
    pub mtime_ms: i64,
    /// 这一段在文件里的字节范围 [offset, end)
    pub offset: u64,
    pub end: u64,
    /// 这一段第一行是文件的第几行（从 1 起；UTF-16 不算行号时为 None）
    pub line: Option<u64>,
}

/// 读一段。`offset` / `line`（给了 line 就以它为准）指定从哪里开始，`len` 最多 [`READ_MAX`]。
/// `encoding`：手动指定编码重读（界面上“换编码”）。不从文件开头读时，开头那半行丢掉，从下一行开始。
pub fn read(path: &str, offset: Option<u64>, line: Option<u64>, len: Option<u64>, encoding: Option<&str>) -> R<ReadOut> {
    let p = absolute(path)?;
    let m = std::fs::metadata(&p).map_err(|e| io_err(e, &p))?;
    if !m.is_file() {
        return Err(LocalError::new("NOT_A_FILE").with("path", path));
    }
    let size = m.len();
    let mut f = std::fs::File::open(&p).map_err(|e| io_err(e, &p))?;
    let mut head = vec![0u8; size.min(SNIFF as u64) as usize];
    f.read_exact(&mut head).map_err(|e| io_err(e, &p))?;
    let forced = match encoding.filter(|s| !s.is_empty()) {
        Some(name) => Some(Encoding::for_label(name.as_bytes()).ok_or_else(|| LocalError::invalid("encoding", name))?),
        None => None,
    };
    let det = match forced {
        Some(enc) => Some(Detected { encoding: enc, bom: Encoding::for_bom(&head).is_some_and(|(e, _)| e == enc) }),
        None => detect(&head, size <= SNIFF as u64),
    };
    let base = ReadOut { text: None, encoding: None, bom: false, eol: None, eol_major: None, size, mtime_ms: mtime_ms(&m), offset: 0, end: 0, line: None };
    let Some(det) = det else { return Ok(base) };
    let len = len.unwrap_or(READ_DEFAULT).clamp(1, READ_MAX);
    let utf16 = is_utf16(det.encoding);
    let bom_len = if det.bom { Encoding::for_bom(&head).map_or(0, |(_, n)| n as u64) } else { 0 };
    // 起点
    let (mut start, mut first_line) = match line.filter(|&l| l > 1 && !utf16) {
        Some(l) => {
            let want = l.saturating_sub(LINE_CONTEXT).max(1);
            match line_offset(&mut f, want).map_err(|e| io_err(e, &p))? {
                Some(o) => (o, Some(want)),
                // 没那么多行：给最后一段
                None => (size.saturating_sub(len), None),
            }
        }
        None => (offset.unwrap_or(0).min(size), None),
    };
    if start <= bom_len {
        start = bom_len;
        first_line = Some(1);
    }
    if utf16 && (start - bom_len) % 2 == 1 {
        start += 1;
    }
    // 不从文件开头（或某一行开头）开始：多读前一个字节，丢掉前面半行（正好在行首时只丢那个换行）
    let mid = start > bom_len && first_line.is_none() && !utf16;
    let from = if mid { start - 1 } else { start };
    f.seek(SeekFrom::Start(from)).map_err(|e| io_err(e, &p))?;
    let mut buf = Vec::with_capacity((len + 1).min(size - from) as usize);
    (&mut f).take(len + u64::from(mid)).read_to_end(&mut buf).map_err(|e| io_err(e, &p))?;
    let start = from;
    let mut end = start + buf.len() as u64;
    let skip = if mid { buf.iter().position(|&b| b == b'\n').map_or(buf.len(), |i| i + 1) } else { 0 };
    // 没读到文件末尾：退到最后一个换行（下一段从下一行接着）；一行特别长、找不到换行时 UTF-8 退到完整字符、UTF-16 退成偶数
    let mut keep = buf.len();
    if end < size {
        if utf16 {
            keep -= (keep - skip) % 2;
        } else if let Some(i) = buf[skip..].iter().rposition(|&b| b == b'\n') {
            keep = skip + i + 1;
        } else if det.encoding == UTF_8 {
            while keep > skip && std::str::from_utf8(&buf[skip..keep]).is_err_and(|e| e.error_len().is_none()) {
                keep -= 1;
            }
        }
        end = start + keep as u64;
    }
    let offset = start + skip as u64;
    let chunk = &buf[skip..keep];
    let (text, _) = det.encoding.decode_without_bom_handling(chunk);
    let text = text.into_owned();
    if first_line.is_none() && !utf16 {
        first_line = Some(count_lines_before(&mut f, offset).map_err(|e| io_err(e, &p))? + 1);
    }
    let eol = eol_of(&text);
    Ok(ReadOut {
        eol_major: (eol == Some("mixed")).then(|| eol_major(&text)),
        text: Some(text),
        encoding: Some(det.encoding.name()),
        bom: det.bom,
        eol,
        offset,
        end,
        line: first_line,
        ..base
    })
}

/// 第 `line` 行（从 1 起）从哪个字节开始；文件没这么多行时 None
fn line_offset(f: &mut std::fs::File, line: u64) -> std::io::Result<Option<u64>> {
    if line <= 1 {
        return Ok(Some(0));
    }
    f.seek(SeekFrom::Start(0))?;
    let mut buf = vec![0u8; 256 * 1024];
    let (mut seen, mut pos) = (0u64, 0u64);
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(None);
        }
        for i in memchr::memchr_iter(b'\n', &buf[..n]) {
            seen += 1;
            if seen == line - 1 {
                return Ok(Some(pos + i as u64 + 1));
            }
        }
        pos += n as u64;
    }
}

/// `offset` 之前有几个换行
fn count_lines_before(f: &mut std::fs::File, offset: u64) -> std::io::Result<u64> {
    if offset == 0 {
        return Ok(0);
    }
    f.seek(SeekFrom::Start(0))?;
    let mut buf = vec![0u8; 256 * 1024];
    let (mut left, mut n_lines) = (offset, 0u64);
    while left > 0 {
        let want = left.min(buf.len() as u64) as usize;
        let n = f.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        n_lines += memchr::memchr_iter(b'\n', &buf[..n]).count() as u64;
        left -= n as u64;
    }
    Ok(n_lines)
}

/// 浏览器里的文字（`\n` 换行）按原来的格式编成要写进文件的字节
pub fn encode(content: &str, encoding: Option<&str>, eol: Option<&str>, bom: bool) -> R<Vec<u8>> {
    let enc = match encoding.filter(|s| !s.is_empty()) {
        Some(name) => Encoding::for_label(name.as_bytes()).ok_or_else(|| LocalError::invalid("encoding", name))?,
        None => UTF_8,
    };
    let unified;
    let text: &str = if content.contains('\r') {
        unified = content.replace("\r\n", "\n").replace('\r', "\n");
        &unified
    } else {
        content
    };
    let converted;
    let text: &str = match eol {
        Some("crlf") => {
            converted = text.replace('\n', "\r\n");
            &converted
        }
        Some("cr") => {
            converted = text.replace('\n', "\r");
            &converted
        }
        _ => text,
    };
    let mut out = Vec::with_capacity(text.len() + 3);
    if enc == UTF_16LE || enc == UTF_16BE {
        let le = enc == UTF_16LE;
        if bom {
            out.extend_from_slice(if le { &[0xFF, 0xFE] } else { &[0xFE, 0xFF] });
        }
        for u in text.encode_utf16() {
            out.extend_from_slice(&if le { u.to_le_bytes() } else { u.to_be_bytes() });
        }
        return Ok(out);
    }
    if enc == UTF_8 {
        if bom {
            out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        }
        out.extend_from_slice(text.as_bytes());
        return Ok(out);
    }
    let (bytes, _, unmappable) = enc.encode(text);
    if unmappable {
        // 找出第一个表示不了的字符，告诉用户
        let ch = text.chars().find(|c| enc.encode(c.encode_utf8(&mut [0; 4])).2).map(String::from).unwrap_or_default();
        return Err(LocalError::new("ENCODING_UNMAPPABLE").with("encoding", enc.name()).with("char", ch));
    }
    out.extend_from_slice(&bytes);
    Ok(out)
}

/// 保存。`if_mtime`：打开时的修改时间，对不上（期间被改过）报 FILE_CHANGED；`create`：新建，已经有了报 FILE_EXISTS。
/// 原地写（保留权限、属主、硬链接）。返回新的修改时间。
pub fn write(path: &str, content: &str, if_mtime: Option<i64>, encoding: Option<&str>, eol: Option<&str>, bom: bool, create: bool) -> R<i64> {
    let p = absolute(path)?;
    let bytes = encode(content, encoding, eol, bom)?;
    if bytes.len() as u64 > EDIT_MAX {
        return Err(LocalError::new("FILE_TOO_BIG").with("max", EDIT_MAX));
    }
    if create {
        use std::io::Write;
        let mut f = std::fs::File::options().write(true).create_new(true).open(&p).map_err(|e| io_err(e, &p))?;
        f.write_all(&bytes).map_err(|e| io_err(e, &p))?;
    } else {
        let m = std::fs::metadata(&p).map_err(|e| io_err(e, &p))?;
        if !m.is_file() {
            return Err(LocalError::new("NOT_A_FILE").with("path", path));
        }
        if m.len() > EDIT_MAX {
            return Err(LocalError::new("FILE_TOO_BIG").with("max", EDIT_MAX));
        }
        if let Some(want) = if_mtime {
            let now = mtime_ms(&m);
            if want != now {
                return Err(LocalError::new("FILE_CHANGED").with("mtime_ms", now));
            }
        }
        std::fs::write(&p, &bytes).map_err(|e| io_err(e, &p))?;
    }
    let m = std::fs::metadata(&p).map_err(|e| io_err(e, &p))?;
    Ok(mtime_ms(&m))
}

#[derive(Debug, Serialize)]
pub struct Stat {
    pub exists: bool,
    pub dir: bool,
    pub size: u64,
    pub mtime_ms: i64,
}

pub fn stat(path: &str) -> R<Stat> {
    let p = absolute(path)?;
    Ok(match std::fs::metadata(&p) {
        Ok(m) => Stat { exists: true, dir: m.is_dir(), size: m.len(), mtime_ms: mtime_ms(&m) },
        Err(e) if e.kind() == ErrorKind::NotFound => Stat { exists: false, dir: false, size: 0, mtime_ms: 0 },
        Err(e) => return Err(io_err(e, &p)),
    })
}

/// 一次最多问这么多个
pub const EXISTS_MAX: usize = 200;

/// 对话里认出来的路径哪些真的存在（§4.2）。相对路径按 `base` 解析，`~/` 按主目录。
/// 只回答在不在、是不是文件夹和解析后的绝对路径，不给内容。
pub fn exists(paths: &[String], base: Option<&str>) -> Vec<serde_json::Value> {
    let home = dirs::home_dir();
    paths
        .iter()
        .take(EXISTS_MAX)
        .filter_map(|raw| {
            let s = raw.trim();
            if s.is_empty() || s.len() > 4096 || s.contains('\0') {
                return None;
            }
            let p = if let Some(rest) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
                home.as_ref()?.join(rest)
            } else if s == "~" {
                home.clone()?
            } else {
                let p = PathBuf::from(s);
                if p.is_absolute() {
                    p
                } else {
                    PathBuf::from(base.filter(|b| Path::new(b).is_absolute())?).join(p)
                }
            };
            let m = std::fs::metadata(&p).ok()?;
            let abs = normalize(&p);
            Some(serde_json::json!({ "input": raw, "path": abs.display().to_string(), "dir": m.is_dir() }))
        })
        .collect()
}

/// 去掉 `.`、`..`（不碰符号链接，和用户看到的路径一致）
fn normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(bytes: &[u8]) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        std::fs::write(&p, bytes).unwrap();
        let r = read(p.to_str().unwrap(), None, None, None, None).unwrap();
        // 浏览器里：统一成 \n，改一个字
        let edited = r.text.unwrap().replace("\r\n", "\n").replacen('b', "B", 1);
        let eol = if r.eol == Some("mixed") { r.eol_major } else { r.eol };
        write(p.to_str().unwrap(), &edited, Some(r.mtime_ms), r.encoding, eol, r.bom, false).unwrap();
        std::fs::read(&p).unwrap()
    }

    #[test]
    fn keeps_crlf_and_bom() {
        assert_eq!(roundtrip(b"\xEF\xBB\xBFa\r\nb\r\nc"), b"\xEF\xBB\xBFa\r\nB\r\nc");
        assert_eq!(roundtrip(b"a\nb\n"), b"a\nB\n");
        assert_eq!(roundtrip(b"a\rb\r"), b"a\rB\r");
    }

    #[test]
    fn keeps_gb18030() {
        let (src, _, _) = encoding_rs::GBK.encode("中文\r\nbanana\r\n");
        let out = roundtrip(&src);
        let (want, _, _) = encoding_rs::GBK.encode("中文\r\nBanana\r\n");
        assert_eq!(out, want.into_owned());
    }

    #[test]
    fn keeps_utf16le_bom() {
        let mut src = vec![0xFF, 0xFE];
        for u in "x\r\nbanana".encode_utf16() {
            src.extend_from_slice(&u.to_le_bytes());
        }
        let mut want = vec![0xFF, 0xFE];
        for u in "x\r\nBanana".encode_utf16() {
            want.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(roundtrip(&src), want);
    }

    #[test]
    fn detects() {
        assert_eq!(detect(b"hello", true).unwrap().encoding, UTF_8);
        assert!(detect(b"\x00\x01\x02\x03\xff\xfe\x00\x00\x10\x11\x12", true).is_none());
        let (gbk, _, _) = encoding_rs::GBK.encode("这是一段比较长的中文文字，用来测试编码检测是不是靠谱。日志里常见。");
        let d = detect(&gbk, true).unwrap();
        assert!(d.encoding == encoding_rs::GBK || d.encoding == encoding_rs::GB18030, "{}", d.encoding.name());
        let mut le = vec![];
        for u in "plain ascii text in utf16".encode_utf16() {
            le.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(detect(&le, true).unwrap().encoding, UTF_16LE);
    }

    #[test]
    fn unmappable_is_error() {
        let e = encode("中文 😀", Some("GBK"), None, false).unwrap_err();
        assert_eq!(e.code, "ENCODING_UNMAPPABLE");
        assert!(encode("中文 😀", Some("gb18030"), None, false).is_ok());
    }

    #[test]
    fn eol_kinds() {
        assert_eq!(eol_of("a\nb"), Some("lf"));
        assert_eq!(eol_of("a\r\nb"), Some("crlf"));
        assert_eq!(eol_of("a\r\nb\nc\r\n"), Some("mixed"));
        assert_eq!(eol_major("a\r\nb\nc\r\n"), "crlf");
        assert_eq!(eol_of("ab"), None);
    }

    #[test]
    fn reads_by_line_and_offset() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.log");
        let body: String = (1..=1000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&p, &body).unwrap();
        let ps = p.to_str().unwrap();
        let r = read(ps, None, Some(500), Some(200), None).unwrap();
        assert_eq!(r.line, Some(450));
        assert!(r.text.as_deref().unwrap().starts_with("line 450\n"), "{:?}", &r.text.as_deref().unwrap()[..20]);
        // 从中间某个字节开始：丢掉前面半行，行号接上
        let r = read(ps, Some(15), None, Some(30), None).unwrap();
        let t = r.text.unwrap();
        assert!(t.starts_with("line 4\n") && t.ends_with('\n'), "{t:?}");
        assert_eq!(r.line, Some(4));
        // 正好在行首：这一行不丢
        let r = read(ps, Some(14), None, Some(30), None).unwrap();
        assert!(r.text.unwrap().starts_with("line 3\n"));
        assert_eq!((r.offset, r.line), (14, Some(3)));
        // 末尾
        let r = read(ps, Some(r.size - 20), None, None, None).unwrap();
        assert!(r.text.unwrap().ends_with("line 1000\n"));
        assert_eq!(r.end, r.size);
    }

    #[test]
    fn create_does_not_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("new.md");
        let ps = p.to_str().unwrap();
        write(ps, "hi\n", None, None, None, false, true).unwrap();
        assert_eq!(write(ps, "again\n", None, None, None, false, true).unwrap_err().code, "FILE_EXISTS");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hi\n");
    }

    #[test]
    fn exists_resolves_relative() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "").unwrap();
        let base = dir.path().to_str().unwrap();
        let r = exists(&["src/a.rs".into(), "src".into(), "nope.rs".into(), "./src/../src/a.rs".into()], Some(base));
        assert_eq!(r.len(), 3);
        assert_eq!(r[1]["dir"], true);
        assert_eq!(r[2]["path"], dir.path().join("src/a.rs").display().to_string());
    }
}
