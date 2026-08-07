//! Message-size caps, shared by every driver that has one.
//!
//! Each backend rejects the whole request when the body is over its limit — so
//! an alert that is too long is an alert that never arrives. Trimming here
//! turns "silently undelivered" into "delivered, minus the tail", and the
//! notice says which one you got.
//!
//! Three cutters, because the backends do not agree on what they are counting:
//! Slack and Pushover cap **characters**, ntfy caps **bytes**, and Telegram
//! caps **UTF-16 code units**. That distinction is not academic for pt-BR alert
//! text — `ç`/`ã` are 2 bytes and an emoji is 4, so 4096 characters can be
//! 16 KB on the wire, and 4096 emoji are 8192 units to Telegram.

/// Appended when the tail was dropped. Leading newline so it does not run into
/// the last line of the body.
pub(crate) const TRUNCATION_NOTICE: &str = "\n[truncated]";

/// Shorter marker for single-line fields (titles), where a newline plus a
/// bracketed word would cost more than the text it saves.
pub(crate) const ELLIPSIS_NOTICE: &str = "…";

/// Trim `text` to at most `max_chars` characters, appending `notice`.
pub(crate) fn truncate_chars(text: &str, max_chars: usize, notice: &str) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let notice_len = notice.chars().count();
    // A notice that does not fit would push the body back over the cap it
    // exists to enforce. Losing the marker beats losing the delivery.
    if notice_len >= max_chars {
        return text.chars().take(max_chars).collect();
    }
    let mut head: String = text.chars().take(max_chars - notice_len).collect();
    head.push_str(notice);
    head
}

/// Trim `text` so its **UTF-16** encoding is at most `max_units` code units,
/// appending `notice`.
///
/// Telegram's 4096 is a UTF-16 count, not a character count
/// (<https://core.telegram.org/api/entities>): every codepoint outside the BMP —
/// which is every emoji — costs 2. A character-based cutter therefore lets
/// 3000 `🚨` through as "well under the cap" and hands Telegram 6000 units,
/// earning the `message is too long` 400 the cutter exists to prevent.
///
/// `fixup` runs on the head *before* the notice is appended, and must never
/// grow it — Telegram uses it to drop a backslash the cut orphaned.
pub(crate) fn truncate_utf16_with(
    text: &str,
    max_units: usize,
    notice: &str,
    fixup: impl FnOnce(&mut String),
) -> String {
    if text.encode_utf16().count() <= max_units {
        return text.to_string();
    }
    let notice_units = notice.encode_utf16().count();
    // Same trade as the other cutters: a notice that does not fit would push
    // the body back over the cap, and losing the marker beats losing the send.
    if notice_units >= max_units {
        return take_utf16(text, max_units);
    }
    let mut head = take_utf16(text, max_units - notice_units);
    fixup(&mut head);
    head.push_str(notice);
    head
}

/// Longest prefix of `text` fitting in `max_units` UTF-16 code units.
///
/// A character is taken whole or not at all. Half a surrogate pair is not a
/// `char` and `String` cannot hold one, so a budget that lands mid-pair leaves
/// the result one unit under — under is correct, over is a 400.
fn take_utf16(text: &str, max_units: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for c in text.chars() {
        let width = c.len_utf16();
        if used + width > max_units {
            break;
        }
        out.push(c);
        used += width;
    }
    out
}

