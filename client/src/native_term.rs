//! 用这台电脑上装的终端（Ghostty、iTerm2、Windows Terminal……）打开会话（设计稿 §8.1、§8.2）。
//!
//! 窗口里跑的是 `looklook attach <id>`（src/attach）：连本机看看，画法见 attach/render.rs。
//! 只在管理台从这台电脑本身打开时提供（窗口弹在这台电脑上，人得在它跟前）。会话在同一账户的另一台电脑上时，
//! 窗口里跑 `looklook attach <id> --device <那台电脑>`，经本机主进程转过去。

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct NativeTerm {
    /// 设置里存的名字
    pub id: &'static str,
    /// 菜单里显示的名字
    pub name: &'static str,
    #[serde(skip)]
    path: PathBuf,
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
}

/// 这台电脑上识别到的终端（顺序就是菜单里的顺序）
pub fn detect() -> Vec<NativeTerm> {
    if !crate::util::has_desktop() {
        return Vec::new();
    }
    let mut out = Vec::new();
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let apps: &[(&str, &str, &str)] = &[
            ("ghostty", "Ghostty", "Ghostty.app"),
            ("iterm2", "iTerm2", "iTerm.app"),
            ("wezterm", "WezTerm", "WezTerm.app"),
            ("kitty", "kitty", "kitty.app"),
            ("alacritty", "Alacritty", "Alacritty.app"),
        ];
        for (id, name, bundle) in apps {
            let found = [PathBuf::from("/Applications"), home.join("Applications")].into_iter().map(|d| d.join(bundle)).find(|p| p.is_dir());
            if let Some(path) = found {
                out.push(NativeTerm { id, name, path });
            }
        }
        let terminal = ["/System/Applications/Utilities/Terminal.app", "/Applications/Utilities/Terminal.app"].into_iter().map(PathBuf::from).find(|p| p.is_dir());
        if let Some(path) = terminal {
            out.push(NativeTerm { id: "terminal", name: "终端 / Terminal", path });
        }
    }
    #[cfg(windows)]
    {
        let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_default();
        if let Some(path) = which("wt.exe").or_else(|| Some(local.join("Microsoft").join("WindowsApps").join("wt.exe")).filter(|p| p.exists())) {
            out.push(NativeTerm { id: "wt", name: "Windows Terminal", path });
        }
        let pf = std::env::var_os("ProgramFiles").map(PathBuf::from).unwrap_or_default();
        if let Some(path) = which("wezterm-gui.exe").or_else(|| Some(pf.join("WezTerm").join("wezterm-gui.exe")).filter(|p| p.is_file())) {
            out.push(NativeTerm { id: "wezterm", name: "WezTerm", path });
        }
        out.push(NativeTerm { id: "console", name: "命令提示符窗口 / Console", path: PathBuf::new() });
    }
    #[cfg(target_os = "linux")]
    {
        let bins: &[(&str, &str, &str)] = &[
            ("ghostty", "Ghostty", "ghostty"),
            ("kitty", "kitty", "kitty"),
            ("wezterm", "WezTerm", "wezterm"),
            ("alacritty", "Alacritty", "alacritty"),
            ("gnome-terminal", "GNOME Terminal", "gnome-terminal"),
            ("konsole", "Konsole", "konsole"),
            ("x-terminal-emulator", "x-terminal-emulator", "x-terminal-emulator"),
        ];
        for (id, name, bin) in bins {
            if let Some(path) = which(bin) {
                out.push(NativeTerm { id, name, path });
            }
        }
    }
    let _ = which;
    out
}

/// 单引号包起来给 shell / AppleScript 里的 shell 用
#[cfg(any(target_os = "macos", test))]
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// 要在终端窗口里跑的命令（`device`：会话在另一台电脑上）
fn attach_argv(exe: &Path, home: &Path, id: &str, device: Option<&str>) -> Vec<String> {
    let mut v = vec![
        exe.to_string_lossy().into_owned(),
        "--home".into(),
        home.to_string_lossy().into_owned(),
        "attach".into(),
        id.into(),
    ];
    if let Some(d) = device {
        v.extend(["--device".into(), d.into()]);
    }
    v
}

