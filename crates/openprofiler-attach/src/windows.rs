//! HotSpot dynamic attach on Windows.
//!
//! HotSpot has no attach socket on Windows. A client creates a named pipe, injects a thread into
//! the JVM that calls `JVM_EnqueueOperation(cmd, arg0, arg1, arg2, pipe)` exported by `jvm.dll`,
//! and the JVM's attach listener connects to the pipe and writes `<status>\n<output>` once the
//! operation has run (the same mechanism as the JDK's `VirtualMachineImpl` and jattach).
//!
//! The injected thread runs a small x86-64 stub copied into the target together with its data:
//! `GetModuleHandleA("jvm.dll")`, `GetProcAddress(.., "JVM_EnqueueOperation")`, then the call.
//! `kernel32.dll` is mapped at the same address in every process of a boot session, so our own
//! `GetModuleHandleA`/`GetProcAddress` addresses are valid in the target. Both processes must be
//! 64-bit x86; other architectures are refused.

use anyhow::{Context, Result, bail};
use std::ffi::c_void;
use std::time::Duration;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_PIPE_CONNECTED, GENERIC_WRITE, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE, STILL_ACTIVE,
};
use windows_sys::Win32::Storage::FileSystem::{CreateFileA, OPEN_EXISTING, PIPE_ACCESS_INBOUND, ReadFile};
use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READ, PAGE_READWRITE, VirtualAllocEx, VirtualFreeEx,
    VirtualProtectEx,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeA, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    CreateRemoteThread, GetCurrentProcess, GetExitCodeProcess, GetExitCodeThread, INFINITE, IsWow64Process,
    OpenProcess, PROCESS_CREATE_THREAD, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE, WaitForSingleObject,
};

/// `JVM_EnqueueOperation` limits (`AttachOperation::name_length_max`, `arg_length_max` and
/// `Win32AttachOperation::pipe_name_max`).
const NAME_MAX: usize = 16;
const ARG_MAX: usize = 1024;
const PIPE_MAX: usize = 256;

// Layout of the data block the stub receives in RCX.
const OFF_GET_MODULE: u32 = 0;
const OFF_GET_PROC: u32 = 8;
const OFF_JVM: u32 = 16;
const OFF_ENQUEUE: u32 = 32;
const OFF_PIPE: u32 = 64;
const OFF_ARGS: u32 = OFF_PIPE + PIPE_MAX as u32;
const ARG_SLOT: u32 = ARG_MAX as u32 + 1;
const DATA_LEN: usize = (OFF_ARGS + 4 * ARG_SLOT) as usize;

/// Exit code of the stub when `jvm.dll` or `JVM_EnqueueOperation` is missing.
const STUB_NO_JVM: u32 = 0xffff;

/// Owned Win32 handle.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle was returned by a successful Win32 call and is closed once.
        unsafe { CloseHandle(self.0) };
    }
}

fn last_error() -> std::io::Error {
    std::io::Error::last_os_error()
}

/// Whether `pid` names a running process.
pub fn process_alive(pid: u32) -> bool {
    // SAFETY: plain process queries; the handle is closed by `Handle`.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            // Access denied still means the process exists.
            return GetLastError() == windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
        }
        let h = Handle(h);
        let mut code = 0u32;
        GetExitCodeProcess(h.0, &mut code) != 0 && code == STILL_ACTIVE as u32
    }
}

