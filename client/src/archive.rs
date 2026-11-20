//! 解压下载的工具包（`.tar.gz` / `.tgz` / `.tar.xz` / `.zip`）：一键安装 Node.js 用（tools.rs）。
//! tar 包去掉最外层目录；路径不能跳出目标目录；保留可执行权限。先解到 `<目标>.tmp`，完整解完再换过去。

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

pub fn extract(file: &Path, dest: &Path) -> Result<()> {
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
        if rel.as_os_str().is_empty() || rel.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
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
            // 符号链接：目标写在内容里
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_tar_gz_stripping_top_dir() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.tar.gz");
        {
            let f = std::fs::File::create(&file).unwrap();
            let gz = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
            let mut b = tar::Builder::new(gz);
            let data = b"hi";
            let mut h = tar::Header::new_gnu();
            h.set_size(2);
            h.set_mode(0o755);
            h.set_cksum();
            b.append_data(&mut h, "top/bin/x", &data[..]).unwrap();
            b.into_inner().unwrap().finish().unwrap();
        }
        let dest = dir.path().join("out");
        extract(&file, &dest).unwrap();
        assert_eq!(std::fs::read(dest.join("bin/x")).unwrap(), b"hi");
    }

    #[test]
    fn extracts_zip_keeping_modes() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.zip");
        {
            let mut z = zip::ZipWriter::new(std::fs::File::create(&file).unwrap());
            let opts = zip::write::SimpleFileOptions::default().unix_permissions(0o755);
            z.add_directory("fonts/", opts).unwrap();
            z.start_file("tool-bin", opts).unwrap();
            z.write_all(b"bin").unwrap();
            z.start_file("fonts/a.ttf", zip::write::SimpleFileOptions::default().unix_permissions(0o644)).unwrap();
            z.write_all(b"ttf").unwrap();
            z.finish().unwrap();
        }
        let dest = dir.path().join("out");
        extract(&file, &dest).unwrap();
        assert_eq!(std::fs::read(dest.join("tool-bin")).unwrap(), b"bin");
        assert_eq!(std::fs::read(dest.join("fonts/a.ttf")).unwrap(), b"ttf");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dest.join("tool-bin")).unwrap().permissions().mode() & 0o777, 0o755);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn extracts_tar_xz() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.tar.xz");
        {
            let f = std::fs::File::create(&file).unwrap();
            let xz = xz2::write::XzEncoder::new(f, 1);
            let mut b = tar::Builder::new(xz);
            let mut h = tar::Header::new_gnu();
            h.set_size(3);
            h.set_mode(0o755);
            h.set_cksum();
            b.append_data(&mut h, "ungoogled/chrome", &b"bin"[..]).unwrap();
            b.into_inner().unwrap().finish().unwrap();
        }
        let dest = dir.path().join("out");
        extract(&file, &dest).unwrap();
        assert_eq!(std::fs::read(dest.join("chrome")).unwrap(), b"bin");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(dest.join("chrome")).unwrap().permissions().mode() & 0o777, 0o755);
    }
}
