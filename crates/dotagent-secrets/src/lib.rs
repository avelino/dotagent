//! Daemon-level secrets loader for dotagent.
//!
//! Reads a single `KEY=VALUE` file (default `~/.config/dotagent/secrets.env`)
//! at daemon startup and exposes its entries to in-process consumers (notifier
//! drivers today, agent env injection later). The file is **never** dumped
//! into agent subprocess environments wholesale — agents only get the keys
//! their manifest opts into.
//!
//! ## Why a dedicated crate
//!
//! `dotagent-notify::telegram` needs a way to resolve `${TELEGRAM_BOT_TOKEN}`
//! that does NOT require the operator to wire env vars into the plist /
//! systemd unit. Putting the loader in `dotagent-core` would create a
//! `core → notify` cycle (core already depends on notify for manifest
//! types); keeping it in its own crate avoids that.
//!
//! ## File format
//!
//! ```env
//! # comments start with `#`
//! TELEGRAM_BOT_TOKEN=1234:abcdef...
//! SLACK_WEBHOOK_URL="https://hooks.slack.com/services/..."
//! PUSHOVER_TOKEN='quoted-literally-no-escapes'
//! export DIRENV_STYLE=ok
//!
//! # 1Password CLI reference — resolved at daemon startup via `op read`
//! TELEGRAM_BOT_TOKEN_2=op://Personal/dotagent/telegram-token
//! ```
//!
//! Roughly compatible with the dotenv format:
//!
//! - One `KEY=VALUE` per line. Whitespace around `=` is trimmed.
//! - Lines may start with `export ` (the prefix is stripped) — convenient for
//!   files copy-pasted from a `.envrc`.
//! - **Double-quoted** values strip the quotes and process the escapes
//!   `\n`, `\r`, `\t`, `\\`, `\"`.
//! - **Single-quoted** values strip the quotes and are otherwise literal
//!   (no escapes — useful for tokens that contain backslashes).
//! - **Unquoted** values are kept verbatim with trailing whitespace trimmed.
//!   No shell expansion (`$HOME` stays literal).
//! - Blank lines and lines starting with `#` are ignored.
//! - Keys must match `[A-Za-z_][A-Za-z0-9_]*` (env-var-shaped).
//!
//! ## Secret references
//!
//! Values matching `op://vault/item[/section]/field` are treated as
//! **1Password CLI references**: the daemon shells out to `op read <ref>`
//! at startup and stores the resolved value in memory. Failures are
//! non-fatal — the key stays unset, the daemon logs a warning, and any
//! notifier that needs it will fail loud with `env var ${…} is unset`
//! rather than sending the literal `op://…` string over the wire.
//!
//! Requirements: the `op` CLI on PATH and an active 1Password session
//! (the daemon does not handle signin — use `op signin` or biometric unlock
//! before starting it, or rely on `op` desktop integration).
//!
//! ## Permission posture
//!
//! On Unix, the file must be mode `0600`. Any group/world bits → refuse to
//! load with a clear error. Same posture as ssh's `~/.ssh/id_*`.
//!
//! ## Process-global handle
//!
//! Notifier `send()` impls cannot thread a store handle through the trait
//! signature without breaking the plugin protocol — so the loader exposes
//! a `RwLock<Option<SecretsStore>>` global initialized once at daemon
//! startup. [`lookup`] returns the in-store value, falling back to
//! `std::env::var` (so existing operators who already wire env vars into
//! the unit file keep working).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SecretsError {
    #[error("io reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// File exists but mode is too permissive (any group or world bit is
    /// set). Daemon refuses to load, matching ssh's posture for private
    /// keys. The exact rule is `mode & 0o077 == 0` — `0600` is the canonical
    /// answer but `0400` / `0700` also pass; anything looser fails.
    #[error(
        "insecure permissions on {path}: mode is {mode:o}, must be owner-only \
        (no group/world bits — e.g. 0600). run: chmod 600 {path}",
        path = path.display(),
    )]
    InsecureMode { path: PathBuf, mode: u32 },
    #[error("invalid line {line} in {path}: {reason}", path = path.display())]
    Parse {
        path: PathBuf,
        line: usize,
        reason: String,
    },
}

