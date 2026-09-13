# `remuda-testing` screen fixtures

Stripped PTY viewports used by `fake-herdr` scripts and by the screen
detectors in `remuda-driver`. No personal paths, credentials, or captured
account data.

| Path | Source |
|---|---|
| `pty-approval.txt` | Authored y/N approval line (`herdr-herdrx.md` §3.2 shape). |
| `pty-question.txt` | Authored numbered menu (`herdr-herdrx.md` §3.2 shape). |
| `claude-onboarding-theme.txt` | Claude Code 2.1.270 first-run theme picker. Reconstructed from the shipped binary's `Onboarding`/`ThemePicker` components — the intro line `Let's get started.`, the heading `Choose the text style that looks best with your terminal`, the `/theme` help text, and all seven built-in options in their source order. |
| `claude-onboarding-security.txt` | 2.1.270 `security` onboarding step: the `Security notes:` heading, both list items, the security docs link, and the `Press Enter to continue…` footer. |
| `claude-onboarding-terminal-setup.txt` | 2.1.270 `terminal-setup` step (`Use Claude Code's terminal setup?`), non-Apple-Terminal wording (`Shift+Enter for newlines`). |
| `claude-onboarding-login.txt` | 2.1.270 `Select login method:` step with the subscription/Console choices. |
| `claude-bypass-disclaimer.txt` | 2.1.270 `BypassPermissionsModeDialog` (`WARNING: Claude Code running in Bypass Permissions mode`), cancel-first option order. |
| `claude-prompt-composer.txt` | An ordinary ready composer, so the detectors can be shown not to fire on it. |
