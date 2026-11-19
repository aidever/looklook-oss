//! 归档解压（Node.js 工具链安装、插件包安装）。

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};

/// 解压 `.zip`、`.tar.gz` / `.tgz`、`.tar.xz` 到 `dest`（先解压到同级 `.tmp` 再原子改名）。
/// tar 包会去掉最外层目录；拒绝任何跳出目标目录的路径。
pub(crate) fn extract(file: &Path, dest: &Path) -> Result<()> {
    let tmp = dest.with_extension("tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = std::fs::remove_dir_all(dest);
    std::fs::create_dir_all(&tmp)?;
    let f = std::fs::File::open(file)?;
    let name = file.to_string_lossy();
    if name.ends_with(".zip") {
        extract_zip(f, &tmp)?;
        std::fs::rename(&tmp, dest)?;
        return Ok(());
    }
    let reader: Box<dyn Read> = if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        Box::new(flate2::read::GzDecoder::new(f))
    } else if name.ends_with(".tar.xz") {
        xz_reader(f)?
    } else {
        bail!("unknown archive");
    };
    let mut ar = tar::Archive::new(reader);
    ar.set_preserve_permissions(true);
    for e in ar.entries()? {
        let mut e = e?;
        let p = e.path()?.into_owned();
        // 去掉最外层目录；不允许跳出目标目录
        let rel: PathBuf = p.components().skip(1).collect();
        if rel.as_os_str().is_empty() || rel.components().any(|c| !matches!(c, Component::Normal(_))) {
            continue;
        }
        let out = tmp.join(&rel);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        e.unpack(&out)?;
    }
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

/// 解压插件包（`.tar.gz`，不剥离顶层目录）：只接受普通文件与目录，拒绝符号链接、硬链接、设备文件与越界路径。
pub(crate) fn extract_plugin_tar_gz(file: &Path, dest: &Path) -> Result<()> {
    let f = std::fs::File::open(file)?;
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(f));
    for e in ar.entries()? {
        let mut e = e?;
        let kind = e.header().entry_type();
        let raw = e.path()?.into_owned();
        let rel: PathBuf = raw
            .components()
            .filter(|c| !matches!(c, Component::CurDir))
            .collect();
        if rel.as_os_str().is_empty() {
            continue;
        }
        if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
            bail!("插件包包含非法路径: {}", raw.display());
        }
        let out = dest.join(&rel);
        if kind.is_dir() {
            std::fs::create_dir_all(&out)?;
        } else if kind.is_file() {
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut w = std::fs::File::create(&out)?;
            std::io::copy(&mut e, &mut w)?;
        } else {
            bail!("插件包包含不支持的条目类型: {}", raw.display());
        }
    }
    Ok(())
}

/// zip 包（没有最外层目录）；保留可执行权限，不允许跳出目标目录
fn extract_zip(f: std::fs::File, tmp: &Path) -> Result<()> {
    let mut z = zip::ZipArchive::new(f).context("zip")?;
    for i in 0..z.len() {
        let mut e = z.by_index(i)?;
        let Some(rel) = e.enclosed_name() else { continue };
        let out = tmp.join(rel);
        if e.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        #[cfg(unix)]
        if e.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000) {
            let mut target = String::new();
            e.read_to_string(&mut target)?;
            if !target.starts_with('/') && !target.split('/').any(|c| c == "..") {
                std::os::unix::fs::symlink(target, &out)?;
            }
            continue;
        }
        let mut w = std::fs::File::create(&out)?;
        std::io::copy(&mut e, &mut w)?;
        #[cfg(unix)]
        if let Some(m) = e.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&out, std::fs::Permissions::from_mode(m & 0o777))?;
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn xz_reader(f: std::fs::File) -> Result<Box<dyn Read>> {
    Ok(Box::new(xz2::read::XzDecoder::new(f)))
}

#[cfg(not(target_os = "linux"))]
fn xz_reader(_f: std::fs::File) -> Result<Box<dyn Read>> {
    bail!("unsupported")
}