/// In-memory secrets table.
///
/// `Debug` is intentionally NOT derived — printing the struct would leak
/// values into `tracing` output. The `Debug` impl below shows only the key
/// count.
#[derive(Default, Clone)]
pub struct SecretsStore {
    entries: BTreeMap<String, String>,
    /// Where the store was loaded from, for diagnostics. `None` when built
    /// in-process (e.g., tests).
    source: Option<PathBuf>,
    /// How many `op://...` (or other future scheme) references failed to
    /// resolve at load time. Surfaced by `dotagent doctor` and the
    /// `secrets_loaded` audit event so the operator notices.
    unresolved_references: usize,
}

impl std::fmt::Debug for SecretsStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretsStore")
            .field("len", &self.entries.len())
            .field("source", &self.source)
            .field("unresolved_references", &self.unresolved_references)
            .finish()
    }
}

impl SecretsStore {
    /// Build an empty store. Useful for tests; production code should call
    /// [`SecretsStore::load`].
    pub fn empty() -> Self {
        Self::default()
    }

    /// Build a store from in-memory entries (tests / smoke).
    pub fn from_entries<I, K, V>(entries: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self {
            entries: entries
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
            source: None,
            unresolved_references: 0,
        }
    }

    /// Load from `path`. Returns:
    /// - `Ok(None)` if the file does not exist — missing file is not an error.
    /// - `Err(SecretsError::InsecureMode)` if mode is not 0600 on Unix.
    /// - `Err(SecretsError::Parse)` if any non-comment / non-blank line is malformed.
    /// - `Ok(Some(store))` otherwise (with `op://` references resolved
    ///   eagerly via the system `op` CLI; failures are non-fatal).
    pub fn load(path: impl AsRef<Path>) -> Result<Option<Self>, SecretsError> {
        Self::load_with_resolver(path, default_reference_resolver)
    }

    /// Like [`SecretsStore::load`] but lets the caller inject a custom
    /// reference resolver. Tests use this to avoid invoking `op` for real.
    pub fn load_with_resolver<R>(
        path: impl AsRef<Path>,
        resolver: R,
    ) -> Result<Option<Self>, SecretsError>
    where
        R: Fn(&str) -> std::result::Result<String, String>,
    {
        let path = path.as_ref();
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(SecretsError::Io {
                    path: path.to_path_buf(),
                    source,
                })
            }
        };
        check_permissions(path, &meta)?;
        let raw = std::fs::read_to_string(path).map_err(|source| SecretsError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mut entries = parse(&raw, path)?;
        let unresolved_references = resolve_references(&mut entries, &resolver);
        Ok(Some(Self {
            entries,
            source: Some(path.to_path_buf()),
            unresolved_references,
        }))
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.get(key).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn source(&self) -> Option<&Path> {
        self.source.as_deref()
    }

    /// Count of references (`op://...`) that failed to resolve when the
    /// store was loaded. `dotagent doctor` surfaces this number.
    pub fn unresolved_references(&self) -> usize {
        self.unresolved_references
    }

    /// Iterate keys — for diagnostics (`dotagent doctor`). Never iterate
    /// values into logs.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }
}

