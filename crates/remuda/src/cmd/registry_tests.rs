use crate::cmd::{self, Command};
use crate::{Cli, build_info, config};
use clap::CommandFactory;
use clap::Parser;

#[test]
fn registered_commands_have_unique_names_and_feature_owned_help() {
    let mut names = std::collections::BTreeSet::new();
    for command in Cli::command().get_subcommands() {
        assert!(names.insert(command.get_name().to_owned()));
        assert!(
            command.get_about().is_some(),
            "{} needs help",
            command.get_name()
        );
    }
    let cli = Cli::try_parse_from(["remuda", "version"]).unwrap();
    assert!(!cli.command.tracing());
    let cli = Cli::try_parse_from(["remuda", "hub"]).unwrap();
    assert!(cli.command.tracing());
}

#[test]
fn doctor_agents_and_worktree_modes_are_registered() {
    assert!(matches!(
        Cli::try_parse_from(["remuda", "doctor", "--local", "--json"])
            .unwrap()
            .command,
        Command::Doctor(_)
    ));
    assert!(Cli::try_parse_from(["remuda", "doctor", "--local", "--host", "hst_1"]).is_err());
    assert!(matches!(
        Cli::try_parse_from(["remuda", "agents", "--watch", "--json"])
            .unwrap()
            .command,
        Command::Agents(_)
    ));
    assert!(Cli::try_parse_from(["remuda", "instance", "ls", "--watch"]).is_ok());
    for args in [
        vec!["remuda", "worktree", "ls"],
        vec!["remuda", "worktree", "prune"],
        vec!["remuda", "worktree", "rm", "reviewer"],
        vec!["remuda", "merge", "--list"],
    ] {
        assert!(Cli::try_parse_from(args).is_ok());
    }
}

#[test]
fn version_json_uses_the_cli_dispatch_and_contains_build_identity() {
    let cli = Cli::try_parse_from([
        "remuda",
        "--config",
        "/missing/remuda.toml",
        "version",
        "--json",
    ])
    .expect("version CLI");
    let Command::Version(args) = cli.command else {
        panic!("version command")
    };
    let mut bytes = Vec::new();
    build_info::write(args.json, &mut bytes).expect("version output");
    assert_eq!(bytes.last(), Some(&b'\n'));
    assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("one JSON object");
    assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(value["git_sha"], env!("REMUDA_GIT_SHA"));
    assert_eq!(value["build_date"], env!("REMUDA_BUILD_DATE"));
    assert_eq!(value["target"], env!("REMUDA_TARGET"));
}

#[test]
fn declares_modes_and_accepts_global_flags_after_subcommands() {
    Cli::command().debug_assert();
    let cli = Cli::try_parse_from([
        "remuda",
        "hub",
        "--config",
        "test.toml",
        "--data-dir",
        "cli-data",
        "--listen",
        "127.0.0.1:1234",
    ])
    .expect("hub flags");
    assert_eq!(
        cli.context.config.as_deref(),
        Some(std::path::Path::new("test.toml"))
    );
    assert_eq!(
        cli.context.data_dir.as_deref(),
        Some(std::path::Path::new("cli-data"))
    );
    let Command::Hub(args) = cli.command else {
        panic!("hub command")
    };
    let mut config = config::Config::default();
    config.hub.listen.set_port(4567);
    args.apply(&mut config);
    assert_eq!(config.hub.listen.port(), 1234);
    assert!(
        Cli::try_parse_from([
            "remuda",
            "node",
            "--stdio",
            "--hub-url",
            "wss://host/v1/node"
        ])
        .is_err()
    );
    let names: Vec<_> = Cli::command()
        .get_subcommands()
        .map(|c| c.get_name().to_string())
        .collect();
    assert!(names.contains(&"instance".to_string()));
    assert!(names.contains(&"fleet".to_string()));
    assert!(names.contains(&"worktree".to_string()));
    assert!(names.contains(&"mcp".to_string()));
}

#[test]
fn merge_requires_gate_or_dry_run_and_accepts_gate_options() {
    assert!(Cli::try_parse_from(["remuda", "merge", "topic"]).is_err());
    assert!(Cli::try_parse_from(["remuda", "merge", "--gate"]).is_err());
    for mode in ["--gate", "--dry-run"] {
        let cli = Cli::try_parse_from([
            "remuda",
            "merge",
            "topic",
            mode,
            "--web",
            "--no-push",
            "--json",
            "--repo",
            ".",
            "--target-dir",
            "target-coordinator",
        ])
        .expect("merge flags");
        let Command::Merge(args) = cli.command else {
            panic!("merge")
        };
        assert_eq!(args.branch.as_deref(), Some("topic"));
        assert_eq!(args.gate, mode == "--gate");
        assert_eq!(args.dry_run, mode == "--dry-run");
        assert!(args.web && args.no_push && args.json);
        assert_eq!(
            args.target_dir.as_deref(),
            Some(std::path::Path::new("target-coordinator"))
        );
    }
}

#[test]
fn instance_create_declares_host_and_labels() {
    let cli = Cli::try_parse_from([
        "remuda", "instance", "create", "--host", "hst_1", "--prompt", "hi",
    ])
    .expect("instance create");
    let Command::Instance(args) = cli.command else {
        panic!("instance command")
    };
    let cmd::instance::InstanceCommand::Create { host, labels, .. } = args.command else {
        panic!("create")
    };
    assert_eq!(host.as_deref(), Some("hst_1"));
    assert!(labels.is_empty());
    let instance = Cli::command()
        .find_subcommand("instance")
        .expect("instance")
        .clone();
    let create = instance.find_subcommand("create").expect("create");
    let names: Vec<_> = create
        .get_arguments()
        .map(|a| a.get_id().as_str().to_string())
        .collect();
    assert!(names.iter().any(|n| n == "host"));
    assert!(names.iter().any(|n| n == "labels"));
    assert!(names.iter().any(|n| n == "worktree"));
    assert!(names.iter().any(|n| n == "name"));
    assert!(names.iter().any(|n| n == "cwd"));
}

#[test]
fn instance_wait_declares_until_and_timeout() {
    let instance = Cli::command()
        .find_subcommand("instance")
        .expect("instance")
        .clone();
    let wait = instance.find_subcommand("wait").expect("wait");
    let names: Vec<_> = wait
        .get_arguments()
        .map(|a| a.get_id().as_str().to_string())
        .collect();
    assert!(names.iter().any(|n| n == "until"));
    assert!(names.iter().any(|n| n == "timeout"));
    assert!(instance.find_subcommand("list").is_some());
    assert!(instance.find_subcommand("keys").is_some());
    assert!(instance.find_subcommand("rm").is_some());
}

#[test]
fn fleet_run_declares_hosts_and_labels() {
    let fleet = Cli::command()
        .find_subcommand("fleet")
        .expect("fleet")
        .clone();
    let run = fleet.find_subcommand("run").expect("run");
    let names: Vec<_> = run
        .get_arguments()
        .map(|a| a.get_id().as_str().to_string())
        .collect();
    assert!(names.iter().any(|n| n == "hosts"));
    assert!(names.iter().any(|n| n == "labels"));
    assert!(fleet.find_subcommand("send").is_some());
}
