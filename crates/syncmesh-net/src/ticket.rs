//! Ticket format for sharing a room with a friend.
//!
//! A ticket is a human-shareable string that a joiner pastes into their TUI. It
//! encodes an `iroh::EndpointAddr` — the host's EndpointId plus any direct IP
//! addresses and/or relay URLs known at room-creation time.
//!
//! Wire format:
//! ```text
//!     syncmesh1<base32-nopad>
//! ```
//! The payload bytes are:
//! ```text
//!     [u8  magic        = 0x01      ]  version byte
//!     [... postcard(EndpointAddr)   ]
//! ```
//!
//! The leading human prefix makes tickets obvious when they land in chat logs
//! or URL bars, and the internal version byte lets us rev the format later
//! without introducing a second prefix.

use iroh::EndpointAddr;

use data_encoding::BASE32_NOPAD;

const PREFIX: &str = "syncmesh1";
const VERSION: u8 = 0x01;

#[derive(Debug, thiserror::Error)]
pub enum TicketError {
    #[error("ticket is missing the expected `{PREFIX}` prefix")]
    MissingPrefix,
    #[error("ticket base32 decode failed: {0}")]
    Base32(#[from] data_encoding::DecodeError),
    #[error("ticket payload is empty")]
    Empty,
    #[error("unsupported ticket version {0}")]
    UnsupportedVersion(u8),
    #[error("ticket postcard decode failed: {0}")]
    Postcard(#[from] postcard::Error),
}

/// Encode an `EndpointAddr` as a shareable ticket string.
pub fn encode(addr: &EndpointAddr) -> String {
    let body = postcard::to_allocvec(addr).expect("EndpointAddr must encode");
    let mut payload = Vec::with_capacity(1 + body.len());
    payload.push(VERSION);
    payload.extend_from_slice(&body);
    let mut out = String::from(PREFIX);
    out.push_str(&BASE32_NOPAD.encode(&payload));
    out
}

/// Parse a ticket string back into the `EndpointAddr` it encodes.
///
/// Input is lowercased before prefix check and base32 decode so callers don't
/// have to pre-normalize.
pub fn decode(ticket: &str) -> Result<EndpointAddr, TicketError> {
    let normalized = ticket.trim().to_lowercase();
    let rest = normalized
        .strip_prefix(PREFIX)
        .ok_or(TicketError::MissingPrefix)?;
    let payload = BASE32_NOPAD.decode(rest.to_uppercase().as_bytes())?;
    let (ver, body) = payload.split_first().ok_or(TicketError::Empty)?;
    if *ver != VERSION {
        return Err(TicketError::UnsupportedVersion(*ver));
    }
    let addr: EndpointAddr = postcard::from_bytes(body)?;
    Ok(addr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity;

    fn addr_with_ip() -> EndpointAddr {
        let k = identity::generate();
        EndpointAddr::new(k.public()).with_ip_addr("127.0.0.1:12345".parse().unwrap())
    }

    #[test]
    fn round_trip_preserves_endpoint_id_and_ip() {
        let original = addr_with_ip();
        let ticket = encode(&original);
        assert!(ticket.starts_with(PREFIX));
        let parsed = decode(&ticket).unwrap();
        assert_eq!(parsed.id, original.id);
        let parsed_ips: Vec<_> = parsed.ip_addrs().copied().collect();
        let orig_ips: Vec<_> = original.ip_addrs().copied().collect();
        assert_eq!(parsed_ips, orig_ips);
    }

    #[test]
    fn accepts_mixed_case_and_surrounding_whitespace() {
        let a = addr_with_ip();
        let t = encode(&a);
        let upper = t.to_uppercase();
        let padded = format!("   {upper}  ");
        let parsed = decode(&padded).unwrap();
        assert_eq!(parsed.id, a.id);
    }

    #[test]
    fn rejects_missing_prefix() {
        assert!(matches!(decode("not-a-ticket"), Err(TicketError::MissingPrefix)));
    }

    #[test]
    fn rejects_bad_base32() {
        assert!(matches!(decode("syncmesh1!!!"), Err(TicketError::Base32(_))));
    }

    #[test]
    fn rejects_wrong_version() {
        // Build a valid prefix with a bad version byte.
        let payload = [0xffu8, 0x00, 0x00];
        let enc = format!("{PREFIX}{}", BASE32_NOPAD.encode(&payload));
        assert!(matches!(decode(&enc), Err(TicketError::UnsupportedVersion(0xff))));
    }
}