#[cfg(unix)]
fn check_permissions(path: &Path, meta: &std::fs::Metadata) -> Result<(), SecretsError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(SecretsError::InsecureMode {
            path: path.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_permissions(_path: &Path, _meta: &std::fs::Metadata) -> Result<(), SecretsError> {
    // No portable equivalent of POSIX mode bits — Windows ACLs are different
    // enough that a naive check would either over- or under-warn. Leaving
    // this as a no-op matches how ssh handles it on Windows.
    Ok(())
}

fn parse(raw: &str, path: &Path) -> Result<BTreeMap<String, String>, SecretsError> {
    let mut out = BTreeMap::new();
    for (idx, line) in raw.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // Allow `export KEY=VALUE` (.envrc / sourced shell habits). The
        // prefix is stripped silently; we do not pass `export` into the
        // key parser.
        let body = trimmed.strip_prefix("export ").unwrap_or(trimmed);

        let Some(eq) = body.find('=') else {
            return Err(SecretsError::Parse {
                path: path.to_path_buf(),
                line: line_no,
                reason: "expected `KEY=VALUE`, found no `=`".into(),
            });
        };
        let key = body[..eq].trim();
        let raw_value = &body[eq + 1..];
        if !is_valid_key(key) {
            return Err(SecretsError::Parse {
                path: path.to_path_buf(),
                line: line_no,
                reason: format!("invalid key {key:?} (must match [A-Za-z_][A-Za-z0-9_]*)"),
            });
        }
        let value = unquote_value(raw_value).map_err(|reason| SecretsError::Parse {
            path: path.to_path_buf(),
            line: line_no,
            reason,
        })?;
        // last-write-wins (matches dotenv convention), but warn so the
        // operator notices the conflict — silently dropping the first
        // value is a debugging nightmare when `chmod 600` and the file
        // contents look right but the wrong token is in use.
        if out.contains_key(key) {
            tracing::warn!(
                path = %path.display(),
                line = line_no,
                key,
                "duplicate key in secrets file; later definition wins"
            );
        }
        out.insert(key.to_string(), value);
    }
    Ok(out)
}

/// Drain a char iterator after a closing quote and ensure everything
/// left is whitespace. The `quote` arg only feeds the error message.
fn ensure_trailing_whitespace<I: Iterator<Item = char>>(
    chars: &mut I,
    quote: char,
) -> Result<(), String> {
    for c in chars {
        if !c.is_whitespace() {
            return Err(format!(
                "unexpected `{c}` after closing `{quote}` (escape with `\\{quote}` or remove it)"
            ));
        }
    }
    Ok(())
}

/// Dotenv-style quote handling.
///
/// - Leading whitespace before the value is trimmed.
/// - `"..."` → strip quotes, process `\n \r \t \\ \"` escapes. After the
///   closing `"`, **only** trailing whitespace is allowed; anything else
///   is an error (silently ignoring `KEY="v" trailing` masks typos).
/// - `'...'` → strip quotes, literal interior (no escapes). Same trailing
///   rule as double-quoted: whitespace ok, anything else errors.
/// - Otherwise → trailing whitespace trimmed, no escapes applied.
fn unquote_value(raw: &str) -> Result<String, String> {
    let v = raw.trim_start();
    if let Some(rest) = v.strip_prefix('"') {
        let mut out = String::with_capacity(rest.len());
        let mut chars = rest.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => {
                    ensure_trailing_whitespace(&mut chars, '"')?;
                    return Ok(out);
                }
                '\\' => match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('r') => out.push('\r'),
                    Some('t') => out.push('\t'),
                    Some('\\') => out.push('\\'),
                    Some('"') => out.push('"'),
                    Some(other) => {
                        return Err(format!(
                            "unsupported escape `\\{other}` in double-quoted value"
                        ))
                    }
                    None => return Err("trailing `\\` in double-quoted value".into()),
                },
                _ => out.push(c),
            }
        }
        return Err("unterminated double-quoted value".into());
    }
    if let Some(rest) = v.strip_prefix('\'') {
        let mut out = String::with_capacity(rest.len());
        let mut chars = rest.chars();
        while let Some(c) = chars.next() {
            if c == '\'' {
                ensure_trailing_whitespace(&mut chars, '\'')?;
                return Ok(out);
            }
            out.push(c);
        }
        return Err("unterminated single-quoted value".into());
    }
    // Unquoted: keep as-is, trim trailing whitespace only.
    Ok(v.trim_end().to_string())
}

/// Iterate the parsed entries and rewrite any value matching a known
/// "reference scheme" (`op://...` today) with its resolved plaintext.
/// Failures are logged and the original `op://...` value is **removed**
/// from the entries — leaving it in place would silently send the
/// literal placeholder string to the upstream API.
///
/// Returns the number of references that failed to resolve.
fn resolve_references<R>(entries: &mut BTreeMap<String, String>, resolver: &R) -> usize
where
    R: Fn(&str) -> std::result::Result<String, String>,
{
    let mut failed = 0usize;
    // Collect references to mutate after the iteration (we cannot mutate
    // the map while borrowing it for the scan).
    let refs: Vec<(String, String)> = entries
        .iter()
        .filter(|(_, v)| is_op_reference(v))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for (key, reference) in refs {
        match resolver(&reference) {
            Ok(value) => {
                entries.insert(key, value);
            }
            Err(reason) => {
                failed += 1;
                entries.remove(&key);
                // Reference (`op://Personal/foo/field`) is not a secret on
                // its own — vault/item names are operator-visible. Logging
                // it speeds debugging.
                tracing::warn!(
                    %key,
                    reference = %reference,
                    %reason,
                    "failed to resolve secret reference; key is unset"
                );
            }
        }
    }
    failed
}

