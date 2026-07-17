//! rsd-worker: the sandboxed extraction worker protocol and host-side pool
//! (P3.1).
//!
//! Capability model (DESIGN.md P3): the host opens the file read-only and
//! passes the *fd* over the socketpair (`SCM_RIGHTS`). The worker sealed
//! itself with a deny-default Seatbelt profile at startup, so the fd is the
//! only thing in the universe it can read. Hostile input's blast radius is
//! one worker process, one request.
//!
//! Wire protocol per request:
//!   1. fd frame: 1 marker byte, with the file fd as SCM_RIGHTS ancillary
//!      data when the request carries one;
//!   2. payload frame: `[len: u32 LE][postcard(WorkerRequest)]`.
//!
//! Responses are plain payload frames (no fds). Small, allocation-light, one
//! syscall per frame.

use nix::sys::socket::{recvmsg, sendmsg, ControlMessage, ControlMessageOwned, MsgFlags};
use rsd_caes::ExtractionRecord;
use rsd_extract::{Budgets, ExtractHints};
use serde::{Deserialize, Serialize};
use std::io::{IoSlice, IoSliceMut, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

pub const FD_FRAME_MARKER: u8 = 0xFD;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WorkerRequest {
    /// Extract the file whose fd rode in on the fd frame.
    Extract {
        hints: ExtractHints,
        budgets: Budgets,
    },
    /// Test/diagnostic: report what ambient authority the worker still has.
    SandboxProbe,
    /// Test hooks: die/hang on command (hostile-input simulation).
    CrashSelf,
    HangSelf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WorkerResponse {
    Extracted(ExtractionRecord),
    Probe {
        sealed: bool,
        open_etc_passwd: bool,
        spawn_ok: bool,
    },
    Err(String),
}

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("encode: {0}")]
    Encode(#[from] postcard::Error),
    #[error("worker crashed mid-request")]
    Crashed,
    #[error("worker exceeded request timeout")]
    Timeout,
    #[error("protocol violation: {0}")]
    Protocol(String),
}

pub type Result<T> = std::result::Result<T, WorkerError>;

fn nix_err(e: nix::errno::Errno) -> WorkerError {
    WorkerError::Io(std::io::Error::from_raw_os_error(e as i32))
}

/// Send the fd frame: one marker byte, fd (if any) as ancillary data.
pub fn send_fd_frame(sock: &UnixStream, fd: Option<&OwnedFd>) -> Result<()> {
    let buf = [FD_FRAME_MARKER];
    let iov = [IoSlice::new(&buf)];
    let raw;
    let cmsgs: &[ControlMessage] = match fd {
        Some(f) => {
            raw = [f.as_raw_fd()];
            &[ControlMessage::ScmRights(&raw)]
        }
        None => &[],
    };
    sendmsg::<()>(sock.as_raw_fd(), &iov, cmsgs, MsgFlags::empty(), None).map_err(nix_err)?;
    Ok(())
}

/// Receive the fd frame; returns the fd if one rode along. `Ok(None)` marker
/// with no fd is valid (non-Extract requests). EOF => peer closed.
pub fn recv_fd_frame(sock: &UnixStream) -> Result<Option<OwnedFd>> {
    let mut buf = [0u8; 1];
    let mut iov = [IoSliceMut::new(&mut buf)];
    let mut cmsg_buf = nix::cmsg_space!([std::os::fd::RawFd; 1]);
    let msg = recvmsg::<()>(
        sock.as_raw_fd(),
        &mut iov,
        Some(&mut cmsg_buf),
        MsgFlags::empty(),
    )
    .map_err(nix_err)?;
    if msg.bytes == 0 {
        return Err(WorkerError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "peer closed",
        )));
    }
    let mut fd = None;
    for cmsg in msg.cmsgs().map_err(nix_err)? {
        if let ControlMessageOwned::ScmRights(fds) = cmsg {
            for raw in fds {
                // Safety note: ownership transfer of a freshly received fd.
                fd = Some(unsafe_owned(raw));
            }
        }
    }
    if buf[0] != FD_FRAME_MARKER {
        return Err(WorkerError::Protocol(format!(
            "bad fd-frame marker {:#x}",
            buf[0]
        )));
    }
    Ok(fd)
}

// nix hands back RawFds from SCM_RIGHTS; wrapping them is the one ownership
// assertion this crate makes. Kept in one place, no other unsafe exists here.
#[allow(unsafe_code)]
fn unsafe_owned(raw: std::os::fd::RawFd) -> OwnedFd {
    use std::os::fd::FromRawFd;
    unsafe { OwnedFd::from_raw_fd(raw) }
}

pub fn send_payload<T: Serialize>(sock: &mut UnixStream, value: &T) -> Result<()> {
    let payload = postcard::to_allocvec(value)?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    sock.write_all(&frame)?;
    Ok(())
}

pub fn recv_payload<T: for<'de> Deserialize<'de>>(sock: &mut UnixStream) -> Result<T> {
    let mut len_buf = [0u8; 4];
    sock.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > 64 * 1024 * 1024 {
        return Err(WorkerError::Protocol(format!("frame too large: {len}")));
    }
    let mut payload = vec![0u8; len];
    sock.read_exact(&mut payload)?;
    Ok(postcard::from_bytes(&payload)?)
}