/// 拼出拉起这个终端的命令（不运行）
pub fn command(term: &NativeTerm, argv: &[String]) -> Result<Command> {
    #[allow(unused_mut)]
    let mut cmd;
    match term.id {
        #[cfg(target_os = "macos")]
        "iterm2" | "terminal" => {
            let line = argv.iter().map(|a| sh_quote(a)).collect::<Vec<_>>().join(" ");
            // AppleScript 字符串里的 \ 和 " 要转义
            let line = line.replace('\\', "\\\\").replace('"', "\\\"");
            let script = if term.id == "iterm2" {
                format!("tell application \"iTerm\"\nactivate\ncreate window with default profile command \"{line}\"\nend tell")
            } else {
                format!("tell application \"Terminal\"\nactivate\ndo script \"{line}\"\nend tell")
            };
            cmd = Command::new("osascript");
            cmd.args(["-e", &script]);
        }
        #[cfg(target_os = "macos")]
        "wezterm" => {
            cmd = Command::new(term.path.join("Contents/MacOS/wezterm"));
            cmd.args(["start", "--"]).args(argv);
        }
        #[cfg(target_os = "macos")]
        "kitty" => {
            cmd = Command::new("open");
            cmd.arg("-na").arg(&term.path).arg("--args").args(argv);
        }
        #[cfg(target_os = "macos")]
        _ => {
            // Ghostty、Alacritty：-e 后面是命令
            cmd = Command::new("open");
            cmd.arg("-na").arg(&term.path).args(["--args", "-e"]).args(argv);
        }
        #[cfg(windows)]
        "wt" => {
            cmd = Command::new(&term.path);
            cmd.args(["new-tab", "--"]).args(argv);
        }
        #[cfg(windows)]
        "wezterm" => {
            cmd = Command::new(&term.path);
            cmd.args(["start", "--"]).args(argv);
        }
        #[cfg(windows)]
        _ => {
            // 系统控制台：新开一个控制台窗口直接跑
            use std::os::windows::process::CommandExt;
            const CREATE_NEW_CONSOLE: u32 = 0x10;
            cmd = Command::new(&argv[0]);
            cmd.args(&argv[1..]).creation_flags(CREATE_NEW_CONSOLE);
        }
        #[cfg(target_os = "linux")]
        "kitty" => {
            cmd = Command::new(&term.path);
            cmd.args(argv);
        }
        #[cfg(target_os = "linux")]
        "wezterm" => {
            cmd = Command::new(&term.path);
            cmd.args(["start", "--"]).args(argv);
        }
        #[cfg(target_os = "linux")]
        "gnome-terminal" => {
            cmd = Command::new(&term.path);
            cmd.arg("--").args(argv);
        }
        #[cfg(target_os = "linux")]
        _ => {
            cmd = Command::new(&term.path);
            cmd.arg("-e").args(argv);
        }
        #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
        _ => anyhow::bail!("unsupported"),
    }
    Ok(cmd)
}

/// 用 `term_id` 这个终端打开会话 `id`（`device`：会话在同一账户的另一台电脑上）
pub fn open(term_id: &str, home: &Path, id: &str, device: Option<&str>) -> Result<()> {
    let term = detect().into_iter().find(|t| t.id == term_id).context("NATIVE_TERM_NOT_FOUND")?;
    let exe = std::env::current_exe().context("current_exe")?;
    let mut cmd = command(&term, &attach_argv(&exe, home, id, device))?;
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().with_context(|| format!("启动 {} 失败", term.name))?;
    // 拉起的进程（open、osascript、终端本身）退出时收掉，不留僵尸进程
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_and_argv() {
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        let argv = attach_argv(Path::new("/opt/ll/looklook"), Path::new("/home/u/.ll"), "abc", None);
        assert_eq!(argv, ["/opt/ll/looklook", "--home", "/home/u/.ll", "attach", "abc"]);
        let argv = attach_argv(Path::new("/opt/ll/looklook"), Path::new("/home/u/.ll"), "abc", Some("dev-1"));
        assert_eq!(argv[4..], ["abc", "--device", "dev-1"]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_commands() {
        let argv: Vec<String> = ["ll", "attach", "x"].map(String::from).to_vec();
        let t = NativeTerm { id: "gnome-terminal", name: "", path: "/usr/bin/gnome-terminal".into() };
        let c = command(&t, &argv).unwrap();
        assert_eq!(c.get_args().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>(), ["--", "ll", "attach", "x"]);
        let t = NativeTerm { id: "alacritty", name: "", path: "/usr/bin/alacritty".into() };
        let c = command(&t, &argv).unwrap();
        assert_eq!(c.get_args().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>(), ["-e", "ll", "attach", "x"]);
    }
}
