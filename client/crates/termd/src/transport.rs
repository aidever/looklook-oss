//! Platform-neutral local IPC transport: Unix domain sockets (`0600`) on Unix,
//! user-ACL named pipes (`\\.\pipe\looklook-termd-...`) on Windows.

use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use term_proto::MAX_FRAME_BYTES;

/// Abstract duplex stream returned by platform listeners and connectors.
pub trait AsyncIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncIo for T {}

pub type BoxedIo = Box<dyn AsyncIo>;

/// Compute default IPC endpoint for the given data home directory.
pub fn default_socket_endpoint(home_dir: Option<&Path>) -> String {
    #[cfg(unix)]
    {
        if let Some(home) = home_dir {
            return home.join("run").join("termd.sock").to_string_lossy().into_owned();
        }
        if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
            if !xdg.trim().is_empty() {
                return PathBuf::from(xdg)
                    .join("looklook-termd.sock")
                    .to_string_lossy()
                    .into_owned();
            }
        }
        let uid = unsafe { libc::geteuid() };
        format!("/tmp/looklook-termd-{uid}.sock")
    }
    #[cfg(windows)]
    {
        let seed = home_dir
            .map(|p| p.to_string_lossy().into_owned())
            .or_else(|| std::env::var("USERNAME").ok())
            .unwrap_or_else(|| "default".to_string());
        format!(r"\\.\pipe\looklook-termd-{:016x}", fnv1a64(seed.as_bytes()))
    }
}

#[cfg(windows)]
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// Write a `[u32 LE len][payload]` IPC frame.
pub async fn write_ipc_frame(io: &mut (impl AsyncWrite + Unpin), payload: &[u8]) -> Result<()> {
    if payload.len() > MAX_FRAME_BYTES {
        anyhow::bail!("ipc frame too large: {} bytes", payload.len());
    }
    let mut framed = Vec::with_capacity(4 + payload.len());
    framed.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    framed.extend_from_slice(payload);
    io.write_all(&framed).await?;
    io.flush().await?;
    Ok(())
}

/// Read the next `[u32 LE len][payload]` IPC frame, returning `Ok(None)` on clean EOF.
pub async fn read_ipc_frame(io: &mut (impl AsyncRead + Unpin)) -> Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match io.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e).context("read ipc frame header"),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        anyhow::bail!("ipc frame too large: {len} bytes");
    }
    let mut buf = vec![0u8; len];
    io.read_exact(&mut buf).await.context("read ipc frame body")?;
    Ok(Some(buf))
}

#[cfg(unix)]
pub use unix_ipc::{IpcListener, connect};

#[cfg(windows)]
pub use win_ipc::{IpcListener, connect};

#[cfg(unix)]
mod unix_ipc {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::{UnixListener, UnixStream};

    pub struct IpcListener {
        inner: UnixListener,
        pub path: PathBuf,
    }

    impl IpcListener {
        pub fn bind(endpoint: &str) -> Result<Self> {
            let path = PathBuf::from(endpoint);
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                    let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
                }
            }
            let _ = std::fs::remove_file(&path);
            let inner = UnixListener::bind(&path)
                .with_context(|| format!("bind unix socket {}", path.display()))?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("chmod 0600 {}", path.display()))?;
            Ok(Self { inner, path })
        }

        pub async fn accept(&self) -> Result<BoxedIo> {
            let (stream, _) = self.inner.accept().await?;
            Ok(Box::new(stream))
        }
    }

    pub async fn connect(endpoint: &str) -> Result<BoxedIo> {
        let stream = UnixStream::connect(endpoint)
            .await
            .with_context(|| format!("connect unix socket {endpoint}"))?;
        Ok(Box::new(stream))
    }
}

#[cfg(windows)]
mod win_ipc {
    use super::*;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::null_mut;
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
    use windows_sys::Win32::Foundation::{CloseHandle, FALSE, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    pub struct IpcListener {
        name: String,
        next: tokio::sync::Mutex<NamedPipeServer>,
    }

    impl IpcListener {
        pub fn bind(endpoint: &str) -> Result<Self> {
            let server = create_pipe_server(endpoint, true)?;
            Ok(Self {
                name: endpoint.to_string(),
                next: tokio::sync::Mutex::new(server),
            })
        }

        pub async fn accept(&self) -> Result<BoxedIo> {
            let mut guard = self.next.lock().await;
            guard.connect().await?;
            let next_server = create_pipe_server(&self.name, false)?;
            let connected = std::mem::replace(&mut *guard, next_server);
            Ok(Box::new(connected))
        }
    }

    pub async fn connect(endpoint: &str) -> Result<BoxedIo> {
        let client = ClientOptions::new()
            .open(endpoint)
            .with_context(|| format!("connect named pipe {endpoint}"))?;
        Ok(Box::new(client))
    }

    fn create_pipe_server(endpoint: &str, first: bool) -> Result<NamedPipeServer> {
        let mut opts = ServerOptions::new();
        opts.first_pipe_instance(first);
        let sd = CurrentUserSecurityDesc::build()?;
        let mut sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.ptr as *mut _,
            bInheritHandle: FALSE,
        };
        let server = unsafe {
            opts.create_with_security_attributes_raw(endpoint, &mut sa as *mut _ as *mut _)
        }
        .with_context(|| format!("create named pipe {endpoint}"))?;
        Ok(server)
    }

    struct CurrentUserSecurityDesc {
        ptr: PSECURITY_DESCRIPTOR,
    }

    impl Drop for CurrentUserSecurityDesc {
        fn drop(&mut self) {
            if !self.ptr.is_null() {
                unsafe {
                    LocalFree(self.ptr as _);
                }
            }
        }
    }

    impl CurrentUserSecurityDesc {
        fn build() -> Result<Self> {
            unsafe {
                let mut token: HANDLE = null_mut();
                if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == FALSE {
                    return Err(std::io::Error::last_os_error()).context("OpenProcessToken");
                }
                let mut needed: u32 = 0;
                GetTokenInformation(token, TokenUser, null_mut(), 0, &mut needed);
                let mut buf = vec![0u8; needed.max(64) as usize];
                let ok = GetTokenInformation(
                    token,
                    TokenUser,
                    buf.as_mut_ptr() as *mut _,
                    buf.len() as u32,
                    &mut needed,
                );
                CloseHandle(token);
                if ok == FALSE {
                    return Err(std::io::Error::last_os_error()).context("GetTokenInformation");
                }
                let user = &*(buf.as_ptr() as *const TOKEN_USER);
                let mut sid_wide: *mut u16 = null_mut();
                if ConvertSidToStringSidW(user.User.Sid, &mut sid_wide) == FALSE {
                    return Err(std::io::Error::last_os_error()).context("ConvertSidToStringSidW");
                }
                let mut len = 0usize;
                while *sid_wide.add(len) != 0 {
                    len += 1;
                }
                let sid_str = String::from_utf16_lossy(std::slice::from_raw_parts(sid_wide, len));
                LocalFree(sid_wide as _);

                let sddl = format!("D:P(A;;GA;;;{sid_str})");
                let sddl_w: Vec<u16> = OsStr::new(&sddl).encode_wide().chain(Some(0)).collect();
                let mut sd: PSECURITY_DESCRIPTOR = null_mut();
                if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl_w.as_ptr(),
                    SDDL_REVISION_1,
                    &mut sd,
                    null_mut(),
                ) == FALSE
                {
                    return Err(std::io::Error::last_os_error())
                        .context("ConvertStringSecurityDescriptorToSecurityDescriptorW");
                }
                Ok(Self { ptr: sd })
            }
        }
    }
}
