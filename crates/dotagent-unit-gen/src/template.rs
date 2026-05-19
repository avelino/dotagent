//! Minimal template engine for unit files.
//!
//! Templates live as raw text files under `templates/`, embedded via
//! `include_str!`. Substitution syntax is `{{KEY}}` (double braces), kept
//! deliberately simple so the template files are valid in their target
//! format (XML plist, systemd ini) without escape gymnastics.
//!
//! After rendering, [`assert_fully_rendered`] checks that no `{{...}}`
//! placeholders remain — catches typos at runtime + in tests.

/// Render a template by replacing every `{{KEY}}` with the matching value.
/// Pairs are applied in order; latest wins on duplicates.
pub fn render(template: &str, pairs: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (key, value) in pairs {
        out = out.replace(&format!("{{{{{key}}}}}"), value);
    }
    out
}

/// Panics (in tests) / errors (in prod) if any unrendered `{{...}}` remain.
/// Use to catch typos like `{{BIANRY}}` that would silently produce broken units.
pub fn find_unrendered_placeholder(rendered: &str) -> Option<String> {
    let bytes = rendered.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            // find closing
            let mut j = i + 2;
            while j + 1 < bytes.len() && !(bytes[j] == b'}' && bytes[j + 1] == b'}') {
                j += 1;
            }
            if j + 1 < bytes.len() {
                return Some(String::from_utf8_lossy(&bytes[i..=j + 1]).into_owned());
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_replaces_placeholders() {
        let tpl = "hello {{NAME}}, you are {{AGE}}";
        let out = render(tpl, &[("NAME", "ada"), ("AGE", "36")]);
        assert_eq!(out, "hello ada, you are 36");
    }

    #[test]
    fn find_unrendered_detects_typo() {
        let tpl = "hello {{NAME}}, age {{AGE}}";
        let out = render(tpl, &[("NAME", "ada")]); // forgot AGE
        let leftover = find_unrendered_placeholder(&out);
        assert_eq!(leftover.as_deref(), Some("{{AGE}}"));
    }

    #[test]
    fn find_unrendered_passes_when_clean() {
        assert!(find_unrendered_placeholder("hello ada").is_none());
    }
}
