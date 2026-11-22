//! Windows ConPTY 跨进程句柄交接验证原型。
//!
//! 关键调查与验证点（见 docs/TERMINAL_BASELINE_2026-11-20.md）：
//! 1. `CreatePseudoConsole` 返回的 `HPCON` 在 `kernelbase.dll` / `winconpty` 中是指向
//!    进程内用户态堆结构 `PseudoConsole` 的不透明指针（并非内核句柄），因此 `DuplicateHandle`
//!    无法复制 `HPCON` 本身（预期失败，`ERROR_INVALID_HANDLE = 6`）。
//! 2. 输入输出管道（`hPipeInWrite` / `hPipeOutRead`）是标准内核句柄，可以通过 `DuplicateHandle`
//!    复制给进程 B。
//! 3. 但是 `PseudoConsole` 内部持有的 `hPtyReference`（`\Device\ConDrv` 引用句柄）和
//!    `hSignal`（`--signal` 匿名管道写端，供 `ResizePseudoConsole` 发送尺寸包）没有公开 API
//!    导出给进程 B。进程 A 退出时，内核关闭这两个句柄，导致 `conhost.exe --headless` 断开或
//!    后续 `ResizePseudoConsole` 失效。

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::process::Command;
use std::ptr::{null, null_mut};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, GetLastError, DUPLICATE_SAME_ACCESS, ERROR_BROKEN_PIPE,
    FALSE, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, STILL_ACTIVE,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, CreatePipe, PeekNamedPipe, WaitNamedPipeW,
    PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess, GetCurrentProcessId,
    GetExitCodeProcess, InitializeProcThreadAttributeList, OpenProcess, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject, EXTENDED_STARTUPINFO_PRESENT,
    PROCESS_DUP_HANDLE, PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, STARTUPINFOEXW, STARTUPINFOW,
};

// `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` = ProcThreadAttributeValue(22, FALSE, TRUE, FALSE) = 0x00020016
const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;
const ERROR_PIPE_CONNECTED: u32 = 535;
const PIPE_UNLIMITED_INSTANCES: u32 = 255;

#[derive(Debug, Serialize, Deserialize)]
struct WinHandoverMsg {
    proc_a_pid: u32,
    child_pid: u32,
    dup_in_write: usize,
    dup_out_read: usize,
    dup_hpcon_ok: bool,
    dup_hpcon_err: u32,
    dup_hpcon_val: usize,
    raw_hpcon_val: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct WinProbeReport {
    proc_a_pid: u32,
    child_pid: u32,
    dup_hpcon_ok: bool,
    dup_hpcon_err: u32,
    proc_a_exited: bool,
    child_alive_after_a_exit: bool,
    pipe_write_ok: bool,
    pipe_write_err: u32,
    pipe_read_bytes: usize,
    pipe_read_err: u32,
    resize_hresult: i32,
}

fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

fn write_pipe_frame(pipe: HANDLE, data: &[u8]) -> Result<()> {
    let len = (data.len() as u32).to_le_bytes();
    write_exact(pipe, &len)?;
    write_exact(pipe, data)
}

fn read_pipe_frame(pipe: HANDLE) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    read_exact(pipe, &mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    read_exact(pipe, &mut buf)?;
    Ok(buf)
}

fn write_exact(h: HANDLE, mut data: &[u8]) -> Result<()> {
    while !data.is_empty() {
        let mut written = 0u32;
        let ok = unsafe { WriteFile(h, data.as_ptr(), data.len() as u32, &mut written, null_mut()) };
        if ok == FALSE {
            return Err(std::io::Error::last_os_error()).context("WriteFile");
        }
        data = &data[written as usize..];
    }
    Ok(())
}

fn read_exact(h: HANDLE, mut buf: &mut [u8]) -> Result<()> {
    while !buf.is_empty() {
        let mut got = 0u32;
        let ok = unsafe { ReadFile(h, buf.as_mut_ptr(), buf.len() as u32, &mut got, null_mut()) };
        if ok == FALSE || got == 0 {
            return Err(std::io::Error::last_os_error()).context("ReadFile");
        }
        buf = &mut buf[got as usize..];
    }
    Ok(())
}

fn is_pid_alive(pid: u32) -> bool {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        ok != FALSE && code == STILL_ACTIVE as u32
    }
}

