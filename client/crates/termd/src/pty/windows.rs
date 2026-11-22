//! Windows ConPTY spawning, resizing, reading, writing, and process checks.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr::{null, null_mut};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use term_proto::CreateSessionReq;
use tokio::sync::mpsc::UnboundedSender;
use windows_sys::Win32::Foundation::{
    CloseHandle, FALSE, HANDLE, INVALID_HANDLE_VALUE, S_OK, STILL_ACTIVE,
};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON,
};
use windows_sys::Win32::System::Pipes::{CreatePipe, PeekNamedPipe};
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, InitializeProcThreadAttributeList,
    OpenProcess, PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    STARTUPINFOEXW, STARTUPINFOW, TerminateProcess, UpdateProcThreadAttribute,
};

use super::{ProcessInspect, PtyEvent};

const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;

struct RawHandle(HANDLE);
unsafe impl Send for RawHandle {}
unsafe impl Sync for RawHandle {}

struct ConPtyHandle(HPCON);
unsafe impl Send for ConPtyHandle {}
unsafe impl Sync for ConPtyHandle {}

pub struct PlatformPty {
    hpc: Mutex<Option<ConPtyHandle>>,
    in_write: RawHandle,
    out_read: RawHandle,
    child_pid: u32,
    stop_flag: std::sync::Arc<AtomicBool>,
    reader_handle: Mutex<Option<JoinHandle<()>>>,
    write_lock: Mutex<()>,
}

impl Drop for PlatformPty {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        unsafe {
            if let Some(h) = self.hpc.lock().unwrap().take() {
                ClosePseudoConsole(h.0);
            }
            if !self.in_write.0.is_null() {
                CloseHandle(self.in_write.0);
            }
            if !self.out_read.0.is_null() {
                CloseHandle(self.out_read.0);
            }
        }
    }
}

impl PlatformPty {
    pub fn spawn(req: &CreateSessionReq, tx: UnboundedSender<PtyEvent>) -> Result<Self> {
        unsafe {
            let mut h_in_read: HANDLE = INVALID_HANDLE_VALUE;
            let mut h_in_write: HANDLE = INVALID_HANDLE_VALUE;
            let mut h_out_read: HANDLE = INVALID_HANDLE_VALUE;
            let mut h_out_write: HANDLE = INVALID_HANDLE_VALUE;

            if CreatePipe(&mut h_in_read, &mut h_in_write, null(), 0) == FALSE {
                return Err(std::io::Error::last_os_error()).context("CreatePipe stdin");
            }
            if CreatePipe(&mut h_out_read, &mut h_out_write, null(), 0) == FALSE {
                CloseHandle(h_in_read);
                CloseHandle(h_in_write);
                return Err(std::io::Error::last_os_error()).context("CreatePipe stdout");
            }

            let mut hpc: HPCON = 0;
            let size = COORD {
                X: req.cols.max(2) as i16,
                Y: req.rows.max(1) as i16,
            };
            let hr = CreatePseudoConsole(size, h_in_read, h_out_write, 0, &mut hpc);
            CloseHandle(h_in_read);
            CloseHandle(h_out_write);
            if hr != S_OK {
                CloseHandle(h_in_write);
                CloseHandle(h_out_read);
                anyhow::bail!("CreatePseudoConsole failed: 0x{hr:08x}");
            }

            let mut attr_size: usize = 0;
            InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut attr_size);
            let mut attr_buf = vec![0u8; attr_size];
            let attr_list = attr_buf.as_mut_ptr() as *mut _;
            if InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_size) == FALSE {
                ClosePseudoConsole(hpc);
                CloseHandle(h_in_write);
                CloseHandle(h_out_read);
                return Err(std::io::Error::last_os_error())
                    .context("InitializeProcThreadAttributeList");
            }
            if UpdateProcThreadAttribute(
                attr_list,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                hpc as *const _,
                std::mem::size_of::<HPCON>(),
                null_mut(),
                null_mut(),
            ) == FALSE
            {
                DeleteProcThreadAttributeList(attr_list);
                ClosePseudoConsole(hpc);
                CloseHandle(h_in_write);
                CloseHandle(h_out_read);
                return Err(std::io::Error::last_os_error()).context("UpdateProcThreadAttribute");
            }

            let mut si_ex: STARTUPINFOEXW = std::mem::zeroed();
            si_ex.StartupInfo = std::mem::zeroed::<STARTUPINFOW>();
            si_ex.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            si_ex.lpAttributeList = attr_list;

            let cmd_line = build_command_line(&req.cmd, &req.args);
            let mut cmd_w = to_wide(&cmd_line);
            let env_w = build_env_block(&req.env, &req.env_remove);
            let cwd_w = (!req.cwd.is_empty()).then(|| to_wide(&req.cwd));

