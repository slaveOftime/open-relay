//! Opaque `oly logs` cursor tokens.
//!
//! A token names one position in one session's canonical output stream. It
//! is opaque on purpose: it exists to be passed back verbatim to
//! `--since`, not to be parsed or compared by callers. The session id and
//! journal incarnation are embedded so that handing a token to the wrong
//! session — or to a session that restarted since it was issued — is
//! rejected loudly instead of silently reading the wrong window.
//!
//! Decoding is done client-side (the CLI turns a token into a
//! [`StreamPosition`] before the RPC); the daemon validates the incarnation
//! it was derived from, so a forged or stale token cannot smuggle a read of
//! a different stream.

use base64::Engine as _;
use base64::prelude::BASE64_URL_SAFE_NO_PAD;

use crate::error::{AppError, Result};
use crate::protocol::StreamPosition;

/// Version tag of the token layout; bumped only for incompatible changes,
/// which then fail with a "re-run without --since" style message.
const TOKEN_VERSION: &str = "v1";
const FIELD_SEPARATOR: char = '|';

/// Encode the position a read stopped at into a token for `--since`.
pub fn encode(session: &str, position: StreamPosition) -> String {
    let plain = format!(
        "{TOKEN_VERSION}{FIELD_SEPARATOR}{session}{FIELD_SEPARATOR}{}{FIELD_SEPARATOR}{}",
        position.incarnation, position.offset
    );
    BASE64_URL_SAFE_NO_PAD.encode(plain)
}

/// Decode a token issued for `session`. Any other content — truncated,
/// hand-written, or from another session — is a user-facing error, because
/// the alternative is reading a plausible but wrong window of output.
pub fn decode(session: &str, token: &str) -> Result<StreamPosition> {
    let usage = || {
        AppError::Protocol(format!(
            "invalid --since cursor token {token:?}: pass back a token printed by \
             `oly logs` for this session"
        ))
    };
    let decoded = BASE64_URL_SAFE_NO_PAD
        .decode(token.trim())
        .map_err(|_| usage())?;
    let plain = String::from_utf8(decoded).map_err(|_| usage())?;
    let mut fields = plain.split(FIELD_SEPARATOR);
    if fields.next() != Some(TOKEN_VERSION) {
        return Err(usage());
    }
    let fields: Vec<&str> = fields.collect();
    let [token_session, incarnation, offset] = fields.as_slice() else {
        return Err(usage());
    };
    if *token_session != session {
        return Err(AppError::Protocol(format!(
            "cursor token belongs to session {token_session}, not {session}"
        )));
    }
    let (Ok(incarnation), Ok(offset)) = (incarnation.parse::<u64>(), offset.parse::<u64>()) else {
        return Err(usage());
    };
    Ok(StreamPosition {
        incarnation,
        offset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_round_trips() {
        let position = StreamPosition {
            incarnation: 3,
            offset: 918_273,
        };
        let token = encode("sess-abc", position);
        assert_eq!(decode("sess-abc", &token).unwrap(), position);
    }

    #[test]
    fn token_is_not_a_bare_number() {
        // The whole point of opacity: a caller cannot eyeball or hand-build
        // an offset, so format changes stay invisible to them.
        let token = encode(
            "sess-abc",
            StreamPosition {
                incarnation: 1,
                offset: 42,
            },
        );
        assert!(token.parse::<u64>().is_err());
        assert!(!token.contains('4'));
    }

    #[test]
    fn token_from_another_session_is_rejected() {
        let token = encode(
            "sess-a",
            StreamPosition {
                incarnation: 1,
                offset: 10,
            },
        );
        let err = decode("sess-b", &token).unwrap_err().to_string();
        assert!(err.contains("sess-a"), "{err}");
        assert!(err.contains("sess-b"), "{err}");
    }

    #[test]
    fn garbage_token_is_rejected_with_usage_guidance() {
        for token in [
            "",
            "nonsense",
            "v2",
            &BASE64_URL_SAFE_NO_PAD.encode("garbage"),
        ] {
            let err = decode("sess-a", token).unwrap_err().to_string();
            assert!(err.contains("invalid --since cursor token"), "{err}");
        }
    }
}
