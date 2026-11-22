use std::ffi::CStr;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct HandoverMeta {
    pub proc_a_pid: i32,
    pub shell_pid: i32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HandoverReport {
    pub proc_a_pid: Option<i32>,
    pub proc_a_exited: bool,
    pub shell_pid: i32,
    pub shell_alive_after_a_exit: bool,
    pub echo_ok: bool,
    pub reported_shell_pid: Option<i32>,
    pub resized_size: Option<(u16, u16)>,
}

/// 打开一对 Unix PTY (master, slave)，并设置初始行列数。
pub fn open_pty(cols: u16, rows: u16) -> Result<(OwnedFd, OwnedFd)> {
    unsafe {
        let master_raw = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
        if master_raw < 0 {
            return Err(std::io::Error::last_os_error()).context("posix_openpt");
        }
        let master = OwnedFd::from_raw_fd(master_raw);

        if libc::grantpt(master.as_raw_fd()) != 0 {
            return Err(std::io::Error::last_os_error()).context("grantpt");
        }
        if libc::unlockpt(master.as_raw_fd()) != 0 {
            return Err(std::io::Error::last_os_error()).context("unlockpt");
        }

        let name_ptr = libc::ptsname(master.as_raw_fd());
        if name_ptr.is_null() {
            return Err(std::io::Error::last_os_error()).context("ptsname");
        }
        let slave_path = CStr::from_ptr(name_ptr);
        let slave_raw = libc::open(slave_path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
        if slave_raw < 0 {
            return Err(std::io::Error::last_os_error()).context("open slave pty");
        }
        let slave = OwnedFd::from_raw_fd(slave_raw);

        resize_pty(BorrowedFd::borrow_raw(master.as_raw_fd()), cols, rows)?;
        Ok((master, slave))
    }
}

pub fn resize_pty(master: BorrowedFd<'_>, cols: u16, rows: u16) -> Result<()> {
    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    let rc = unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("ioctl(TIOCSWINSZ)");
    }
    Ok(())
}

/// 在 PTY 从端上拉起交互式 shell（独立会话 + 控制终端）。
pub fn spawn_shell(slave: OwnedFd, shell: &str) -> Result<Child> {
    let slave_fd = slave.as_raw_fd();
    let mut cmd = Command::new(shell);
    cmd.arg("-i")
        .env("TERM", "xterm-256color")
        .env("PS1", "PTY_HANDOVER> ")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::dup2(slave_fd, 0) < 0 || libc::dup2(slave_fd, 1) < 0 || libc::dup2(slave_fd, 2) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if slave_fd > 2 {
                libc::close(slave_fd);
            }
            Ok(())
        });
    }

    let child = cmd.spawn().with_context(|| format!("启动 shell 失败: {shell}"))?;
    drop(slave);
    Ok(child)
}

/// 经 Unix Domain Socket 用 `SCM_RIGHTS` 发送带外 fd 和元数据载荷。
pub fn send_fds(sock: &UnixStream, payload: &[u8], fds: &[RawFd]) -> Result<()> {
    assert!(!payload.is_empty(), "SCM_RIGHTS 需要至少 1 字节载荷");
    let mut iov = libc::iovec { iov_base: payload.as_ptr() as *mut libc::c_void, iov_len: payload.len() };

    let fds_bytes = std::mem::size_of_val(fds);
    let cmsg_space = unsafe { libc::CMSG_SPACE(fds_bytes as u32) as usize };
    let mut cmsg_buf = vec![0u8; cmsg_space];

    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = cmsg_space as _;

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        assert!(!cmsg.is_null());
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(fds_bytes as u32) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr() as *const u8, libc::CMSG_DATA(cmsg), fds_bytes);
    }

    let sent = unsafe { libc::sendmsg(sock.as_raw_fd(), &msg, 0) };
    if sent < 0 {
        return Err(std::io::Error::last_os_error()).context("sendmsg(SCM_RIGHTS)");
    }
    if sent as usize != payload.len() {
        bail!("sendmsg 只发送了 {sent}/{} 字节", payload.len());
    }
    Ok(())
}

