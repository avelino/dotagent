//! Shared `reqwest` clients.
//!
//! `Client::new()` per send builds and throws away an entire TLS config and
//! connection pool to make one request. Three of the four HTTP drivers were
//! doing that on the notify hot path — the path every failed run walks at least
//! once. A `Client` is an `Arc` internally and is designed to be reused; the
//! only reason not to hold one is not having somewhere to put it.
//!
//! **Two** clients rather than one, because the timeout is the single setting
//! these paths genuinely disagree on. The Telegram inbound poller holds a
//! `getUpdates` connection open for up to 50 seconds *by design*, so its
//! ceiling has to clear that with room to spare. An outbound webhook POST has
//! no such shape, and collapsing the two would hand fire-and-forget
//! notifications a long-poll-sized deadline. Everything else — TLS, DNS
//! resolution, the connection pool — is shared within each.

use std::sync::OnceLock;
use std::time::Duration;

/// Client for one-shot outbound notifications (Slack, ntfy, Pushover).
///
/// No default timeout, which is exactly what `Client::new()` gave these drivers
/// before and is preserved here deliberately: picking a ceiling is a behavior
/// change, and it does not belong in a change whose whole point is to stop
/// rebuilding the client. Sizing one is worth doing — an unbounded notify POST
/// is a daemon that waits forever on a webhook — but on its own, with its own
/// tests.
pub(crate) fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| reqwest::Client::builder().build().unwrap_or_default())
}

/// Client for the Telegram Bot API, inbound long-poll included.
///
/// 90s clears the poller's 50s `getUpdates` hold with margin. `unwrap_or_default`
/// rather than `unwrap`: a client that failed to build is still a client that
/// can attempt a request and report a real transport error, whereas a panic
/// here takes the daemon down over a TLS backend that was never going to work.
pub(crate) fn long_poll_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(90))
            .build()
            .unwrap_or_default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_client_is_built_once_and_reused() {
        // The point of the module. If the `OnceLock` were bypassed, every call
        // would hand back a freshly built client and the pool would never be
        // shared — which is the bug this replaces, just relocated.
        assert!(std::ptr::eq(client(), client()));
        assert!(std::ptr::eq(long_poll_client(), long_poll_client()));
    }

    #[test]
    fn the_long_poll_client_is_not_the_outbound_one() {
        // They differ in the one setting that matters: a 90s ceiling sized for
        // `getUpdates` must not silently become the deadline for a webhook POST.
        assert!(!std::ptr::eq(client(), long_poll_client()));
    }
}
