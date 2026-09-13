//! The launch shim (D-028 §4.2, §5.1, risk #2).
//!
//! `<instance dir>/launch/bin/{claude,codex,grok}`: transparent wrappers placed
//! at the front of the PATH of the shell a human is typing into. When they type
//! `claude`, the shim adds the overlay flags and `exec`s the real binary, so a
//! hand-typed agent produces the same structured signal as one Remuda spawned.
//! This is the only mechanism that makes the unification principle physical
//! rather than aspirational.
//!
//! Transparency is the whole design constraint, because a human is going to
//! interact with this and must not be able to tell:
//!
//! - **`exec`, not a subprocess.** The shim replaces itself with the agent, so
//!   there is no extra pid, signals and the controlling terminal go straight
//!   to the agent, and the exit status is the agent's own.
//! - **The user's flags win.** Their arguments are appended after ours, so
//!   `claude --resume X` still resumes X. An explicit `--settings` of their own
//!   suppresses ours entirely rather than being silently overridden — losing
//!   signal is better than ignoring what the user asked for.
//! - **It resolves the real binary by skipping itself.** The shim searches PATH
//!   with its own directory removed, so it cannot recurse into itself, and
//!   `command -v claude` from inside the session still reports a working path.
//! - **It fails open.** If the real binary cannot be found the shim says so on
//!   stderr and exits 127, the same as a missing command would.

use crate::error::DriverResult;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Commands a shim is generated for.
///
/// Only `claude` is wired to an overlay this phase; `codex` and `grok` get a
/// pass-through shim so the PATH shape and `command -v` behaviour are already
/// what P6 needs, without pretending their signals exist yet.
pub const SHIMMED: &[&str] = &["claude", "codex", "grok"];

/// Where the shims landed and what to prepend to PATH.
#[derive(Debug, Clone)]
pub struct ShimSet {
    /// `<instance dir>/launch/bin`.
    pub bin_dir: PathBuf,
    /// Commands that were generated.
    pub commands: Vec<String>,
    /// Shadow `ZDOTDIR` that re-asserts the shim after the user's zsh rc.
    pub zdotdir: PathBuf,
    /// Environment the child needs for the shims to work.
    pub env: BTreeMap<String, String>,
}

impl ShimSet {
    /// `PATH` with the shim directory in front of `inherited`.
    #[must_use]
    pub fn path_with(&self, inherited: &str) -> String {
        let bin = self.bin_dir.to_string_lossy();
        if inherited.is_empty() {
            return bin.into_owned();
        }
        format!("{bin}:{inherited}")
    }
}

/// True when the operator turned the shim off (`REMUDA_SHIM=off`).
///
/// Anything other than an explicit off value leaves it on, so a typo does not
/// silently disable the signal path.
#[must_use]
pub fn shim_disabled(value: Option<&str>) -> bool {
    matches!(
        value.map(|raw| raw.trim().to_ascii_lowercase()).as_deref(),
        Some("off" | "0" | "false" | "no")
    )
}

/// Write the shim set for one instance.
///
/// `overlay` is the settings file the claude shim injects; `credential` is the
/// per-instance hook credential, exported into the agent's environment so the
/// relay can authenticate without it appearing in any command line.
pub fn materialize_shims(
    launch_dir: &Path,
    overlay: &Path,
    credential: &str,
) -> DriverResult<ShimSet> {
    let bin_dir = launch_dir.join("bin");
    std::fs::create_dir_all(&bin_dir)?;
    set_mode(&bin_dir, 0o700)?;
    let mut commands = Vec::new();
    for command in SHIMMED {
        let body = if *command == "claude" {
            claude_shim(command, overlay)
        } else {
            passthrough_shim(command)
        };
        let path = bin_dir.join(command);
        std::fs::write(&path, body)?;
        set_mode(&path, 0o700)?;
        commands.push((*command).to_owned());
    }
    let zdotdir = materialize_zdotdir(launch_dir, &bin_dir)?;
    let mut env = BTreeMap::new();
    env.insert("REMUDA_HOOK_CREDENTIAL".into(), credential.to_owned());
    // The shim's own marker, for diagnostics and for a nested agent to notice
    // it is already inside a shimmed session.
    env.insert("REMUDA_SHIM_DIR".into(), bin_dir.to_string_lossy().into());
    // zsh reads its rc files from ZDOTDIR. Ours source the user's and then put
    // the shim back in front; REMUDA_USER_ZDOTDIR carries where theirs live so
    // nothing they configured is lost. See `materialize_zdotdir`.
    env.insert("ZDOTDIR".into(), zdotdir.to_string_lossy().into());
    Ok(ShimSet {
        bin_dir,
        commands,
        zdotdir,
        env,
    })
}