// ---------------------------------------------------------------- host pool

pub struct PoolConfig {
    pub size: usize,
    pub request_timeout: Duration,
    /// Path to the rsd-worker binary; default: alongside current_exe.
    pub worker_path: Option<PathBuf>,
    /// Disable the worker's self-seal (tests of the probe itself only).
    pub no_sandbox: bool,
}

impl Default for PoolConfig {
    fn default() -> Self {
        PoolConfig {
            size: 2,
            request_timeout: Duration::from_secs(10),
            worker_path: None,
            no_sandbox: false,
        }
    }
}

struct WorkerHandle {
    child: Child,
    sock: UnixStream,
}

/// Fixed pool of sealed workers. Crashes and timeouts kill + respawn the
/// worker; the daemon is never taken down by input (P3 pillar).
pub struct WorkerPool {
    cfg: PoolConfig,
    workers: Vec<WorkerHandle>,
    next: usize,
    pub respawns: u64,
}

fn default_worker_path() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = exe
        .parent()
        .ok_or_else(|| std::io::Error::other("no exe dir"))?;
    // Test binaries live in target/debug/deps; the worker lives in target/debug.
    for cand in [dir.join("rsd-worker"), dir.join("../rsd-worker")] {
        if cand.exists() {
            return cand.canonicalize();
        }
    }
    Err(std::io::Error::other(
        "rsd-worker binary not found near current_exe",
    ))
}

impl WorkerPool {
    pub fn new(cfg: PoolConfig) -> Result<WorkerPool> {
        let mut pool = WorkerPool {
            cfg,
            workers: Vec::new(),
            next: 0,
            respawns: 0,
        };
        for _ in 0..pool.cfg.size.max(1) {
            let w = pool.spawn_worker()?;
            pool.workers.push(w);
        }
        Ok(pool)
    }

    fn spawn_worker(&self) -> Result<WorkerHandle> {
        let path = match &self.cfg.worker_path {
            Some(p) => p.clone(),
            None => default_worker_path()?,
        };
        let (host_end, worker_end) = UnixStream::pair()?;
        let mut cmd = Command::new(&path);
        cmd.stdin(Stdio::from(std::os::fd::OwnedFd::from(worker_end)))
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        if self.cfg.no_sandbox {
            cmd.env("RSD_WORKER_NO_SANDBOX", "1");
        }
        let child = cmd.spawn()?;
        host_end.set_read_timeout(Some(self.cfg.request_timeout))?;
        Ok(WorkerHandle {
            child,
            sock: host_end,
        })
    }

    fn respawn(&mut self, idx: usize) -> Result<()> {
        let _ = self.workers[idx].child.kill();
        let _ = self.workers[idx].child.wait();
        self.workers[idx] = self.spawn_worker()?;
        self.respawns += 1;
        Ok(())
    }

    /// One request/response round trip against the next worker. Crash => kill
    /// + respawn + `Crashed`; timeout => kill + respawn + `Timeout`.
    pub fn request(&mut self, req: &WorkerRequest, fd: Option<OwnedFd>) -> Result<WorkerResponse> {
        let idx = self.next % self.workers.len();
        self.next = self.next.wrapping_add(1);

        let round_trip = (|| -> Result<WorkerResponse> {
            let w = &mut self.workers[idx];
            send_fd_frame(&w.sock, fd.as_ref())?;
            send_payload(&mut w.sock, req)?;
            recv_payload::<WorkerResponse>(&mut w.sock)
        })();

        match round_trip {
            Ok(resp) => Ok(resp),
            Err(WorkerError::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                self.respawn(idx)?;
                Err(WorkerError::Timeout)
            }
            Err(WorkerError::Io(_)) | Err(WorkerError::Protocol(_)) => {
                self.respawn(idx)?;
                Err(WorkerError::Crashed)
            }
            Err(e) => Err(e),
        }
    }

    /// Extract a file: open read-only here, hand the fd across the boundary.
    pub fn extract(
        &mut self,
        path: &Path,
        hints: ExtractHints,
        budgets: Budgets,
    ) -> Result<ExtractionRecord> {
        let file = std::fs::File::open(path)?;
        self.extract_fd(file, hints, budgets)
    }

    /// Extract from an already-open, identity-pinned file. This is the daemon
    /// fast path: hashing and extraction share one open file description, so
    /// a path replacement cannot switch bytes between the two operations.
    pub fn extract_fd(
        &mut self,
        file: std::fs::File,
        hints: ExtractHints,
        budgets: Budgets,
    ) -> Result<ExtractionRecord> {
        match self.request(
            &WorkerRequest::Extract { hints, budgets },
            Some(OwnedFd::from(file)),
        )? {
            WorkerResponse::Extracted(rec) => Ok(rec),
            WorkerResponse::Err(e) => Err(WorkerError::Protocol(e)),
            other => Err(WorkerError::Protocol(format!(
                "unexpected response {other:?}"
            ))),
        }
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        for w in &mut self.workers {
            let _ = w.child.kill();
            let _ = w.child.wait();
        }
    }
}
