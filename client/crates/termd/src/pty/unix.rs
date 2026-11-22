//! Unix PTY spawning (`openpty` + `setsid` + `TIOCSCTTY`), pollable reader with wake pipe for
//! zero-loss `SCM_RIGHTS` handover, and `/proc` / `tcgetpgrp` process inspection.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use term_proto::CreateSessionReq;
use tokio::sync::mpsc::UnboundedSender;

use super::{ProcessInspect, PtyEvent};

struct ReaderControl {
    wake_write: OwnedFd,
    handle: JoinHandle<()>,
}

pub struct PlatformPty {
    master: OwnedFd,
    child_pid: u32,
    child: Mutex<Option<Child>>,
    reader: Mutex<Option<ReaderControl>>,
    write_lock: Mutex<()>,
}

impl PlatformPty {
    pub fn spawn(req: &CreateSessionReq, tx: UnboundedSender<PtyEvent>) -> Result<Self> {
        let mut master_raw: RawFd = -1;
        let mut slave_raw: RawFd = -1;
        let mut ws = libc::winsize {
            ws_row: req.rows.max(1),
            ws_col: req.cols.max(2),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        #[allow(clippy::unnecessary_mut_passed)]
        let rc = unsafe {
            libc::openpty(
                &mut master_raw,
                &mut slave_raw,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut ws,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).context("openpty");
        }
        let master = unsafe { OwnedFd::from_raw_fd(master_raw) };
        let slave = unsafe { OwnedFd::from_raw_fd(slave_raw) };
        set_cloexec(master.as_raw_fd())?;
        set_cloexec(slave.as_raw_fd())?;

        let slave_fd = slave.as_raw_fd();
        let mut cmd = Command::new(&req.cmd);
        cmd.args(&req.args);
        if !req.cwd.is_empty() {
            cmd.current_dir(&req.cwd);
        }

        // Strip tmux/outer agent markers and apply caller overrides (design §4.5).
        cmd.env_remove("TMUX").env_remove("TMUX_PANE");
        for k in &req.env_remove {
            cmd.env_remove(k);
        }
        // Always default to xterm-256color/truecolor regardless of parent daemon's TERM
        // (e.g. systemd/GUI runners often inherit TERM=dumb), while letting req.env override.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        for (k, v) in &req.env {
            if v.is_empty() && (k == "TMUX" || k == "TMUX_PANE") {
                cmd.env_remove(k);
            } else {
                cmd.env(k, v);
            }
        }

        // Keep inherited fd 0/1/2 duplicated from the PTY slave inside `pre_exec`.
        let devnull_in = Stdio::null();
        cmd.stdin(devnull_in).stdout(Stdio::null()).stderr(Stdio::null());

        unsafe {
            cmd.pre_exec(move || {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                libc::signal(libc::SIGQUIT, libc::SIG_DFL);
                libc::signal(libc::SIGTERM, libc::SIG_DFL);
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                libc::signal(libc::SIGCHLD, libc::SIG_DFL);

                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                for target in [0, 1, 2] {
                    if libc::dup2(slave_fd, target) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                if slave_fd > 2 {
                    libc::close(slave_fd);
                }
                Ok(())
            });
        }

        let child = cmd
            .spawn()
            .with_context(|| format!("spawn '{}' in pty", req.cmd))?;
        let child_pid = child.id();
        drop(slave);

        let pty = Self {
            master,
            child_pid,
            child: Mutex::new(Some(child)),
            reader: Mutex::new(None),
            write_lock: Mutex::new(()),
        };
        pty.start_reader(tx)?;
        Ok(pty)
    }

    /// Reconstruct a [`PlatformPty`] from an inherited `master_fd` received via `SCM_RIGHTS`.
    pub fn from_handover(
        master: OwnedFd,
        child_pid: u32,
        tx: UnboundedSender<PtyEvent>,
    ) -> Result<Self> {
        set_cloexec(master.as_raw_fd())?;
        let pty = Self {
            master,
            child_pid,
            child: Mutex::new(None),
            reader: Mutex::new(None),
            write_lock: Mutex::new(()),
        };
        pty.start_reader(tx)?;
        Ok(pty)
    }

    fn start_reader(&self, tx: UnboundedSender<PtyEvent>) -> Result<()> {
        let mut pipe_fds = [-1 as RawFd; 2];
        if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error()).context("wake pipe");
        }
        let wake_read = unsafe { OwnedFd::from_raw_fd(pipe_fds[0]) };
        let wake_write = unsafe { OwnedFd::from_raw_fd(pipe_fds[1]) };
        let _ = set_cloexec(wake_read.as_raw_fd());
        let _ = set_cloexec(wake_write.as_raw_fd());

