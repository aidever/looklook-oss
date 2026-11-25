//! 文件查看器、文件页用的几个只读接口（docs/FILES_REDO_DESIGN_2026-11-24.md §4.4、§4.6、§5.3）：
//!
//! - HTML 预览的静态目录：`POST /fs/site {path}` 发一个绑定这个文件所在文件夹的令牌（10 分钟，用一次续一次），
//!   `GET /fs/site/{token}/{*rest}` 只读地给出那个文件夹以下的文件（不跟随指到外面的符号链接）。页面在不同源的
//!   沙盒 iframe 里跑（界面那边不给 `allow-same-origin`），响应头再加一层 CSP 沙盒、不许连网。
//! - 缩略图：`GET /fs/thumb?path&size`，图片用 `image` 缩，视频有 ffmpeg 时截一帧；缓存在数据目录 `thumbs/`，
//!   按路径 + 修改时间 + 大小，总量超过 300 MB 删最久没用的。
//! - 压缩包：列出 zip / tar / tar.gz 里的文件（最多 5000 项），取里面的单个文件，解压到旁边的新文件夹。
//! - 二进制：前 64 KiB 原样给出，附上按内容猜的类型（十六进制查看）。

use std::collections::HashMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::header;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::error::LocalError;

type R<T> = Result<T, LocalError>;

fn absolute(path: &str) -> R<PathBuf> {
    let p = PathBuf::from(path.trim());
    if path.trim().is_empty() || !p.is_absolute() {
        return Err(LocalError::invalid("path", "absolute"));
    }
    Ok(p)
}

fn io_err(e: std::io::Error, path: &Path) -> LocalError {
    let p = path.display().to_string();
    match e.kind() {
        std::io::ErrorKind::NotFound => LocalError::not_found().with("path", p),
        std::io::ErrorKind::PermissionDenied => LocalError::new("FORBIDDEN").with("path", p),
        _ => LocalError::from(e),
    }
}

// ---------------- HTML 预览的静态目录 ----------------

const SITE_TTL: Duration = Duration::from_secs(600);
const SITE_MAX: usize = 64;
/// 单个文件最多给这么大
const SITE_FILE_MAX: u64 = 50 * 1024 * 1024;

static SITES: Mutex<Option<HashMap<String, (PathBuf, Instant)>>> = Mutex::new(None);

/// 发令牌：绑定 `path`（一个 .html 文件）所在的文件夹。返回 `{ token, name }`，界面用 `/api/fs/site/{token}/{name}` 打开
pub fn site_open(path: &str) -> R<Value> {
    let p = absolute(path)?;
    let m = std::fs::metadata(&p).map_err(|e| io_err(e, &p))?;
    if !m.is_file() {
        return Err(LocalError::new("NOT_A_FILE").with("path", path));
    }
    let dir = p.parent().ok_or_else(|| LocalError::invalid("path", "file"))?.canonicalize().map_err(|e| io_err(e, &p))?;
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let token = data_encoding::HEXLOWER.encode(&rand::random::<[u8; 16]>());
    let mut g = SITES.lock().unwrap_or_else(|e| e.into_inner());
    let map = g.get_or_insert_with(HashMap::new);
    let now = Instant::now();
    map.retain(|_, (_, at)| now.duration_since(*at) < SITE_TTL);
    if map.len() >= SITE_MAX {
        if let Some(oldest) = map.iter().min_by_key(|(_, (_, at))| *at).map(|(k, _)| k.clone()) {
            map.remove(&oldest);
        }
    }
    map.insert(token.clone(), (dir, now));
    Ok(json!({ "token": token, "name": name }))
}

/// 令牌对应的文件夹（用一次续 10 分钟）
fn site_dir(token: &str) -> Option<PathBuf> {
    let mut g = SITES.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, at) = g.as_mut()?.get_mut(token)?;
    if at.elapsed() >= SITE_TTL {
        return None;
    }
    *at = Instant::now();
    Some(dir.clone())
}

