//! Small system-level helpers (current user lookup, …).
//!
//! Cross-platform: `$USER` on Unix, `$USERNAME` on Windows, with a
//! `whoami` command fallback. Output is sanitised for use as a pay
//! account name (lowercased, ASCII-alphanumeric / `-` / `_` only;
//! Windows `DOMAIN\user` is reduced to the user portion).

/// Default fallback when the system user cannot be determined or
/// sanitises to nothing.
pub const FALLBACK_ACCOUNT_NAME: &str = "default";

/// Best-effort current system username, sanitised for use as an account name.
pub fn current_user_account_name() -> String {
    let raw = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .or_else(|| {
            // `whoami.exe` on Windows, `whoami` elsewhere — both write the
            // username to stdout (Windows form: `DOMAIN\user`).
            let bin = if cfg!(windows) {
                "whoami.exe"
            } else {
                "whoami"
            };
            std::process::Command::new(bin)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        });

    sanitize_username(raw.as_deref().unwrap_or(FALLBACK_ACCOUNT_NAME))
}

fn sanitize_username(s: &str) -> String {
    // Windows `whoami` returns `DOMAIN\user`; some shells return `user@host`.
    let s = s
        .trim()
        .rsplit(['\\', '/', '@'])
        .next()
        .unwrap_or("")
        .trim();

    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('-');

    if cleaned.is_empty() {
        FALLBACK_ACCOUNT_NAME.to_string()
    } else {
        cleaned.to_ascii_lowercase()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_windows_domain() {
        assert_eq!(sanitize_username("DOMAIN\\jdoe"), "jdoe");
        assert_eq!(sanitize_username("CONTOSO\\Jane.Doe"), "jane-doe");
    }

    #[test]
    fn strips_unix_at_host() {
        assert_eq!(sanitize_username("ludo@laptop"), "laptop");
    }

    #[test]
    fn lowercases() {
        assert_eq!(sanitize_username("Ludo"), "ludo");
    }

    #[test]
    fn fallback_on_empty_or_garbage() {
        assert_eq!(sanitize_username(""), FALLBACK_ACCOUNT_NAME);
        assert_eq!(sanitize_username("---"), FALLBACK_ACCOUNT_NAME);
        assert_eq!(sanitize_username("   \n"), FALLBACK_ACCOUNT_NAME);
    }

    #[test]
    fn keeps_dashes_underscores() {
        assert_eq!(sanitize_username("ludo_test"), "ludo_test");
        assert_eq!(sanitize_username("ludo-test"), "ludo-test");
    }

    #[test]
    fn replaces_other_chars() {
        assert_eq!(sanitize_username("ludo galabru"), "ludo-galabru");
        assert_eq!(sanitize_username("ludo.smith"), "ludo-smith");
    }

    #[test]
    fn trims_trailing_newline_from_whoami_output() {
        assert_eq!(sanitize_username("ludo\n"), "ludo");
    }
}
