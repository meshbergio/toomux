//! Secrets never go into memory. Memory is searchable from every session and
//! kept far longer than the outputs and transcripts it's drawn from, so values
//! that look like credentials are replaced before anything is indexed.

use regex::Regex;
use std::sync::LazyLock;

static PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        // Bearer / basic credentials in headers.
        r"(?i)\b(authorization:\s*(?:bearer|basic|token)\s+)(\S{8,})",
        // Credentials inside URLs: scheme://user:pass@host
        r"(?i)\b([a-z][a-z0-9+.-]*://[^/\s:@]+:)([^@\s/]{3,})(@)",
        // key = value, key: value, KEY="value" for secret-sounding keys.
        r#"(?i)\b([A-Z0-9_.-]*(?:password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key|private[_-]?key|client[_-]?secret|auth[_-]?(?:token|key)))(\s*[:=]\s*)("[^"\s]{4,}"|'[^'\s]{4,}'|[^\s"',;]{6,})"#,
        // Prose: "the password is `x`", "password was hunter2!", "passphrase `x`".
        r#"(?i)\b((?:password|passwd|passphrase)s?\b[^\S\n]*(?:(?:is|was|to|now|of|becomes|set to)[^\S\n]+)?)(`[^`\s]{4,64}`|"[^"\s]{4,64}"|[A-Za-z]{3,}[0-9!@#$%^&*][^\s,;.)]*|[0-9!@#$%^&*][^\s,;.)]{5,})"#,
    ]
    .iter()
    .map(|p| Regex::new(p).expect("redaction pattern"))
    .collect()
});

/// Well-known token shapes, redacted wherever they appear.
static TOKENS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"\b(?:",
        r"AKIA[0-9A-Z]{16}|ASIA[0-9A-Z]{16}",                      // AWS access key ids
        r"|gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{40,}", // GitHub
        r"|sk-(?:ant-|proj-)?[A-Za-z0-9_-]{20,}",                   // Anthropic, OpenAI
        r"|xox[baprs]-[A-Za-z0-9-]{10,}",                           // Slack
        r"|AIza[0-9A-Za-z_-]{35}",                                  // Google API keys
        r"|glpat-[A-Za-z0-9_-]{20,}",                               // GitLab
        r"|eyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}", // JWTs
        r")\b",
        r"|-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
    ))
    .expect("token pattern")
});

pub const MARK: &str = "[redacted]";

/// `text` with anything that looks like a credential replaced.
pub fn redact(text: &str) -> String {
    let mut out = TOKENS.replace_all(text, MARK).into_owned();
    for (i, re) in PATTERNS.iter().enumerate() {
        out = match i {
            2 => re.replace_all(&out, |c: &regex::Captures| {
                // Leave obvious non-secrets alone: placeholders, env references.
                let v = c[3].trim_matches(['"', '\'']);
                let literal = ["true", "false", "null", "none", "required", "optional", "undefined", "missing", "present"].contains(&v.to_ascii_lowercase().as_str());
                if v.starts_with('$') || v.starts_with('<') || v.starts_with('{') || v.starts_with(':') || literal || v == MARK || v.chars().all(|ch| ch == '*' || ch == 'x' || ch == '.') {
                    c[0].to_string()
                } else {
                    format!("{}{}{MARK}", &c[1], &c[2])
                }
            }),
            3 => re.replace_all(&out, |c: &regex::Captures| {
                // Names and places, not values: env vars, paths, placeholders.
                let v = c[2].trim_matches(['`', '"']);
                let name = v.chars().all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_');
                if name || v.contains('/') || v.starts_with('$') || v.starts_with('<') || v == MARK {
                    c[0].to_string()
                } else {
                    format!("{}{MARK}", &c[1])
                }
            }),
            0 => re.replace_all(&out, format!("${{1}}{MARK}").as_str()),
            _ => re.replace_all(&out, format!("${{1}}{MARK}${{3}}").as_str()),
        }
        .into_owned();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_go_and_the_rest_stays() {
        let r = redact("GITHUB_AUTH_TOKEN=ghx_abcdefgh12345 and auth_key: 'abc12345'"); // gitleaks:allow: fake keys, what redact must mask
        assert!(!r.contains("ghx_abcdefgh12345") && !r.contains("abc12345"), "{r}");
        let r = redact("PACS password = hunter2secret\nexport AWS_SECRET_ACCESS_KEY=abcd1234efgh5678\nkey AKIAABCDEFGHIJKLMNOP here"); // gitleaks:allow
        assert!(!r.contains("hunter2secret") && !r.contains("abcd1234efgh5678") && !r.contains("AKIAABCDEFGHIJKLMNOP"), "{r}"); // gitleaks:allow
        assert!(r.contains("PACS password = [redacted]") && r.contains("key [redacted] here"), "{r}");
        let r = redact("curl -H 'Authorization: Bearer abcdef0123456789xyz' https://u:pa55word@db.example/x"); // gitleaks:allow
        assert!(!r.contains("abcdef0123456789xyz") && !r.contains("pa55word") && r.contains("@db.example"), "{r}");
        // Not secrets: references and prose.
        for (said, gone) in [("the PACS password is `Omni2024!`, ask them", "Omni2024!"), ("its password was hunter2 until", "hunter2"), ("a passphrase \"correct-horse\" here", "correct-horse")] {
            let r = redact(said);
            assert!(!r.contains(gone) && r.contains(MARK), "{r}");
        }
        for keep in [
            "TOKEN=$GH_TOKEN",
            "password: <your password>",
            "the token budget is 400k",
            "handover_tokens = 0",
            "the password is weak",
            "password `DB_PASSWORD` in ~/.env",
            "password in `~/.config/pw.txt`",
            "all 12 tests pass (F9 included)",
            "panicked at src/authority_reach.rs:42:10",
            "Co-Authored-By: Someone <a@b.c>",
            "test auth::tests::login ... ok",
            "origin/main:docs/AUTH.md:12: token rotation",
            "auth_mode=oauth",
            "require_token: false",
        ] {
            assert_eq!(redact(keep), keep);
        }
    }
}