/// 预览页面的 CSP：脚本能跑（很多 AI 做的页面要），但不能连网、不能提交表单、不能当顶层页面；
/// 外部的 CDN 脚本 / 样式 / 图片可以加载（不然很多页面是空白），这和直接双击打开这个 HTML 一样
const SITE_CSP: &str = "sandbox allow-scripts allow-popups allow-modals allow-forms; default-src 'self' https: data: blob: 'unsafe-inline' 'unsafe-eval'; \
connect-src 'none'; form-action 'none'; frame-ancestors 'self'; base-uri 'self'";

pub async fn site_file(token: &str, rest: &str) -> R<Response> {
    let dir = site_dir(token).ok_or_else(|| LocalError::new("SITE_EXPIRED"))?;
    // axum 的 Path 已经解过百分号编码
    let mut p = dir.clone();
    for c in Path::new(rest).components() {
        match c {
            Component::Normal(s) => p.push(s),
            Component::CurDir => {}
            // 不许往上、不许绝对路径
            _ => return Err(LocalError::not_found()),
        }
    }
    let real = tokio::fs::canonicalize(&p).await.map_err(|_| LocalError::not_found())?;
    if !real.starts_with(&dir) {
        return Err(LocalError::not_found());
    }
    let m = tokio::fs::metadata(&real).await.map_err(|_| LocalError::not_found())?;
    if !m.is_file() || m.len() > SITE_FILE_MAX {
        return Err(LocalError::not_found());
    }
    let bytes = tokio::fs::read(&real).await.map_err(|e| io_err(e, &real))?;
    let mime = mime_guess::from_path(&real).first_or_octet_stream();
    let ctype = if mime.type_() == "text" || mime.subtype() == "javascript" || mime.subtype() == "json" { format!("{mime}; charset=utf-8") } else { mime.to_string() };
    Ok((
        [
            (header::CONTENT_TYPE, ctype),
            (header::CONTENT_SECURITY_POLICY, SITE_CSP.to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (header::CACHE_CONTROL, "no-store".to_string()),
            (header::REFERRER_POLICY, "no-referrer".to_string()),
        ],
        bytes,
    )
        .into_response())
}

// ---------------- 缩略图 ----------------

const THUMB_CACHE_MAX: u64 = 300 * 1024 * 1024;
/// 原图太大就不缩了（解码要占很多内存）
const THUMB_SRC_MAX: u64 = 80 * 1024 * 1024;
static THUMB_WRITES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

const IMAGE_EXT: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];
const VIDEO_EXT: &[&str] = &["mp4", "m4v", "webm", "mov", "mkv", "ogv"];

pub fn thumb_dir(home: &Path) -> PathBuf {
    home.join("thumbs")
}

/// 缩略图（JPEG）。图片、视频以外 / 做不出来时 404，界面照常显示类型图标
pub async fn thumb(home: &Path, path: &str, size: u32) -> R<Response> {
    let p = absolute(path)?;
    let size = if size > 128 { 256 } else { 128 };
    let m = tokio::fs::metadata(&p).await.map_err(|e| io_err(e, &p))?;
    if !m.is_file() || m.len() > THUMB_SRC_MAX {
        return Err(LocalError::not_found());
    }
    let ext = p.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    let video = VIDEO_EXT.contains(&ext.as_str());
    if !video && !IMAGE_EXT.contains(&ext.as_str()) {
        return Err(LocalError::not_found());
    }
    let key = data_encoding::HEXLOWER.encode(&Sha256::digest(format!("{}|{}|{}|{size}", p.display(), crate::textfile::mtime_ms(&m), m.len())));
    let dir = thumb_dir(home);
    let file = dir.join(format!("{}.jpg", &key[..32]));
    let bytes = match tokio::fs::read(&file).await {
        Ok(b) => b,
        Err(_) => {
            let src = p.clone();
            let out = file.clone();
            let made = tokio::task::spawn_blocking(move || -> Option<Vec<u8>> {
                let bytes = if video { video_frame(&src, size)? } else { scale(&std::fs::read(&src).ok()?, size)? };
                let _ = std::fs::create_dir_all(out.parent()?);
                let _ = std::fs::write(&out, &bytes);
                if THUMB_WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 100 == 0 {
                    prune(out.parent()?, THUMB_CACHE_MAX);
                }
                Some(bytes)
            })
            .await
            .ok()
            .flatten();
            made.ok_or_else(LocalError::not_found)?
        }
    };
    Ok(([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "private, max-age=86400")], bytes).into_response())
}

