//! rsd-ipc: the wire protocol between rsd clients and the daemon (P5.3).
//!
//! Transport: Unix domain socket at `<state>/rsd.sock`, same-uid peers only
//! (checked via getpeereid on the server). Frames: `[len: u32 LE][postcard]`.
//!
//! Authorization model v1 (honest scope): two tiers. First-party clients
//! (same product, same uid) get the full index. Named principals get explicit
//! path-prefix scope grants; enforcement filters BEFORE any output — results,
//! counts, and live deltas are all computed over the authorized subset only.
//! XPC audit-token code identity (third-party app tier) is the documented
//! next step and rides behind the same Request::Hello handshake.

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

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
