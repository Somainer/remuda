//! Read-only remote inventory: uname, glibc, remuda / claude / herdr.

use std::time::Duration;

use crate::client::SshClient;
use crate::error::Error;
use crate::target::SshTarget;

const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// Presence of one CLI on the remote `PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinStatus {
    /// `command -v` result.
    pub path: Option<String>,
    /// First line of `--version` / `remuda version`.
    pub version: Option<String>,
}

impl BinStatus {
    /// Whether the binary was found.
    #[must_use]
    pub fn present(&self) -> bool {
        self.path.is_some()
    }
}

/// Result of [`probe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    /// `ssh -G` target.
    pub target: SshTarget,
    /// `uname -a`.
    pub uname: String,
    /// `getconf GNU_LIBC_VERSION` or `ldd --version` line.
    pub glibc: String,
    /// `remuda` on PATH.
    pub remuda: BinStatus,
    /// `claude` on PATH.
    pub claude: BinStatus,
    /// `herdr` on PATH.
    pub herdr: BinStatus,
}

impl ProbeReport {
    /// Key=value lines for the CLI.
    #[must_use]
    pub fn display_text(&self) -> String {
        let mut out = String::new();
        push_kv(&mut out, "alias", &self.target.alias);
        push_kv(&mut out, "hostname", &self.target.hostname);
        push_kv(&mut out, "user", &self.target.user);
        push_kv(&mut out, "port", &self.target.port.to_string());
        push_kv(
            &mut out,
            "proxy_jump",
            self.target.proxy_jump.as_deref().unwrap_or(""),
        );
        push_kv(&mut out, "uname", &self.uname);
        push_kv(&mut out, "glibc", &self.glibc);
        push_bin(&mut out, "remuda", &self.remuda);
        push_bin(&mut out, "claude", &self.claude);
        push_bin(&mut out, "herdr", &self.herdr);
        out
    }
}

/// Run the probe script over SSH (read-only).
pub async fn probe(client: &SshClient, target: SshTarget) -> Result<ProbeReport, Error> {
    let output = client
        .exec(&["sh", "-c", PROBE_SCRIPT], None, PROBE_TIMEOUT)
        .await?
        .ok()?;
    Ok(ProbeReport {
        target,
        uname: kv(&output.stdout, "uname").unwrap_or_default(),
        glibc: kv(&output.stdout, "glibc").unwrap_or_else(|| "unknown".into()),
        remuda: bin_from(&output.stdout, "remuda"),
        claude: bin_from(&output.stdout, "claude"),
        herdr: bin_from(&output.stdout, "herdr"),
    })
}

const PROBE_SCRIPT: &str = r#"
printf 'uname=%s\n' "$(uname -a 2>/dev/null || uname)"
glibc=$(getconf GNU_LIBC_VERSION 2>/dev/null || true)
if [ -z "$glibc" ]; then
  glibc=$(ldd --version 2>/dev/null | awk 'NR==1{print; exit}' || true)
fi
[ -n "$glibc" ] || glibc=unknown
printf 'glibc=%s\n' "$glibc"
probe_bin() {
  name="$1"
  path=$(command -v "$name" 2>/dev/null || true)
  printf '%s.path=%s\n' "$name" "$path"
  ver=
  if [ -n "$path" ]; then
    if [ "$name" = remuda ]; then
      ver=$("$path" version 2>/dev/null | awk 'NR==1{print; exit}' || true)
    else
      ver=$("$path" --version 2>/dev/null | awk 'NR==1{print; exit}' || true)
    fi
  fi
  printf '%s.version=%s\n' "$name" "$ver"
}
probe_bin remuda
probe_bin claude
probe_bin herdr
"#;

fn kv(text: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(&prefix) {
            return Some(rest.to_string());
        }
    }
    None
}

fn bin_from(text: &str, name: &str) -> BinStatus {
    let path = kv(text, &format!("{name}.path")).filter(|s| !s.is_empty());
    let version = kv(text, &format!("{name}.version")).filter(|s| !s.is_empty());
    BinStatus { path, version }
}

fn push_kv(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push('=');
    out.push_str(value);
    out.push('\n');
}

fn push_bin(out: &mut String, name: &str, bin: &BinStatus) {
    push_kv(
        out,
        &format!("{name}.path"),
        bin.path.as_deref().unwrap_or(""),
    );
    push_kv(
        out,
        &format!("{name}.version"),
        bin.version.as_deref().unwrap_or(""),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_probe_script_output() {
        let text = "\
uname=Linux devbox 5.4\n\
glibc=glibc 2.31\n\
remuda.path=\n\
remuda.version=\n\
claude.path=/home/u/.nvm/versions/node/v22.0.0/bin/claude\n\
claude.version=2.1.221 (Claude Code)\n\
herdr.path=/home/u/.local/bin/herdr\n\
herdr.version=herdr 0.8.2\n";
        assert_eq!(kv(text, "uname").unwrap(), "Linux devbox 5.4");
        let claude = bin_from(text, "claude");
        assert_eq!(
            claude.path.as_deref(),
            Some("/home/u/.nvm/versions/node/v22.0.0/bin/claude")
        );
        assert!(bin_from(text, "remuda").path.is_none());
    }
}