fn scale(bytes: &[u8], size: u32) -> Option<Vec<u8>> {
    let img = image::load_from_memory(bytes).ok()?;
    let t = img.thumbnail(size, size).to_rgb8();
    let mut out = std::io::Cursor::new(Vec::new());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 78).encode_image(&t).ok()?;
    Some(out.into_inner())
}

/// 视频截一帧（有 ffmpeg 才做）
fn video_frame(src: &Path, size: u32) -> Option<Vec<u8>> {
    let ffmpeg = crate::shells::which("ffmpeg")?;
    let tmp = tempfile::Builder::new().suffix(".jpg").tempfile().ok()?;
    let scale = format!("scale='min({size},iw)':-2");
    let mut cmd = std::process::Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-ss", "1", "-i"]).arg(src).args(["-frames:v", "1", "-vf", &scale]).arg(tmp.path());
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd.spawn().ok()?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match child.try_wait() {
            Ok(Some(st)) if st.success() => break,
            Ok(Some(_)) => return None,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                return None;
            }
        }
    }
    let b = std::fs::read(tmp.path()).ok()?;
    (!b.is_empty()).then_some(b)
}

/// 缓存超过上限：删最久没改过的，删到上限的八成
fn prune(dir: &Path, max: u64) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> =
        rd.flatten().filter_map(|e| e.metadata().ok().filter(|m| m.is_file()).map(|m| (m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len(), e.path()))).collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total <= max {
        return;
    }
    files.sort_by_key(|f| f.0);
    for (_, len, path) in files {
        if total <= max / 10 * 8 {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= len;
        }
    }
}

// ---------------- 压缩包 ----------------

const ARCHIVE_LIST_MAX: usize = 5000;
/// 取里面单个文件的上限
const ARCHIVE_ENTRY_MAX: u64 = 50 * 1024 * 1024;
/// 解压总量上限（防止解压炸弹）
const EXTRACT_MAX: u64 = 4 * 1024 * 1024 * 1024;

#[derive(Debug, Serialize)]
pub struct ArchiveEntry {
    pub name: String,
    pub size: u64,
    pub dir: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Zip,
    Tar,
    TarGz,
}

fn kind(path: &Path) -> Option<Kind> {
    let n = path.file_name()?.to_string_lossy().to_ascii_lowercase();
    if n.ends_with(".zip") || n.ends_with(".jar") || n.ends_with(".apk") || n.ends_with(".whl") {
        Some(Kind::Zip)
    } else if n.ends_with(".tar.gz") || n.ends_with(".tgz") {
        Some(Kind::TarGz)
    } else if n.ends_with(".tar") {
        Some(Kind::Tar)
    } else {
        None
    }
}

fn tar_entries(p: &Path, gz: bool) -> R<tar::Archive<Box<dyn Read>>> {
    let f = std::fs::File::open(p).map_err(|e| io_err(e, p))?;
    let r: Box<dyn Read> = if gz { Box::new(flate2::read::GzDecoder::new(std::io::BufReader::new(f))) } else { Box::new(std::io::BufReader::new(f)) };
    Ok(tar::Archive::new(r))
}

fn bad_archive<E: std::fmt::Display>(e: E) -> LocalError {
    LocalError::new("ARCHIVE_BAD").with("detail", e.to_string())
}