/// 从 Unix Domain Socket 接收 `SCM_RIGHTS` 传来的 fd 与载荷。
pub fn recv_fds(sock: &UnixStream, max_fds: usize) -> Result<(Vec<u8>, Vec<OwnedFd>)> {
    let mut data_buf = vec![0u8; 4096];
    let mut iov = libc::iovec { iov_base: data_buf.as_mut_ptr() as *mut libc::c_void, iov_len: data_buf.len() };

    let cmsg_space = unsafe { libc::CMSG_SPACE((max_fds * std::mem::size_of::<RawFd>()) as u32) as usize };
    let mut cmsg_buf = vec![0u8; cmsg_space];

    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = cmsg_space as _;

    #[cfg(target_os = "linux")]
    let flags = libc::MSG_CMSG_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = 0;

    let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, flags) };
    if n < 0 {
        return Err(std::io::Error::last_os_error()).context("recvmsg(SCM_RIGHTS)");
    }
    data_buf.truncate(n as usize);

    let mut fds = Vec::new();
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let hdr_len = libc::CMSG_LEN(0) as usize;
                let data_len = ((*cmsg).cmsg_len as usize).saturating_sub(hdr_len);
                let count = data_len / std::mem::size_of::<RawFd>();
                let ptr = libc::CMSG_DATA(cmsg) as *const RawFd;
                for i in 0..count {
                    let fd = std::ptr::read_unaligned(ptr.add(i));
                    #[cfg(not(target_os = "linux"))]
                    libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                    fds.push(OwnedFd::from_raw_fd(fd));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }

    Ok((data_buf, fds))
}

fn read_until_marker(fd: RawFd, marker: &str, timeout: Duration) -> Result<String> {
    set_nonblocking(fd, true)?;
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];

    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n > 0 {
            out.extend_from_slice(&buf[..n as usize]);
            let s = String::from_utf8_lossy(&out);
            if s.contains(marker) {
                return Ok(s.into_owned());
            }
            continue;
        }
        if n == 0 {
            bail!("PTY EOF before marker {marker:?}; output={}", String::from_utf8_lossy(&out));
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::WouldBlock {
            return Err(err).context("read pty");
        }
        if Instant::now() >= deadline {
            bail!("timeout waiting for {marker:?}; output={}", String::from_utf8_lossy(&out));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn write_all_fd(fd: RawFd, data: &[u8]) -> Result<()> {
    set_nonblocking(fd, false)?;
    let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
    let r = f.write_all(data);
    std::mem::forget(f);
    r.context("write pty")
}

fn set_nonblocking(fd: RawFd, nonblock: bool) -> Result<()> {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return Err(std::io::Error::last_os_error()).context("fcntl(F_GETFL)");
        }
        let new_flags = if nonblock { flags | libc::O_NONBLOCK } else { flags & !libc::O_NONBLOCK };
        if libc::fcntl(fd, libc::F_SETFL, new_flags) < 0 {
            return Err(std::io::Error::last_os_error()).context("fcntl(F_SETFL)");
        }
    }
    Ok(())
}