/// The x86-64 thread entry: `rcx` points at the data block.
///
/// ```text
/// push rbx; sub rsp, 0x30; mov rbx, rcx
/// lea rcx, [rbx+JVM];     call [rbx+GET_MODULE]; test rax, rax; jz fail
/// mov rcx, rax; lea rdx, [rbx+ENQUEUE]; call [rbx+GET_PROC]; test rax, rax; jz fail
/// lea rcx/rdx/r8/r9, [rbx+ARG0..3]; lea r10, [rbx+PIPE]; mov [rsp+0x20], r10; call rax; jmp done
/// fail: mov eax, 0xffff
/// done: add rsp, 0x30; pop rbx; ret
/// ```
fn stub_x64() -> Vec<u8> {
    let mut c = vec![0x53, 0x48, 0x83, 0xEC, 0x30, 0x48, 0x89, 0xCB];
    fn rbx_disp(c: &mut Vec<u8>, prefix: &[u8], off: u32) {
        c.extend_from_slice(prefix);
        c.extend_from_slice(&off.to_le_bytes());
    }
    let mut fail_jumps = vec![];
    rbx_disp(&mut c, &[0x48, 0x8D, 0x8B], OFF_JVM); // lea rcx, [rbx+d32]
    rbx_disp(&mut c, &[0xFF, 0x93], OFF_GET_MODULE); // call [rbx+d32]
    c.extend_from_slice(&[0x48, 0x85, 0xC0, 0x74, 0]); // test rax, rax; jz fail
    fail_jumps.push(c.len() - 1);
    c.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
    rbx_disp(&mut c, &[0x48, 0x8D, 0x93], OFF_ENQUEUE); // lea rdx, [rbx+d32]
    rbx_disp(&mut c, &[0xFF, 0x93], OFF_GET_PROC); // call [rbx+d32]
    c.extend_from_slice(&[0x48, 0x85, 0xC0, 0x74, 0]); // test rax, rax; jz fail
    fail_jumps.push(c.len() - 1);
    rbx_disp(&mut c, &[0x48, 0x8D, 0x8B], OFF_ARGS); // lea rcx, cmd
    rbx_disp(&mut c, &[0x48, 0x8D, 0x93], OFF_ARGS + ARG_SLOT); // lea rdx, arg0
    rbx_disp(&mut c, &[0x4C, 0x8D, 0x83], OFF_ARGS + 2 * ARG_SLOT); // lea r8, arg1
    rbx_disp(&mut c, &[0x4C, 0x8D, 0x8B], OFF_ARGS + 3 * ARG_SLOT); // lea r9, arg2
    rbx_disp(&mut c, &[0x4C, 0x8D, 0x93], OFF_PIPE); // lea r10, pipe
    c.extend_from_slice(&[0x4C, 0x89, 0x54, 0x24, 0x20]); // mov [rsp+0x20], r10
    c.extend_from_slice(&[0xFF, 0xD0, 0xEB, 5]); // call rax; jmp done (over the 5-byte mov)
    let fail = c.len();
    for j in fail_jumps {
        c[j] = (fail - (j + 1)) as u8;
    }
    c.push(0xB8); // mov eax, imm32
    c.extend_from_slice(&STUB_NO_JVM.to_le_bytes());
    c.extend_from_slice(&[0x48, 0x83, 0xC4, 0x30, 0x5B, 0xC3]); // add rsp, 0x30; pop rbx; ret
    c
}

fn put_str(data: &mut [u8], off: u32, s: &str, max: usize, what: &str) -> Result<()> {
    if s.len() > max || s.as_bytes().contains(&0) {
        bail!("attach {what} too long or contains NUL ({} bytes, at most {max})", s.len());
    }
    let off = off as usize;
    data[off..off + s.len()].copy_from_slice(s.as_bytes());
    Ok(())
}

fn data_block(pipe: &str, cmd: &str, args: &[&str]) -> Result<Vec<u8>> {
    let mut d = vec![0u8; DATA_LEN];
    // SAFETY: kernel32 is always loaded; the names are NUL-terminated.
    let (get_module, get_proc) = unsafe {
        let k32 = GetModuleHandleA(c"kernel32.dll".as_ptr() as _);
        (
            GetProcAddress(k32, c"GetModuleHandleA".as_ptr() as _).context("GetModuleHandleA")? as usize,
            GetProcAddress(k32, c"GetProcAddress".as_ptr() as _).context("GetProcAddress")? as usize,
        )
    };
    d[OFF_GET_MODULE as usize..][..8].copy_from_slice(&(get_module as u64).to_le_bytes());
    d[OFF_GET_PROC as usize..][..8].copy_from_slice(&(get_proc as u64).to_le_bytes());
    put_str(&mut d, OFF_JVM, "jvm.dll", 15, "module")?;
    put_str(&mut d, OFF_ENQUEUE, "JVM_EnqueueOperation", 31, "function")?;
    put_str(&mut d, OFF_PIPE, pipe, PIPE_MAX - 1, "pipe name")?;
    put_str(&mut d, OFF_ARGS, cmd, NAME_MAX, "command")?;
    for i in 0..3 {
        let a = args.get(i).copied().unwrap_or("");
        put_str(&mut d, OFF_ARGS + (i as u32 + 1) * ARG_SLOT, a, ARG_MAX, "argument")?;
    }
    Ok(d)
}

