//! Unix `SCM_RIGHTS` + serialized `term-core` state handover for zero-downtime `looklook-termd`
//! upgrades (design §4.4). Windows returns an explicit unsupported error per Step 0 conclusions.

use anyhow::Result;

use crate::session::SessionManager;

pub const HANDOVER_SUPPORTED: bool = cfg!(unix);

#[cfg(unix)]
mod unix_handover {
    use super::*;
    use std::io::IoSlice;
    use std::mem;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::net::UnixStream as StdUnixStream;

    use anyhow::{Context, bail};
    use serde::{Deserialize, Serialize};
    use term_core::{AlacrittyCore, TerminalCore};
    use tokio::sync::mpsc;

    use crate::pty::PlatformPty;
    use crate::session::Session;

    const MAX_FDS_PER_MSG: usize = 64;

    #[derive(Serialize, Deserialize)]
    struct HandoverSessionEntry {
        id: String,
        child_pid: u32,
        created_at_ms: i64,
        last_output_at_ms: i64,
        last_input_at_ms: i64,
        raw_ring: Vec<u8>,
        core_state: Vec<u8>,
    }

    #[derive(Serialize, Deserialize)]
    struct HandoverManifest {
        entries: Vec<HandoverSessionEntry>,
    }

    pub async fn send_sessions_over_handover(
        mgr: &SessionManager,
        takeover_socket: &str,
    ) -> Result<()> {
        *mgr.handover_lock.lock().unwrap() = true;
        let sessions = {
            let mut map = mgr.inner.write().await;
            let entries: Vec<_> = map.drain().collect();
            entries
        };

        let mut manifest_entries = Vec::with_capacity(sessions.len());
        let mut fds = Vec::with_capacity(sessions.len());

        for (id, session) in sessions {
            let guard = session.state.lock().await;
            let fd = guard.pty.pause_and_clone_fd_for_handover()?;
            let core_state = guard.core.serialize_state()?;
            manifest_entries.push(HandoverSessionEntry {
                id,
                child_pid: guard.pty.child_pid(),
                created_at_ms: guard.created_at_ms,
                last_output_at_ms: guard.last_output_at_ms,
                last_input_at_ms: guard.last_input_at_ms,
                raw_ring: guard.raw_ring.iter().copied().collect(),
                core_state,
            });
            fds.push(fd);
        }

        let manifest = HandoverManifest {
            entries: manifest_entries,
        };
        let manifest_bytes = bincode::serialize(&manifest).context("encode handover manifest")?;
        let sock_owned = takeover_socket.to_string();

        tokio::task::spawn_blocking(move || -> Result<()> {
            let stream = StdUnixStream::connect(&sock_owned)
                .with_context(|| format!("connect takeover socket {sock_owned}"))?;
            let count = fds.len();
            write_std_frame(&stream, &(count as u32).to_le_bytes())?;
            write_std_frame(&stream, &manifest_bytes)?;
            for chunk in fds.chunks(MAX_FDS_PER_MSG) {
                let raw_chunk: Vec<RawFd> = chunk.iter().map(AsRawFd::as_raw_fd).collect();
                send_fds_blocking(&stream, &raw_chunk)?;
            }
            let ack = read_std_frame(&stream)?;
            if ack != b"HANDOVER_OK" {
                bail!("unexpected handover ack from new termd: {:?}", ack);
            }
            Ok(())
        })
        .await??;

        Ok(())
    }

    pub async fn receive_sessions_from_handover(
        mgr: &SessionManager,
        listener: std::os::unix::net::UnixListener,
    ) -> Result<usize> {
        let (stream, manifest, fds) =
            tokio::task::spawn_blocking(move || -> Result<(StdUnixStream, HandoverManifest, Vec<OwnedFd>)> {
                let (stream, _) = listener.accept().context("accept handover sender")?;
                let count_bytes = read_std_frame(&stream)?;
                if count_bytes.len() != 4 {
                    bail!("invalid handover session count header");
                }
                let expected = u32::from_le_bytes(count_bytes.try_into().unwrap()) as usize;
                let manifest_bytes = read_std_frame(&stream)?;
                let manifest: HandoverManifest =
                    bincode::deserialize(&manifest_bytes).context("decode handover manifest")?;
                if manifest.entries.len() != expected {
                    bail!(
                        "handover entry count mismatch: expected {expected}, got {}",
                        manifest.entries.len()
                    );
                }
                let mut fds = Vec::with_capacity(expected);
                while fds.len() < expected {
                    let want = (expected - fds.len()).min(MAX_FDS_PER_MSG);
                    let batch = recv_fds_blocking(&stream, want)?;
                    if batch.is_empty() {
                        bail!("unexpected EOF receiving handover fds");
                    }
                    fds.extend(batch);
                }
                Ok((stream, manifest, fds))
            })
            .await??;

        let count = manifest.entries.len();
        {
            let mut map = mgr.inner.write().await;
            for (entry, fd) in manifest.entries.into_iter().zip(fds) {
                let core = AlacrittyCore::from_serialized(&entry.core_state)?;
                let (tx, rx) = mpsc::unbounded_channel();
                let pty = PlatformPty::from_handover(fd, entry.child_pid, tx)?;
                let session = Session::from_handover(crate::session::HandoverRestore {
                    id: entry.id.clone(),
                    pty,
                    core,
                    created_at_ms: entry.created_at_ms,
                    last_output_at_ms: entry.last_output_at_ms,
                    last_input_at_ms: entry.last_input_at_ms,
                    raw_ring: entry.raw_ring,
                    rx,
                });
                map.insert(entry.id, session);
            }
        }

        tokio::task::spawn_blocking(move || write_std_frame(&stream, b"HANDOVER_OK")).await??;
        Ok(count)
    }