fn pid_alive(pid: i32) -> bool {
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub fn run_proc_a(sock_path: &Path, meta_path: Option<&Path>, shell: &str, cols: u16, rows: u16) -> Result<()> {
    let _ = std::fs::remove_file(sock_path);
    let listener = UnixListener::bind(sock_path).with_context(|| format!("bind {}", sock_path.display()))?;

    let (master, slave) = open_pty(cols, rows)?;
    let child = spawn_shell(slave, shell)?;
    let shell_pid = child.id() as i32;
    // 不让 Child drop 时杀子进程；我们有意让 shell 脱离进程 A 存活。
    std::mem::forget(child);

    // 验证进程 A 自己能正常读写 shell
    write_all_fd(master.as_raw_fd(), b"echo __PROC_A_\"\"READY_$$__\n")?;
    let ready_marker = format!("__PROC_A_READY_{shell_pid}__");
    let _ = read_until_marker(master.as_raw_fd(), &ready_marker, Duration::from_secs(5))?;

    let meta = HandoverMeta { proc_a_pid: std::process::id() as i32, shell_pid };
    if let Some(p) = meta_path {
        std::fs::write(p, serde_json::to_vec(&meta)?)?;
    }

    let (mut stream, _) = listener.accept().context("accept handover client")?;
    let payload = serde_json::to_vec(&meta)?;
    send_fds(&stream, &payload, &[master.as_raw_fd()])?;

    // 等进程 B 确认收到 fd 后，进程 A 立刻退出（不关闭 shell）。
    let mut ack = [0u8; 2];
    let _ = stream.read_exact(&mut ack);
    drop(master);
    Ok(())
}

pub fn run_proc_b(sock_path: &Path, wait_pid: Option<i32>, resize_cols: u16, resize_rows: u16) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stream = loop {
        match UnixStream::connect(sock_path) {
            Ok(s) => break s,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(e).with_context(|| format!("connect {}", sock_path.display())),
        }
    };

    let (payload, mut fds) = recv_fds(&stream, 4)?;
    let meta: HandoverMeta = serde_json::from_slice(&payload).context("decode HandoverMeta")?;
    let master = fds.pop().context("未收到 PTY master fd")?;

    stream.write_all(b"OK")?;
    drop(stream);

    // 等待进程 A 完全退出
    let a_pid = wait_pid.unwrap_or(meta.proc_a_pid);
    let wait_deadline = Instant::now() + Duration::from_secs(5);
    let mut proc_a_exited = false;
    while Instant::now() < wait_deadline {
        if !pid_alive(a_pid) {
            proc_a_exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let shell_alive = pid_alive(meta.shell_pid);

    // 在进程 A 已退出后，通过接管过来的 master fd 继续发命令 & 调整窗口尺寸
    resize_pty(master.as_fd(), resize_cols, resize_rows)?;
    write_all_fd(master.as_raw_fd(), b"echo __PROC_B_\"\"ALIVE_$$__; stty size\n")?;

    let b_marker = format!("__PROC_B_ALIVE_{}__", meta.shell_pid);
    let size_marker = format!("{resize_rows} {resize_cols}");
    let out = read_until_marker(master.as_raw_fd(), &size_marker, Duration::from_secs(5))?;
    let echo_ok = out.contains(&b_marker);

    // 清理：让 shell 退出
    let _ = write_all_fd(master.as_raw_fd(), b"exit\n");

    let report = HandoverReport {
        proc_a_pid: Some(a_pid),
        proc_a_exited,
        shell_pid: meta.shell_pid,
        shell_alive_after_a_exit: shell_alive,
        echo_ok,
        reported_shell_pid: echo_ok.then_some(meta.shell_pid),
        resized_size: out.contains(&size_marker).then_some((resize_cols, resize_rows)),
    };
    println!("{}", serde_json::to_string_pretty(&report)?);

    if !report.proc_a_exited || !report.shell_alive_after_a_exit || !report.echo_ok || report.resized_size.is_none() {
        bail!("Unix PTY 交接验证未完全通过");
    }
    Ok(())
}

pub fn run_demo(shell: &str) -> Result<()> {
    let dir = tempfile::tempdir()?;
    let sock = dir.path().join("handover.sock");
    let meta = dir.path().join("meta.json");
    let exe = std::env::current_exe()?;

    let mut proc_a = Command::new(&exe)
        .args(["unix-proc-a", "--sock"])
        .arg(&sock)
        .args(["--meta"])
        .arg(&meta)
        .args(["--shell", shell])
        .spawn()
        .context("spawn unix-proc-a")?;

    let deadline = Instant::now() + Duration::from_secs(5);
    while !sock.exists() {
        if Instant::now() >= deadline {
            let _ = proc_a.kill();
            bail!("等待 unix-proc-a 创建 socket 超时");
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let proc_a_pid = proc_a.id();
    let waiter = std::thread::spawn(move || proc_a.wait());

    let proc_b_out = Command::new(&exe)
        .args(["unix-proc-b", "--sock"])
        .arg(&sock)
        .args(["--wait-pid", &proc_a_pid.to_string()])
        .output()
        .context("run unix-proc-b")?;

    let a_status = waiter.join().map_err(|_| anyhow::anyhow!("waiter panicked"))??;
    print!("{}", String::from_utf8_lossy(&proc_b_out.stdout));
    if !proc_b_out.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&proc_b_out.stderr));
    }
    if !a_status.success() || !proc_b_out.status.success() {
        bail!("unix-demo 失败: a={a_status}, b={}", proc_b_out.status);
    }
    Ok(())
}
