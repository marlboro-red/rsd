//! rsd-ipc: the wire protocol between rsd clients and the daemon (P5.3).
//!
//! Transport: Unix domain socket at `<state>/rsd.sock`, same-uid peers only
//! (checked via getpeereid on the server). Frames: `[len: u32 LE][postcard]`.
//!
//! Authorization model v1 is deny-by-default. Enforcement filters before
//! output — results, counts, and live deltas are computed over the authorized
//! subset.
//!
//! A caller obtains a scope one of two ways:
//!
//!  - **First-party token.** `Hello.token` carries the loopback secret from
//!    `<state>/http.token` (0600, same-uid readable, regenerated each start).
//!    A constant-time match grants `Scope::Unrestricted` — the same authority
//!    the HTTP surface already gives any holder of that file.
//!  - **Named grant.** `Hello.principal` selects a configured grant. The
//!    identity is caller-asserted, so the shipped daemon configures no such
//!    grants until XPC audit-token identity or an equivalent verifier is wired.
//!    An unlisted principal presenting no token receives nothing.
//!
//! `Hello.restrict_to` lets a client voluntarily ask for *less* than it is
//! entitled to. The effective scope is the intersection, computed daemon-side,
//! so a client's self-restriction is enforced by the server rather than
//! trusted to the client's own filtering.

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

    /// The scope allowing exactly the paths both scopes allow.
    ///
    /// For prefix sets the intersection is itself a prefix set: a requested
    /// root survives if some grant contains it (the grant is broader), and a
    /// grant survives if some requested root contains it (the request is
    /// broader). Anything else is disjoint and drops out, so the result can
    /// never allow a path that either input denied.
    pub fn intersect(&self, other: &Scope) -> Scope {
        match (self, other) {
            (Self::Unrestricted, Self::Unrestricted) => Self::Unrestricted,
            (Self::Unrestricted, narrow) | (narrow, Self::Unrestricted) => narrow.clone(),
            (Self::Paths(left), Self::Paths(right)) => {
                let mut kept: Vec<PathPrefix> = Vec::new();
                for candidate in left.iter().chain(right.iter()) {
                    let allowed_by_both = Self::Paths(left.clone()).allows(candidate.as_path())
                        && Self::Paths(right.clone()).allows(candidate.as_path());
                    if allowed_by_both && !kept.contains(candidate) {
                        kept.push(candidate.clone());
                    }
                }
                Self::Paths(kept)
            }
        }
    }
}

/// v2 added token auth + `restrict_to` on `Hello`, and the `Hybrid` and
/// `Snippets` requests that let one-shot clients run entirely over IPC.
pub const PROTOCOL_VERSION: u32 = 2;
const MAX_FRAME: u32 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// Identify and authorize. Must be the first request on a connection.
    Hello {
        principal: String,
        /// Loopback secret from `<state>/http.token`; grants first-party
        /// authority on a constant-time match. `None` falls back to the
        /// named-grant table, which is deny-by-default.
        token: Option<String>,
        /// Voluntary self-restriction: serve no more than these roots. The
        /// daemon intersects it with the granted scope.
        restrict_to: Option<Vec<String>>,
    },
    Query {
        rql: String,
        scope: Option<String>,
        count_only: bool,
    },
    /// Fused lexical+semantic retrieval. Not expressible as RQL, so it is its
    /// own request rather than a query string the client has to construct.
    Hybrid {
        query: String,
        scope: Option<String>,
        limit: u32,
    },
    /// Hybrid search returning grounded excerpts for citation.
    Snippets {
        query: String,
        scope: Option<String>,
        limit: u32,
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
    Snippets(Vec<Snippet>),
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

/// A grounded excerpt: the byte range is into the extracted text the daemon
/// holds, so an agent can cite a span rather than paraphrase from memory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Snippet {
    pub path: String,
    pub start: u64,
    pub end: u64,
    pub text: String,
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

    #[test]
    fn intersection_never_widens_either_input() {
        // Unrestricted is the identity; narrowing against it yields the narrow side.
        assert_eq!(
            Scope::Unrestricted.intersect(&Scope::Unrestricted),
            Scope::Unrestricted
        );
        let narrow = Scope::paths(["/root/docs"]);
        assert_eq!(Scope::Unrestricted.intersect(&narrow), narrow);
        assert_eq!(narrow.intersect(&Scope::Unrestricted), narrow);

        // A request narrower than the grant is honored at the request's width.
        let granted = Scope::paths(["/root"]);
        let requested = Scope::paths(["/root/docs"]);
        let effective = granted.intersect(&requested);
        assert!(effective.allows("/root/docs/a.txt"));
        assert!(!effective.allows("/root/other/a.txt"));

        // A request WIDER than the grant cannot escalate past the grant.
        let granted = Scope::paths(["/root/docs"]);
        let requested = Scope::paths(["/root"]);
        let effective = granted.intersect(&requested);
        assert!(effective.allows("/root/docs/a.txt"));
        assert!(!effective.allows("/root/secrets/a.txt"));

        // Disjoint roots intersect to deny-all, not to either side.
        let effective = Scope::paths(["/root/a"]).intersect(&Scope::paths(["/root/b"]));
        assert!(!effective.allows("/root/a/x"));
        assert!(!effective.allows("/root/b/x"));

        // Deny-all absorbs.
        assert!(!Scope::Unrestricted
            .intersect(&Scope::default())
            .allows("/anything"));
    }

    #[test]
    fn intersection_keeps_sibling_prefix_confusion_out() {
        let effective =
            Scope::paths(["/root/docs"]).intersect(&Scope::paths(["/root/docs-private"]));
        assert!(!effective.allows("/root/docs-private/secret.txt"));
        assert!(!effective.allows("/root/docs/report.txt"));
    }
}