/// Shell shared by every shim: resolve the real binary, skipping ourselves.
///
/// `command -v` would find the shim again, so PATH is rebuilt with the shim
/// directory removed before the lookup. Written as a POSIX `sh` loop rather
/// than anything bash-specific: the user's login shell is whatever they chose,
/// but `#!/bin/sh` is what runs this.
fn resolver(command: &str) -> String {
    format!(
        r#"# Guard against re-entry before anything else. If resolution ever goes
# wrong — a PATH we cannot parse, a symlinked shim we fail to recognise — this
# turns an infinite exec loop into one plain pass-through. A loop here would
# fork-bomb the machine of whoever typed the command.
if [ -n "${{REMUDA_SHIM_ACTIVE_{upper}:-}}" ]; then
    unset REMUDA_SHIM_ACTIVE_{upper}
    exec {command} "$@"
fi
REMUDA_SHIM_ACTIVE_{upper}=1
export REMUDA_SHIM_ACTIVE_{upper}

# Our own directory, using only shell builtins. `dirname` lives in /usr/bin,
# and a PATH that does not include it would leave us unable to recognise
# ourselves — which is exactly the case where we would then resolve to
# ourselves and recurse.
case "$0" in
    */*) shim_dir=${{0%/*}} ;;
    *) shim_dir=. ;;
esac
if (CDPATH= cd -- "$shim_dir" 2>/dev/null); then
    shim_dir=$(CDPATH= cd -- "$shim_dir" && pwd)
fi

# Resolve the real {command} with our directory removed from the search, so the
# lookup cannot find us again.
real_path=""
saved_ifs=$IFS
IFS=:
for dir in $PATH; do
    IFS=$saved_ifs
    [ -n "$dir" ] || dir=.
    case "$dir" in
        "$shim_dir") IFS=:; continue ;;
    esac
    if [ -x "$dir/{command}" ] && [ ! -d "$dir/{command}" ] && [ "$dir/{command}" != "$0" ]; then
        real_path="$dir/{command}"
        break
    fi
    IFS=:
done
IFS=$saved_ifs
if [ -z "$real_path" ]; then
    echo "remuda: {command} not found on PATH" >&2
    exit 127
fi
unset REMUDA_SHIM_ACTIVE_{upper}
"#,
        upper = command.to_ascii_uppercase(),
    )
}

/// The claude shim: add the overlay unless the user brought their own.
fn claude_shim(command: &str, overlay: &Path) -> String {
    format!(
        r#"#!/bin/sh
# Remuda per-session launch shim (D-028 §4.2). Transparent: it execs the real
# binary, so there is no extra process, signals reach the agent directly, and
# the exit status is the agent's own.
#
# REMUDA_SHIM=off makes this a plain pass-through.
{resolver}
overlay={overlay}

# An explicit --settings from the user wins outright. Overriding it would be
# worse than losing our signal: they asked for that file.
user_settings=0
for arg in "$@"; do
    case "$arg" in
        --settings|--settings=*) user_settings=1 ;;
    esac
done

case "${{REMUDA_SHIM:-on}}" in
    off|0|false|no) exec "$real_path" "$@" ;;
esac

if [ "$user_settings" = 1 ] || [ ! -f "$overlay" ]; then
    exec "$real_path" "$@"
fi

# Our flags first, the user's after, so theirs take precedence on any clash.
exec "$real_path" --settings "$overlay" --setting-sources user,project,local "$@"
"#,
        resolver = resolver(command),
        overlay = single_quote(&overlay.to_string_lossy()),
    )
}

/// Codex / grok: resolve and exec, no overlay yet (P6).
///
/// It exists so PATH shape and `command -v` are already right, and so turning
/// their overlays on later is a change to this file and not to the launch path.
fn passthrough_shim(command: &str) -> String {
    format!(
        r#"#!/bin/sh
# Remuda per-session launch shim (D-028 §4.2), pass-through.
# {command} has no overlay until P6; this only keeps PATH resolution honest.
{resolver}
exec "$real_path" "$@"
"#,
        resolver = resolver(command),
    )
}

/// Files a zsh shadow `ZDOTDIR` needs, in the order zsh reads them.
const ZSH_RC_FILES: &[&str] = &[".zshenv", ".zprofile", ".zshrc", ".zlogin"];

/// The last file zsh reads during startup.
///
/// Only this one restores the user's own `ZDOTDIR`. Restoring it earlier would
/// send zsh to `$HOME` for every remaining startup file, so only the first of
/// ours would ever run — which is exactly the bug the live run found.
const LAST_ZSH_RC: &str = ".zlogin";

/// Generate a shadow `ZDOTDIR` that re-asserts the shim after the user's rc.
///
/// Putting the shim directory on the child's `PATH` is not enough for a login
/// shell. `~/.zprofile` and `~/.zshrc` routinely *prepend* to `PATH` (every
/// version manager does), so by the time the human has a prompt our entry has
/// been pushed down the list and their own `claude` wins. Measured on a real
/// run: the shim landed at position 22 of 35.
///
/// The fix is ordering, not force. Each generated file sources the user's real
/// one first — so their environment is exactly what they configured — and then
/// puts the shim back in front. `ZDOTDIR` is restored to their own value
/// before any of that runs, so a nested shell behaves normally and nothing the
/// user sources can tell the difference.
///
/// zsh only. bash's login sequence has no equivalent single hook (`--rcfile`
/// does not apply to login shells and `BASH_ENV` is denied as a code-execution
/// vector), so a bash login shell keeps the plain `PATH` injection and
/// degrades to whatever position its profile leaves us in.
pub fn materialize_zdotdir(launch_dir: &Path, bin_dir: &Path) -> DriverResult<PathBuf> {
    let dir = launch_dir.join("zdotdir");
    std::fs::create_dir_all(&dir)?;
    set_mode(&dir, 0o700)?;
    for name in ZSH_RC_FILES {
        let body = zsh_rc(name, bin_dir);
        let path = dir.join(name);
        std::fs::write(&path, body)?;
        set_mode(&path, 0o600)?;
    }
    Ok(dir)
}

/// One shadow rc file.
///
/// The last file in the sequence hands `ZDOTDIR` back to the user, so the
/// interactive shell the human ends up in reports their own value rather than
/// ours. Every earlier file keeps it pointed here, because zsh re-reads it
/// before each startup file.
fn zsh_rc(name: &str, bin_dir: &Path) -> String {
    format!(
        r#"# Remuda per-session shell integration (D-028 §4.2). Generated; not the
# user's file. It sources their real {name} and then puts the Remuda shim
# directory back at the front of PATH, because a login profile that prepends
# to PATH would otherwise bury it.
# Source the user's own file with ZDOTDIR temporarily set to theirs, so
# anything it reads sees their configuration and not ours — then put ours back.
# ZDOTDIR must stay pointed here for the whole startup sequence: zsh re-reads
# it before each of .zshenv, .zprofile, .zshrc and .zlogin, and restoring it
# permanently in the first file would mean only that file ever ran.
__remuda_our_zdotdir=$ZDOTDIR
if [ -n "${{REMUDA_USER_ZDOTDIR:-}}" ]; then
    ZDOTDIR=$REMUDA_USER_ZDOTDIR
else
    unset ZDOTDIR
fi
__remuda_user_rc=${{ZDOTDIR:-$HOME}}/{name}
[ -r "$__remuda_user_rc" ] && . "$__remuda_user_rc"
unset __remuda_user_rc
ZDOTDIR=$__remuda_our_zdotdir
export ZDOTDIR
unset __remuda_our_zdotdir

# Re-assert the shim, dropping any earlier copy first so that re-sourcing an
# rc — by hand, or from a nested login shell — cannot grow PATH without bound.
# Done with parameter expansion rather than external commands: this runs before
# the user's PATH is necessarily usable.
__remuda_bin={bin}
__remuda_path=":$PATH:"
while [ "$__remuda_path" != "${{__remuda_path#*:$__remuda_bin:}}" ]; do
    __remuda_path="${{__remuda_path%%:$__remuda_bin:*}}:${{__remuda_path#*:$__remuda_bin:}}"
done
__remuda_path="${{__remuda_path#:}}"
__remuda_path="${{__remuda_path%:}}"
if [ -n "$__remuda_path" ]; then
    PATH="$__remuda_bin:$__remuda_path"
else
    PATH="$__remuda_bin"
fi
export PATH
unset __remuda_bin __remuda_path
{handback}"#,
        name = name,
        bin = single_quote(&bin_dir.to_string_lossy()),
        handback = if name == LAST_ZSH_RC {
            // Startup is over: give the interactive shell the user's own
            // ZDOTDIR so `echo $ZDOTDIR` reports what they configured, and a
            // shell they start by hand reads their files and not ours.
            r#"
# Last file zsh reads. Hand ZDOTDIR back so the interactive shell the human
# ends up in is indistinguishable from the one they would have had.
if [ -n "${REMUDA_USER_ZDOTDIR:-}" ]; then
    ZDOTDIR=$REMUDA_USER_ZDOTDIR
    export ZDOTDIR
else
    unset ZDOTDIR
fi
"#
        } else {
            ""
        },
    )
}

fn single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> DriverResult<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> DriverResult<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_off_disables_the_shim_and_a_typo_does_not() {
        for value in ["off", "OFF", "0", "false", "no", " off "] {
            assert!(shim_disabled(Some(value)), "{value} should disable");
        }
        // A typo must not silently drop the signal path.
        for value in ["on", "1", "true", "", "offf", "yes"] {
            assert!(!shim_disabled(Some(value)), "{value} should stay on");
        }
        assert!(!shim_disabled(None), "unset means on");
    }

    #[test]
    fn the_shim_directory_goes_in_front_of_the_inherited_path() {
        let dir = tempfile::tempdir().unwrap();
        let set = materialize_shims(dir.path(), &dir.path().join("settings.json"), "cred").unwrap();
        let path = set.path_with("/usr/bin:/bin");
        assert!(
            path.starts_with(&set.bin_dir.to_string_lossy().into_owned()),
            "{path}"
        );
        assert!(path.ends_with("/usr/bin:/bin"), "{path}");
    }

    #[test]
    fn every_shim_is_generated_and_executable_by_its_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let set = materialize_shims(dir.path(), &dir.path().join("settings.json"), "cred").unwrap();
        for command in SHIMMED {
            let path = set.bin_dir.join(command);
            assert!(path.is_file(), "{command} shim missing");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o700, "{command} shim must not be group/world");
            }
        }
    }

    #[test]
    fn the_credential_reaches_the_child_through_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        let set =
            materialize_shims(dir.path(), &dir.path().join("settings.json"), "cred-x").unwrap();
        assert_eq!(
            set.env.get("REMUDA_HOOK_CREDENTIAL").map(String::as_str),
            Some("cred-x")
        );
        // And never into a shim body, which any process can read.
        for command in SHIMMED {
            let body = std::fs::read_to_string(set.bin_dir.join(command)).unwrap();
            assert!(!body.contains("cred-x"), "{command} leaked the credential");
        }
    }

    #[test]
    fn the_claude_shim_execs_rather_than_spawning_a_child() {
        // A subprocess would add a pid, break signal delivery and change the
        // foreground process group the promotion poller reads.
        let dir = tempfile::tempdir().unwrap();
        let set = materialize_shims(dir.path(), &dir.path().join("settings.json"), "cred").unwrap();
        let body = std::fs::read_to_string(set.bin_dir.join("claude")).unwrap();
        assert!(body.contains(r#"exec "$real_path""#), "{body}");
        assert!(
            !body.contains("$real_path\" \"$@\" &"),
            "the shim must not background the agent"
        );
    }

    #[test]
    fn the_shim_removes_itself_from_path_before_resolving() {
        // Otherwise it finds itself and recurses until the process limit.
        let dir = tempfile::tempdir().unwrap();
        let set = materialize_shims(dir.path(), &dir.path().join("settings.json"), "cred").unwrap();
        let body = std::fs::read_to_string(set.bin_dir.join("claude")).unwrap();
        assert!(
            body.contains(r#""$shim_dir") IFS=:; continue ;;"#),
            "{body}"
        );
        assert!(
            body.contains(r#"[ "$dir/claude" != "$0" ]"#),
            "a symlinked or relatively-invoked shim must also be skipped: {body}"
        );
    }

    #[test]
    fn the_shim_finds_its_own_directory_without_calling_dirname() {
        // `dirname` lives in /usr/bin. A PATH without it would leave the shim
        // unable to recognise itself — which is exactly the case where it then
        // resolves to itself and exec-loops.
        let dir = tempfile::tempdir().unwrap();
        let set = materialize_shims(dir.path(), &dir.path().join("settings.json"), "cred").unwrap();
        for command in SHIMMED {
            let body = std::fs::read_to_string(set.bin_dir.join(command)).unwrap();
            let invokes_dirname = body
                .lines()
                .filter(|line| !line.trim_start().starts_with('#'))
                .any(|line| line.contains("dirname"));
            assert!(
                !invokes_dirname,
                "{command} depends on an external command to locate itself"
            );
            assert!(body.contains("shim_dir=${0%/*}"), "{command}: {body}");
        }
    }

    #[test]
    fn a_re_entered_shim_gives_up_and_passes_through_instead_of_looping() {
        // The last line of defence: if resolution is ever wrong, one extra
        // pass-through beats a fork bomb on the machine of whoever typed it.
        let dir = tempfile::tempdir().unwrap();
        let set = materialize_shims(dir.path(), &dir.path().join("settings.json"), "cred").unwrap();
        for command in SHIMMED {
            let body = std::fs::read_to_string(set.bin_dir.join(command)).unwrap();
            let guard = format!("REMUDA_SHIM_ACTIVE_{}", command.to_ascii_uppercase());
            assert!(body.contains(&guard), "{command} has no re-entry guard");
            assert!(
                body.contains(&format!("exec {command} \"$@\"")),
                "{command} must pass through on re-entry: {body}"
            );
        }
    }

    #[test]
    fn the_shadow_zdotdir_sources_the_users_rc_before_re_asserting_the_shim() {
        // PATH injection alone is not enough for a login shell: ~/.zprofile and
        // ~/.zshrc routinely prepend to PATH, and on a real run that pushed the
        // shim to position 22 of 35, so the user's own claude won. Ordering is
        // the fix — their file first, our directory back in front after.
        let dir = tempfile::tempdir().unwrap();
        let set = materialize_shims(dir.path(), &dir.path().join("settings.json"), "cred").unwrap();
        for name in ZSH_RC_FILES {
            let body = std::fs::read_to_string(set.zdotdir.join(name)).unwrap();
            let sourced = body
                .find(". \"$__remuda_user_rc\"")
                .expect("sources the user rc");
            let reasserted = body
                .find(r#"PATH="$__remuda_bin:$__remuda_path""#)
                .expect("re-asserts the shim");
            assert!(
                sourced < reasserted,
                "{name} must re-assert the shim *after* the user's rc, or the \
                 profile's own prepends bury it again"
            );
        }
    }

    #[test]
    fn every_startup_file_except_the_last_keeps_zdotdir_pointed_at_us() {
        // The live run's second bug: .zshenv restored the user's ZDOTDIR
        // permanently, so zsh read .zprofile and .zshrc from $HOME and only
        // the first of our files ever ran — the shim was never re-asserted and
        // PATH stayed at position 22.
        let dir = tempfile::tempdir().unwrap();
        let set = materialize_shims(dir.path(), &dir.path().join("settings.json"), "cred").unwrap();
        for name in ZSH_RC_FILES {
            let body = std::fs::read_to_string(set.zdotdir.join(name)).unwrap();
            if *name == LAST_ZSH_RC {
                continue;
            }
            assert!(
                body.contains("ZDOTDIR=$__remuda_our_zdotdir"),
                "{name} must leave ZDOTDIR pointed here, or zsh reads the rest \
                 of its startup files from $HOME: {body}"
            );
        }
    }

    #[test]
    fn the_last_startup_file_hands_zdotdir_back_to_the_user() {
        // So the interactive shell the human ends up in is indistinguishable
        // from the one they would have had.
        let dir = tempfile::tempdir().unwrap();
        let set = materialize_shims(dir.path(), &dir.path().join("settings.json"), "cred").unwrap();
        let body = std::fs::read_to_string(set.zdotdir.join(LAST_ZSH_RC)).unwrap();
        let reassert = body
            .find(r#"PATH="$__remuda_bin:$__remuda_path""#)
            .expect("re-asserts");
        let handback = body.rfind("# Last file zsh reads").expect("hands back");
        assert!(
            reassert < handback,
            "the hand-back must come after the shim is in place: {body}"
        );
    }

    #[test]
    fn the_shadow_zdotdir_restores_the_users_own_zdotdir_first() {
        // Anything the user's rc sources must see their configuration, not
        // ours, and a nested shell must behave normally.
        let dir = tempfile::tempdir().unwrap();
        let set = materialize_shims(dir.path(), &dir.path().join("settings.json"), "cred").unwrap();
        let body = std::fs::read_to_string(set.zdotdir.join(".zshrc")).unwrap();
        let saved = body
            .find("__remuda_our_zdotdir=$ZDOTDIR")
            .expect("saves ours before swapping");
        let restored = body.find("ZDOTDIR=$REMUDA_USER_ZDOTDIR").expect("restores");
        let sourced = body.find(". \"$__remuda_user_rc\"").expect("sources");
        assert!(saved < restored && restored < sourced, "{body}");
        assert!(
            body.contains("unset ZDOTDIR"),
            "a user with no ZDOTDIR of their own must end up with none set"
        );
    }

    #[test]
    fn re_sourcing_an_rc_does_not_grow_path_without_bound() {
        // A user who sources ~/.zshrc by hand, or a nested login shell, must
        // not accumulate copies of the shim directory.
        let dir = tempfile::tempdir().unwrap();
        let set = materialize_shims(dir.path(), &dir.path().join("settings.json"), "cred").unwrap();
        let body = std::fs::read_to_string(set.zdotdir.join(".zshrc")).unwrap();
        assert!(
            body.contains(r#"while [ "$__remuda_path" !="#),
            "an existing entry must be stripped before re-adding it: {body}"
        );
        assert!(
            !body.contains("| grep ") && !body.contains("| paste "),
            "the rc runs before the user's PATH is necessarily usable, so it \
             must not depend on external commands: {body}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_shadow_zdotdir_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let set = materialize_shims(dir.path(), &dir.path().join("settings.json"), "cred").unwrap();
        let mode = std::fs::metadata(&set.zdotdir)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn a_path_with_a_quote_cannot_break_out_of_the_shim_body() {
        let dir = tempfile::tempdir().unwrap();
        let set =
            materialize_shims(dir.path(), Path::new("/tmp/it's/settings.json"), "cred").unwrap();
        let body = std::fs::read_to_string(set.bin_dir.join("claude")).unwrap();
        assert!(
            body.contains(r"overlay='/tmp/it'\''s/settings.json'"),
            "{body}"
        );
    }
}
