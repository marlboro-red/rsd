//! The sealed extraction worker. Sequence:
//!   1. claim the socketpair end the host wired to stdin;
//!   2. warm anything that needs ambient authority (none today);
//!   3. `rsd_sandbox::seal()` — deny-default, irreversible;
//!   4. serve extract requests: everything arrives as fds, nothing else is
//!      reachable.
//!
//! Refuses to run unsealed unless RSD_WORKER_NO_SANDBOX=1 (tests only).

use rsd_extract::extract_bytes;
use rsd_worker::{recv_fd_frame, recv_payload, send_payload, WorkerRequest, WorkerResponse};
use std::io::Read;

fn main() {
    let mut sock = rsd_sandbox::stdin_unix_stream();

    let sealed = if std::env::var_os("RSD_WORKER_NO_SANDBOX").is_some() {
        false
    } else {
        match rsd_sandbox::seal() {
            Ok(()) => true,
            Err(e) => {
                eprintln!("rsd-worker: refusing to run unsealed: {e}");
                std::process::exit(3);
            }
        }
    };

    loop {
        let fd = match recv_fd_frame(&sock) {
            Ok(fd) => fd,
            Err(_) => return, // host closed or protocol failure: exit quietly
        };
        let req: WorkerRequest = match recv_payload(&mut sock) {
            Ok(r) => r,
            Err(_) => return,
        };
        let resp = match req {
            WorkerRequest::Extract { hints, budgets } => match fd {
                Some(fd) => {
                    let mut file = std::fs::File::from(fd);
                    let cap = budgets.max_input_bytes;
                    let mut bytes = Vec::new();
                    // Read at most the input budget; full_size in hints tells
                    // the extractor whether it saw everything.
                    match file.by_ref().take(cap).read_to_end(&mut bytes) {
                        Ok(_) => WorkerResponse::Extracted(extract_bytes(&hints, &budgets, &bytes)),
                        Err(e) => WorkerResponse::Err(format!("read: {e}")),
                    }
                }
                None => WorkerResponse::Err("Extract without fd".into()),
            },
            WorkerRequest::SandboxProbe => WorkerResponse::Probe {
                sealed,
                open_etc_passwd: std::fs::File::open("/etc/passwd").is_ok(),
                spawn_ok: std::process::Command::new("/usr/bin/true").status().is_ok(),
            },
            WorkerRequest::CrashSelf => std::process::abort(),
            WorkerRequest::HangSelf => loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            },
        };
        if send_payload(&mut sock, &resp).is_err() {
            return;
        }
    }
}