/// 里面的文件（最多 5000 项）。返回 `{ entries, total, truncated }`
pub fn archive_list(path: &str) -> R<Value> {
    let p = absolute(path)?;
    let k = kind(&p).ok_or_else(|| LocalError::new("ARCHIVE_UNSUPPORTED"))?;
    let mut out = Vec::new();
    let mut total = 0usize;
    match k {
        Kind::Zip => {
            let f = std::fs::File::open(&p).map_err(|e| io_err(e, &p))?;
            let mut z = zip::ZipArchive::new(f).map_err(bad_archive)?;
            total = z.len();
            for i in 0..z.len().min(ARCHIVE_LIST_MAX) {
                let e = z.by_index_raw(i).map_err(bad_archive)?;
                out.push(ArchiveEntry { name: e.name().to_string(), size: e.size(), dir: e.is_dir() });
            }
        }
        Kind::Tar | Kind::TarGz => {
            let mut ar = tar_entries(&p, k == Kind::TarGz)?;
            for e in ar.entries().map_err(bad_archive)? {
                let e = e.map_err(bad_archive)?;
                total += 1;
                if out.len() < ARCHIVE_LIST_MAX {
                    let name = e.path().map_err(bad_archive)?.to_string_lossy().into_owned();
                    out.push(ArchiveEntry { size: e.size(), dir: e.header().entry_type().is_dir(), name });
                }
            }
        }
    }
    Ok(json!({ "entries": out, "total": total, "truncated": total > out.len() }))
}

/// 里面的一个文件（预览用），最多 50 MiB
pub fn archive_read(path: &str, entry: &str) -> R<(String, Vec<u8>)> {
    let p = absolute(path)?;
    let k = kind(&p).ok_or_else(|| LocalError::new("ARCHIVE_UNSUPPORTED"))?;
    let mut buf = Vec::new();
    match k {
        Kind::Zip => {
            let f = std::fs::File::open(&p).map_err(|e| io_err(e, &p))?;
            let mut z = zip::ZipArchive::new(f).map_err(bad_archive)?;
            let e = z.by_name(entry).map_err(|_| LocalError::not_found())?;
            if e.size() > ARCHIVE_ENTRY_MAX {
                return Err(LocalError::new("FILE_TOO_BIG").with("max", ARCHIVE_ENTRY_MAX));
            }
            e.take(ARCHIVE_ENTRY_MAX).read_to_end(&mut buf).map_err(bad_archive)?;
        }
        Kind::Tar | Kind::TarGz => {
            let mut ar = tar_entries(&p, k == Kind::TarGz)?;
            let mut found = false;
            for e in ar.entries().map_err(bad_archive)? {
                let e = e.map_err(bad_archive)?;
                if e.path().map_err(bad_archive)?.to_string_lossy() == entry {
                    if e.size() > ARCHIVE_ENTRY_MAX {
                        return Err(LocalError::new("FILE_TOO_BIG").with("max", ARCHIVE_ENTRY_MAX));
                    }
                    e.take(ARCHIVE_ENTRY_MAX).read_to_end(&mut buf).map_err(bad_archive)?;
                    found = true;
                    break;
                }
            }
            if !found {
                return Err(LocalError::not_found());
            }
        }
    }
    let name = entry.rsplit('/').next().unwrap_or(entry).to_string();
    Ok((name, buf))
}

