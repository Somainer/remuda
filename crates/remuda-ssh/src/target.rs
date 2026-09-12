//! `SshTarget` from `ssh -G` and Host aliases from an OpenSSH config file.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::Error;

/// Resolved OpenSSH destination for one config alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    /// `Host` alias passed to `ssh` (not necessarily the hostname).
    pub alias: String,
    /// Effective `HostName`.
    pub hostname: String,
    /// Effective `User`.
    pub user: String,
    /// Effective `Port`.
    pub port: u16,
    /// Effective `ProxyJump`, if any.
    pub proxy_jump: Option<String>,
    /// Effective `IdentityFile` entries, `~` expanded.
    pub identity_files: Vec<PathBuf>,
}

impl SshTarget {
    /// Run `ssh -G <alias>` and parse HostName/User/Port/ProxyJump/IdentityFile.
    pub fn resolve(alias: &str) -> Result<Self, Error> {
        Self::resolve_with(Path::new("ssh"), alias)
    }

    /// Like [`Self::resolve`] but with an explicit `ssh` binary (tests).
    pub fn resolve_with(ssh_binary: &Path, alias: &str) -> Result<Self, Error> {
        validate_alias(alias)?;
        let output = Command::new(ssh_binary)
            .arg("-G")
            .arg(alias)
            .output()
            .map_err(|err| {
                if err.kind() == std::io::ErrorKind::NotFound {
                    Error::SshNotFound(ssh_binary.to_path_buf())
                } else {
                    Error::from(err)
                }
            })?;
        if !output.status.success() {
            return Err(Error::remote(
                output.status.code(),
                String::from_utf8_lossy(&output.stderr),
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        Self::from_g_output(alias, &stdout)
    }

    /// Parse `ssh -G` stdout. Comment lines (`#`) are ignored so fixtures can
    /// record their source.
    pub fn from_g_output(alias: &str, output: &str) -> Result<Self, Error> {
        let mut hostname = None;
        let mut user = None;
        let mut port = None;
        let mut proxy_jump = None;
        let mut identity_files = Vec::new();

        for raw in output.lines() {
            let line = strip_comment(raw).trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once(|c: char| c.is_whitespace()) else {
                continue;
            };
            let key = key.to_ascii_lowercase();
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            match key.as_str() {
                "hostname" => hostname = Some(value.to_string()),
                "user" => user = Some(value.to_string()),
                "port" => {
                    port = Some(value.parse::<u16>().map_err(|_| {
                        Error::parse(format!("invalid port {value:?} for {alias}"))
                    })?);
                }
                "proxyjump" => {
                    if !value.eq_ignore_ascii_case("none") {
                        proxy_jump = Some(value.to_string());
                    }
                }
                "identityfile" => identity_files.push(expand_tilde(value)),
                _ => {}
            }
        }

        Ok(Self {
            alias: alias.to_string(),
            hostname: hostname
                .ok_or_else(|| Error::parse(format!("ssh -G {alias}: missing hostname")))?,
            user: user.ok_or_else(|| Error::parse(format!("ssh -G {alias}: missing user")))?,
            port: port.unwrap_or(22),
            proxy_jump,
            identity_files,
        })
    }
}

/// Host aliases in the current user's `~/.ssh/config` (wildcards omitted).
pub fn list_user_hosts() -> Result<Vec<String>, Error> {
    let path = user_ssh_config()?;
    list_config_hosts(&path)
}

/// Host aliases in `path`. `Include` files without glob characters are followed.
/// Patterns containing `*`, `?`, or `!` are skipped.
pub fn list_config_hosts(path: &Path) -> Result<Vec<String>, Error> {
    let mut hosts = Vec::new();
    let mut seen = HashSet::new();
    let mut visited = HashSet::new();
    collect_hosts(path, &mut hosts, &mut seen, &mut visited)?;
    Ok(hosts)
}

fn collect_hosts(
    path: &Path,
    hosts: &mut Vec<String>,
    seen: &mut HashSet<String>,
    visited: &mut HashSet<PathBuf>,
) -> Result<(), Error> {
    let canon = path.to_path_buf();
    if !visited.insert(canon) {
        return Ok(());
    }
    let text = fs::read_to_string(path).map_err(|err| Error::Config {
        path: path.to_path_buf(),
        detail: err.to_string(),
    })?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(kw) = parts.next() else {
            continue;
        };
        if kw.eq_ignore_ascii_case("include") {
            for spec in parts {
                let include_path = expand_tilde(spec);
                let resolved = if include_path.is_absolute() {
                    include_path
                } else {
                    base.join(include_path)
                };
                if is_wildcard_path(&resolved) {
                    tracing::debug!(path = %resolved.display(), "skipping glob ssh Include");
                    continue;
                }
                if resolved.is_file() {
                    collect_hosts(&resolved, hosts, seen, visited)?;
                }
            }
            continue;
        }
        if !kw.eq_ignore_ascii_case("host") {
            continue;
        }
        for pat in parts {
            if is_wildcard_host(pat) {
                continue;
            }
            if seen.insert(pat.to_string()) {
                hosts.push(pat.to_string());
            }
        }
    }
    Ok(())
}

/// Reject OpenSSH flag injection (`-oProxyCommand=…`) and empty/whitespace aliases.
pub(crate) fn validate_alias(alias: &str) -> Result<(), Error> {
    let mut chars = alias.chars();
    let Some(first) = chars.next() else {
        return Err(Error::parse("alias must be a single token"));
    };
    if !first.is_ascii_alphanumeric() {
        return Err(Error::parse(
            "alias must start with an alphanumeric character (not a flag)",
        ));
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-') {
        return Err(Error::parse(
            "alias may contain only [A-Za-z0-9._-] (no ssh flags)",
        ));
    }
    Ok(())
}

fn user_ssh_config() -> Result<PathBuf, Error> {
    let home = std::env::var_os("HOME").ok_or_else(|| Error::parse("HOME is not set"))?;
    Ok(PathBuf::from(home).join(".ssh").join("config"))
}

pub(crate) fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    PathBuf::from(path)
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn strip_comment(line: &str) -> &str {
    match line.split_once('#') {
        Some((before, _)) => before,
        None => line,
    }
}

fn is_wildcard_host(pat: &str) -> bool {
    pat.contains('*') || pat.contains('?') || pat.starts_with('!')
}

fn is_wildcard_path(path: &Path) -> bool {
    path.to_string_lossy()
        .chars()
        .any(|c| c == '*' || c == '?' || c == '[')
}

#[cfg(test)]
mod tests {
    use super::{is_wildcard_host, strip_comment};

    #[test]
    fn comment_strip_keeps_value_before_hash() {
        assert_eq!(
            strip_comment("Host forge-doloris # note").trim(),
            "Host forge-doloris"
        );
    }

    #[test]
    fn alias_rejects_ssh_flags() {
        assert!(super::validate_alias("devbox-sg").is_ok());
        assert!(super::validate_alias("-oProxyCommand=touch").is_err());
        assert!(super::validate_alias("--help").is_err());
        assert!(super::validate_alias("host=evil").is_err());
        assert!(super::validate_alias("host name").is_err());
    }

    #[test]
    fn wildcard_hosts_detected() {
        assert!(is_wildcard_host("10.*"));
        assert!(is_wildcard_host("jump-proxy-*"));
        assert!(is_wildcard_host("?"));
        assert!(is_wildcard_host("!negated"));
        assert!(!is_wildcard_host("devbox-sg"));
    }
}
