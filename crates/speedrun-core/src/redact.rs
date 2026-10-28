use std::sync::LazyLock;

use regex::Regex;

static PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        // Provider API keys and tokens.
        r"sk-ant-[A-Za-z0-9_\-]{20,}",
        r"sk-(?:proj-)?[A-Za-z0-9_\-]{20,}",
        r"gh[pousr]_[A-Za-z0-9]{30,}",
        r"github_pat_[A-Za-z0-9_]{30,}",
        r"xox[abprs]-[A-Za-z0-9\-]{10,}",
        r"(?:sk|rk)_live_[A-Za-z0-9]{16,}",
        r"AKIA[0-9A-Z]{16}",
        r"AIza[0-9A-Za-z_\-]{35}",
        // PEM private keys.
        r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
        // KEY=value / "secret": "value" style assignments.
        r#"(?i)\b[A-Z0-9_]*(?:SECRET|TOKEN|PASSWORD|PASSWD|API_KEY|PRIVATE_KEY)[A-Z0-9_]*\s*[:=]\s*["']?[^\s"']{8,}"#,
    ]
    .iter()
    .map(|p| Regex::new(p).expect("valid redaction regex"))
    .collect()
});

pub const REDACTED: &str = "[REDACTED]";

/// Replace likely secrets in `text`. Returns the new text and whether
/// anything changed. Deliberately over-eager: a false positive costs a few
/// characters of prompt history, a false negative leaks a credential.
pub fn redact(text: &str) -> (String, bool) {
    let mut out = text.to_string();
    for re in PATTERNS.iter() {
        if re.is_match(&out) {
            out = re.replace_all(&out, REDACTED).into_owned();
        }
    }
    let changed = out != text;
    (out, changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_common_secrets() {
        let (out, changed) = redact(
            "use key sk-ant-api03-abcdefghijklmnopqrstuvwx and AWS AKIAABCDEFGHIJKLMNOP\nSTRIPE_SECRET_KEY=sk_live_abcdefghijklmnop1234",
        );
        assert!(changed);
        assert!(!out.contains("abcdefghijklmnopqrstuvwx"));
        assert!(!out.contains("AKIAABCDEFGHIJKLMNOP"));
        assert!(!out.contains("sk_live_"));
    }

    #[test]
    fn leaves_ordinary_prompts_alone() {
        let text = "build a todo app with a password reset flow";
        assert_eq!(redact(text), (text.to_string(), false));
    }
}