pub fn run_proc_a(pipe_name: &str, cmd_line: &str, cols: i16, rows: i16, close_hpcon: bool) -> Result<()> {
    unsafe {
        let mut sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: null_mut(),
            bInheritHandle: FALSE,
        };

        let mut h_in_read: HANDLE = null_mut();
        let mut h_in_write: HANDLE = null_mut();
        let mut h_out_read: HANDLE = null_mut();
        let mut h_out_write: HANDLE = null_mut();

        if CreatePipe(&mut h_in_read, &mut h_in_write, &mut sa, 0) == FALSE {
            return Err(std::io::Error::last_os_error()).context("CreatePipe(in)");
        }
        if CreatePipe(&mut h_out_read, &mut h_out_write, &mut sa, 0) == FALSE {
            return Err(std::io::Error::last_os_error()).context("CreatePipe(out)");
        }

        let mut hpc: HPCON = 0;
        let size = COORD { X: cols, Y: rows };
        let hr = CreatePseudoConsole(size, h_in_read, h_out_write, 0, &mut hpc);
        if hr != 0 {
            bail!("CreatePseudoConsole 失败: HRESULT=0x{hr:08x}");
        }
        // ConPTY 内部已接管这两端
        CloseHandle(h_in_read);
        CloseHandle(h_out_write);

        // 初始化 EXTENDED_STARTUPINFO 并绑定 PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE
        let mut attr_size: usize = 0;
        InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut attr_size);
        let mut attr_buf = vec![0u8; attr_size];
        let attr_list = attr_buf.as_mut_ptr() as *mut _;
        if InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_size) == FALSE {
            return Err(std::io::Error::last_os_error()).context("InitializeProcThreadAttributeList");
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
            return Err(std::io::Error::last_os_error()).context("UpdateProcThreadAttribute");
        }

        let mut si_ex: STARTUPINFOEXW = std::mem::zeroed();
        si_ex.StartupInfo = std::mem::zeroed::<STARTUPINFOW>();
        si_ex.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si_ex.lpAttributeList = attr_list;

        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let mut cmd_w = to_wide(cmd_line);
        let ok = CreateProcessW(
            null(),
            cmd_w.as_mut_ptr(),
            null(),
            null(),
            FALSE,
            EXTENDED_STARTUPINFO_PRESENT,
            null(),
            null(),
            &si_ex.StartupInfo,
            &mut pi,
        );
        DeleteProcThreadAttributeList(attr_list);
        if ok == FALSE {
            return Err(std::io::Error::last_os_error()).context("CreateProcessW");
        }
        let child_pid = pi.dwProcessId;
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);

        // 建命名管道等进程 B 连接并报出它的 PID
        let pipe_w = to_wide(pipe_name);
        let pipe = CreateNamedPipeW(
            pipe_w.as_ptr(),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            PIPE_UNLIMITED_INSTANCES,
            4096,
            4096,
            0,
            null(),
        );
        if pipe == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error()).context("CreateNamedPipeW");
        }
        if ConnectNamedPipe(pipe, null_mut()) == FALSE && GetLastError() != ERROR_PIPE_CONNECTED {
            CloseHandle(pipe);
            return Err(std::io::Error::last_os_error()).context("ConnectNamedPipe");
        }

        let pid_bytes = read_pipe_frame(pipe)?;
        let pid_b = u32::from_le_bytes(pid_bytes.try_into().map_err(|_| anyhow::anyhow!("bad pid frame"))?);

        let proc_b = OpenProcess(PROCESS_DUP_HANDLE, FALSE, pid_b);
        if proc_b.is_null() {
            CloseHandle(pipe);
            return Err(std::io::Error::last_os_error()).context("OpenProcess(proc_b)");
        }

        let self_proc = GetCurrentProcess();
        let mut dup_in_write: HANDLE = null_mut();
        if DuplicateHandle(self_proc, h_in_write, proc_b, &mut dup_in_write, 0, FALSE, DUPLICATE_SAME_ACCESS) == FALSE {
            CloseHandle(proc_b);
            CloseHandle(pipe);
            return Err(std::io::Error::last_os_error()).context("DuplicateHandle(h_in_write)");
        }
        let mut dup_out_read: HANDLE = null_mut();
        if DuplicateHandle(self_proc, h_out_read, proc_b, &mut dup_out_read, 0, FALSE, DUPLICATE_SAME_ACCESS) == FALSE {
            CloseHandle(proc_b);
            CloseHandle(pipe);
            return Err(std::io::Error::last_os_error()).context("DuplicateHandle(h_out_read)");
        }

        // 关键测试：尝试 DuplicateHandle 复制 HPCON 本身
        let mut dup_hpcon: HANDLE = null_mut();
        let dup_hpcon_ok: BOOL =
            DuplicateHandle(self_proc, hpc as HANDLE, proc_b, &mut dup_hpcon, 0, FALSE, DUPLICATE_SAME_ACCESS);
        let dup_hpcon_err = if dup_hpcon_ok == FALSE { GetLastError() } else { 0 };

        let msg = WinHandoverMsg {
            proc_a_pid: GetCurrentProcessId(),
            child_pid,
            dup_in_write: dup_in_write as usize,
            dup_out_read: dup_out_read as usize,
            dup_hpcon_ok: dup_hpcon_ok != FALSE,
            dup_hpcon_err,
            dup_hpcon_val: dup_hpcon as usize,
            raw_hpcon_val: hpc as usize,
        };
        write_pipe_frame(pipe, &serde_json::to_vec(&msg)?)?;
        let _ = read_pipe_frame(pipe);

        CloseHandle(proc_b);
        CloseHandle(pipe);
        CloseHandle(h_in_write);
        CloseHandle(h_out_read);

        if close_hpcon {
            ClosePseudoConsole(hpc);
        }
        // 进程 A 退出（若未调 ClosePseudoConsole，内核也会回收 A 持有的 hPtyReference / hSignal）。
        Ok(())
    }
}

