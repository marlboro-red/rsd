//! rsd-ipc: the wire protocol between rsd clients and the daemon (P5.3).
//!
//! Transport: Unix domain socket at `<state>/rsd.sock`, same-uid peers only
//! (checked via getpeereid on the server). Frames: `[len: u32 LE][postcard]`.
//!
//! Authorization model v1 is deny-by-default. Named principals receive an
//! explicit unrestricted or path-prefix scope; unknown principals receive no
//! data. Enforcement filters before output — results, counts, and live deltas
//! are computed over the authorized subset. The current `Hello` identity is
//! caller-asserted, so the shipped daemon configures no UDS grants until XPC
//! audit-token identity or an equivalent verifier is wired.

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// A path-rooted authorization grant.
///
/// Matching uses path components rather than string prefixes, so a grant for
/// `/root/docs` includes `/root/docs/report.txt` but not `/root/docs-private`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PathPrefix(PathBuf);

impl PathPrefix {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    pub fn matches(&self, path: impl AsRef<Path>) -> bool {
        path.as_ref().starts_with(&self.0)
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl From<String> for PathPrefix {
    fn from(path: String) -> Self {
        Self::new(path)
    }
}

impl From<&str> for PathPrefix {
    fn from(path: &str) -> Self {
        Self::new(path)
    }
}

/// Complete authorization scope for a principal.
///
/// Unrestricted access is deliberately explicit. `Paths(Vec::new())` is the
/// deny-all scope used for unknown and revoked principals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Unrestricted,
    Paths(Vec<PathPrefix>),
}

impl Default for Scope {
    fn default() -> Self {
        Self::Paths(Vec::new())
    }
}

impl Scope {
    pub fn paths(paths: impl IntoIterator<Item = impl Into<PathPrefix>>) -> Self {
        Self::Paths(paths.into_iter().map(Into::into).collect())
    }

    pub fn allows(&self, path: impl AsRef<Path>) -> bool {
        match self {
            Self::Unrestricted => true,
            Self::Paths(prefixes) => prefixes.iter().any(|prefix| prefix.matches(&path)),
        }
    }
}

pub const PROTOCOL_VERSION: u32 = 1;
const MAX_FRAME: u32 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// Identify the principal. Must be the first request on a connection.
    Hello {
        principal: String,
    },
    Query {
        rql: String,
        scope: Option<String>,
        count_only: bool,
    },
    Subscribe {
        rql: String,
    },
    /// Semantic alert: fire when new/changed content clears the similarity
    /// threshold. Threshold semantics by design (never top-k).
    SubscribeAlert {
        query: String,
        threshold: f32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Hello {
        protocol: u32,
        grammar: u32,
    },
    Hits(Vec<Hit>),
    Count(u64),
    /// Initial result set of a subscription, then a stream of Events.
    Subscribed(Vec<Hit>),
    Event {
        enter: bool,
        oid: u64,
        path: String,
    },
    Resync,
    Err(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hit {
    pub oid: u64,
    pub path: String,
}

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("encode: {0}")]
    Encode(#[from] postcard::Error),
    #[error("frame too large: {0}")]
    TooLarge(u32),
}

pub fn send<T: Serialize>(w: &mut impl Write, value: &T) -> Result<(), IpcError> {
    let payload = postcard::to_allocvec(value)?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    w.write_all(&frame)?;
    Ok(())
}

pub fn recv<T: for<'de> Deserialize<'de>>(r: &mut impl Read) -> Result<T, IpcError> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME {
        return Err(IpcError::TooLarge(len));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    Ok(postcard::from_bytes(&payload)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_is_deny_by_default_and_unrestricted_is_explicit() {
        assert!(!Scope::default().allows("/root/anything"));
        assert!(Scope::Unrestricted.allows("/root/anything"));
    }

    #[test]
    fn path_scope_matches_components_not_string_prefixes() {
        let scope = Scope::paths(["/root/docs"]);
        assert!(scope.allows("/root/docs"));
        assert!(scope.allows("/root/docs/report.txt"));
        assert!(!scope.allows("/root/docs-private/report.txt"));
        assert!(!scope.allows("/root/doc"));
    }
}
