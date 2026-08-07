//! Keeping credentials out of `tracing`.
//!
//! Every HTTP driver in this crate has the same hazard: `reqwest::Error`'s
//! `Display` appends ` for url (…)` with the full request URL, and for two of
//! our drivers the URL *is* the credential —
//! `hooks.slack.com/services/T…/B…/<secret>` and
//! `api.telegram.org/bot<token>/sendMessage`. A `?` that converts such an error
//! into `NotifyError` reaches `warn!(driver, error = %e, "notifier failed")` in
//! `dotagent-runner`, which writes it to
//! `~/.config/dotagent/logs/daemon/dotagent.log.*` — on disk, for the whole
//! retention window, from a failure as mundane as a DNS blip.
//!
//! So transport errors are converted **here, at the call site**, before they
//! can become a `NotifyError`. `NotifyError` deliberately has no
//! `From<reqwest::Error>`: without it, `?` on a `reqwest` call does not
//! compile, and the next driver cannot reintroduce this by accident.

/// What a redacted span is replaced with. Visible on purpose — a blanked
/// secret should read as "we removed this", not as a truncation artifact.
const PLACEHOLDER: &str = "<redacted>";

/// Longest API error description kept in a log line.
pub(crate) const MAX_ERROR_DETAIL_CHARS: usize = 300;

/// Convert a `reqwest::Error` into a short, URL-free message.
///
/// Only the failure *kind* and the HTTP status survive; everything the error
/// knows about the request — URL included — is dropped. `driver` names who
/// failed, since the log line is the only context left.
pub(crate) fn sanitize_reqwest_err(driver: &str, e: &reqwest::Error) -> String {
    let kind = if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connect"
    } else if e.is_builder() {
        "builder"
    } else if e.is_request() {
        "request"
    } else if e.is_body() {
        "body"
    } else if e.is_decode() {
        "decode"
    } else {
        "http"
    };
    match e.status() {
        Some(status) => format!("{driver} transport error ({kind}, status {status})"),
        None => format!("{driver} transport error ({kind})"),
    }
}

/// Blank out `secret` wherever it appears in `text`.
///
/// Two forms, because a truncated echo can carry the half that matters without
/// the part that identifies it: the whole string, and the trailing segment
/// after the last `:` or `/`. That tail is the high-entropy half of both shapes
/// we handle — `<bot-id>:<token>` and `…/B00000000/<webhook-secret>`.
///
/// Tails under 8 chars are left alone: they over-match ordinary words, and
/// nothing that short is worth stealing.
pub(crate) fn redact(text: &str, secret: &str) -> String {
    let secret = secret.trim();
    if secret.is_empty() {
        return text.to_string();
    }
    let mut out = text.replace(secret, PLACEHOLDER);
    if let Some(tail) = secret.rsplit([':', '/']).next() {
        if tail.len() >= 8 && tail != secret {
            out = out.replace(tail, PLACEHOLDER);
        }
    }
    out
}

/// Blank out the `user:pass@` half of a URL's authority, keeping the rest.
///
/// A self-hosted ntfy behind basic auth is configured as
/// `https://user:pass@ntfy.example.com` — the whole credential sits in a field
/// (`base_url`) whose remainder is genuinely useful in a log line. Dropping the
/// userinfo keeps the host, which is what an operator is reading the line for.
///
/// A URL with no `://` or no `@` is returned unchanged; this is display code
/// and must not fail on a malformed value.
pub(crate) fn redact_url_userinfo(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let authority_start = scheme_end + 3;
    let rest = &url[authority_start..];
    // The authority ends at the first `/`, `?` or `#`; an `@` past that point
    // is path data, not credentials.
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let Some(at) = rest[..authority_end].rfind('@') else {
        return url.to_string();
    };
    format!(
        "{}{PLACEHOLDER}@{}",
        &url[..authority_start],
        &rest[at + 1..]
    )
}

