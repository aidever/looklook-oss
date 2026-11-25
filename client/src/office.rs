//! 旧版 Office（doc / ppt）预览用的转换器 x2t：CryptPad 编译成 WebAssembly 的 OnlyOffice 转换器（AGPL-3.0，
//! <https://github.com/cryptpad/onlyoffice-x2t-wasm>）。浏览器在 Web Worker 里用它把 doc / ppt 转成 docx / pptx 再显示
//! （docs/FILES_REDO_DESIGN_2026-11-24.md §4.4.1）。
//!
//! 解压后 65 MB，不放进安装包：第一次有人预览旧格式时由电脑下载官方发布包（SHA-256 写死在这里），解出 `x2t.js` 和
//! `x2t.wasm`（各自再校验），wasm 存成 gzip（约 14 MB）放在 `<数据目录>/office/x2t-<版本>/`，浏览器经现有连接来取、自己解压。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::http::header;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::error::LocalError;
use crate::platform::Platform;

pub const VERSION: &str = "8.3.0-0";
const URL: &str = "https://github.com/cryptpad/onlyoffice-x2t-wasm/releases/download/v8.3.0%2B0/x2t.zip";
const ZIP_SHA256: &str = "6c28a5b98718dd4488a39b457b746126d2e9baa3ea3019c68797447cd6443e54";
const JS_SHA256: &str = "fe04bdfa43c1bb150bf0ec4f2a063893733a0b919801ba6054ad25b53635955e";
const WASM_SHA256: &str = "1f9d84763368d5ad31f5bb25cc99c4d883d9928c94cbc9af5b8b5f1e84a4bef7";
/// 发布包 23.5 MB，留点余量
const ZIP_MAX: u64 = 40 * 1024 * 1024;
/// 解出来的单个文件上限（wasm 65.6 MB）
const ENTRY_MAX: u64 = 100 * 1024 * 1024;

#[derive(Default)]
struct State {
    running: bool,
    done: u64,
    total: Option<u64>,
    error: Option<String>,
}

static STATE: Mutex<State> = Mutex::new(State { running: false, done: 0, total: None, error: None });

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn dir(home: &Path) -> PathBuf {
    home.join("office").join(format!("x2t-{VERSION}"))
}

fn ready(home: &Path) -> bool {
    let d = dir(home);
    d.join("x2t.js").is_file() && d.join("x2t.wasm.gz").is_file()
}

/// `{ version, ready, running, done, total, error }`
pub fn status(home: &Path) -> Value {
    let s = state();
    json!({ "version": VERSION, "ready": ready(home), "running": s.running, "done": s.done, "total": s.total, "error": s.error })
}

/// 开始下载（已经有了、正在下就什么也不做），立刻返回状态。
pub fn start(home: &Path, platform: Arc<Platform>) -> Value {
    {
        let mut s = state();
        if !s.running && !ready(home) {
            *s = State { running: true, ..State::default() };
            let home = home.to_path_buf();
            tokio::spawn(async move {
                let r = fetch(&home, &platform).await;
                let mut s = state();
                s.running = false;
                if let Err(e) = r {
                    tracing::warn!("下载 Office 转换器失败：{e:#}");
                    s.error = Some(format!("{e:#}"));
                }
            });
        }
    }
    status(home)
}

async fn fetch(home: &Path, platform: &Platform) -> anyhow::Result<()> {
    let root = home.join("office");
    tokio::fs::create_dir_all(&root).await?;
    let zip = root.join(format!("x2t-{VERSION}.zip.part"));
    let sha = platform
        .download(URL, &zip, ZIP_MAX, |done, total| {
            let mut s = state();
            s.done = done;
            s.total = total;
        })
        .await
        .map_err(|e| anyhow::anyhow!("{e}"));
    let sha = match sha {
        Ok(s) => s,
        Err(e) => {
            let _ = tokio::fs::remove_file(&zip).await;
            return Err(e);
        }
    };
    let home = home.to_path_buf();
    let r = tokio::task::spawn_blocking(move || {
        let r = if sha == ZIP_SHA256 { unpack(&zip, &home) } else { Err(anyhow::anyhow!("校验不对（{sha}）")) };
        let _ = std::fs::remove_file(&zip);
        r
    })
    .await?;
    r
}