fn is_wow64(process: HANDLE) -> bool {
    let mut w = 0;
    // SAFETY: valid process handle and out pointer.
    unsafe { IsWow64Process(process, &mut w) != 0 && w != 0 }
}

/// Memory allocated in the target, freed on drop.
struct Remote<'a> {
    process: &'a Handle,
    addr: *mut c_void,
}

impl<'a> Remote<'a> {
    fn write(process: &'a Handle, bytes: &[u8], executable: bool) -> Result<Self> {
        // SAFETY: allocation and writes in the target through a handle opened with VM rights.
        unsafe {
            let addr = VirtualAllocEx(
                process.0,
                std::ptr::null(),
                bytes.len(),
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            );
            if addr.is_null() {
                bail!("VirtualAllocEx: {}", last_error());
            }
            let r = Self { process, addr };
            let mut n = 0;
            if WriteProcessMemory(process.0, addr, bytes.as_ptr() as _, bytes.len(), &mut n) == 0
                || n != bytes.len()
            {
                bail!("WriteProcessMemory: {}", last_error());
            }
            let mut old = 0;
            if executable && VirtualProtectEx(process.0, addr, bytes.len(), PAGE_EXECUTE_READ, &mut old) == 0
            {
                bail!("VirtualProtectEx: {}", last_error());
            }
            Ok(r)
        }
    }
}

impl Drop for Remote<'_> {
    fn drop(&mut self) {
        // SAFETY: frees the allocation made in `write`, after the remote thread has finished.
        unsafe { VirtualFreeEx(self.process.0, self.addr, 0, MEM_RELEASE) };
    }
}

fn enqueue_error(code: u32) -> String {
    match code {
        STUB_NO_JVM => "jvm.dll or JVM_EnqueueOperation not found (not a HotSpot JVM?)".into(),
        100 => "attach mechanism disabled (-XX:+DisableAttachMechanism)".into(),
        101 => "attach listener not started".into(),
        102 => "illegal argument".into(),
        103 => "internal error".into(),
        104 => "out of resources".into(),
        c => format!("error {c}"),
    }
}