/// Flatten an API error body into one bounded, redacted log line.
///
/// The body is the only thing that says *why* a 4xx happened, so it is worth
/// keeping — but it is server-controlled, so it goes through [`redact`]
/// regardless, is stripped of control chars so a multi-line HTML page from a
/// proxy cannot span the log, and is capped so it cannot flood it.
///
/// Returns `None` when nothing readable is left; the caller then logs the bare
/// status rather than a dangling separator.
pub(crate) fn scrub_detail(raw: &str, secrets: &[&str]) -> Option<String> {
    let mut cleaned = raw.to_string();
    for secret in secrets {
        cleaned = redact(&cleaned, secret);
    }
    let flattened: String = cleaned
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = flattened.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() > MAX_ERROR_DETAIL_CHARS {
        let head: String = trimmed.chars().take(MAX_ERROR_DETAIL_CHARS).collect();
        return Some(format!("{head}…"));
    }
    Some(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const WEBHOOK: &str = "https://hooks.slack.com/services/T00000000/B00000000/aBcDeFgHiJkLmN";
    const WEBHOOK_SECRET: &str = "aBcDeFgHiJkLmN";
    const BOT_TOKEN: &str = "1234567:AAH-SUPER-SECRET-VALUE";

    // --- deny: the secret must not survive ---------------------------------

    #[test]
    fn redact_blanks_a_whole_webhook_url() {
        let out = redact(&format!("posting to {WEBHOOK} failed"), WEBHOOK);
        assert!(!out.contains(WEBHOOK_SECRET), "{out}");
        assert!(out.contains(PLACEHOLDER), "{out}");
    }

    #[test]
    fn redact_blanks_the_webhook_secret_alone() {
        // A server that echoes only the last path segment still leaks it.
        let out = redact(&format!("no_service for {WEBHOOK_SECRET}"), WEBHOOK);
        assert!(!out.contains(WEBHOOK_SECRET), "{out}");
    }

    #[test]
    fn redact_blanks_a_bot_token_and_its_secret_half() {
        let out = redact(&format!("{BOT_TOKEN} / AAH-SUPER-SECRET-VALUE"), BOT_TOKEN);
        assert!(!out.contains("AAH-SUPER-SECRET-VALUE"), "{out}");
        assert!(!out.contains(BOT_TOKEN), "{out}");
    }

    #[test]
    fn redact_blanks_every_occurrence() {
        let text = format!("{WEBHOOK_SECRET} then {WEBHOOK_SECRET} again");
        let out = redact(&text, WEBHOOK);
        assert!(!out.contains(WEBHOOK_SECRET), "{out}");
    }

    #[test]
    fn redact_blanks_a_separator_free_secret() {
        let out = redact("token=hunter2hunter2 rejected", "hunter2hunter2");
        assert!(!out.contains("hunter2hunter2"), "{out}");
    }

    #[test]
    fn scrub_detail_blanks_secrets_from_every_slot() {
        let body = format!("{WEBHOOK} rejected by {BOT_TOKEN}");
        let out = scrub_detail(&body, &[WEBHOOK, BOT_TOKEN]).unwrap();
        assert!(!out.contains(WEBHOOK_SECRET), "{out}");
        assert!(!out.contains("AAH-SUPER-SECRET-VALUE"), "{out}");
    }

    #[test]
    fn scrub_detail_blanks_before_bounding_not_after() {
        // The cap must not be what saves us: a secret past char 300 has to be
        // gone already, or a longer body would put it back.
        let body = format!("{}{WEBHOOK_SECRET}", "z".repeat(MAX_ERROR_DETAIL_CHARS * 2));
        let out = scrub_detail(&body, &[WEBHOOK]).unwrap();
        assert!(!out.contains(WEBHOOK_SECRET), "{out}");
    }

    // --- deny: URL userinfo ------------------------------------------------

    #[test]
    fn redact_url_userinfo_blanks_basic_credentials() {
        let out = redact_url_userinfo("https://admin:hunter2hunter2@ntfy.example.com");
        assert!(!out.contains("hunter2hunter2"), "{out}");
        assert!(!out.contains("admin"), "{out}");
        assert!(out.contains("ntfy.example.com"), "host must survive: {out}");
    }

    #[test]
    fn redact_url_userinfo_blanks_a_password_containing_an_at_sign() {
        // `rfind` inside the authority, not `find`: the last `@` is the
        // separator, everything before it is the credential.
        let out = redact_url_userinfo("https://admin:p@ssw0rd-secret@ntfy.example.com/x");
        assert!(!out.contains("p@ssw0rd-secret"), "{out}");
        assert!(out.contains("/x"), "path must survive: {out}");
    }

    #[test]
    fn redact_url_userinfo_ignores_an_at_sign_in_the_path() {
        // `@` past the authority is data — blanking there would eat the path.
        let url = "https://ntfy.example.com/topic@home";
        assert_eq!(redact_url_userinfo(url), url);
    }

    #[test]
    fn redact_url_userinfo_leaves_a_credential_free_url_alone() {
        let url = "https://ntfy.sh";
        assert_eq!(redact_url_userinfo(url), url);
    }

    #[test]
    fn redact_url_userinfo_survives_a_malformed_value() {
        // `base_url` can hold anything an operator typed, including a `${VAR}`
        // that was never expanded.
        for input in ["", "not a url", "${NTFY_BASE_URL}", "https://"] {
            assert_eq!(redact_url_userinfo(input), input);
        }
    }

    // --- deny: malformed / empty / absent inputs ---------------------------

    #[test]
    fn redact_leaves_text_alone_for_an_empty_secret() {
        assert_eq!(redact("nothing to hide", ""), "nothing to hide");
        assert_eq!(redact("nothing to hide", "   "), "nothing to hide");
    }

    #[test]
    fn redact_ignores_a_short_tail_that_would_over_match() {
        // "…/ab" would otherwise blank the "ab" in ordinary prose.
        let out = redact("cannot grab the value", "https://example.test/ab");
        assert_eq!(out, "cannot grab the value");
    }

    #[test]
    fn redact_survives_a_trailing_separator() {
        // Malformed config: URL ends in "/", so the tail is empty.
        let out = redact("plain text", "https://hooks.slack.com/services/");
        assert_eq!(out, "plain text");
    }

    #[test]
    fn scrub_detail_returns_none_for_an_empty_body() {
        assert!(scrub_detail("", &[WEBHOOK]).is_none());
        assert!(scrub_detail("  \n\t ", &[WEBHOOK]).is_none());
    }

    #[test]
    fn scrub_detail_returns_none_when_redaction_is_all_there_was() {
        assert!(scrub_detail(WEBHOOK_SECRET, &[WEBHOOK]).is_some());
        assert!(scrub_detail("   ", &[]).is_none());
    }

    #[test]
    fn scrub_detail_stays_on_one_bounded_line() {
        let body = format!("line one\nline two\r\n{}", "z".repeat(2_000));
        let out = scrub_detail(&body, &[]).unwrap();
        assert!(!out.contains('\n'), "detail must stay on one log line");
        assert!(!out.contains('\r'), "detail must stay on one log line");
        assert!(out.chars().count() <= MAX_ERROR_DETAIL_CHARS + 1);
    }

    // --- allow: the diagnostic value has to survive too --------------------

    #[test]
    fn scrub_detail_keeps_an_innocent_reason() {
        let out = scrub_detail("invalid_payload", &[WEBHOOK]).unwrap();
        assert_eq!(out, "invalid_payload");
    }

    #[test]
    fn sanitize_reqwest_err_names_the_driver_and_kind() {
        // Built through a real failed request: constructing `reqwest::Error`
        // directly is not possible from outside the crate.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt
            .block_on(async {
                reqwest::Client::new()
                    .post(format!("http://127.0.0.1:1/services/{WEBHOOK_SECRET}"))
                    .send()
                    .await
            })
            .unwrap_err();
        let out = sanitize_reqwest_err("slack", &err);
        assert!(!out.contains(WEBHOOK_SECRET), "{out}");
        assert!(!out.contains("127.0.0.1"), "{out}");
        assert!(out.starts_with("slack transport error ("), "{out}");
    }
}