/// 包里的路径 → 解压目标下的安全相对路径（去掉开头的 `/`，有 `..` 的跳过）
fn safe_rel(name: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in Path::new(name).components() {
        match c {
            Component::Normal(s) => out.push(s),
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
            Component::ParentDir => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// 解压到压缩包旁边的新文件夹（`名字`，重名加序号），返回这个文件夹。不覆盖任何已有文件
pub fn archive_extract(path: &str) -> R<String> {
    let p = absolute(path)?;
    let k = kind(&p).ok_or_else(|| LocalError::new("ARCHIVE_UNSUPPORTED"))?;
    let parent = p.parent().ok_or_else(|| LocalError::invalid("path", "file"))?;
    let file = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let lower = file.to_ascii_lowercase();
    let stem = [".tar.gz", ".tgz", ".tar", ".zip", ".jar", ".apk", ".whl"].iter().find_map(|s| lower.ends_with(s).then(|| &file[..file.len() - s.len()])).unwrap_or(&file);
    let mut dest = parent.join(stem);
    let mut n = 2;
    while dest.exists() {
        dest = parent.join(format!("{stem} ({n})"));
        n += 1;
    }
    std::fs::create_dir(&dest).map_err(|e| io_err(e, &dest))?;
    let mut written = 0u64;
    let mut write = |rel: &Path, dir: bool, r: &mut dyn Read| -> R<()> {
        let target = dest.join(rel);
        if dir {
            return std::fs::create_dir_all(&target).map_err(|e| io_err(e, &target));
        }
        if let Some(d) = target.parent() {
            std::fs::create_dir_all(d).map_err(|e| io_err(e, d))?;
        }
        let mut f = std::fs::File::options().write(true).create_new(true).open(&target).map_err(|e| io_err(e, &target))?;
        let n = std::io::copy(&mut r.take(EXTRACT_MAX - written + 1), &mut f).map_err(LocalError::from)?;
        written += n;
        if written > EXTRACT_MAX {
            return Err(LocalError::new("FILE_TOO_BIG").with("max", EXTRACT_MAX));
        }
        Ok(())
    };
    let r = (|| -> R<()> {
        match k {
            Kind::Zip => {
                let f = std::fs::File::open(&p).map_err(|e| io_err(e, &p))?;
                let mut z = zip::ZipArchive::new(f).map_err(bad_archive)?;
                for i in 0..z.len() {
                    let mut e = z.by_index(i).map_err(bad_archive)?;
                    // 符号链接不解出来（可能指到外面）
                    if e.is_symlink() {
                        continue;
                    }
                    let Some(rel) = safe_rel(e.name()) else { continue };
                    let dir = e.is_dir();
                    write(&rel, dir, &mut e)?;
                }
            }
            Kind::Tar | Kind::TarGz => {
                let mut ar = tar_entries(&p, k == Kind::TarGz)?;
                for e in ar.entries().map_err(bad_archive)? {
                    let mut e = e.map_err(bad_archive)?;
                    let t = e.header().entry_type();
                    if !(t.is_file() || t.is_dir()) {
                        continue;
                    }
                    let Some(rel) = safe_rel(&e.path().map_err(bad_archive)?.to_string_lossy()) else { continue };
                    write(&rel, t.is_dir(), &mut e)?;
                }
            }
        }
        Ok(())
    })();
    if let Err(e) = r {
        // 解了一半出错：留下的东西删掉，不要半截
        let _ = std::fs::remove_dir_all(&dest);
        return Err(e);
    }
    Ok(dest.display().to_string())
}

// ---------------- 二进制 ----------------

const HEX_MAX: u64 = 64 * 1024;

/// 前 64 KiB 和猜出来的类型（`x-looklook-kind`：MIME，`x-looklook-ext`：常见扩展名）
pub fn head(path: &str) -> R<Response> {
    let p = absolute(path)?;
    let m = std::fs::metadata(&p).map_err(|e| io_err(e, &p))?;
    if !m.is_file() {
        return Err(LocalError::new("NOT_A_FILE").with("path", path));
    }
    let mut buf = Vec::new();
    std::fs::File::open(&p).map_err(|e| io_err(e, &p))?.take(HEX_MAX).read_to_end(&mut buf).map_err(|e| io_err(e, &p))?;
    let t = infer::get(&buf);
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CACHE_CONTROL, "no-store".to_string()),
            (header::HeaderName::from_static("x-looklook-size"), m.len().to_string()),
            (header::HeaderName::from_static("x-looklook-kind"), t.map(|t| t.mime_type().to_string()).unwrap_or_default()),
            (header::HeaderName::from_static("x-looklook-ext"), t.map(|t| t.extension().to_string()).unwrap_or_default()),
        ],
        buf,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn safe_rel_blocks_escape() {
        assert_eq!(safe_rel("a/b.txt"), Some(PathBuf::from("a/b.txt")));
        assert_eq!(safe_rel("/etc/passwd"), Some(PathBuf::from("etc/passwd")));
        assert_eq!(safe_rel("../x"), None);
        assert_eq!(safe_rel("a/../../x"), None);
    }

    #[test]
    fn zip_list_read_extract() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("pack.zip");
        let mut w = zip::ZipWriter::new(std::fs::File::create(&p).unwrap());
        w.start_file("docs/a.md", zip::write::SimpleFileOptions::default()).unwrap();
        w.write_all(b"# hi").unwrap();
        w.start_file("../evil.txt", zip::write::SimpleFileOptions::default()).unwrap();
        w.write_all(b"x").unwrap();
        w.finish().unwrap();
        let ps = p.to_str().unwrap();
        let l = archive_list(ps).unwrap();
        assert_eq!(l["total"], 2);
        let (name, bytes) = archive_read(ps, "docs/a.md").unwrap();
        assert_eq!((name.as_str(), bytes.as_slice()), ("a.md", b"# hi".as_slice()));
        let out = archive_extract(ps).unwrap();
        assert_eq!(std::fs::read_to_string(Path::new(&out).join("docs/a.md")).unwrap(), "# hi");
        assert!(!dir.path().join("evil.txt").exists());
        // 再解一次：新文件夹，不覆盖
        let out2 = archive_extract(ps).unwrap();
        assert!(out2.ends_with("pack (2)"), "{out2}");
    }

    #[test]
    fn tar_gz_list() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.tar.gz");
        let gz = flate2::write::GzEncoder::new(std::fs::File::create(&p).unwrap(), flate2::Compression::fast());
        let mut b = tar::Builder::new(gz);
        let mut h = tar::Header::new_gnu();
        h.set_size(3);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, "x/y.txt", &b"abc"[..]).unwrap();
        b.into_inner().unwrap().finish().unwrap();
        let ps = p.to_str().unwrap();
        assert_eq!(archive_list(ps).unwrap()["entries"][0]["name"], "x/y.txt");
        assert_eq!(archive_read(ps, "x/y.txt").unwrap().1, b"abc");
    }

    #[tokio::test]
    async fn site_stays_inside() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("site");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("index.html"), "<p>hi</p>").unwrap();
        std::fs::write(dir.path().join("secret.txt"), "no").unwrap();
        let t = site_open(root.join("index.html").to_str().unwrap()).unwrap();
        let token = t["token"].as_str().unwrap();
        let r = site_file(token, "index.html").await.unwrap();
        assert!(r.headers()[header::CONTENT_SECURITY_POLICY].to_str().unwrap().contains("connect-src 'none'"));
        assert!(site_file(token, "../secret.txt").await.is_err());
        assert!(site_file(token, "%2e%2e/secret.txt").await.is_err());
        assert!(site_file("nope", "index.html").await.is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join("secret.txt"), root.join("link.txt")).unwrap();
            assert!(site_file(token, "link.txt").await.is_err(), "符号链接指到外面");
        }
    }

    #[test]
    fn thumb_scales_png() {
        let img = image::RgbImage::from_pixel(600, 300, image::Rgb([200, 10, 10]));
        let mut png = std::io::Cursor::new(Vec::new());
        img.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let jpg = scale(png.get_ref(), 128).unwrap();
        let t = image::load_from_memory(&jpg).unwrap();
        assert_eq!((t.width(), t.height()), (128, 64));
    }
}