/// Trim `text` so its UTF-8 encoding is at most `max_bytes` bytes, appending
/// `notice`.
///
/// The cut is walked back to a char boundary: a sliced codepoint is not valid
/// UTF-8, and the request would be rejected for a different reason than the one
/// we were trying to avoid. That walk-back is why the result can land a few
/// bytes under the cap — under is correct, over is a 413.
pub(crate) fn truncate_bytes(text: &str, max_bytes: usize, notice: &str) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let notice_bytes = notice.len();
    let keep_notice = notice_bytes < max_bytes;
    let budget = if keep_notice {
        max_bytes - notice_bytes
    } else {
        max_bytes
    };
    let mut end = budget.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let head = &text[..end];
    if keep_notice {
        format!("{head}{notice}")
    } else {
        head.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chars_leaves_short_text_alone() {
        assert_eq!(truncate_chars("hi", 100, TRUNCATION_NOTICE), "hi");
    }

    #[test]
    fn chars_keeps_text_that_lands_exactly_on_the_cap() {
        let text = "x".repeat(10);
        assert_eq!(truncate_chars(&text, 10, TRUNCATION_NOTICE), text);
    }

    #[test]
    fn chars_marks_and_respects_the_cap() {
        let out = truncate_chars(&"x".repeat(500), 100, TRUNCATION_NOTICE);
        assert!(out.ends_with("[truncated]"));
        assert!(out.chars().count() <= 100);
    }

    #[test]
    fn chars_counts_characters_not_bytes() {
        // A byte-based cut would slice these 2-byte codepoints in half.
        let out = truncate_chars(&"é".repeat(200), 100, TRUNCATION_NOTICE);
        assert_eq!(out.chars().count(), 100);
    }

    #[test]
    fn chars_drops_a_notice_that_does_not_fit() {
        let out = truncate_chars(&"x".repeat(50), 5, TRUNCATION_NOTICE);
        assert_eq!(out.chars().count(), 5);
        assert_eq!(out, "xxxxx");
    }

    // --- utf-16 ------------------------------------------------------------

    #[test]
    fn utf16_leaves_short_text_alone() {
        assert_eq!(
            truncate_utf16_with("hi", 100, TRUNCATION_NOTICE, |_| {}),
            "hi"
        );
    }

    #[test]
    fn utf16_counts_astral_codepoints_as_two() {
        // 200 emoji is 200 *chars* — a char-based cutter calls that half of a
        // 400 cap and ships 400 units. This is the axis nobody counted.
        let text = "🚨".repeat(200);
        assert_eq!(text.chars().count(), 200);
        assert_eq!(text.encode_utf16().count(), 400);
        let out = truncate_utf16_with(&text, 100, TRUNCATION_NOTICE, |_| {});
        assert!(
            out.encode_utf16().count() <= 100,
            "{} units",
            out.encode_utf16().count()
        );
        assert!(out.ends_with("[truncated]"));
    }

    #[test]
    fn utf16_treats_bmp_text_exactly_like_the_char_cutter() {
        // Inside the BMP the two counts agree, so the stricter cutter must not
        // trim anything the char cutter would have kept.
        let text = "ação concluída ".repeat(40);
        for cap in 1..200 {
            assert_eq!(
                truncate_utf16_with(&text, cap, TRUNCATION_NOTICE, |_| {}),
                truncate_chars(&text, cap, TRUNCATION_NOTICE),
                "cap {cap}"
            );
        }
    }

    #[test]
    fn utf16_never_splits_a_surrogate_pair() {
        // Mixed BMP + astral at every cap in a range: the budget lands mid-pair
        // for half of them, and the answer is to drop the whole character.
        let text = "ação 🚨 ok çé 🔥 ".repeat(20);
        for cap in 1..200 {
            let out = truncate_utf16_with(&text, cap, TRUNCATION_NOTICE, |_| {});
            assert!(
                out.encode_utf16().count() <= cap,
                "cap {cap} produced {} units",
                out.encode_utf16().count()
            );
        }
    }

    #[test]
    fn utf16_drops_a_notice_that_does_not_fit() {
        let out = truncate_utf16_with(&"x".repeat(50), 5, TRUNCATION_NOTICE, |_| {});
        assert_eq!(out, "xxxxx");
    }

    #[test]
    fn utf16_handles_a_zero_cap() {
        assert_eq!(
            truncate_utf16_with("🚨anything", 0, TRUNCATION_NOTICE, |_| {}),
            ""
        );
    }

    #[test]
    fn utf16_keeps_text_that_lands_exactly_on_the_cap() {
        let text = "🚨🚨"; // 2 chars, 4 units
        assert_eq!(text.encode_utf16().count(), 4);
        assert_eq!(
            truncate_utf16_with(text, 4, TRUNCATION_NOTICE, |_| {}),
            text
        );
    }

    #[test]
    fn utf16_runs_the_fixup_on_the_head_only() {
        let out = truncate_utf16_with(&"x".repeat(50), 20, "!", |head| {
            head.pop();
        });
        assert_eq!(out, format!("{}!", "x".repeat(18)));
    }

    // --- bytes -------------------------------------------------------------

    #[test]
    fn bytes_leaves_short_text_alone() {
        assert_eq!(truncate_bytes("hi", 100, TRUNCATION_NOTICE), "hi");
    }

    #[test]
    fn bytes_respects_a_byte_cap_on_accented_text() {
        // 200 chars of "ç" is 400 bytes: a char-based cutter would call this
        // well under a 100-*byte* cap and ship 4x the limit.
        let text = "ç".repeat(200);
        assert_eq!(text.chars().count(), 200);
        assert_eq!(text.len(), 400);
        let out = truncate_bytes(&text, 100, TRUNCATION_NOTICE);
        assert!(out.len() <= 100, "{} bytes", out.len());
        assert!(out.ends_with("[truncated]"));
    }

    #[test]
    fn bytes_respects_a_byte_cap_on_emoji() {
        // 4 bytes per codepoint, and the cap is deliberately not a multiple of
        // 4 so the walk-back has to do real work.
        let text = "🚨".repeat(100);
        assert_eq!(text.len(), 400);
        let out = truncate_bytes(&text, 51, TRUNCATION_NOTICE);
        assert!(out.len() <= 51, "{} bytes", out.len());
        assert!(out.ends_with("[truncated]"));
    }

    #[test]
    fn bytes_never_slices_a_codepoint() {
        // Mixed pt-BR + emoji at every cap in a range: any cut landing
        // mid-codepoint would make this string invalid UTF-8, which `String`
        // cannot even represent — so the check is that the head round-trips.
        let text = "ação 🚨 concluída çãé 🔥 ".repeat(20);
        for cap in 1..120 {
            let out = truncate_bytes(&text, cap, TRUNCATION_NOTICE);
            assert!(out.len() <= cap, "cap {cap} produced {} bytes", out.len());
            assert!(std::str::from_utf8(out.as_bytes()).is_ok());
        }
    }

    #[test]
    fn bytes_drops_a_notice_that_does_not_fit() {
        let out = truncate_bytes(&"x".repeat(50), 5, TRUNCATION_NOTICE);
        assert_eq!(out, "xxxxx");
    }

    #[test]
    fn bytes_handles_a_zero_cap() {
        assert_eq!(truncate_bytes("anything", 0, TRUNCATION_NOTICE), "");
    }

    #[test]
    fn bytes_keeps_text_that_lands_exactly_on_the_cap() {
        let text = "çç"; // 4 bytes
        assert_eq!(truncate_bytes(text, 4, TRUNCATION_NOTICE), text);
    }

    #[test]
    fn ellipsis_notice_is_one_char() {
        assert_eq!(ELLIPSIS_NOTICE.chars().count(), 1);
    }
}
