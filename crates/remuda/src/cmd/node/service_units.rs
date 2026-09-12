//! User service rendering. Credentials belong in private files, never unit arguments.

use anyhow::{Result, ensure};
use std::{collections::BTreeMap, path::Path};

pub(super) const SERVICE_NAME: &str = "remuda-node.service";
pub(super) const LAUNCHD_LABEL: &str = "com.remuda.node";

pub(super) struct UnitConfig<'a> {
    pub executable: &'a Path,
    pub config: &'a Path,
    pub data_dir: &'a Path,
    pub display_label: Option<&'a str>,
    pub no_orphan_sweep: bool,
    pub environment: &'a BTreeMap<String, String>,
}

impl UnitConfig<'_> {
    pub(super) fn arguments(&self) -> Result<Vec<String>> {
        let mut args = vec![
            path_text(self.executable)?.into(),
            "--config".into(),
            path_text(self.config)?.into(),
            "--data-dir".into(),
            path_text(self.data_dir)?.into(),
            "node".into(),
            "daemon".into(),
        ];
        if let Some(label) = self.display_label {
            args.extend(["--display-label".into(), label.into()]);
        }
        if self.no_orphan_sweep {
            args.push("--no-herdr-orphan-sweep".into());
        }
        Ok(args)
    }
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow::anyhow!("service paths must be UTF-8"))
}

pub(super) fn data_dir_marker(path: &Path) -> Result<String> {
    let encoded: String = path_text(path)?
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(format!("remuda-node-data-dir={encoded}"))
}

fn systemd_quote(value: &str, exec: bool) -> Result<String> {
    ensure!(
        !value.chars().any(char::is_control),
        "service values must not contain control characters"
    );
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    Ok(format!(
        "\"{}\"",
        if exec {
            escaped.replace('$', "$$")
        } else {
            escaped
        }
    ))
}

pub(super) fn systemd_unit(unit: &UnitConfig<'_>) -> Result<String> {
    let marker = data_dir_marker(unit.data_dir)?;
    let command = unit
        .arguments()?
        .iter()
        .map(|arg| systemd_quote(arg, true))
        .collect::<Result<Vec<_>>>()?
        .join(" ");
    let mut output = format!(
        "# {marker}\n[Unit]\nDescription=Remuda persistent Node\nAfter=network-online.target\n\n[Service]\nType=simple\nExecStart={command}\nRestart=on-failure\nRestartSec=2\nTimeoutStopSec=30\nUMask=0077\n"
    );
    for (key, value) in unit.environment {
        output.push_str(&format!(
            "Environment={}\n",
            systemd_quote(&format!("{key}={value}"), false)?
        ));
    }
    output.push_str("\n[Install]\nWantedBy=default.target\n");
    Ok(output)
}

fn xml(value: &str) -> Result<String> {
    ensure!(
        !value.chars().any(char::is_control),
        "service values must not contain control characters"
    );
    Ok(value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;"))
}

pub(super) fn launchd_unit(unit: &UnitConfig<'_>) -> Result<String> {
    let marker = data_dir_marker(unit.data_dir)?;
    let args = unit
        .arguments()?
        .iter()
        .map(|arg| Ok(format!("<string>{}</string>", xml(arg)?)))
        .collect::<Result<Vec<_>>>()?
        .join("\n");
    let mut environment = String::new();
    for (key, value) in unit.environment {
        environment.push_str(&format!(
            "<key>{}</key><string>{}</string>\n",
            xml(key)?,
            xml(value)?
        ));
    }
    let log = xml(path_text(&unit.data_dir.join("node/daemon.log"))?)?;
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<!-- {marker} -->\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{LAUNCHD_LABEL}</string>\n<key>ProgramArguments</key><array>\n{args}\n</array>\n<key>EnvironmentVariables</key><dict>{environment}</dict>\n<key>RunAtLoad</key><true/>\n<key>KeepAlive</key><true/>\n<key>ProcessType</key><string>Background</string>\n<key>Umask</key><integer>63</integer>\n<key>StandardOutPath</key><string>{log}</string>\n<key>StandardErrorPath</key><string>{log}</string>\n</dict></plist>\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_escapes_specifiers_environment_and_argument_metacharacters() {
        assert_eq!(
            systemd_quote("/tmp/remuda-$HOME/%n/quote\"/slash\\", true).unwrap(),
            "\"/tmp/remuda-$$HOME/%%n/quote\\\"/slash\\\\\""
        );
        assert!(systemd_quote("x\nExecStart=bad", true).is_err());
        assert!(systemd_quote("x\0bad", false).is_err());
    }

    #[test]
    fn units_preserve_argument_boundaries_and_xml_safe_values() {
        let environment = BTreeMap::from([("PATH".into(), "/opt/bin & tools".into())]);
        let unit = UnitConfig {
            executable: Path::new("/tmp/remuda-tools/my remuda"),
            config: Path::new("/tmp/remuda-data/daemon.toml"),
            data_dir: Path::new("/tmp/remuda-data"),
            display_label: Some("<sg-host> & 'label'"),
            no_orphan_sweep: true,
            environment: &environment,
        };
        let systemd = systemd_unit(&unit).unwrap();
        assert!(systemd.contains("ExecStart=\"/tmp/remuda-tools/my remuda\" \"--config\""));
        assert!(systemd.contains("\"node\" \"daemon\""));
        assert!(!systemd.contains("enroll-token"));
        let plist = launchd_unit(&unit).unwrap();
        assert!(plist.contains("<string>&lt;sg-host&gt; &amp; &apos;label&apos;</string>"));
        assert!(plist.contains("<string>/opt/bin &amp; tools</string>"));
        assert!(plist.contains("<string>daemon</string>"));
        assert!(!plist.contains("enroll-token"));
    }
}