/// Recognise `op://vault/item/field` (or `op://vault/item/section/field`).
/// Loose check — the actual validation happens inside `op read`.
fn is_op_reference(value: &str) -> bool {
    if let Some(rest) = value.strip_prefix("op://") {
        // Minimum shape: `vault/item/field` → at least two slashes after
        // the scheme.
        rest.matches('/').count() >= 2
    } else {
        false
    }
}

/// Production resolver: shells out to `op read <reference>`.
///
/// The `op` CLI must be on `PATH` and an active 1Password session has
/// to exist — we do not handle signin. On `op` invocation failure the
/// error message includes the exit code + stderr first line so the
/// operator can act without having to re-run `op` themselves.
fn default_reference_resolver(reference: &str) -> std::result::Result<String, String> {
    if !is_op_reference(reference) {
        // Defense-in-depth: should never happen — `resolve_references`
        // only calls us for matching values.
        return Err(format!("not a recognised reference scheme: {reference}"));
    }
    let output = std::process::Command::new("op")
        .args(["read", "--no-newline", reference])
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "1Password CLI (`op`) not found on PATH — install it or remove the op:// reference"
                    .into()
            } else {
                format!("failed to spawn `op read`: {e}")
            }
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first_line = stderr.lines().next().unwrap_or("").trim();
        let code = output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into());
        return Err(format!("`op read` exited {code}: {first_line}"));
    }
    String::from_utf8(output.stdout).map_err(|e| format!("`op read` returned non-utf8 output: {e}"))
}

