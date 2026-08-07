//! `${VAR}` interpolation for credential fields, shared by every driver.
//!
//! Every notifier in this crate is configured from `agent.toml`, and an
//! `agent.toml` lives in a dotfiles repo. A credential written literally there
//! is a credential in git history — so each secret-bearing field accepts
//! `${VAR}` and resolves it at **send time** against
//! [`dotagent_secrets::lookup`]: the daemon-loaded `~/.config/dotagent/secrets.env`
//! first, then `std::env::var` for operators who wired vars into the plist /
//! systemd unit directly.
//!
//! Which fields: `slack.webhook_url` (the URL *is* the credential),
//! `ntfy.token` / `ntfy.base_url` / `ntfy.topic`, `pushover.token` /
//! `pushover.user`, `telegram.bot_token`.
//!
//! ## Failing beats guessing
//!
//! An unresolved reference is an error, never a fallback to the literal
//! `"${SLACK_WEBHOOK}"`. Sending that string is not a degraded notification —
//! it is a request authenticated as a placeholder, which answers 404 and looks
//! like an outage rather than a typo.
//!
//! ## What the error may say
//!
//! The message names the **field** and the **variable** and nothing else. It
//! never carries a resolved value: the input is a credential template, and the
//! store holds sibling credentials, so echoing either turns a config typo into
//! a disclosure in `~/.config/dotagent/logs/`.

/// Expand `${VAR}` references in `input` against the secrets store + process
/// env. `field` names the manifest key (e.g. `slack.webhook_url`) and is the
/// only context the error message gets, so it should be the dotted path an
/// operator can grep for in their `agent.toml`.
pub(crate) fn expand_env(field: &str, input: &str) -> std::result::Result<String, String> {
    // `dotagent_secrets::lookup` already does store-first, env-fallback.
    expand_env_with(field, input, dotagent_secrets::lookup)
}