        // Dup master for reader thread so closing/handing over `self.master` never causes use-after-close
        // races while reader unwinds cleanly.
        let read_master_raw = unsafe { libc::fcntl(self.master.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        if read_master_raw < 0 {
            return Err(std::io::Error::last_os_error()).context("dup master pty for reader");
        }
        let read_master = unsafe { OwnedFd::from_raw_fd(read_master_raw) };
        let _ = set_nonblocking(read_master.as_raw_fd(), true);

        let child_pid = self.child_pid;
        let handle = std::thread::Builder::new()
            .name(format!("pty-read-{child_pid}"))
            .spawn(move || {
                run_reader_loop(read_master, wake_read, child_pid, tx);
            })
            .context("spawn pty reader thread")?;

        *self.reader.lock().unwrap() = Some(ReaderControl { wake_write, handle });
        Ok(())
    }

    /// Stop the reader thread cleanly (draining any immediately buffered PTY bytes first) and
    /// duplicate the `master_fd` for `SCM_RIGHTS` transfer.
    pub fn pause_and_clone_fd_for_handover(&self) -> Result<OwnedFd> {
        let ctrl = self.reader.lock().unwrap().take();
        if let Some(ctrl) = ctrl {
            let wake = ctrl.wake_write;
            let _ = unsafe {
                let mut f = std::fs::File::from_raw_fd(wake.as_raw_fd());
                let res = f.write_all(&[1]);
                std::mem::forget(f);
                res
            };
            drop(wake);
            let _ = ctrl.handle.join();
        }
        let dup = unsafe { libc::fcntl(self.master.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        if dup < 0 {
            return Err(std::io::Error::last_os_error()).context("dup master fd for handover");
        }
        Ok(unsafe { OwnedFd::from_raw_fd(dup) })
    }

    pub fn write_all(&self, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let _guard = self.write_lock.lock().unwrap();
        let fd = self.master.as_raw_fd();
        let mut offset = 0usize;
        let deadline = Instant::now() + Duration::from_secs(5);
        while offset < bytes.len() {
            let rc = unsafe {
                libc::write(
                    fd,
                    bytes[offset..].as_ptr() as *const libc::c_void,
                    bytes.len() - offset,
                )
            };
            if rc > 0 {
                offset += rc as usize;
                continue;
            }
            if rc == 0 {
                bail!("pty write returned 0");
            }
            let err = std::io::Error::last_os_error();
            if err.kind() == ErrorKind::Interrupted {
                continue;
            }
            if err.kind() == ErrorKind::WouldBlock {
                if Instant::now() >= deadline {
                    bail!("pty write timed out");
                }
                let mut pfd = libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                unsafe { libc::poll(&mut pfd, 1, 50) };
                continue;
            }
            return Err(err).context("write pty");
        }
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let ws = libc::winsize {
            ws_row: rows.max(1),
            ws_col: cols.max(2),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let rc = unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).context("TIOCSWINSZ");
        }
        Ok(())
    }

    pub fn child_pid(&self) -> u32 {
        self.child_pid
    }

    pub fn poll_exit(&self) -> Option<Option<i32>> {
        let mut guard = self.child.lock().unwrap();
        if let Some(child) = guard.as_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    *guard = None;
                    return Some(status.code());
                }
                Ok(None) => return None,
                Err(_) => return None,
            }
        }
        // After handover `child` is None; check process existence via kill(pid, 0).
        if !is_pid_alive(self.child_pid) {
            return Some(None);
        }
        None
    }

    pub fn inspect_processes(&self) -> ProcessInspect {
        let alive = self.poll_exit().is_none() && is_pid_alive(self.child_pid);
        let pgid = unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) };
        let fg_pid = (pgid > 0).then_some(pgid as u32);
        let fg_process = fg_pid.and_then(read_proc_name);
        let cwd = read_proc_cwd(fg_pid.unwrap_or(self.child_pid))
            .or_else(|| read_proc_cwd(self.child_pid));
        let mut pids = collect_process_tree(self.child_pid);
        if !pids.contains(&self.child_pid) && alive {
            pids.insert(0, self.child_pid);
        }
        if let Some(fg) = fg_pid {
            if !pids.contains(&fg) {
                pids.push(fg);
            }
        }
        ProcessInspect {
            alive,
            pids,
            fg_pid,
            fg_process,
            cwd,
        }
    }

    pub fn terminate(&self) {
        if let Some(ctrl) = self.reader.lock().unwrap().take() {
            let wake = ctrl.wake_write;
            let _ = unsafe {
                let mut f = std::fs::File::from_raw_fd(wake.as_raw_fd());
                let res = f.write_all(&[1]);
                std::mem::forget(f);
                res
            };
            drop(wake);
            let _ = ctrl.handle.join();
        }
        let pid = self.child_pid as libc::pid_t;
        if pid > 0 {
            unsafe {
                libc::kill(-pid, libc::SIGHUP);
                libc::kill(pid, libc::SIGHUP);
            }
            for _ in 0..10 {
                if self.poll_exit().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
                libc::kill(pid, libc::SIGKILL);
            }
            let _ = self.poll_exit();
        }
    }
}

