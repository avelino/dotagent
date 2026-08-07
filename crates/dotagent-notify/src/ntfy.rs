//! ntfy.sh / self-hosted ntfy notifications.
//!
//! The access token travels in an `Authorization: Bearer` header, not in the
//! URL — but the URL still carries the topic, which on a public ntfy.sh is the
//! only thing standing between an alert stream and anyone who guesses it, and
//! `base_url` can legitimately embed HTTP basic credentials. So transport
//! errors are sanitized here too rather than trusted to be harmless, and all
//! three fields (`base_url`, `topic`, `token`) accept `${VAR}` so none of them
//! has to be written literally into a versioned `agent.toml` — see
//! [`crate::secrets`].
//!
//! ## Title and tags are HTTP headers
//!
//! ntfy takes them as `X-Title` / `X-Tags` rather than in a body, and a header
//! value is bytes, not text. Raw UTF-8 there is `obs-text` — RFC 9110 says a
//! sender SHOULD NOT generate it and a recipient may treat it as opaque
//! bytes — so "Falha na execução 🚨" is at the mercy of every proxy on the
//! path and, per ntfy's own docs, can land as `?` characters. ntfy's documented
//! answer is RFC 2047 encoded-words, which its server decodes with
//! `mime.WordDecoder` before reading any header param; [`encode_header_value`]
//! emits them.
//!
//! Headers were kept over ntfy's JSON publishing endpoint deliberately: with
//! headers the request body *is* the message bytes, which is exactly what
//! [`MAX_MESSAGE_BYTES`] measures. Moving to JSON would wrap the message in an
//! envelope and make the cap a statement about something other than what ntfy
//! counts.

use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::limits::{truncate_bytes, ELLIPSIS_NOTICE, TRUNCATION_NOTICE};
use crate::redact::{redact_url_userinfo, sanitize_reqwest_err, scrub_detail};
use crate::secrets::expand_env;
use crate::{Notifier, NotifyContext, NotifyError, Result};

/// Manifest keys these fields are configured under — the labels an operator
/// sees in a `${VAR}` expansion error.
const BASE_URL_FIELD: &str = "ntfy.base_url";
const TOPIC_FIELD: &str = "ntfy.topic";
const TOKEN_FIELD: &str = "ntfy.token";

/// ntfy's cap on a message body — **bytes**, not characters.
///
/// This is the whole reason [`truncate_bytes`] exists. Alert text here is
/// pt-BR: `ç`/`ã`/`é` cost 2 bytes each and an emoji costs 4, so a
/// character-based trim can hand ntfy up to 4x its limit while believing it
/// stayed under. Over the limit, ntfy rejects the request — the alert simply
/// never arrives.
pub(crate) const MAX_MESSAGE_BYTES: usize = 4096;

/// Cap on the title, applied to the **raw** text before encoding.
///
/// ntfy publishes no title limit, but the title is a header and headers live in
/// a fixed server-side buffer (8 KB in nginx's default). Encoding multiplies:
/// a 4-byte emoji becomes 12 characters of `=XX`, so the cap has to bite before
/// the encoder runs, not after.
pub(crate) const MAX_TITLE_BYTES: usize = 250;

/// Longest legal RFC 2047 encoded-word, delimiters included.
const ENCODED_WORD_MAX: usize = 75;
const ENCODED_WORD_OPEN: &str = "=?UTF-8?Q?";
const ENCODED_WORD_CLOSE: &str = "?=";

fn default_base_url() -> String {
    "https://ntfy.sh".into()
}

#[derive(Clone, Serialize, Deserialize)]
pub struct NtfyConfig {
    #[serde(default = "default_base_url")]
    pub base_url: String,
    pub topic: String,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub priority: Option<u8>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Hand-written so the bearer token cannot reach a log through `{:?}` on this
/// struct or on the `NotifierEntry` that holds it. `base_url` is shown, minus
/// any `user:pass@` — a self-hosted instance behind basic auth carries its
/// whole credential there, and the host is the half worth logging.
impl fmt::Debug for NtfyConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NtfyConfig")
            .field("base_url", &redact_url_userinfo(&self.base_url))
            .field("topic", &self.topic)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("priority", &self.priority)
            .field("title", &self.title)
            .field("tags", &self.tags)
            .finish()
    }
}