fn is_valid_key(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// ---------------------------------------------------------------------------
// Process-global handle
// ---------------------------------------------------------------------------
//
// Notifier `send()` runs deep inside the trait dispatch — the daemon cannot
// pass a `&SecretsStore` through `Notifier::send`. So we keep a singleton
// behind an `RwLock<Option<SecretsStore>>` that the daemon initializes once
// at startup. `lookup` first consults the store, then falls back to
// `std::env::var` so operators who already wire env vars into the unit file
// keep working.

static GLOBAL: RwLock<Option<SecretsStore>> = RwLock::new(None);

/// Install the process-global store. Replaces any previously installed one
/// — daemon SIGHUP reload may call this again.
pub fn install(store: SecretsStore) {
    let mut guard = GLOBAL.write().expect("secrets store lock poisoned");
    *guard = Some(store);
}

/// Drop the process-global store (tests + daemon reload path).
pub fn reset() {
    let mut guard = GLOBAL.write().expect("secrets store lock poisoned");
    *guard = None;
}

/// Drop the store if one is installed and return whether anything was
/// dropped. Used by the daemon on SIGHUP-driven reload failures so the
/// audit log can distinguish "reload failed, nothing was loaded
/// previously either" from "reload failed AND we just threw away the
/// old store" — the second case is the one the operator needs to know
/// about, because callers fall back to `std::env::var` from now on.
pub fn reset_if_present() -> bool {
    let mut guard = GLOBAL.write().expect("secrets store lock poisoned");
    if guard.is_some() {
        *guard = None;
        true
    } else {
        false
    }
}

/// Look up `key`. Checks the loaded store first, then falls back to
/// `std::env::var`. Returns `None` only when both miss.
///
/// This is the function notifier drivers call from `${VAR}` interpolation —
/// keep it allocation-cheap on the hot path (the read lock + a `BTreeMap`
/// lookup is fine for the realistic key count, which is single digits).
pub fn lookup(key: &str) -> Option<String> {
    {
        let guard = GLOBAL.read().expect("secrets store lock poisoned");
        if let Some(store) = guard.as_ref() {
            if let Some(v) = store.get(key) {
                return Some(v.to_string());
            }
        }
    }
    std::env::var(key).ok()
}

/// Snapshot of the currently installed store (mostly for `dotagent doctor`).
pub fn snapshot() -> Option<SecretsStore> {
    GLOBAL.read().expect("secrets store lock poisoned").clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_file(dir: &Path, name: &str, body: &str, mode: u32) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let _ = mode;
        path
    }

    #[test]
    fn missing_file_returns_none() {
        let dir = tempdir().unwrap();
        let res = SecretsStore::load(dir.path().join("does-not-exist.env")).unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn parses_basic_kv() {
        let dir = tempdir().unwrap();
        let body = "FOO=bar\nBAZ=qux\n";
        let p = write_file(dir.path(), "secrets.env", body, 0o600);
        let store = SecretsStore::load(&p).unwrap().unwrap();
        assert_eq!(store.len(), 2);
        assert_eq!(store.get("FOO"), Some("bar"));
        assert_eq!(store.get("BAZ"), Some("qux"));
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let dir = tempdir().unwrap();
        let body = "# top comment\n\nFOO=bar\n   # indented\nBAZ=qux\n";
        let p = write_file(dir.path(), "secrets.env", body, 0o600);
        let store = SecretsStore::load(&p).unwrap().unwrap();
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn double_quoted_values_strip_quotes_and_process_escapes() {
        let dir = tempdir().unwrap();
        // `\n` → newline, `\"` → literal quote, `\\` → single backslash.
        let body = "TOKEN=\"with spaces\"\nNL=\"line1\\nline2\"\nQ=\"a\\\"b\"\nBS=\"a\\\\b\"\n";
        let p = write_file(dir.path(), "secrets.env", body, 0o600);
        let store = SecretsStore::load(&p).unwrap().unwrap();
        assert_eq!(store.get("TOKEN"), Some("with spaces"));
        assert_eq!(store.get("NL"), Some("line1\nline2"));
        assert_eq!(store.get("Q"), Some("a\"b"));
        assert_eq!(store.get("BS"), Some("a\\b"));
    }

    #[test]
    fn single_quoted_values_are_literal() {
        // Single quotes: no escape processing, no `${VAR}` expansion (we
        // don't do shell expansion anyway, but the intent is "what you
        // typed is what you get").
        let dir = tempdir().unwrap();
        let body = "RAW='a\\nb \"with\" $stuff'\n";
        let p = write_file(dir.path(), "secrets.env", body, 0o600);
        let store = SecretsStore::load(&p).unwrap().unwrap();
        assert_eq!(store.get("RAW"), Some("a\\nb \"with\" $stuff"));
    }

    #[test]
    fn unquoted_values_keep_inner_whitespace() {
        let dir = tempdir().unwrap();
        let body = "GREETING=hello world\nSPACED=   leading then trailing   \n";
        let p = write_file(dir.path(), "secrets.env", body, 0o600);
        let store = SecretsStore::load(&p).unwrap().unwrap();
        assert_eq!(store.get("GREETING"), Some("hello world"));
        // Leading whitespace IS trimmed (dotenv-compatible), trailing too.
        assert_eq!(store.get("SPACED"), Some("leading then trailing"));
    }

    #[test]
    fn export_prefix_is_stripped() {
        // Convenience for copy-pasting from a `.envrc`.
        let dir = tempdir().unwrap();
        let body = "export TOKEN=abc\nexport WRAPPED=\"hi\"\n";
        let p = write_file(dir.path(), "secrets.env", body, 0o600);
        let store = SecretsStore::load(&p).unwrap().unwrap();
        assert_eq!(store.get("TOKEN"), Some("abc"));
        assert_eq!(store.get("WRAPPED"), Some("hi"));
    }

    #[test]
    fn duplicate_key_last_wins() {
        // Last write wins (dotenv convention). The runtime additionally
        // emits a `tracing::warn` — we can't assert that here without a
        // subscriber, but `cargo test --workspace -- --nocapture` shows
        // it for manual verification.
        let dir = tempdir().unwrap();
        let body = "TOKEN=first\nTOKEN=second\n";
        let p = write_file(dir.path(), "secrets.env", body, 0o600);
        let store = SecretsStore::load(&p).unwrap().unwrap();
        assert_eq!(store.get("TOKEN"), Some("second"));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn unterminated_double_quote_errors() {
        let dir = tempdir().unwrap();
        let body = "TOKEN=\"hello\n";
        let p = write_file(dir.path(), "secrets.env", body, 0o600);
        let err = SecretsStore::load(&p).unwrap_err();
        let reason = match err {
            SecretsError::Parse { reason, .. } => reason,
            other => panic!("expected Parse, got {other:?}"),
        };
        assert!(reason.contains("unterminated"), "{reason}");
    }

    #[test]
    fn trailing_whitespace_after_close_quote_is_ok() {
        // Both quote styles must accept trailing whitespace before EOL —
        // it's how real `.env` files end up after a copy-paste.
        let dir = tempdir().unwrap();
        let body = "DBL=\"hi\"   \nSGL='hi'   \n";
        let p = write_file(dir.path(), "secrets.env", body, 0o600);
        let store = SecretsStore::load(&p).unwrap().unwrap();
        assert_eq!(store.get("DBL"), Some("hi"));
        assert_eq!(store.get("SGL"), Some("hi"));
    }

    #[test]
    fn trailing_junk_after_close_quote_errors() {
        // Both quote styles must reject non-whitespace trailing content —
        // covers `KEY="v" trailing` (was silently accepted) AND
        // `KEY='v'x` (was rejected; now uniform with double).
        let dir = tempdir().unwrap();
        let p_dbl = write_file(dir.path(), "dbl.env", "TOKEN=\"v\" trailing\n", 0o600);
        let err = SecretsStore::load(&p_dbl).unwrap_err();
        match err {
            SecretsError::Parse { reason, .. } => {
                assert!(reason.contains("after closing"), "{reason}")
            }
            other => panic!("expected Parse, got {other:?}"),
        }
        let p_sgl = write_file(dir.path(), "sgl.env", "TOKEN='v'trailing\n", 0o600);
        let err = SecretsStore::load(&p_sgl).unwrap_err();
        assert!(matches!(err, SecretsError::Parse { .. }));
    }

    #[test]
    fn unsupported_escape_errors() {
        let dir = tempdir().unwrap();
        let body = "TOKEN=\"\\q\"\n";
        let p = write_file(dir.path(), "secrets.env", body, 0o600);
        let err = SecretsStore::load(&p).unwrap_err();
        let reason = match err {
            SecretsError::Parse { reason, .. } => reason,
            other => panic!("expected Parse, got {other:?}"),
        };
        assert!(reason.contains("unsupported escape"), "{reason}");
    }

    // -- op:// reference resolution -----------------------------------

    #[test]
    fn op_reference_is_detected() {
        assert!(is_op_reference("op://Personal/dotagent/telegram-token"));
        assert!(is_op_reference("op://vault/item/section/field"));
        assert!(!is_op_reference("op://only/one"));
        assert!(!is_op_reference("opaque-token-not-a-ref"));
        assert!(!is_op_reference("https://example.com/op://fake"));
    }

    #[test]
    fn op_references_are_resolved_at_load() {
        let dir = tempdir().unwrap();
        let body = "PLAIN=literal\nTG=op://Personal/dotagent/telegram-token\n";
        let p = write_file(dir.path(), "secrets.env", body, 0o600);
        let store = SecretsStore::load_with_resolver(&p, |reference| {
            assert_eq!(reference, "op://Personal/dotagent/telegram-token");
            Ok("resolved-token".into())
        })
        .unwrap()
        .unwrap();
        assert_eq!(store.get("PLAIN"), Some("literal"));
        assert_eq!(store.get("TG"), Some("resolved-token"));
        assert_eq!(store.unresolved_references(), 0);
    }

    #[test]
    fn failed_op_reference_removes_key_and_counts() {
        // Operator's expectation: a failed `op read` must NOT silently
        // leave the literal `op://...` string in place — that would get
        // sent over the wire to Telegram. We unset the key so callers
        // hit the "${VAR} unset" failure path instead.
        let dir = tempdir().unwrap();
        let body = "GOOD=value\nBROKEN=op://Personal/missing/item\n";
        let p = write_file(dir.path(), "secrets.env", body, 0o600);
        let store = SecretsStore::load_with_resolver(&p, |reference| {
            if reference.contains("missing") {
                Err("not found".into())
            } else {
                Ok("ok".into())
            }
        })
        .unwrap()
        .unwrap();
        assert_eq!(store.get("GOOD"), Some("value"));
        assert!(
            store.get("BROKEN").is_none(),
            "literal op:// string MUST NOT leak into the store"
        );
        assert_eq!(store.unresolved_references(), 1);
    }

    #[test]
    fn rejects_line_without_equals() {
        let dir = tempdir().unwrap();
        let body = "FOO=bar\nMALFORMED\n";
        let p = write_file(dir.path(), "secrets.env", body, 0o600);
        let err = SecretsStore::load(&p).unwrap_err();
        match err {
            SecretsError::Parse { line, .. } => assert_eq!(line, 2),
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_invalid_key_shape() {
        let dir = tempdir().unwrap();
        let body = "1FOO=bar\n";
        let p = write_file(dir.path(), "secrets.env", body, 0o600);
        let err = SecretsStore::load(&p).unwrap_err();
        assert!(matches!(err, SecretsError::Parse { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_group_readable_mode() {
        let dir = tempdir().unwrap();
        let body = "FOO=bar\n";
        let p = write_file(dir.path(), "secrets.env", body, 0o640);
        let err = SecretsStore::load(&p).unwrap_err();
        match err {
            SecretsError::InsecureMode { mode, .. } => assert_eq!(mode, 0o640),
            other => panic!("expected InsecureMode, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn accepts_owner_only_modes_other_than_0600() {
        // 0400 (read-only owner) and 0700 (rwx owner) both have zero
        // group/world bits, so they pass — matches the ssh posture
        // documented in `docs/concepts/secrets.md`.
        let dir = tempdir().unwrap();
        for mode in [0o400u32, 0o600, 0o700] {
            let p = write_file(dir.path(), &format!("m{mode:o}.env"), "K=v\n", mode);
            let store = SecretsStore::load(&p)
                .unwrap_or_else(|e| panic!("mode {mode:o} should pass, got {e}"))
                .unwrap();
            assert_eq!(store.get("K"), Some("v"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_world_readable_mode() {
        let dir = tempdir().unwrap();
        let body = "FOO=bar\n";
        let p = write_file(dir.path(), "secrets.env", body, 0o604);
        let err = SecretsStore::load(&p).unwrap_err();
        assert!(matches!(err, SecretsError::InsecureMode { .. }));
    }

    #[test]
    fn debug_never_leaks_values() {
        let store = SecretsStore::from_entries([("TOKEN", "SUPER-SECRET-DO-NOT-PRINT")]);
        let dbg = format!("{store:?}");
        assert!(!dbg.contains("SUPER-SECRET"), "debug leaked value: {dbg}");
        assert!(dbg.contains("len: 1"));
    }

    // The global handle is process-wide. Tests can race each other if cargo
    // schedules them in parallel — guard against that with a mutex.
    use std::sync::Mutex;
    static GLOBAL_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn lookup_falls_back_to_env_when_unset_in_store() {
        let _guard = GLOBAL_TEST_LOCK.lock().unwrap();
        reset();
        // Use a vanishingly-unlikely key so we don't collide with the env.
        let key = "DOTAGENT_TEST_SECRETS_FALLBACK_KEY_QQQ";
        unsafe { std::env::set_var(key, "from-env") };
        assert_eq!(lookup(key).as_deref(), Some("from-env"));
        unsafe { std::env::remove_var(key) };
    }

    #[test]
    fn lookup_prefers_store_over_env() {
        let _guard = GLOBAL_TEST_LOCK.lock().unwrap();
        reset();
        let key = "DOTAGENT_TEST_SECRETS_PRIORITY_KEY_QQQ";
        unsafe { std::env::set_var(key, "from-env") };
        install(SecretsStore::from_entries([(key, "from-store")]));
        assert_eq!(lookup(key).as_deref(), Some("from-store"));
        reset();
        unsafe { std::env::remove_var(key) };
    }

    #[test]
    fn reset_if_present_reports_whether_it_dropped_anything() {
        let _guard = GLOBAL_TEST_LOCK.lock().unwrap();
        reset();
        assert!(!reset_if_present(), "nothing installed → false");
        install(SecretsStore::from_entries([("K", "v")]));
        assert!(reset_if_present(), "had a store → true");
        assert!(!reset_if_present(), "now empty again → false");
    }

    #[test]
    fn lookup_returns_none_when_both_miss() {
        let _guard = GLOBAL_TEST_LOCK.lock().unwrap();
        reset();
        let key = "DOTAGENT_TEST_SECRETS_MISSING_KEY_QQQ";
        unsafe { std::env::remove_var(key) };
        assert!(lookup(key).is_none());
    }
}