    fn write_std_frame(mut stream: &StdUnixStream, data: &[u8]) -> Result<()> {
        use std::io::Write;
        stream.write_all(&(data.len() as u32).to_le_bytes())?;
        stream.write_all(data)?;
        stream.flush()?;
        Ok(())
    }

    fn read_std_frame(mut stream: &StdUnixStream) -> Result<Vec<u8>> {
        use std::io::Read;
        let mut hdr = [0u8; 4];
        stream.read_exact(&mut hdr)?;
        let len = u32::from_le_bytes(hdr) as usize;
        if len > 256 * 1024 * 1024 {
            bail!("handover frame too large: {len}");
        }
        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf)?;
        Ok(buf)
    }

    fn send_fds_blocking(stream: &StdUnixStream, fds: &[RawFd]) -> Result<()> {
        let mut marker = [0u8; 1];
        marker[0] = fds.len() as u8;
        let iov = [IoSlice::new(&marker)];
        let cmsg_space = unsafe { libc::CMSG_SPACE(std::mem::size_of_val(fds) as u32) };
        let mut cmsg_buf = vec![0u8; cmsg_space as usize];

        let mut msg: libc::msghdr = unsafe { mem::zeroed() };
        msg.msg_iov = iov.as_ptr() as *mut libc::iovec;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = cmsg_buf.len() as _;

        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            if cmsg.is_null() {
                bail!("CMSG_FIRSTHDR null");
            }
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len =
                libc::CMSG_LEN(std::mem::size_of_val(fds) as u32) as _;
            std::ptr::copy_nonoverlapping(
                fds.as_ptr(),
                libc::CMSG_DATA(cmsg) as *mut RawFd,
                fds.len(),
            );
            let sent = libc::sendmsg(stream.as_raw_fd(), &msg, 0);
            if sent <= 0 {
                return Err(std::io::Error::last_os_error()).context("sendmsg SCM_RIGHTS");
            }
        }
        Ok(())
    }

    fn recv_fds_blocking(stream: &StdUnixStream, expected: usize) -> Result<Vec<OwnedFd>> {
        let mut marker = [0u8; 1];
        let mut iov = [std::io::IoSliceMut::new(&mut marker)];
        let cmsg_space =
            unsafe { libc::CMSG_SPACE((expected * mem::size_of::<RawFd>()) as u32) };
        let mut cmsg_buf = vec![0u8; cmsg_space as usize];

        let mut msg: libc::msghdr = unsafe { mem::zeroed() };
        msg.msg_iov = iov.as_mut_ptr() as *mut libc::iovec;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = cmsg_buf.len() as _;

        #[cfg(any(target_os = "linux", target_os = "android"))]
        let recv_flags = libc::MSG_CMSG_CLOEXEC;
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let recv_flags = 0;

        let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, recv_flags) };
        if n <= 0 {
            return Err(std::io::Error::last_os_error()).context("recvmsg SCM_RIGHTS");
        }
        if (msg.msg_flags & libc::MSG_CTRUNC) != 0 {
            bail!("recvmsg control data truncated");
        }

        let mut out = Vec::with_capacity(expected);
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                    let hdr_len = libc::CMSG_LEN(0) as usize;
                    let data_bytes = ((*cmsg).cmsg_len as usize).saturating_sub(hdr_len);
                    let count = data_bytes / mem::size_of::<RawFd>();
                    let ptr = libc::CMSG_DATA(cmsg) as *const RawFd;
                    for i in 0..count {
                        let fd = std::ptr::read_unaligned(ptr.add(i));
                        out.push(OwnedFd::from_raw_fd(fd));
                    }
                }
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }
        }
        Ok(out)
    }
}

#[cfg(unix)]
pub use unix_handover::{receive_sessions_from_handover, send_sessions_over_handover};

#[cfg(not(unix))]
pub async fn send_sessions_over_handover(_mgr: &SessionManager, _sock: &str) -> Result<()> {
    anyhow::bail!("cross-process PTY handover is not supported on Windows ConPTY")
}
