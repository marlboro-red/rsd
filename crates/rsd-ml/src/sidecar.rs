//! The ANE embedding sidecar client (P6.1): talks to the `rsd-embed` helper,
//! which keeps Apple's NLContextualEmbedding (a Neural-Engine transformer)
//! resident and answers over a binary pipe. Model memory lives in the sidecar,
//! so it is evictable and a device fault can't crash the daemon. Behind the
//! `Embedder` trait it's just another implementation.

use rsd_vector::Embedder;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;

pub const SIDECAR_ID: &str = "rsd.ane-nl";

struct Pipes {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

pub struct SidecarEmbedder {
    helper: PathBuf,
    dim: usize,
    pipes: Mutex<Option<Pipes>>,
}

impl SidecarEmbedder {
    /// Find `rsd-embed` next to the current executable or on PATH, spawn it,
    /// and confirm it produces a usable embedding. None => sidecar unavailable
    /// (caller falls back to the in-process embedder).
    pub fn discover() -> Option<SidecarEmbedder> {
        let helper = std::env::var_os("RSD_EMBED_BIN")
            .map(PathBuf::from)
            .filter(|p| p.exists())
            .or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|e| e.parent().map(|d| d.join("rsd-embed")))
                    .filter(|p| p.exists())
            })
            .or_else(|| {
                Command::new("rsd-embed")
                    .arg("--probe")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .spawn()
                    .ok()
                    .map(|mut c| {
                        let _ = c.kill();
                        PathBuf::from("rsd-embed")
                    })
            })?;

        let (pipes, dim) = Self::spawn(&helper).ok()?;
        let s = SidecarEmbedder {
            helper,
            dim,
            pipes: Mutex::new(Some(pipes)),
        };
        // Probe: must return a finite, nonzero vector.
        let v = s.embed("probe");
        if v.len() == dim && v.iter().any(|x| *x != 0.0) && v.iter().all(|x| x.is_finite()) {
            Some(s)
        } else {
            None
        }
    }

    fn spawn(helper: &PathBuf) -> std::io::Result<(Pipes, usize)> {
        let mut child = Command::new(helper)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdin = child.stdin.take().expect("piped");
        let raw = child.stdout.take().expect("piped");

        // Bounded READY handshake: a helper stuck loading (e.g. NL assets not
        // present on a headless box) must never block us — read the first line
        // on a thread and time out.
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut stdout = BufReader::new(raw);
            let mut line = String::new();
            let r = stdout.read_line(&mut line);
            let _ = tx.send(r.map(|_| (line, stdout)));
        });
        match rx.recv_timeout(std::time::Duration::from_secs(20)) {
            Ok(Ok((line, stdout))) => {
                let _ = reader.join();
                let dim = line
                    .trim()
                    .strip_prefix("READY ")
                    .and_then(|d| d.parse::<usize>().ok())
                    .ok_or_else(|| std::io::Error::other(format!("bad READY line: {line:?}")))?;
                Ok((
                    Pipes {
                        child,
                        stdin,
                        stdout,
                    },
                    dim,
                ))
            }
            _ => {
                let _ = child.kill();
                Err(std::io::Error::other(
                    "rsd-embed did not become ready in time",
                ))
            }
        }
    }

    fn one_shot(pipes: &mut Pipes, text: &str, dim: usize) -> std::io::Result<Vec<f32>> {
        let bytes = text.as_bytes();
        let len = (bytes.len().min(16 * 1024 * 1024)) as u32;
        pipes.stdin.write_all(&len.to_le_bytes())?;
        pipes.stdin.write_all(&bytes[..len as usize])?;
        pipes.stdin.flush()?;
        let mut dbuf = [0u8; 4];
        pipes.stdout.read_exact(&mut dbuf)?;
        let out_dim = u32::from_le_bytes(dbuf) as usize;
        if out_dim != dim {
            return Err(std::io::Error::other("dim mismatch"));
        }
        let mut raw = vec![0u8; out_dim * 4];
        pipes.stdout.read_exact(&mut raw)?;
        Ok(raw
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect())
    }
}

impl Embedder for SidecarEmbedder {
    fn id(&self) -> &str {
        SIDECAR_ID
    }
    fn version(&self) -> u32 {
        1
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, text: &str) -> Vec<f32> {
        let mut guard = self.pipes.lock().unwrap();
        // Try the live sidecar; on any IO failure, respawn once and retry.
        for attempt in 0..2 {
            if guard.is_none() {
                match Self::spawn(&self.helper) {
                    Ok((p, _)) => *guard = Some(p),
                    Err(e) => {
                        tracing::warn!("rsd-embed respawn failed: {e}");
                        break;
                    }
                }
            }
            let pipes = guard.as_mut().unwrap();
            match Self::one_shot(pipes, text, self.dim) {
                Ok(v) => return v,
                Err(e) => {
                    tracing::warn!("rsd-embed request failed (attempt {attempt}): {e}");
                    let _ = pipes.child.kill();
                    *guard = None;
                }
            }
        }
        vec![0.0; self.dim] // degraded: a zero vector, never a panic
    }
}

impl Drop for SidecarEmbedder {
    fn drop(&mut self) {
        if let Some(mut p) = self.pipes.lock().unwrap().take() {
            let _ = p.child.kill();
        }
    }
}
