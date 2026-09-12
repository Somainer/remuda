//! The environment an agent child process is allowed to see.
//!
//! Agent children are built with [`Command::env_clear`] and then given only
//! what this module hands back: a small set of names inherited from the Node
//! (`PATH`, `HOME`, locale, …) plus whatever the materializer resolved into
//! the recipe's `env_allowlist`. Nothing else is inherited.
//!
//! Without this the child inherits the Node's whole environment, including
//! `REMUDA_BOOTSTRAP_TOKEN`, `REMUDA_HOST_TOKEN`, and any provider API key —
//! all readable by the model and by any tool it runs
//! (`security-review-2.md` S1).
//!
//! The denylist is the second half: an allowlisted *name* can still carry a
//! dangerous *value*. `LD_PRELOAD` and `NODE_OPTIONS` are code execution in
//! the child; `HTTPS_PROXY` and `SSL_CERT_FILE` are traffic redirection and
//! TLS interception (S2). These are refused no matter who supplies them — an
//! instance spec, a provider profile, or the Node's own environment.

use std::collections::BTreeMap;

/// Names inherited from the parent process when present.
///
/// Everything here is needed for a CLI to run at all (find its binary, find
/// its config, render text) and carries no credential.
const INHERIT: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "TERM",
    "TMPDIR",
    "SHELL",
    "USER",
    "LOGNAME",
    // Claude and Codex resolve their config under XDG when it is set; without
    // it they fall back to $HOME, which is already allowed.
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "XDG_DATA_HOME",
];

/// Inherited prefixes: locale categories only (`LC_ALL`, `LC_CTYPE`, …).
const INHERIT_PREFIXES: &[&str] = &["LC_"];

/// Exact names that may never reach an agent child, whatever the source.
const DENY: &[&str] = &[
    // Loader / interpreter hijack.
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "NODE_OPTIONS",
    "PYTHONSTARTUP",
    "PERL5OPT",
    "RUBYOPT",
    "BASH_ENV",
    "ENV",
    "GIT_SSH_COMMAND",
    // TLS interception.
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "REQUESTS_CA_BUNDLE",
    "NODE_EXTRA_CA_CERTS",
    "CURL_CA_BUNDLE",
    // Native features the driver contract disables; see `flags.rs`.
    "CLAUDE_CODE_SIMPLE",
    "CLAUDE_CODE_SAFE_MODE",
];

/// Denied prefixes.
///
/// `REMUDA_` covers the bootstrap and host tokens; `DYLD_`/`LD_` the macOS and
/// glibc loaders. A trailing-match list below covers the proxy family, which
/// is spelled both `HTTPS_PROXY` and `https_proxy`.
const DENY_PREFIXES: &[&str] = &["LD_", "DYLD_", "REMUDA_"];

/// Denied suffixes, matched case-insensitively (`HTTP_PROXY`, `no_proxy`, …).
const DENY_SUFFIXES: &[&str] = &["_PROXY", "PROXY"];

/// True when `name` must never be set on an agent child.
///
/// Matching is case-insensitive: the proxy family is conventionally lowercase,
/// and on the platforms we target a differently-cased spelling of a blocked
/// name is a bypass, not a distinct variable.
pub fn is_denied(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    DENY.contains(&upper.as_str())
        || DENY_PREFIXES.iter().any(|prefix| upper.starts_with(prefix))
        || DENY_SUFFIXES.iter().any(|suffix| upper.ends_with(suffix))
}