/// The credential-bearing fields, resolved for one request.
struct Resolved {
    base_url: String,
    topic: String,
    token: Option<String>,
}

impl NtfyConfig {
    /// Expand every `${VAR}`, then validate.
    ///
    /// That order is the point: `topic = "${NTFY_TOPIC}"` is not an empty
    /// topic, and reporting it as one sends the operator looking at the wrong
    /// line of the manifest.
    fn resolve(&self) -> Result<Resolved> {
        let base_url = expand_env(BASE_URL_FIELD, &self.base_url).map_err(NotifyError::Config)?;
        let topic = expand_env(TOPIC_FIELD, &self.topic).map_err(NotifyError::Config)?;
        let token = match &self.token {
            Some(t) => Some(expand_env(TOKEN_FIELD, t).map_err(NotifyError::Config)?),
            None => None,
        };
        if topic.trim().is_empty() {
            return Err(NotifyError::Config("ntfy: topic is required".into()));
        }
        if !base_url.trim().starts_with("http") {
            return Err(NotifyError::Config(
                "ntfy: base_url must be an http(s) URL".into(),
            ));
        }
        Ok(Resolved {
            base_url: base_url.trim().to_string(),
            topic: topic.trim().to_string(),
            token,
        })
    }
}

/// Encode `raw` so it survives an HTTP header value byte-for-byte.
///
/// Plain visible ASCII is passed through — an encoded-word around `disk-alert`
/// would only make the wire harder to read. Anything else becomes RFC 2047
/// `=?UTF-8?Q?…?=` words, which ntfy decodes server-side.
///
/// Three things force encoding:
///
/// - **Non-ASCII** (`execução`, `🚨`): legal for `HeaderValue` to hold, but
///   `obs-text` on the wire and documented by ntfy as possibly arriving as `?`.
/// - **Control characters**: a `\n` in a title is not a header value at all —
///   `HeaderValue` rejects it and the request dies in the builder, taking the
///   whole notification with it. They are flattened to spaces before encoding
///   rather than encoded, because a header is one line by definition.
/// - **A literal `=?…?=`**: ntfy runs every header param through its decoder,
///   so an operator whose title happens to look like an encoded word would see
///   it silently rewritten. Encoding it makes it decode back to itself.
pub(crate) fn encode_header_value(raw: &str) -> String {
    let flattened: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let text = flattened.trim();
    if text.is_ascii() && !text.contains("=?") {
        return text.to_string();
    }

    // Each word must stay under the RFC's 75-char total, and must decode on
    // its own — so the split happens between characters, never inside one
    // character's `=XX` run.
    let budget = ENCODED_WORD_MAX - ENCODED_WORD_OPEN.len() - ENCODED_WORD_CLOSE.len();
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    for c in text.chars() {
        let mut encoded = String::new();
        q_encode_char(c, &mut encoded);
        if !current.is_empty() && current.len() + encoded.len() > budget {
            words.push(std::mem::take(&mut current));
        }
        current.push_str(&encoded);
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
        .iter()
        .map(|w| format!("{ENCODED_WORD_OPEN}{w}{ENCODED_WORD_CLOSE}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// RFC 2047 "Q" encoding of one character.
///
/// Q rather than B (base64) for two reasons: pt-BR is mostly ASCII, so `Q`
/// leaves the text readable on the wire and shorter than base64 would make it;
/// and it needs no dependency to produce.
///
/// Only alphanumerics pass through literally. The RFC requires escaping `=`,
/// `?`, `_` and space, and permits escaping anything else — escaping the rest
/// of the punctuation costs 2 characters each and removes a class of bug where
/// a character that is special in some context slips out unencoded.
fn q_encode_char(c: char, out: &mut String) {
    if c.is_ascii_alphanumeric() {
        out.push(c);
        return;
    }
    if c == ' ' {
        out.push('_');
        return;
    }
    let mut buf = [0u8; 4];
    for b in c.encode_utf8(&mut buf).as_bytes() {
        out.push_str(&format!("={b:02X}"));
    }
}

#[async_trait]
impl Notifier for NtfyConfig {
    fn driver_name(&self) -> &'static str {
        "ntfy"
    }

    async fn send(&self, ctx: &NotifyContext<'_>) -> Result<Option<i64>> {
        let Resolved {
            base_url,
            topic,
            token,
        } = self.resolve()?;
        let url = format!("{}/{}", base_url.trim_end_matches('/'), topic);
        // The body is the message and nothing else, so ntfy's byte cap is
        // measured against exactly the bytes it will count.
        let mut req = reqwest::Client::new().post(&url).body(truncate_bytes(
            ctx.message,
            MAX_MESSAGE_BYTES,
            TRUNCATION_NOTICE,
        ));
        if let Some(t) = &token {
            req = req.bearer_auth(t);
        }
        if let Some(p) = self.priority {
            req = req.header("X-Priority", p.to_string());
        }
        let title = self.title.as_deref().unwrap_or(ctx.agent);
        req = req.header(
            "X-Title",
            // Trim first, encode second: the encoder inflates, and a cap
            // applied to its output would cut an `=XX` run in half.
            encode_header_value(&truncate_bytes(title, MAX_TITLE_BYTES, ELLIPSIS_NOTICE)),
        );
        if !self.tags.is_empty() {
            // ntfy decodes the header before splitting on commas, so encoding
            // the joined string keeps the separators intact.
            req = req.header("X-Tags", encode_header_value(&self.tags.join(",")));
        }
        let res = match req.send().await {
            Ok(r) => r,
            Err(e) => return Err(NotifyError::Backend(sanitize_reqwest_err("ntfy", &e))),
        };
        let status = res.status();
        if !status.is_success() {
            let secrets: Vec<&str> = token
                .as_deref()
                .into_iter()
                .chain(std::iter::once(url.as_str()))
                .collect();
            let detail = match res.text().await {
                Ok(raw) => scrub_detail(&raw, &secrets),
                Err(_) => None,
            };
            return Err(NotifyError::Backend(match detail {
                Some(d) => format!("ntfy returned {status}: {d}"),
                None => format!("ntfy returned {status}"),
            }));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    const TOKEN: &str = "tk_AbCdEf1234567890";

    fn cfg(base_url: &str) -> NtfyConfig {
        NtfyConfig {
            base_url: base_url.into(),
            topic: "dotagent-alerts".into(),
            token: Some(TOKEN.into()),
            priority: None,
            title: None,
            tags: vec![],
        }
    }

    fn ctx(message: &str) -> NotifyContext<'_> {
        NotifyContext {
            agent: "disk-alert",
            schedule: "hourly",
            event: "given_up",
            message,
        }
    }

    /// Decode RFC 2047 encoded-words the way ntfy's server does (Go's
    /// `mime.WordDecoder`): decode each `=?UTF-8?Q?…?=`, and drop the
    /// whitespace that separates two adjacent ones.
    ///
    /// Written out rather than asserted against a golden string because the
    /// property that matters is round-tripping — the operator has to get back
    /// the title they wrote, whatever the encoder chose to do in between.
    fn decode_encoded_words(input: &str) -> String {
        let mut out = String::new();
        let mut rest = input;
        let mut previous_was_word = false;
        loop {
            let Some(start) = rest.find(ENCODED_WORD_OPEN) else {
                out.push_str(rest);
                return out;
            };
            let gap = &rest[..start];
            // Whitespace between adjacent encoded-words is a fold, not content.
            if !(previous_was_word && gap.trim().is_empty()) {
                out.push_str(gap);
            }
            let after = &rest[start + ENCODED_WORD_OPEN.len()..];
            let end = after
                .find(ENCODED_WORD_CLOSE)
                .expect("encoded word must be closed");
            let mut bytes: Vec<u8> = Vec::new();
            let mut chars = after[..end].chars();
            while let Some(c) = chars.next() {
                match c {
                    '_' => bytes.push(b' '),
                    '=' => {
                        let hex: String = chars.by_ref().take(2).collect();
                        bytes.push(u8::from_str_radix(&hex, 16).expect("valid hex"));
                    }
                    other => {
                        let mut buf = [0u8; 4];
                        bytes.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
                    }
                }
            }
            out.push_str(&String::from_utf8(bytes).expect("each word decodes to valid UTF-8"));
            rest = &after[end + ENCODED_WORD_CLOSE.len()..];
            previous_was_word = true;
        }
    }

    fn encoded_words(value: &str) -> Vec<&str> {
        value.split(' ').filter(|w| w.starts_with("=?")).collect()
    }

    // --- deny: a non-ASCII title must never go out raw ---------------------

    #[test]
    fn an_accented_title_is_encoded_not_sent_raw() {
        let out = encode_header_value("Falha na execução");
        assert!(!out.contains('ç'), "raw UTF-8 reached the header: {out}");
        assert!(out.starts_with("=?UTF-8?Q?"), "{out}");
        assert_eq!(decode_encoded_words(&out), "Falha na execução");
    }

    #[test]
    fn an_emoji_title_is_encoded() {
        let out = encode_header_value("🚨 alerta");
        assert!(out.is_ascii(), "{out}");
        assert_eq!(decode_encoded_words(&out), "🚨 alerta");
    }

    #[test]
    fn a_title_with_an_accent_and_an_emoji_round_trips() {
        // The common case in this repo, not the exotic one.
        let title = "🚨 Falha na execução do agente — não há espaço em disco ✅";
        let out = encode_header_value(title);
        assert!(out.is_ascii(), "header value must be pure ASCII: {out}");
        assert!(
            HeaderValue::from_str(&out).is_ok(),
            "must be a legal header value: {out}"
        );
        assert_eq!(decode_encoded_words(&out), title);
    }

    #[test]
    fn a_title_with_a_newline_cannot_kill_the_request() {
        // `HeaderValue` rejects control characters outright, and a rejected
        // value fails the whole request in the builder — the notification is
        // lost, not degraded.
        assert!(HeaderValue::from_str("Falha\nna execução").is_err());
        let out = encode_header_value("Falha\nna execução");
        assert!(HeaderValue::from_str(&out).is_ok(), "{out}");
        assert_eq!(decode_encoded_words(&out), "Falha na execução");
    }

    #[test]
    fn control_characters_alone_cannot_kill_the_request() {
        for raw in ["a\rb", "a\tb", "a\u{0}b", "\u{7f}"] {
            let out = encode_header_value(raw);
            assert!(
                HeaderValue::from_str(&out).is_ok(),
                "{raw:?} produced {out:?}"
            );
        }
    }

    #[test]
    fn a_title_that_looks_like_an_encoded_word_is_itself_encoded() {
        // ntfy decodes every header param, so an ASCII title shaped like an
        // encoded-word would be silently rewritten on arrival.
        let raw = "=?UTF-8?B?8J+HqfCfh6o=?=";
        let out = encode_header_value(raw);
        assert_eq!(decode_encoded_words(&out), raw);
    }

    #[test]
    fn every_encoded_word_stays_within_the_rfc_limit() {
        let title = "execução ".repeat(40);
        let out = encode_header_value(&title);
        let words = encoded_words(&out);
        assert!(words.len() > 1, "long input must fold: {out}");
        for w in words {
            assert!(w.len() <= ENCODED_WORD_MAX, "{} chars: {w}", w.len());
        }
    }

    #[test]
    fn folding_never_splits_a_character_across_two_words() {
        // Each encoded-word has to decode on its own; half of an emoji's four
        // bytes in one word and half in the next is not valid UTF-8.
        let title = "🚨".repeat(60);
        let out = encode_header_value(&title);
        for w in encoded_words(&out) {
            // `decode_encoded_words` asserts valid UTF-8 per word internally.
            decode_encoded_words(w);
        }
        assert_eq!(decode_encoded_words(&out), title);
    }

    #[test]
    fn a_long_non_ascii_title_is_capped_before_encoding() {
        let title = "execução ".repeat(200);
        let capped = truncate_bytes(&title, MAX_TITLE_BYTES, ELLIPSIS_NOTICE);
        assert!(capped.len() <= MAX_TITLE_BYTES, "{} bytes", capped.len());
        let out = encode_header_value(&capped);
        assert!(HeaderValue::from_str(&out).is_ok(), "{out}");
        // Encoding inflates ~3x for accented text plus per-word overhead; the
        // point of capping first is that the header stays bounded regardless.
        assert!(out.len() < 1_000, "{} chars", out.len());
    }

    #[test]
    fn non_ascii_tags_are_encoded_with_their_separators_intact() {
        // ntfy decodes the header, *then* splits on commas.
        let out = encode_header_value(&["alerta", "execução", "🚨"].join(","));
        assert!(out.is_ascii(), "{out}");
        let decoded = decode_encoded_words(&out);
        assert_eq!(
            decoded.split(',').collect::<Vec<_>>(),
            vec!["alerta", "execução", "🚨"]
        );
    }

    /// Answer one request with `204 No Content` and hand back what arrived.
    async fn capture_one() -> (u16, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return String::new();
            };
            let mut buf = vec![0u8; 16_384];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let _ = sock
                .write_all(
                    b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await;
            let _ = sock.shutdown().await;
            // Lossy on purpose: raw UTF-8 in a header would show up here as
            // replacement characters rather than failing the read.
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        (port, handle)
    }

    #[tokio::test]
    async fn a_ptbr_title_leaves_the_process_as_ascii_and_the_body_untouched() {
        let (port, captured) = capture_one().await;
        let mut c = cfg(&format!("http://127.0.0.1:{port}"));
        c.token = None;
        c.title = Some("🚨 Falha na execução".into());
        c.tags = vec!["alerta".into(), "execução".into()];
        c.send(&ctx("não há espaço em disco")).await.unwrap();

        let raw = captured.await.unwrap();
        let (head, body) = raw.split_once("\r\n\r\n").expect("request head and body");
        assert!(head.is_ascii(), "non-ASCII reached the wire: {head}");

        let title = head
            .lines()
            .find_map(|l| l.strip_prefix("x-title: ").or(l.strip_prefix("X-Title: ")))
            .expect("X-Title must be sent");
        assert_eq!(decode_encoded_words(title.trim()), "🚨 Falha na execução");

        let tags = head
            .lines()
            .find_map(|l| l.strip_prefix("x-tags: ").or(l.strip_prefix("X-Tags: ")))
            .expect("X-Tags must be sent");
        assert_eq!(decode_encoded_words(tags.trim()), "alerta,execução");

        // The body is still the message verbatim — that is what ntfy's byte
        // cap is measured against.
        assert_eq!(body, "não há espaço em disco");
    }

    #[tokio::test]
    async fn a_title_with_a_newline_still_delivers() {
        // Before encoding this died in `HeaderValue`, and a builder error kills
        // the request before it is ever sent.
        let (port, captured) = capture_one().await;
        let mut c = cfg(&format!("http://127.0.0.1:{port}"));
        c.token = None;
        c.title = Some("Falha\nna execução".into());
        c.send(&ctx("m")).await.unwrap();
        let raw = captured.await.unwrap();
        assert!(raw.contains("POST /dotagent-alerts"), "{raw}");
    }

    // --- deny: credentials must not survive an error path ------------------

    #[tokio::test]
    async fn transport_error_never_carries_the_token_or_topic() {
        let err = cfg("http://127.0.0.1:1").send(&ctx("m")).await.unwrap_err();
        let rendered = format!("{err}");
        assert!(!rendered.contains(TOKEN), "leaked token: {rendered}");
        assert!(!rendered.contains("dotagent-alerts"), "{rendered}");
        assert!(rendered.contains("ntfy transport error"), "{rendered}");
    }

    #[tokio::test]
    async fn transport_error_never_carries_basic_credentials_in_base_url() {
        // A self-hosted ntfy behind basic auth: the URL itself is a secret.
        let err = cfg("http://admin:hunter2hunter2@127.0.0.1:1")
            .send(&ctx("m"))
            .await
            .unwrap_err();
        let rendered = format!("{err}");
        assert!(!rendered.contains("hunter2hunter2"), "{rendered}");
    }

    #[test]
    fn debug_redacts_the_token() {
        let dbg = format!("{:?}", cfg("https://ntfy.sh"));
        assert!(!dbg.contains(TOKEN), "debug leaked token: {dbg}");
        assert!(dbg.contains("<redacted>"), "{dbg}");
    }

    #[test]
    fn debug_redacts_basic_credentials_in_base_url() {
        let dbg = format!("{:?}", cfg("https://admin:hunter2hunter2@ntfy.example.com"));
        assert!(
            !dbg.contains("hunter2hunter2"),
            "debug leaked basic auth: {dbg}"
        );
        assert!(dbg.contains("ntfy.example.com"), "host must survive: {dbg}");
    }

    #[test]
    fn debug_of_the_enclosing_entry_redacts_too() {
        let toml_str = format!(
            r#"
            driver = "ntfy"
            topic = "alerts"
            token = "{TOKEN}"
            "#
        );
        let entry: crate::NotifierEntry = toml::from_str(&toml_str).unwrap();
        assert!(!format!("{entry:?}").contains(TOKEN));
    }

    #[tokio::test]
    async fn send_rejects_an_empty_topic() {
        for bad in ["", "   "] {
            let mut c = cfg("https://ntfy.sh");
            c.topic = bad.into();
            let err = c.send(&ctx("m")).await.unwrap_err();
            assert!(
                matches!(err, NotifyError::Config(_)),
                "topic {bad:?} should be rejected, got {err:?}"
            );
        }
    }

    // --- deny: `${VAR}` that does not resolve -------------------------------

    #[tokio::test]
    async fn send_rejects_an_unset_var_in_any_field() {
        const UNSET: &str = "${DOTAGENT_TEST_NTFY_UNSET_QQQ}";
        for field in ["ntfy.base_url", "ntfy.topic", "ntfy.token"] {
            let mut c = cfg("https://ntfy.sh");
            match field {
                "ntfy.base_url" => c.base_url = UNSET.into(),
                "ntfy.topic" => c.topic = UNSET.into(),
                _ => c.token = Some(UNSET.into()),
            }
            let err = c.send(&ctx("m")).await.unwrap_err();
            match err {
                NotifyError::Config(msg) => {
                    assert!(msg.contains("unset"), "{field}: {msg}");
                    assert!(msg.contains("DOTAGENT_TEST_NTFY_UNSET_QQQ"), "{msg}");
                    assert!(msg.contains(field), "must name the field: {msg}");
                }
                other => panic!("{field}: expected Config error, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn an_unresolved_topic_is_not_reported_as_an_empty_one() {
        // Expansion before validation: "topic is required" would send the
        // operator to the wrong line of the manifest.
        let mut c = cfg("https://ntfy.sh");
        c.topic = "${DOTAGENT_TEST_NTFY_UNSET_QQQ}".into();
        let err = c.send(&ctx("m")).await.unwrap_err();
        let NotifyError::Config(msg) = err else {
            panic!("expected Config, got {err:?}");
        };
        assert!(!msg.contains("topic is required"), "wrong diagnosis: {msg}");
    }

    #[test]
    fn resolution_rejects_a_base_url_that_is_not_http() {
        let out =
            crate::secrets::with_store(&[("DOTAGENT_TEST_NTFY_BAD", "ntfy.example.com")], || {
                cfg("${DOTAGENT_TEST_NTFY_BAD}").resolve()
            });
        assert!(matches!(out, Err(NotifyError::Config(_))), "not http");
    }

    #[test]
    fn a_resolution_error_never_quotes_a_resolved_sibling() {
        let out =
            crate::secrets::with_store(&[("DOTAGENT_TEST_NTFY_TOPIC", "dotagent-alerts")], || {
                let mut c = cfg("https://ntfy.sh");
                c.topic = "${DOTAGENT_TEST_NTFY_TOPIC}".into();
                c.token = Some("${DOTAGENT_TEST_NTFY_UNSET_QQQ}".into());
                c.resolve()
            });
        let Err(NotifyError::Config(msg)) = out else {
            panic!("expected Config error, got resolution success");
        };
        assert!(!msg.contains("dotagent-alerts"), "leaked the topic: {msg}");
    }

    // --- allow: `${VAR}` that resolves --------------------------------------

    #[test]
    fn resolution_expands_every_field_from_the_secrets_store() {
        let resolved = crate::secrets::with_store(
            &[
                ("DOTAGENT_TEST_NTFY_URL", "https://ntfy.example.com"),
                ("DOTAGENT_TEST_NTFY_TOPIC2", "dotagent-alerts"),
                ("DOTAGENT_TEST_NTFY_TOKEN", TOKEN),
            ],
            || {
                let mut c = cfg("${DOTAGENT_TEST_NTFY_URL}");
                c.topic = "${DOTAGENT_TEST_NTFY_TOPIC2}".into();
                c.token = Some("${DOTAGENT_TEST_NTFY_TOKEN}".into());
                c.resolve()
            },
        )
        .unwrap();
        assert_eq!(resolved.base_url, "https://ntfy.example.com");
        assert_eq!(resolved.topic, "dotagent-alerts");
        assert_eq!(resolved.token.as_deref(), Some(TOKEN));
    }

    #[test]
    fn resolution_leaves_literal_fields_alone() {
        // Backwards compatibility: configs that predate `${VAR}` keep working.
        let resolved = cfg("https://ntfy.sh").resolve().unwrap();
        assert_eq!(resolved.base_url, "https://ntfy.sh");
        assert_eq!(resolved.topic, "dotagent-alerts");
        assert_eq!(resolved.token.as_deref(), Some(TOKEN));
    }

    #[test]
    fn a_plain_ascii_title_is_left_unencoded() {
        assert_eq!(encode_header_value("disk-alert"), "disk-alert");
        assert_eq!(
            encode_header_value("disk at 95% [prod]"),
            "disk at 95% [prod]"
        );
    }

    // --- the byte cap ------------------------------------------------------

    #[test]
    fn title_encoding_does_not_touch_the_message_cap() {
        // The title is a header and the message is the body; encoding one must
        // not move the budget of the other. Regression guard for a JSON-body
        // rewrite, which would make both share one envelope.
        let text = "🚨 execução falhou ".repeat(400);
        let body = truncate_bytes(&text, MAX_MESSAGE_BYTES, TRUNCATION_NOTICE);
        assert!(body.len() <= MAX_MESSAGE_BYTES, "{} bytes", body.len());
        let with_title = encode_header_value("Falha na execução 🚨");
        assert_eq!(
            truncate_bytes(&text, MAX_MESSAGE_BYTES, TRUNCATION_NOTICE).len(),
            body.len(),
            "the title must not consume the message budget"
        );
        assert!(with_title.is_ascii());
    }

    #[test]
    fn accented_text_is_capped_in_bytes_not_characters() {
        // 3000 chars of "ç" is 6000 bytes — under a 4096-*char* cap, well over
        // the 4096-*byte* one that ntfy actually enforces.
        let text = "ç".repeat(3_000);
        assert!(text.chars().count() < MAX_MESSAGE_BYTES);
        assert!(text.len() > MAX_MESSAGE_BYTES);
        let out = truncate_bytes(&text, MAX_MESSAGE_BYTES, TRUNCATION_NOTICE);
        assert!(out.len() <= MAX_MESSAGE_BYTES, "{} bytes", out.len());
        assert!(out.ends_with("[truncated]"));
    }

    #[test]
    fn emoji_text_is_capped_in_bytes_not_characters() {
        // 4 bytes per emoji: 2000 chars is 8000 bytes.
        let text = "🚨".repeat(2_000);
        assert!(text.chars().count() < MAX_MESSAGE_BYTES);
        assert!(text.len() > MAX_MESSAGE_BYTES);
        let out = truncate_bytes(&text, MAX_MESSAGE_BYTES, TRUNCATION_NOTICE);
        assert!(out.len() <= MAX_MESSAGE_BYTES, "{} bytes", out.len());
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn a_mixed_ptbr_body_is_capped_without_slicing_a_codepoint() {
        let text = "🚨 execução falhou: não há espaço em disco. ".repeat(300);
        assert!(text.len() > MAX_MESSAGE_BYTES);
        let out = truncate_bytes(&text, MAX_MESSAGE_BYTES, TRUNCATION_NOTICE);
        assert!(out.len() <= MAX_MESSAGE_BYTES, "{} bytes", out.len());
        // Round-tripping proves no codepoint was cut in half.
        assert_eq!(String::from_utf8(out.clone().into_bytes()).unwrap(), out);
    }

    // --- allow -------------------------------------------------------------

    #[test]
    fn an_ordinary_accented_body_is_left_alone() {
        let text = "execução concluída ✅";
        assert_eq!(
            truncate_bytes(text, MAX_MESSAGE_BYTES, TRUNCATION_NOTICE),
            text
        );
    }
}