/// 从校验过的发布包里取出两个文件、各自校验，写到临时目录再整个换到正式位置（不会留下半截）。
fn unpack(zip: &Path, home: &Path) -> anyhow::Result<()> {
    let mut ar = zip::ZipArchive::new(std::fs::File::open(zip)?)?;
    let read = |ar: &mut zip::ZipArchive<std::fs::File>, name: &str, sha: &str| -> anyhow::Result<Vec<u8>> {
        let f = ar.by_name(name)?;
        let mut buf = Vec::new();
        f.take(ENTRY_MAX + 1).read_to_end(&mut buf)?;
        anyhow::ensure!(buf.len() as u64 <= ENTRY_MAX, "{name} 太大");
        anyhow::ensure!(data_encoding::HEXLOWER.encode(&Sha256::digest(&buf)) == sha, "{name} 校验不对");
        Ok(buf)
    };
    let js = read(&mut ar, "x2t.js", JS_SHA256)?;
    let wasm = read(&mut ar, "x2t.wasm", WASM_SHA256)?;
    let fin = dir(home);
    let tmp = fin.with_extension("tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    std::fs::write(tmp.join("x2t.js"), js)?;
    let mut gz = flate2::write::GzEncoder::new(std::fs::File::create(tmp.join("x2t.wasm.gz"))?, flate2::Compression::best());
    gz.write_all(&wasm)?;
    gz.finish()?.sync_all()?;
    let _ = std::fs::remove_dir_all(&fin);
    std::fs::rename(&tmp, &fin)?;
    // 旧版本留下的目录
    if let Ok(rd) = std::fs::read_dir(home.join("office")) {
        for e in rd.flatten() {
            let p = e.path();
            if p != fin && e.file_name().to_string_lossy().starts_with("x2t-") {
                let _ = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
            }
        }
    }
    Ok(())
}

/// `x2t.js` / `x2t.wasm.gz`。wasm 是 gzip 原样给出（不加 Content-Encoding），浏览器自己解压：远程连接上不用再压一遍，
/// 也不依赖中间环节认不认 Content-Encoding。
pub async fn file(home: &Path, name: &str) -> Result<Response, LocalError> {
    let ctype = match name {
        "x2t.js" => "text/javascript; charset=utf-8",
        "x2t.wasm.gz" => "application/octet-stream",
        _ => return Err(LocalError::not_found()),
    };
    let bytes = tokio::fs::read(dir(home).join(name)).await.map_err(|_| LocalError::not_found())?;
    Ok(([(header::CONTENT_TYPE, ctype), (header::CACHE_CONTROL, "private, max-age=31536000, immutable")], bytes).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zip_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (n, b) in entries {
            w.start_file(*n, zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(b).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    #[test]
    fn unpack_rejects_wrong_entry_hash() {
        let home = tempfile::tempdir().unwrap();
        let z = home.path().join("x.zip");
        std::fs::write(&z, zip_with(&[("x2t.js", b"js"), ("x2t.wasm", b"wasm")])).unwrap();
        let e = unpack(&z, home.path()).unwrap_err();
        assert!(e.to_string().contains("x2t.js"), "{e}");
        assert!(!ready(home.path()));
        assert!(!dir(home.path()).with_extension("tmp").join("x2t.js").exists());
    }

    #[test]
    fn missing_entry_is_error() {
        let home = tempfile::tempdir().unwrap();
        let z = home.path().join("x.zip");
        std::fs::write(&z, zip_with(&[("other", b"x")])).unwrap();
        assert!(unpack(&z, home.path()).is_err());
        assert!(!ready(home.path()));
    }

    #[tokio::test]
    async fn file_only_serves_known_names() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir(home.path())).unwrap();
        std::fs::write(dir(home.path()).join("x2t.js"), "ok").unwrap();
        assert!(file(home.path(), "x2t.js").await.is_ok());
        assert!(file(home.path(), "../x2t.js").await.is_err());
        assert!(file(home.path(), "x2t.wasm.gz").await.is_err());
    }
}