/// Inner form that takes a `getter`, so tests can pass a closure and avoid
/// touching `std::env` (mutation across threads is UB under the modern
/// `set_var` contract, and Cargo runs tests in parallel).
fn expand_env_with<F>(field: &str, input: &str, getter: F) -> std::result::Result<String, String>
where
    F: Fn(&str) -> Option<String>,
{
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            return Err(format!("{field}: unterminated `${{…}}` placeholder"));
        };
        let name = &after[..end];
        if name.is_empty() {
            return Err(format!("{field}: empty `${{}}` placeholder"));
        }
        match getter(name) {
            Some(v) => out.push_str(&v),
            // Only the *name* travels. `out` at this point holds already-resolved
            // secrets, and the store holds the rest — neither belongs in a log.
            None => return Err(format!("{field}: env var ${{{name}}} is unset")),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Run `f` with a process-global secrets store installed, serialized against
/// every other test that does the same.
///
/// The store is a process global, so two tests installing different entries in
/// parallel would see each other's. Tests that assert a variable is *unset*
/// need no lock: they use names nothing installs, and `lookup` falls through to
/// an equally-unset `std::env`.
#[cfg(test)]
pub(crate) fn with_store<T>(entries: &[(&str, &str)], f: impl FnOnce() -> T) -> T {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // A panicking test poisons the lock; that is its own failure, and it must
    // not cascade into every other test in the file.
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    dotagent_secrets::install(dotagent_secrets::SecretsStore::from_entries(
        entries.iter().copied(),
    ));
    let out = f();
    dotagent_secrets::reset();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIELD: &str = "slack.webhook_url";

    // --- deny: unresolvable references fail, and fail loudly ---------------

    #[test]
    fn errors_on_a_missing_var() {
        let err = expand_env_with(FIELD, "${MISSING}", |_| None).unwrap_err();
        assert!(err.contains("unset"), "{err}");
        assert!(err.contains("MISSING"), "must name the variable: {err}");
        assert!(err.contains(FIELD), "must name the field: {err}");
    }

    #[test]
    fn errors_on_an_unterminated_placeholder() {
        let err = expand_env_with(FIELD, "${UNCLOSED", |_| None).unwrap_err();
        assert!(err.contains("unterminated"), "{err}");
        assert!(err.contains(FIELD), "{err}");
    }

    #[test]
    fn errors_on_an_empty_placeholder() {
        let err = expand_env_with(FIELD, "foo${}bar", |_| None).unwrap_err();
        assert!(err.contains("empty"), "{err}");
        assert!(err.contains(FIELD), "{err}");
    }

    #[test]
    fn the_error_never_echoes_a_sibling_secret() {
        // The first reference resolves, the second does not. The half-built
        // output holds a live credential — it must not ride along in the error.
        let env = |k: &str| (k == "RESOLVED").then_some("s3cr3t-webhook-value".to_string());
        let err = expand_env_with(FIELD, "${RESOLVED}/${MISSING}", env).unwrap_err();
        assert!(
            !err.contains("s3cr3t-webhook-value"),
            "leaked sibling: {err}"
        );
        assert!(err.contains("MISSING"), "{err}");
    }

    #[test]
    fn the_error_never_echoes_the_literal_input() {
        // Operators do paste literals next to placeholders; the failing input
        // is still a credential template.
        let input = "https://hooks.slack.com/services/T0/B0/aBcDeF123456${MISSING}";
        let err = expand_env_with(FIELD, input, |_| None).unwrap_err();
        assert!(!err.contains("aBcDeF123456"), "leaked input: {err}");
    }

    #[test]
    fn a_missing_var_late_in_the_template_still_fails() {
        // Partial expansion must never be returned as success — the result
        // would be a half-real credential that fails at the far end.
        let env = |k: &str| (k == "A").then_some("alpha".to_string());
        let err = expand_env_with(FIELD, "${A}-${B}", env).unwrap_err();
        assert!(err.contains("${B}"), "{err}");
    }

    #[test]
    fn an_unterminated_placeholder_after_a_resolved_one_still_fails() {
        let env = |_: &str| Some("v".to_string());
        let err = expand_env_with(FIELD, "${A}${B", env).unwrap_err();
        assert!(err.contains("unterminated"), "{err}");
    }

    #[test]
    fn a_closing_brace_before_the_opener_does_not_terminate_it() {
        // `}` earlier in the string must not be mistaken for this one's end.
        let err = expand_env_with(FIELD, "}${UNCLOSED", |_| None).unwrap_err();
        assert!(err.contains("unterminated"), "{err}");
    }

    #[test]
    fn the_field_label_reaches_every_error_variant() {
        for input in ["${MISSING}", "${UNCLOSED", "${}"] {
            let err = expand_env_with("ntfy.token", input, |_| None).unwrap_err();
            assert!(err.starts_with("ntfy.token: "), "{input} → {err}");
        }
    }

    // --- allow: the resolving paths still resolve --------------------------

    #[test]
    fn replaces_a_single_var() {
        let env = |k: &str| (k == "TG_TOKEN_A").then_some("abc123".to_string());
        assert_eq!(
            expand_env_with(FIELD, "${TG_TOKEN_A}", env).unwrap(),
            "abc123"
        );
    }

    #[test]
    fn replaces_multiple_vars() {
        let env = |k: &str| match k {
            "A" => Some("alpha".into()),
            "B" => Some("beta".into()),
            _ => None,
        };
        let out = expand_env_with(FIELD, "prefix-${A}-mid-${B}-end", env).unwrap();
        assert_eq!(out, "prefix-alpha-mid-beta-end");
    }

    #[test]
    fn preserves_a_literal() {
        let out = expand_env_with(FIELD, "plain-literal-token", |_| None).unwrap();
        assert_eq!(out, "plain-literal-token");
    }

    #[test]
    fn preserves_a_bare_dollar() {
        // `$VAR` (no braces) is not our syntax; a password containing `$` must
        // survive verbatim rather than half-expand.
        let out = expand_env_with(FIELD, "pa$$word$HOME", |_| None).unwrap();
        assert_eq!(out, "pa$$word$HOME");
    }

    #[test]
    fn an_empty_input_expands_to_empty() {
        assert_eq!(expand_env_with(FIELD, "", |_| None).unwrap(), "");
    }

    #[test]
    fn the_installed_store_is_what_expand_env_reads() {
        let out = with_store(&[("DOTAGENT_TEST_STORE_KEY", "from-store")], || {
            expand_env(FIELD, "${DOTAGENT_TEST_STORE_KEY}")
        })
        .unwrap();
        assert_eq!(out, "from-store");
    }
}
