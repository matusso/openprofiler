//! `pty-exec`: run a command attached to a pseudo-terminal (for tools that insist on a TTY, such
//! as interactive `sudo` or login shells), relaying it to our stdio.

use std::ffi::CString;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;

pub fn exec(argv: &[String]) -> Result<i32, String> {
    let cargs: Vec<CString> =
        argv.iter().map(|a| CString::new(a.as_str()).map_err(|e| e.to_string())).collect::<Result<_, _>>()?;
    let mut ptrs: Vec<*const libc::c_char> = cargs.iter().map(|c| c.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    let (mut master, mut slave) = (0, 0);
    // SAFETY: openpty writes two fds into the provided out-params; name/termios/winsize may be null.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if rc != 0 {
        return Err(format!("openpty: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: fork has no preconditions; in the child we only call async-signal-safe functions
    // (setsid, ioctl, dup2, close, execvp, _exit) before exec.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("fork: {}", std::io::Error::last_os_error()));
    }
    if pid == 0 {
        // SAFETY: child process after fork; see above.
        unsafe {
            libc::setsid();
            libc::ioctl(slave, libc::TIOCSCTTY as _, 0);
            libc::dup2(slave, 0);
            libc::dup2(slave, 1);
            libc::dup2(slave, 2);
            if slave > 2 {
                libc::close(slave);
            }
            libc::close(master);
            libc::execvp(ptrs[0], ptrs.as_ptr());
            libc::_exit(127);
        }
    }
    // SAFETY: `slave` is a valid fd we own and no longer need in the parent.
    unsafe { libc::close(slave) };
    // SAFETY: `master` is a valid fd owned by us; each File takes its own dup'd fd.
    let mut from_pty = unsafe { std::fs::File::from_raw_fd(master) };
    let mut to_pty = from_pty.try_clone().map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 8192];
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 || to_pty.write_all(&buf[..n]).is_err() {
                break;
            }
        }
    });
    let mut stdout = std::io::stdout().lock();
    let mut buf = [0u8; 8192];
    // Reading the master fails with EIO once the child side closes.
    while let Ok(n) = from_pty.read(&mut buf) {
        if n == 0 || stdout.write_all(&buf[..n]).and_then(|_| stdout.flush()).is_err() {
            break;
        }
    }
    let mut status = 0;
    // SAFETY: waiting for our own child pid.
    unsafe { libc::waitpid(pid, &mut status, 0) };
    Ok(if libc::WIFEXITED(status) { libc::WEXITSTATUS(status) } else { 128 + libc::WTERMSIG(status) })
}
