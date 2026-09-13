//! Mirror CI's generated OpenAPI client check in the isolated merge worktree.

use super::*;

// Keep this pathspec identical to "Generated OpenAPI client is current" in ci.yml.
const GENERATED_PATHS: &[&str] = &["web/src/lib/api.generated.ts"];
const GENERATE_COMMAND: &[&str] = &["pnpm", "--dir", "web", "run", "gen:api"];

pub(super) fn plan(selected: bool) -> Step {
    let mut step = Step::planned("gen-api-current");
    if !selected {
        step.status = "skipped".into();
    }
    step.command = match std::env::var_os("REMUDA_MERGE_GATE_COMMAND") {
        Some(command) if !command.is_empty() => {
            vec![command.to_string_lossy().into_owned(), step.name.clone()]
        }
        _ => GENERATE_COMMAND.iter().map(|arg| (*arg).into()).collect(),
    };
    step.cwd = Some(".".into());
    step
}

pub(super) fn run(report: &mut MergeReport, worktree: &Path, target: &Path) -> Result<()> {
    let mut step = plan(report.web);
    let result = if report.web {
        eprintln!("gate: {}", step.name);
        let started = Instant::now();
        let result = check(worktree, target, &step.command);
        step.status = if result.is_ok() { "ok" } else { "failed" }.into();
        step.duration_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
        step.attempts = 1;
        step.error = result.as_ref().err().map(|error| format!("{error:#}"));
        result
    } else {
        Ok(())
    };
    report.steps.push(step);
    result
}

fn check(worktree: &Path, target: &Path, command: &[String]) -> Result<()> {
    let (program, args) = command.split_first().context("missing gen:api command")?;
    let generation = Command::new(program)
        .args(args)
        .current_dir(worktree)
        .env("CARGO_TARGET_DIR", target)
        .env("CARGO_INCREMENTAL", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::io::stderr()))
        .stderr(Stdio::inherit())
        .status()
        .context("run pnpm --dir web run gen:api")
        .and_then(|status| {
            ensure!(
                status.success(),
                "pnpm --dir web run gen:api failed ({status})"
            );
            Ok(())
        });
    let mut status_args = vec!["status", "--porcelain", "--untracked-files=all", "--"];
    status_args.extend_from_slice(GENERATED_PATHS);
    let freshness = git(worktree, &status_args).and_then(|changes| {
        ensure!(
            changes.is_empty(),
            "generated OpenAPI client is stale; changed generated files:\n{changes}\nRun pnpm --dir web run gen:api and commit the generated files"
        );
        Ok(())
    });
    // Generation is observational: restore even after a failed or partial command.
    // Other tracked changes stay visible to the subsequent verify-tree step.
    let mut restore_args = vec!["checkout", "--"];
    restore_args.extend_from_slice(GENERATED_PATHS);
    let restore = git(worktree, &restore_args)
        .context("restore generated OpenAPI client")
        .map(|_| ());
    let errors: Vec<_> = [generation, freshness, restore]
        .into_iter()
        .filter_map(|result| result.err().map(|error| format!("{error:#}")))
        .collect();
    ensure!(errors.is_empty(), "{}", errors.join("; "));
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn generated_client_is_restored_after_success_staleness_and_command_failure() {
        let temporary = tempfile::tempdir().unwrap();
        let repo = temporary.path();
        git(repo, &["init", "-b", "main"]).unwrap();
        git(repo, &["config", "user.email", "test@example.com"]).unwrap();
        git(repo, &["config", "user.name", "merge test"]).unwrap();
        git(repo, &["config", "commit.gpgsign", "false"]).unwrap();
        git(repo, &["config", "core.hooksPath", "/dev/null"]).unwrap();
        fs::create_dir_all(repo.join("web/src/lib")).unwrap();
        fs::write(repo.join(GENERATED_PATHS[0]), "committed\n").unwrap();
        fs::write(repo.join("other.txt"), "committed\n").unwrap();
        git(repo, &["add", "--", GENERATED_PATHS[0], "other.txt"]).unwrap();
        git(
            repo,
            &[
                "commit",
                "-m",
                "fixture",
                "--",
                GENERATED_PATHS[0],
                "other.txt",
            ],
        )
        .unwrap();
        for (generated, exit_code) in [("committed", 0), ("stale", 0), ("partial", 7)] {
            let script = format!(
                "printf '%s\\n' {generated} > web/src/lib/api.generated.ts; exit {exit_code}"
            );
            let command = vec!["sh".into(), "-c".into(), script];
            let result = check(repo, &repo.join("target"), &command);
            if generated == "committed" {
                result.unwrap();
            } else {
                let error = format!("{:#}", result.unwrap_err());
                assert!(error.contains("stale"), "{error}");
                assert!(error.contains(GENERATED_PATHS[0]), "{error}");
                if exit_code != 0 {
                    assert!(error.contains("gen:api failed"), "{error}");
                }
            }
            assert_eq!(
                fs::read_to_string(repo.join(GENERATED_PATHS[0])).unwrap(),
                "committed\n"
            );
            assert!(git(repo, &["status", "--porcelain"]).unwrap().is_empty());
        }
        fs::write(repo.join("other.txt"), "unrelated mutation\n").unwrap();
        check(repo, &repo.join("target"), &["true".into()]).unwrap();
        assert!(git(repo, &["diff", "--exit-code", "HEAD", "--"]).is_err());
        let error = check(repo, &repo.join("target"), &["./missing-generator".into()]).unwrap_err();
        assert!(error.to_string().contains("run pnpm --dir web run gen:api"));
    }
}