/// True when `name` is inherited from the Node's own environment.
fn is_inherited(name: &str) -> bool {
    INHERIT.contains(&name) || INHERIT_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// The base environment for an agent child: allowlisted names present in this
/// process, minus anything denied.
///
/// The caller adds the recipe's resolved `env_allowlist` on top — those are
/// the driver-provided provider variables, which are *values the driver
/// computed*, not values it inherited.
pub fn base_env() -> BTreeMap<String, String> {
    inherit_from(std::env::vars())
}

/// [`base_env`] over an explicit iterator; the seam the tests use so they do
/// not have to mutate the real process environment.
pub fn inherit_from<I>(vars: I) -> BTreeMap<String, String>
where
    I: IntoIterator<Item = (String, String)>,
{
    vars.into_iter()
        .filter(|(name, _)| is_inherited(name) && !is_denied(name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn secrets_in_the_node_environment_are_not_inherited() {
        let env = inherit_from(vars(&[
            ("PATH", "/usr/bin"),
            ("HOME", "/home/node"),
            ("REMUDA_BOOTSTRAP_TOKEN", "bootstrap-secret"),
            ("REMUDA_HOST_TOKEN", "host-secret"),
            ("ANTHROPIC_API_KEY", "sk-secret"),
            ("ANTHROPIC_AUTH_TOKEN", "sk-secret-2"),
            ("OPENAI_API_KEY", "sk-secret-3"),
            ("AWS_SECRET_ACCESS_KEY", "aws-secret"),
            ("GITHUB_TOKEN", "gh-secret"),
        ]));
        assert_eq!(env.get("PATH").map(String::as_str), Some("/usr/bin"));
        assert_eq!(env.get("HOME").map(String::as_str), Some("/home/node"));
        for leaked in [
            "REMUDA_BOOTSTRAP_TOKEN",
            "REMUDA_HOST_TOKEN",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "OPENAI_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
        ] {
            assert!(!env.contains_key(leaked), "{leaked} was inherited");
        }
        // No value from a secret survives anywhere in the map.
        let rendered = format!("{env:?}");
        for secret in ["bootstrap-secret", "host-secret", "sk-secret", "aws-secret"] {
            assert!(!rendered.contains(secret), "{secret} leaked: {rendered}");
        }
    }

    #[test]
    fn locale_and_terminal_are_inherited() {
        let env = inherit_from(vars(&[
            ("LANG", "en_US.UTF-8"),
            ("LC_ALL", "C"),
            ("LC_CTYPE", "UTF-8"),
            ("TERM", "xterm-256color"),
            ("TMPDIR", "/tmp"),
            ("SHELL", "/bin/bash"),
            ("USER", "node"),
        ]));
        assert_eq!(env.len(), 7, "{env:?}");
    }

    #[test]
    fn loader_tls_and_proxy_names_are_denied() {
        for name in [
            "LD_PRELOAD",
            "LD_LIBRARY_PATH",
            "DYLD_INSERT_LIBRARIES",
            "NODE_OPTIONS",
            "BASH_ENV",
            "GIT_SSH_COMMAND",
            "SSL_CERT_FILE",
            "NODE_EXTRA_CA_CERTS",
            "HTTPS_PROXY",
            "HTTP_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "REMUDA_BOOTSTRAP_TOKEN",
            "CLAUDE_CODE_SIMPLE",
            "CLAUDE_CODE_SAFE_MODE",
        ] {
            assert!(is_denied(name), "{name} must be denied");
        }
        // Lowercase spellings are the same variable.
        for name in ["https_proxy", "no_proxy", "ld_preload"] {
            assert!(is_denied(name), "{name} must be denied");
        }
        // Names a legitimate launch needs are not swept up.
        for name in [
            "PATH",
            "HOME",
            "TERM",
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_API_KEY",
            "CLAUDE_CONFIG_DIR",
        ] {
            assert!(!is_denied(name), "{name} must not be denied");
        }
    }

    #[test]
    fn denied_names_are_dropped_even_when_otherwise_inheritable() {
        // PATH is inheritable, LD_LIBRARY_PATH is not, and neither list should
        // let the other's decision through.
        let env = inherit_from(vars(&[
            ("PATH", "/usr/bin"),
            ("LD_LIBRARY_PATH", "/evil/lib"),
            ("LD_PRELOAD", "/evil/hook.so"),
            ("HTTPS_PROXY", "http://mitm:8080"),
        ]));
        assert_eq!(env.keys().collect::<Vec<_>>(), vec!["PATH"], "{env:?}");
    }

    #[test]
    fn unlisted_names_are_not_inherited_by_default() {
        // The allowlist is closed: a name nobody thought about stays out.
        let env = inherit_from(vars(&[
            ("SOME_FUTURE_VARIABLE", "value"),
            ("FAKE_CLAUDE_SCRIPT", "/tmp/script"),
        ]));
        assert!(env.is_empty(), "{env:?}");
    }
}