pub fn run_proc_b(pipe_name: &str) -> Result<()> {
    unsafe {
        let pipe_w = to_wide(pipe_name);
        let connect_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let pipe = loop {
            let _ = WaitNamedPipeW(pipe_w.as_ptr(), 500);
            let h = CreateFileW(
                pipe_w.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            );
            if h != INVALID_HANDLE_VALUE {
                break h;
            }
            if std::time::Instant::now() >= connect_deadline {
                return Err(std::io::Error::last_os_error()).context("CreateFileW(named pipe)");
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        };

        let my_pid = GetCurrentProcessId();
        write_pipe_frame(pipe, &my_pid.to_le_bytes())?;
        let raw = read_pipe_frame(pipe)?;
        let msg: WinHandoverMsg = serde_json::from_slice(&raw)?;
        write_pipe_frame(pipe, b"ACK")?;
        CloseHandle(pipe);

        // 等待进程 A 彻底退出
        let proc_a = OpenProcess(PROCESS_SYNCHRONIZE, FALSE, msg.proc_a_pid);
        let proc_a_exited = if !proc_a.is_null() {
            let w = WaitForSingleObject(proc_a, 5000);
            CloseHandle(proc_a);
            w == 0
        } else {
            true
        };
        std::thread::sleep(std::time::Duration::from_millis(200));

        let child_alive = is_pid_alive(msg.child_pid);

        let h_in = msg.dup_in_write as HANDLE;
        let h_out = msg.dup_out_read as HANDLE;

        let probe = b"echo __CONPTY_B_ALIVE__\r\n";
        let mut written = 0u32;
        let write_ok = WriteFile(h_in, probe.as_ptr(), probe.len() as u32, &mut written, null_mut()) != FALSE;
        let write_err = if write_ok { 0 } else { GetLastError() };

        let mut buf = [0u8; 4096];
        let mut read_bytes = 0u32;
        let mut read_err = 0u32;
        let read_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let mut avail = 0u32;
            let peek_ok = PeekNamedPipe(h_out, null_mut(), 0, null_mut(), &mut avail, null_mut()) != FALSE;
            if !peek_ok {
                read_err = GetLastError();
                break;
            }
            if avail > 0 {
                let ok = ReadFile(h_out, buf.as_mut_ptr(), buf.len() as u32, &mut read_bytes, null_mut()) != FALSE;
                if !ok {
                    read_err = GetLastError();
                }
                break;
            }
            if std::time::Instant::now() >= read_deadline {
                // 管道仍存在但超时无输出，再做一次非阻塞探测或视为 0 字节
                read_err = if is_pid_alive(msg.child_pid) { 0 } else { ERROR_BROKEN_PIPE };
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        // 仅当 DuplicateHandle(HPCON) 成功时才拿复制后的句柄调 ResizePseudoConsole；
        // 若复制失败，传入 A 进程的指针值会导致未定义行为/崩溃，这里显式用 dup_hpcon_err 说明无法跨进程调用。
        let resize_hresult = if msg.dup_hpcon_ok {
            ResizePseudoConsole(msg.dup_hpcon_val as HPCON, COORD { X: 100, Y: 30 })
        } else {
            // E_HANDLE (0x80070006): HPCON 无法跨进程复制
            0x8007_0006u32 as i32
        };

        CloseHandle(h_in);
        CloseHandle(h_out);

        // 清理可能残留的测试子进程
        if child_alive {
            let ch = OpenProcess(0x0001, FALSE, msg.child_pid);
            if !ch.is_null() {
                TerminateProcess(ch, 0);
                CloseHandle(ch);
            }
        }

        let report = WinProbeReport {
            proc_a_pid: msg.proc_a_pid,
            child_pid: msg.child_pid,
            dup_hpcon_ok: msg.dup_hpcon_ok,
            dup_hpcon_err: msg.dup_hpcon_err,
            proc_a_exited,
            child_alive_after_a_exit: child_alive,
            pipe_write_ok: write_ok,
            pipe_write_err: write_err,
            pipe_read_bytes: read_bytes as usize,
            pipe_read_err: read_err,
            resize_hresult,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
        Ok(())
    }
}

pub fn run_demo(cmd: &str) -> Result<()> {
    let pipe = format!(r"\\.\pipe\looklook-conpty-proto-{}", std::process::id());
    let exe = std::env::current_exe()?;

    let mut proc_a = Command::new(&exe)
        .args(["win-proc-a", "--pipe", &pipe, "--cmd", cmd])
        .spawn()
        .context("spawn win-proc-a")?;

    std::thread::sleep(std::time::Duration::from_millis(300));
    let out_b = Command::new(&exe)
        .args(["win-proc-b", "--pipe", &pipe])
        .output()
        .context("run win-proc-b")?;

    let status_a = proc_a.wait()?;
    print!("{}", String::from_utf8_lossy(&out_b.stdout));
    if !out_b.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&out_b.stderr));
    }
    if !status_a.success() || !out_b.status.success() {
        bail!("win-demo 执行失败: a={status_a}, b={}", out_b.status);
    }
    Ok(())
}