            let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
            let ok = CreateProcessW(
                null(),
                cmd_w.as_mut_ptr(),
                null(),
                null(),
                FALSE,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                env_w.as_ptr() as *const _,
                cwd_w.as_ref().map_or(null(), |v| v.as_ptr()),
                &si_ex.StartupInfo,
                &mut pi,
            );
            DeleteProcThreadAttributeList(attr_list);
            if ok == FALSE {
                ClosePseudoConsole(hpc);
                CloseHandle(h_in_write);
                CloseHandle(h_out_read);
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("CreateProcessW({cmd_line})"));
            }

            let child_pid = pi.dwProcessId;
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);

            let stop_flag = std::sync::Arc::new(AtomicBool::new(false));
            let reader_stop = stop_flag.clone();
            let out_handle = RawHandle(h_out_read);
            let handle = std::thread::Builder::new()
                .name(format!("conpty-read-{child_pid}"))
                .spawn(move || {
                    run_conpty_reader(out_handle, child_pid, reader_stop, tx);
                })
                .context("spawn conpty reader thread")?;

            Ok(Self {
                hpc: Mutex::new(Some(ConPtyHandle(hpc))),
                in_write: RawHandle(h_in_write),
                out_read: RawHandle(h_out_read),
                child_pid,
                stop_flag,
                reader_handle: Mutex::new(Some(handle)),
                write_lock: Mutex::new(()),
            })
        }
    }

    pub fn write_all(&self, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let _guard = self.write_lock.lock().unwrap();
        let mut offset = 0usize;
        while offset < bytes.len() {
            let mut written = 0u32;
            let ok = unsafe {
                WriteFile(
                    self.in_write.0,
                    bytes[offset..].as_ptr(),
                    (bytes.len() - offset) as u32,
                    &mut written,
                    null_mut(),
                )
            };
            if ok == FALSE {
                return Err(std::io::Error::last_os_error()).context("WriteFile(conpty)");
            }
            if written == 0 {
                anyhow::bail!("conpty write returned 0");
            }
            offset += written as usize;
        }
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let guard = self.hpc.lock().unwrap();
        let Some(hpc) = guard.as_ref() else {
            return Ok(());
        };
        let hr = unsafe {
            ResizePseudoConsole(
                hpc.0,
                COORD {
                    X: cols.max(2) as i16,
                    Y: rows.max(1) as i16,
                },
            )
        };
        if hr != S_OK {
            anyhow::bail!("ResizePseudoConsole failed: 0x{hr:08x}");
        }
        Ok(())
    }

    pub fn child_pid(&self) -> u32 {
        self.child_pid
    }

    pub fn poll_exit(&self) -> Option<Option<i32>> {
        check_pid_exit(self.child_pid)
    }

    pub fn inspect_processes(&self) -> ProcessInspect {
        let exited = self.poll_exit();
        let alive = exited.is_none();
        ProcessInspect {
            alive,
            pids: if alive { vec![self.child_pid] } else { Vec::new() },
            fg_pid: alive.then_some(self.child_pid),
            fg_process: None,
            cwd: None,
        }
    }

    pub fn terminate(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, FALSE, self.child_pid);
            if !h.is_null() {
                TerminateProcess(h, 1);
                CloseHandle(h);
            }
            if let Some(hpc) = self.hpc.lock().unwrap().take() {
                ClosePseudoConsole(hpc.0);
            }
        }
        if let Some(handle) = self.reader_handle.lock().unwrap().take() {
            let _ = handle.join();
        }
    }
}

fn run_conpty_reader(
    out_read: RawHandle,
    child_pid: u32,
    stop: std::sync::Arc<AtomicBool>,
    tx: UnboundedSender<PtyEvent>,
) {
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        unsafe {
            let mut avail = 0u32;
            if PeekNamedPipe(out_read.0, null_mut(), 0, null_mut(), &mut avail, null_mut()) == FALSE
            {
                let exit_code = check_pid_exit(child_pid).flatten();
                let _ = tx.send(PtyEvent::Closed { exit_code });
                break;
            }
            if avail == 0 {
                if let Some(exit_code) = check_pid_exit(child_pid) {
                    let _ = tx.send(PtyEvent::Closed { exit_code });
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }
            let mut read = 0u32;
            let to_read = (avail as usize).min(buf.len()) as u32;
            if ReadFile(out_read.0, buf.as_mut_ptr(), to_read, &mut read, null_mut()) == FALSE {
                let exit_code = check_pid_exit(child_pid).flatten();
                let _ = tx.send(PtyEvent::Closed { exit_code });
                break;
            }
            if read > 0 && tx.send(PtyEvent::Output(buf[..read as usize].to_vec())).is_err() {
                break;
            }
        }
    }
}

pub fn is_pid_alive(pid: u32) -> bool {
    check_pid_exit(pid).is_none()
}

fn check_pid_exit(pid: u32) -> Option<Option<i32>> {
    if pid == 0 {
        return Some(None);
    }
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid);
        if h.is_null() {
            return Some(None);
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        if ok == FALSE {
            Some(None)
        } else if code == STILL_ACTIVE as u32 {
            None
        } else {
            Some(Some(code as i32))
        }
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

fn build_command_line(cmd: &str, args: &[String]) -> String {
    let mut parts = Vec::with_capacity(1 + args.len());
    parts.push(quote_win_arg(cmd));
    for a in args {
        parts.push(quote_win_arg(a));
    }
    parts.join(" ")
}

fn quote_win_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"', '\n', '\r']) {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        if ch == '\\' {
            backslashes += 1;
        } else if ch == '"' {
            out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
            out.push('"');
            backslashes = 0;
        } else {
            out.extend(std::iter::repeat_n('\\', backslashes));
            out.push(ch);
            backslashes = 0;
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

fn build_env_block(overrides: &[(String, String)], remove: &[String]) -> Vec<u16> {
    let mut map: BTreeMap<String, String> = std::env::vars().collect();
    map.remove("TMUX");
    map.remove("TMUX_PANE");
    for k in remove {
        map.remove(k);
    }
    map.insert("TERM".into(), "xterm-256color".into());
    map.insert("COLORTERM".into(), "truecolor".into());
    for (k, v) in overrides {
        if v.is_empty() && (k == "TMUX" || k == "TMUX_PANE") {
            map.remove(k);
        } else {
            map.insert(k.clone(), v.clone());
        }
    }
    let mut block = Vec::new();
    for (k, v) in map {
        let entry = format!("{k}={v}");
        block.extend(OsStr::new(&entry).encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}
