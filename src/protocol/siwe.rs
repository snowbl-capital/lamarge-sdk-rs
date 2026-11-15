//! Sign-In with Ethereum challenge checks.

use alloy_primitives::Address;
use chrono::{DateTime, Duration, Utc};

use super::{
    checks::details::{self, differs, stale},
    eip712,
    signing::Verified,
};
use crate::error::VerificationError;

/// Tolerated clock skew between the client and the server.
const SKEW: Duration = Duration::seconds(120);
/// The longest a challenge may be valid.
const LIFETIME: Duration = Duration::seconds(600);

/// Checks a sign-in challenge (`POST /v1/auth/challenge`) for `address` and the app `origin`
/// at `now`: the exact EIP-4361 text the server writes, its nonce and expiry, issued no later
/// than two minutes ahead, not expired more than two minutes ago, valid ten minutes at most.
/// Returns the text to sign with `personal_sign`, signable until two minutes after expiry.
pub(crate) fn check_login_challenge(
    message: &str,
    nonce: &str,
    expires_at: DateTime<Utc>,
    address: Address,
    origin: &str,
    now: DateTime<Utc>,
) -> Result<Verified, VerificationError> {
    let lines: Vec<&str> = message.split('\n').collect();
    let (_, host) = origin.split_once("://").ok_or(differs(details::CHALLENGE))?;
    let [
        intro,
        signer,
        "",
        "Sign in to LaMarge.",
        "",
        uri,
        "Version: 1",
        "Chain ID: 1",
        nonce_line,
        issued,
        expires,
    ] = lines[..]
    else {
        return Err(differs(details::CHALLENGE));
    };
    let time = |line: &str, label: &str| {
        line.strip_prefix(label)
            .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
            .map(|time| time.with_timezone(&Utc))
    };
    let issued = time(issued, "Issued At: ");
    let shaped = intro.strip_suffix(" wants you to sign in with your Ethereum account:")
        == Some(host)
        && eip712::address_text(signer) == Some(address)
        && uri.strip_prefix("URI: ") == Some(origin)
        && nonce_line.strip_prefix("Nonce: ") == Some(nonce)
        && time(expires, "Expiration Time: ") == Some(expires_at);
    let Some(issued) = issued.filter(|_| shaped) else {
        return Err(differs(details::CHALLENGE));
    };
    if issued > now + SKEW
        || expires_at <= now - SKEW
        || expires_at <= issued
        || expires_at - issued > LIFETIME
    {
        return Err(stale(details::CHALLENGE_WINDOW));
    }
    let not_after = (expires_at + SKEW).timestamp() - 1;
    Ok(Verified::text(message.to_owned(), address, Some(not_after)))
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;
    use crate::error::VerificationKind;

    const ORIGIN: &str = "http://localhost:5173";
    const ADDRESS: Address = address!("5aaeb6053f3e94c9b9a09f33669435e7ef1beaed");

    /// `server/src/auth.rs`'s `siwe_message_follows_eip4361` vector.
    const MESSAGE: &str = "localhost:5173 wants you to sign in with your Ethereum account:\n\
        0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed\n\n\
        Sign in to LaMarge.\n\n\
        URI: http://localhost:5173\n\
        Version: 1\n\
        Chain ID: 1\n\
        Nonce: 0123456789abcdef\n\
        Issued At: 2026-09-27T10:00:00Z\n\
        Expiration Time: 2026-09-27T10:10:00Z";
    const NONCE: &str = "0123456789abcdef";

    fn at(text: &str) -> DateTime<Utc> {
        text.parse().unwrap()
    }

    fn check(
        message: &str,
        nonce: &str,
        origin: &str,
        now: DateTime<Utc>,
    ) -> Result<Verified, VerificationError> {
        check_login_challenge(message, nonce, at("2026-09-27T10:10:00Z"), ADDRESS, origin, now)
    }

    #[test]
    fn the_servers_message_is_accepted_within_two_minutes_of_skew() {
        let now = at("2026-09-27T10:00:00Z");
        for skew in [-120, 0, 120] {
            let verified = check(MESSAGE, NONCE, ORIGIN, now + Duration::seconds(skew)).unwrap();
            assert_eq!(verified.digest(), alloy_primitives::eip191_hash_message(MESSAGE));
        }
        let verified = check(MESSAGE, NONCE, ORIGIN, now).unwrap();
        // Signable until two minutes past expiry.
        assert_eq!(verified.not_after(), Some(at("2026-09-27T10:11:59Z").timestamp()));
        // The address line is compared as an address, in any case.
        let lower = MESSAGE.replace(
            "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed",
            "0x5aaeb6053f3e94c9b9a09f33669435e7ef1beaed",
        );
        assert!(check(&lower, NONCE, ORIGIN, now).is_ok());
    }

    #[test]
    fn origin_address_nonce_and_text_are_pinned() {
        let now = at("2026-09-27T10:00:00Z");
        let differs = |message: &str, nonce: &str, origin: &str| {
            let error = check(message, nonce, origin, now).unwrap_err();
            (error.kind, error.detail) == (VerificationKind::Differs, details::CHALLENGE)
        };
        for (from, to) in [
            (
                "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed",
                "0x1111111111111111111111111111111111111111",
            ),
            ("Chain ID: 1", "Chain ID: 137"),
            ("Sign in to LaMarge.", "Sign something else"),
            ("URI: http://localhost:5173", "URI: https://localhost:5173"),
            ("localhost:5173 wants", "evil.example wants"),
            ("Version: 1", "Version: 2"),
            ("Expiration Time: 2026-09-27T10:10:00Z", "Expiration Time: 2026-09-27T10:09:00Z"),
            ("Issued At: 2026-09-27T10:00:00Z", "Issued At: yesterday"),
        ] {
            assert!(differs(&MESSAGE.replace(from, to), NONCE, ORIGIN), "{to}");
        }
        assert!(differs(MESSAGE, "other", ORIGIN));
        assert!(differs(MESSAGE, NONCE, "https://other.example"));
        assert!(differs(&format!("{MESSAGE}\nResources: x"), NONCE, ORIGIN));
        assert!(differs(&MESSAGE.replacen("\n\n", "\n", 1), NONCE, ORIGIN));
    }

    #[test]
    fn windows_are_bounded() {
        let stale = |message: &str, now: &str| {
            let error = check(message, NONCE, ORIGIN, at(now)).unwrap_err();
            (error.kind, error.detail) == (VerificationKind::Stale, details::CHALLENGE_WINDOW)
        };
        // Expired more than two minutes ago.
        assert!(stale(MESSAGE, "2026-09-27T10:12:00Z"));
        assert!(check(MESSAGE, NONCE, ORIGIN, at("2026-09-27T10:11:59Z")).is_ok());
        // Issued more than two minutes ahead of the clock.
        assert!(stale(MESSAGE, "2026-09-27T09:57:59Z"));
        // Valid more than ten minutes.
        assert!(stale(
            &MESSAGE.replace("Issued At: 2026-09-27T10:00:00Z", "Issued At: 2026-09-27T09:59:59Z"),
            "2026-09-27T10:00:00Z"
        ));
        // Issued after it expires.
        let backwards =
            MESSAGE.replace("Issued At: 2026-09-27T10:00:00Z", "Issued At: 2026-09-27T10:10:01Z");
        assert!(stale(&backwards, "2026-09-27T10:09:00Z"));
    }
}