fn run_reader_loop(
    master: OwnedFd,
    wake_read: OwnedFd,
    child_pid: u32,
    tx: UnboundedSender<PtyEvent>,
) {
    let master_fd = master.as_raw_fd();
    let wake_fd = wake_read.as_raw_fd();
    let mut file = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(master_fd) });
    let mut buf = vec![0u8; 32 * 1024];

    'outer: loop {
        let mut pfds = [
            libc::pollfd {
                fd: master_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let rc = unsafe { libc::poll(pfds.as_mut_ptr(), 2, 1000) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == ErrorKind::Interrupted {
                continue;
            }
            break;
        }

        // Drain whatever is immediately available from the PTY before honoring wake_fd so
        // handover serialization captures all bytes emitted so far.
        loop {
            match file.read(&mut buf) {
                Ok(0) => {
                    let _ = tx.send(PtyEvent::Closed { exit_code: None });
                    break 'outer;
                }
                Ok(n) => {
                    if tx.send(PtyEvent::Output(buf[..n].to_vec())).is_err() {
                        break 'outer;
                    }
                }
                Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(ref e) if e.raw_os_error() == Some(libc::EIO) => {
                    let _ = tx.send(PtyEvent::Closed { exit_code: None });
                    break 'outer;
                }
                Err(_) => {
                    let _ = tx.send(PtyEvent::Closed { exit_code: None });
                    break 'outer;
                }
            }
        }

        if (pfds[1].revents & (libc::POLLIN | libc::POLLHUP)) != 0 {
            break;
        }
        if (pfds[0].revents & libc::POLLHUP) != 0 && !is_pid_alive(child_pid) {
            let _ = tx.send(PtyEvent::Closed { exit_code: None });
            break;
        }
    }
    let _ = master;
}

fn set_cloexec(fd: RawFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error()).context("F_GETFD");
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error()).context("F_SETFD");
    }
    Ok(())
}

fn set_nonblocking(fd: RawFd, nb: bool) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error()).context("F_GETFL");
    }
    let next = if nb {
        flags | libc::O_NONBLOCK
    } else {
        flags & !libc::O_NONBLOCK
    };
    if unsafe { libc::fcntl(fd, libc::F_SETFL, next) } < 0 {
        return Err(std::io::Error::last_os_error()).context("F_SETFL");
    }
    Ok(())
}

pub fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        true
    } else {
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

fn read_proc_name(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) {
            let trimmed = comm.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        let out = std::process::Command::new("ps")
            .args(["-o", "comm=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(
                s.rsplit('/')
                    .next()
                    .unwrap_or(&s)
                    .to_string(),
            )
        }
    }
}

fn read_proc_cwd(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/cwd"))
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

fn collect_process_tree(root: u32) -> Vec<u32> {
    #[cfg(target_os = "linux")]
    {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        let mut children_map: HashMap<u32, Vec<u32>> = HashMap::new();
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(pid) = name.parse::<u32>() else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            // Parse ppid after last ') ' to handle spaces in comm.
            if let Some(rparen) = stat.rfind(") ") {
                let mut fields = stat[rparen + 2..].split_whitespace();
                let _state = fields.next();
                if let Some(ppid) = fields.next().and_then(|s| s.parse::<u32>().ok()) {
                    children_map.entry(ppid).or_default().push(pid);
                }
            }
        }
        let mut seen = HashSet::new();
        let mut queue = VecDeque::new();
        let mut result = Vec::new();
        if is_pid_alive(root) {
            seen.insert(root);
            queue.push_back(root);
            result.push(root);
        }
        while let Some(cur) = queue.pop_front() {
            if let Some(kids) = children_map.get(&cur) {
                for &kid in kids {
                    if seen.insert(kid) {
                        result.push(kid);
                        queue.push_back(kid);
                    }
                }
            }
        }
        result
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (HashMap::<u32, Vec<u32>>::new(), HashSet::<u32>::new(), VecDeque::<u32>::new());
        if is_pid_alive(root) {
            vec![root]
        } else {
            Vec::new()
        }
    }
}
