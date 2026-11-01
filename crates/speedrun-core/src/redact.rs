use std::sync::LazyLock;

use regex::Regex;

const TOKEN_PATTERNS: &[&str] = &[
    // Provider API keys, webhook secrets and session tokens.
    r"sk-ant-[A-Za-z0-9_\-]{20,}",
    r"sk-(?:proj-)?[A-Za-z0-9_\-]{20,}",
    r"gh[pousr]_[A-Za-z0-9]{30,}",
    r"github_pat_[A-Za-z0-9_]{30,}",
    r"xox[abprs]-[A-Za-z0-9\-]{10,}",
    r"(?:sk|rk)_(?:live|test)_[A-Za-z0-9]{16,}",
    r"whsec_[A-Za-z0-9]{16,}",
    r"AKIA[0-9A-Z]{16}",
    r"AIza[0-9A-Za-z_\-]{35}",
    r"eyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+",
    // Authorization headers and URLs that carry userinfo credentials.
    r"(?i)\bAuthorization\s*:\s*Bearer\s+[A-Za-z0-9._~+/=\-]{16,}",
    r"\b[a-zA-Z][a-zA-Z0-9+.\-]*://[^\s:/@]+:[^\s@/]+@",
];

const PRIVATE_KEY_PATTERNS: &[&str] = &[
    // A complete PEM block, or an orphaned header whose body runs to EOF.
    r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?(?:-----END [A-Z ]*PRIVATE KEY-----|$)",
];

const ASSIGNMENT_PATTERNS: &[&str] = &[
    // KEY=value / "secret": "value" style assignments, including camelCase names.
    r#"(?i)\b[A-Z0-9_]*(?:SECRET|TOKEN|PASSWORD|PASSWD|PASS|API_?KEY|PRIVATE_?KEY)[A-Z0-9_]*["']?\s*[:=]\s*["']?[^\s"']{8,}"#,
];

static PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    TOKEN_PATTERNS
        .iter()
        .chain(PRIVATE_KEY_PATTERNS)
        .chain(ASSIGNMENT_PATTERNS)
        .copied()
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