/// Reads the pipe to its end (the JVM closes it after writing the reply).
fn read_all(pipe: &Handle) -> Result<String> {
    let mut out = vec![];
    let mut buf = [0u8; 8192];
    loop {
        let mut n = 0u32;
        // SAFETY: synchronous read into a local buffer.
        let ok =
            unsafe { ReadFile(pipe.0, buf.as_mut_ptr(), buf.len() as u32, &mut n, std::ptr::null_mut()) };
        if ok == 0 {
            // SAFETY: reads the calling thread's last error.
            match unsafe { GetLastError() } {
                ERROR_BROKEN_PIPE => break,
                _ => bail!("reading the attach pipe: {}", last_error()),
            }
        }
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// Sends one attach-protocol command and returns `(status, output)`.
pub fn attach_command(pid: u32, cmd: &str, args: &[&str]) -> Result<(i32, String)> {
    if !cfg!(target_arch = "x86_64") {
        bail!("attaching to local JVMs on Windows is only implemented for x86-64");
    }
    let pipe_name = format!(r"\\.\pipe\javatool{}", super::random_token());
    let data = data_block(&pipe_name, cmd, args)?;
    let c_pipe = std::ffi::CString::new(pipe_name.clone())?;
    // SAFETY: Win32 calls with valid arguments; every handle is owned by a `Handle`.
    unsafe {
        let access = PROCESS_CREATE_THREAD
            | PROCESS_QUERY_INFORMATION
            | PROCESS_VM_OPERATION
            | PROCESS_VM_WRITE
            | PROCESS_VM_READ;
        let process = OpenProcess(access, 0, pid);
        if process.is_null() {
            bail!("cannot open process {pid}: {}", last_error());
        }
        let process = Handle(process);
        if is_wow64(process.0) != is_wow64(GetCurrentProcess()) {
            bail!("process {pid} is not a 64-bit process");
        }
        let pipe = CreateNamedPipeA(
            c_pipe.as_ptr() as _,
            PIPE_ACCESS_INBOUND,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            4096,
            8192,
            0,
            std::ptr::null(),
        );
        if pipe == INVALID_HANDLE_VALUE {
            bail!("cannot create pipe {pipe_name}: {}", last_error());
        }
        let pipe = Handle(pipe);
        let code = Remote::write(&process, &stub_x64(), true)?;
        let param = Remote::write(&process, &data, false)?;
        let thread = CreateRemoteThread(
            process.0,
            std::ptr::null(),
            0,
            Some(std::mem::transmute::<*mut c_void, unsafe extern "system" fn(*mut c_void) -> u32>(
                code.addr,
            )),
            param.addr,
            0,
            std::ptr::null_mut(),
        );
        if thread.is_null() {
            bail!("cannot start a thread in process {pid}: {}", last_error());
        }
        let thread = Handle(thread);
        WaitForSingleObject(thread.0, INFINITE);
        let mut rc = 0u32;
        GetExitCodeThread(thread.0, &mut rc);
        drop((code, param));
        if rc != 0 {
            bail!("JVM {pid} refused the attach operation: {}", enqueue_error(rc));
        }
        connect(&pipe, &c_pipe, Duration::from_secs(120))
            .with_context(|| format!("JVM {pid} did not answer on {pipe_name}"))?;
        let resp = read_all(&pipe)?;
        let (status, rest) = resp.split_once('\n').unwrap_or((resp.as_str(), ""));
        let status = status.trim().parse().with_context(|| format!("bad attach response: {resp:?}"))?;
        Ok((status, rest.to_string()))
    }
}

/// Waits for the JVM to connect to `pipe`. After `timeout` a dummy client connects and closes,
/// which releases the blocked `ConnectNamedPipe`, and an error is returned.
fn connect(pipe: &Handle, name: &std::ffi::CStr, timeout: Duration) -> Result<()> {
    let raw = pipe.0 as usize;
    let (tx, rx) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        // SAFETY: the pipe handle outlives this thread (it is joined below).
        let ok = unsafe { ConnectNamedPipe(raw as HANDLE, std::ptr::null_mut()) } != 0
            || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
        let _ = tx.send(ok);
    });
    let result = match rx.recv_timeout(timeout) {
        Ok(true) => Ok(()),
        Ok(false) => Err(anyhow::anyhow!("ConnectNamedPipe: {}", last_error())),
        Err(_) => {
            // SAFETY: opens the client end of our own pipe and closes it at once.
            let h = unsafe {
                CreateFileA(
                    name.as_ptr() as _,
                    GENERIC_WRITE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if h != INVALID_HANDLE_VALUE {
                drop(Handle(h));
            }
            Err(anyhow::anyhow!("timed out after {timeout:?}"))
        }
    };
    let _ = waiter.join();
    result
}

/// `std::fs::canonicalize` returns verbatim (`\\?\C:\...`) paths; HotSpot's agent loader expects
/// a plain absolute path.
pub fn plain_path(p: &str) -> &str {
    match p.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest,
        _ => p,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_jumps_land_on_fail() {
        let c = stub_x64();
        // fail: mov eax, 0xffff
        let fail = c.len() - 11;
        assert_eq!(c[fail], 0xB8);
        let mut jumps = 0;
        for i in 0..c.len() - 1 {
            if c[i] == 0x74 && i >= 3 && c[i - 3..i] == [0x48, 0x85, 0xC0] {
                assert_eq!(i + 2 + c[i + 1] as usize, fail);
                jumps += 1;
            }
        }
        assert_eq!(jumps, 2);
        // `jmp done` skips exactly the fail block's mov.
        assert_eq!(c[fail - 2..fail], [0xEB, 5]);
    }

    #[test]
    fn data_block_rejects_long_arguments() {
        assert!(data_block(r"\\.\pipe\x", "load", &["a"; 3]).is_ok());
        assert!(data_block(r"\\.\pipe\x", "load", &[&"a".repeat(ARG_MAX + 1)]).is_err());
        assert!(data_block(r"\\.\pipe\x", "a_much_too_long_command", &[]).is_err());
    }

    #[test]
    fn strips_verbatim_prefix() {
        assert_eq!(plain_path(r"\\?\C:\a\agent.dll"), r"C:\a\agent.dll");
        assert_eq!(plain_path(r"\\?\UNC\srv\share"), r"\\?\UNC\srv\share");
    }
}
