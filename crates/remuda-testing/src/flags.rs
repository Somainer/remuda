//! Subset of `claude` flags the fake understands; unknown flags are skipped.

use std::path::PathBuf;

/// Parsed `claude -p` flags relevant to stream-json hosts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaudeFlags {
    /// `--session-id`.
    pub session_id: Option<String>,
    /// `--model`.
    pub model: Option<String>,
    /// `--permission-mode` (default `default`).
    pub permission_mode: String,
    /// `--replay-user-messages`.
    pub replay_user_messages: bool,
    /// `--print` / `-p` was present.
    pub print: bool,
    /// Working directory at spawn (not a CLI flag; `cd` before exec).
    pub cwd: PathBuf,
}

impl Default for ClaudeFlags {
    fn default() -> Self {
        Self {
            session_id: None,
            model: None,
            permission_mode: "default".to_string(),
            replay_user_messages: false,
            print: false,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }
}

impl ClaudeFlags {
    /// Parse argv after the binary name. Unknown flags are ignored.
    pub fn parse<I, S>(args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut flags = Self::default();
        let args: Vec<String> = args.into_iter().map(|s| s.as_ref().to_string()).collect();
        let mut i = 0;
        while i < args.len() {
            let arg = args[i].as_str();
            if arg == "--" {
                break;
            }
            if arg == "-p" || arg == "--print" {
                flags.print = true;
                i += 1;
                continue;
            }
            if arg == "--replay-user-messages" {
                flags.replay_user_messages = true;
                i += 1;
                continue;
            }
            if let Some(value) = take_value(arg, "--session-id", &args, &mut i) {
                flags.session_id = Some(value);
                continue;
            }
            if let Some(value) = take_value(arg, "--model", &args, &mut i) {
                flags.model = Some(value);
                continue;
            }
            if let Some(value) = take_value(arg, "--permission-mode", &args, &mut i) {
                flags.permission_mode = if value == "manual" {
                    "default".to_string()
                } else {
                    value
                };
                continue;
            }
            if arg.starts_with('-') {
                skip_unknown(arg, &args, &mut i);
                continue;
            }
            i += 1;
        }
        flags
    }
}

fn take_value(arg: &str, name: &str, args: &[String], i: &mut usize) -> Option<String> {
    let prefix = format!("{name}=");
    if arg == name {
        if *i + 1 < args.len() && !args[*i + 1].starts_with('-') {
            let value = args[*i + 1].clone();
            *i += 2;
            return Some(value);
        }
        *i += 1;
        return None;
    }
    if let Some(rest) = arg.strip_prefix(&prefix) {
        *i += 1;
        return Some(rest.to_string());
    }
    None
}

fn skip_unknown(arg: &str, args: &[String], i: &mut usize) {
    if arg.contains('=') {
        *i += 1;
        return;
    }
    if *i + 1 < args.len() && !args[*i + 1].starts_with('-') {
        *i += 2;
        return;
    }
    *i += 1;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_flags_and_skips_unknown() {
        let flags = ClaudeFlags::parse([
            "-p",
            "--output-format",
            "stream-json",
            "--input-format=stream-json",
            "--verbose",
            "--session-id",
            "00000000-0000-4000-8000-000000000001",
            "--model=haiku",
            "--permission-mode",
            "auto",
            "--replay-user-messages",
            "--permission-prompt-tool",
            "stdio",
            "positional-prompt-ignored",
        ]);
        assert!(flags.print);
        assert_eq!(
            flags.session_id.as_deref(),
            Some("00000000-0000-4000-8000-000000000001")
        );
        assert_eq!(flags.model.as_deref(), Some("haiku"));
        assert_eq!(flags.permission_mode, "auto");
        assert!(flags.replay_user_messages);
    }
}
