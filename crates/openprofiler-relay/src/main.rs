//! `openprofiler-relay`: the helper OpenProfiler uploads into remote hosts and containers.
//!
//! It speaks only through its stdio, so the daemon can reach a JVM agent through any exec channel
//! (`ssh`, `docker exec -i`, `kubectl exec -i`) without exposing a port.
//!
//! ```text
//! openprofiler-relay relay [host:]port     stdio <-> TCP; prints "OPENPROFILER_RELAY_READY <version>" first
//! openprofiler-relay list                  JSON list of local JVMs
//! openprofiler-relay attach <pid> <agent> [options]
//!                                   load the agent into a JVM, print the attach result JSON
//! openprofiler-relay dump <pid>            stream an HPROF heap dump of <pid> to stdout
//! openprofiler-relay pty-exec -- cmd ...   run cmd in a pseudo-terminal, relaying stdio
//! openprofiler-relay sha256 <file>         print the file's SHA-256 (upload verification)
//! openprofiler-relay version
//! ```

mod jvm;
mod pty;

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn usage() -> ExitCode {
    eprintln!(
        "openprofiler-relay {VERSION}\nusage: openprofiler-relay relay [host:]port | list | attach <pid> <agent> [opts] | dump <pid> | pty-exec -- cmd... | sha256 <file> | version"
    );
    ExitCode::from(2)
}

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    // Machine-readable error line for the daemon, on stdout where it looks for READY.
    println!("OPENPROFILER_RELAY_ERROR {msg}");
    eprintln!("openprofiler-relay: {msg}");
    ExitCode::from(1)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else { return usage() };
    match cmd.as_str() {
        "relay" => match args.get(1) {
            Some(target) => relay(target),
            None => usage(),
        },
        "list" => match jvm::list() {
            Ok(json) => {
                println!("{json}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(e),
        },
        "attach" => {
            let (Some(pid), Some(agent)) = (args.get(1).and_then(|p| p.parse().ok()), args.get(2)) else {
                return usage();
            };
            match jvm::attach(pid, agent, args.get(3).map(String::as_str).unwrap_or("")) {
                Ok(json) => {
                    println!("{json}");
                    ExitCode::SUCCESS
                }
                Err(e) => fail(e),
            }
        }
        "dump" => {
            let Some(pid) = args.get(1).and_then(|p| p.parse().ok()) else { return usage() };
            match jvm::dump(pid, &mut std::io::stdout().lock()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("openprofiler-relay: {e}");
                    ExitCode::from(1)
                }
            }
        }
        "pty-exec" => {
            let rest: Vec<String> = args.iter().skip(1).skip_while(|a| *a == "--").cloned().collect();
            if rest.is_empty() {
                return usage();
            }
            match pty::exec(&rest) {
                Ok(code) => ExitCode::from(code as u8),
                Err(e) => fail(e),
            }
        }
        "sha256" => {
            let Some(path) = args.get(1) else { return usage() };
            match sha256_file(path) {
                Ok(h) => {
                    println!("{h}");
                    ExitCode::SUCCESS
                }
                Err(e) => fail(e),
            }
        }
        "version" | "--version" | "-V" => {
            println!("openprofiler-relay {VERSION} {}-{}", std::env::consts::OS, std::env::consts::ARCH);
            ExitCode::SUCCESS
        }
        _ => usage(),
    }
}

fn sha256_file(path: &str) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// Connects to `target` and pipes stdin -> socket and socket -> stdout until either side closes.
fn relay(target: &str) -> ExitCode {
    let addr = if target.contains(':') { target.to_owned() } else { format!("127.0.0.1:{target}") };
    let sock = match TcpStream::connect(&addr) {
        Ok(s) => s,
        Err(e) => return fail(format!("connect {addr}: {e}")),
    };
    let _ = sock.set_nodelay(true);
    let mut out = std::io::stdout().lock();
    if writeln!(out, "OPENPROFILER_RELAY_READY {VERSION}").and_then(|_| out.flush()).is_err() {
        return ExitCode::from(1);
    }
    drop(out);
    let mut up = match sock.try_clone() {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    // stdin -> socket on a helper thread; socket -> stdout on this one.
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if up.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = up.shutdown(Shutdown::Write);
    });
    let mut down = sock;
    let mut stdout = std::io::stdout().lock();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match down.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if stdout.write_all(&buf[..n]).and_then(|_| stdout.flush()).is_err() {
                    break;
                }
            }
        }
    }
    ExitCode::SUCCESS
}
