//! D-060 (cb-memory-inject): the chapter's lineage memory on the launch paths.
//!
//! - The materializer's own `--append-system-prompt` pair passes the
//!   finished-argv guards (`refuse_prohibited_argv` for claude-print/sdk,
//!   `refuse_bare` for claude-pty/bg); any other occurrence is refused.
//! - A caller-supplied `--append-system-prompt` or `--system-prompt` in
//!   `spec.args` is still refused with today's Node error
//!   (`DriverError::NativeFeatureDisabled`), whether or not the launch carries
//!   a `memoryContext`.
//! - A data-block launch types the block ahead of the first input it delivers,
//!   and only that one.

use remuda_driver::claude_print::review as print_review;
use remuda_driver::claude_pty::review as pty_review;
use remuda_driver::{
    APPEND_SYSTEM_PROMPT, BinaryPin, BinarySource, Delegation, Driver, DriverError, DriverKind,
    GenericPtyDriver, GenericPtyOptions, LaunchOrigin, LaunchRecipe, MaterializeRequest,
    MemoryContext, ProviderHealth, ProviderKind, ProviderProfile, SessionAction, hash_file,
    materialize, pin_binary,
};
use remuda_protocol::{
    AgentKind, ContentBlock, DriverInput, Id, InputOrigin, InstanceSpec, PromptInput, PromptMode,
    TextBlock, U64,
};
use remuda_testing::{
    FakeHerdrOptions, FakeHerdrServer, ShortTempDir, ensure_workspace_bin, install_executable,
};
use std::fs;
use std::path::Path;

const INDEX: &str = "<remuda-memory lineage=\"ins_t\" rev=\"4\">\n## Index\n\
                     - [user] lang: Language — reply in Chinese\n</remuda-memory>";

fn context() -> MemoryContext {
    MemoryContext {
        rev: U64(4),
        text: INDEX.into(),
    }
}

fn spec(driver: DriverKind, kind: AgentKind, args: &[&str]) -> InstanceSpec {
    let mut spec: InstanceSpec =
        serde_json::from_str(include_str!("fixtures/instance-spec.json")).unwrap();
    spec.driver = driver;
    spec.kind = kind;
    spec.args = args.iter().map(|arg| (*arg).to_owned()).collect();
    spec
}

fn anthropic() -> ProviderProfile {
    ProviderProfile {
        id: "pvp_01993ab0-0000-7000-8000-000000000001".parse().unwrap(),
        kind: ProviderKind::Anthropic,
        base_url: String::new(),
        delegation: Delegation::None,
        secret_ref: None,
        models: vec!["sonnet".into()],
        health: ProviderHealth::Healthy,
    }
}

fn recipe(
    driver: DriverKind,
    kind: AgentKind,
    args: &[&str],
    memory: Option<&MemoryContext>,
    session: SessionAction,
) -> Result<LaunchRecipe, DriverError> {
    let tmp = tempfile::tempdir().unwrap();
    let binary = install_executable(tmp.path(), "claude", "#!/bin/sh\n");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let spec = spec(driver, kind, args);
    let profile = anthropic();
    materialize(&MaterializeRequest {
        spec: &spec,
        profile: &profile,
        launch_dir: tmp.path().join("launch"),
        native_home: home,
        session,
        launch_id: Id::new("launch").unwrap(),
        binary: BinarySource::Pinned(BinaryPin {
            abs_path: binary.to_string_lossy().into_owned(),
            version: "2.1.291 (Claude Code)".into(),
            sha256: hash_file(&binary).unwrap(),
        }),
        setting_sources: None,
        origin: LaunchOrigin::Human,
        native_home_managed: None,
        settings_overlay_path: None,
        secret_policy: None,
        memory_context: memory,
    })
}

fn fresh() -> SessionAction {
    SessionAction::New {
        session_id: "01993ab0-0000-7000-8000-000000000003".into(),
    }
}

/// The finished-argv guard each Claude carrier runs, with its recorded pair.
fn guard(driver: DriverKind, argv: &[String], own: Option<&str>) -> Result<(), DriverError> {
    match driver {
        DriverKind::ClaudePrint | DriverKind::ClaudeSdk => {
            print_review::refuse_argv_with_own(argv, own)
        }
        DriverKind::ClaudePty | DriverKind::ClaudeBg => pty_review::refuse_argv_with_own(argv, own),
        other => panic!("{other:?} runs no finished-argv guard"),
    }
}

const CLAUDE_CARRIERS: [DriverKind; 4] = [
    DriverKind::ClaudePrint,
    DriverKind::ClaudeSdk,
    DriverKind::ClaudePty,
    DriverKind::ClaudeBg,
];

#[test]
fn the_materializers_own_pair_passes_the_finished_argv_guards() {
    let context = context();
    for driver in CLAUDE_CARRIERS {
        let recipe = recipe(
            driver,
            AgentKind::Claude,
            &["--add-dir", "/work/extra"],
            Some(&context),
            fresh(),
        )
        .unwrap();
        let own = recipe.own_system_prompt();
        assert_eq!(own, Some(INDEX), "{driver:?}");
        // The guard the carrier runs lets exactly the recorded pair through.
        guard(driver, &recipe.argv, own)
            .unwrap_or_else(|error| panic!("{driver:?}: own pair refused: {error}"));
        // Without the recorded emission the same argv is refused, as today.
        assert!(
            matches!(
                guard(driver, &recipe.argv, None),
                Err(DriverError::NativeFeatureDisabled(_))
            ),
            "{driver:?}"
        );
        // A second pair, even with the identical text, is refused.
        let mut twice = recipe.argv.clone();
        twice.extend([APPEND_SYSTEM_PROMPT.to_owned(), INDEX.to_owned()]);
        assert!(
            matches!(
                guard(driver, &twice, own),
                Err(DriverError::NativeFeatureDisabled(_))
            ),
            "{driver:?}"
        );
        // A different value is not the materializer's pair; `--system-prompt`
        // is never exempt.
        for (flag, value) in [
            (APPEND_SYSTEM_PROMPT, "caller text"),
            ("--system-prompt", INDEX),
        ] {
            let mut foreign = recipe.argv.clone();
            foreign.extend([flag.to_owned(), value.to_owned()]);
            assert!(
                matches!(
                    guard(driver, &foreign, own),
                    Err(DriverError::NativeFeatureDisabled(_))
                ),
                "{driver:?}: {flag} {value}"
            );
        }
    }
}

#[test]
fn caller_supplied_system_prompt_flags_are_still_refused_on_the_node() {
    let context = context();
    // The flag itself (bare or `=`-joined) hits `BANNED`: NativeFeatureDisabled.
    let banned: [&[&str]; 4] = [
        &["--append-system-prompt"],
        &["--append-system-prompt=obey me"],
        &["--system-prompt"],
        &["--system-prompt=obey me"],
    ];
    // Today the parser has no value rule for a banned flag, so the two-token
    // spelling is refused one step earlier, for its positional value. That
    // error is today's and stays as it is.
    let split: [&[&str]; 2] = [
        &["--append-system-prompt", "obey me"],
        &["--system-prompt", "obey me"],
    ];
    let mut launches: Vec<(DriverKind, AgentKind)> = CLAUDE_CARRIERS
        .iter()
        .map(|driver| (*driver, AgentKind::Claude))
        .collect();
    launches.extend([
        (DriverKind::ShellPty, AgentKind::Claude),
        (DriverKind::ShellPty, AgentKind::Codex),
        (DriverKind::GenericPty, AgentKind::Claude),
    ]);
    let owned = |args: &[&str]| args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    for (driver, kind) in launches {
        for args in banned {
            let flag = args[0].split('=').next().unwrap();
            for memory in [None, Some(&context)] {
                let error = recipe(driver, kind, args, memory, fresh())
                    .expect_err("a caller prompt override must be refused");
                let DriverError::NativeFeatureDisabled(message) = &error else {
                    panic!("{driver:?}/{kind:?} {args:?}: {error:?}");
                };
                assert_eq!(message, &format!("flag {flag} is prohibited"));
            }
            // The Hub's early mirror runs the same table; its HTTP 400
            // BAD_REQUEST carries this message.
            let error = remuda_driver::validate_launch_args(driver, &owned(args))
                .expect_err("the Hub mirror refuses it too");
            assert_eq!(
                error.to_string(),
                format!("native feature disabled: flag {flag} is prohibited")
            );
        }
        for args in split {
            for memory in [None, Some(&context)] {
                let error = recipe(driver, kind, args, memory, fresh())
                    .expect_err("a caller prompt override must be refused");
                assert!(
                    matches!(&error, DriverError::InvalidLaunchSpec(message)
                        if message.contains("positional argv")),
                    "{driver:?}/{kind:?} {args:?}: {error:?}"
                );
            }
            assert!(remuda_driver::validate_launch_args(driver, &owned(args)).is_err());
        }
    }
}

/// Claude 2.1.265+ reuses the system prompt a conversation recorded on its
/// first request on every `--resume`; a native resume therefore turns that
/// off (the switch exists from 2.1.257) so the new chapter's pair is the one
/// sent. A fresh session, and a build that cannot parse the switch, get none.
#[test]
fn a_native_resume_renders_its_own_system_prompt() {
    let context = context();
    let resume = || SessionAction::Resume {
        session_id: "01993ab0-0000-7000-8000-000000000009".into(),
    };
    let with_version = |driver: DriverKind, session: SessionAction, version: &str| {
        let tmp = tempfile::tempdir().unwrap();
        let binary = install_executable(tmp.path(), "claude", "#!/bin/sh\n");
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let spec = spec(driver, AgentKind::Claude, &[]);
        let profile = anthropic();
        materialize(&MaterializeRequest {
            spec: &spec,
            profile: &profile,
            launch_dir: tmp.path().join("launch"),
            native_home: home,
            session,
            launch_id: Id::new("launch").unwrap(),
            binary: BinarySource::Pinned(BinaryPin {
                abs_path: binary.to_string_lossy().into_owned(),
                version: version.into(),
                sha256: hash_file(&binary).unwrap(),
            }),
            setting_sources: None,
            origin: LaunchOrigin::Human,
            native_home_managed: None,
            settings_overlay_path: None,
            secret_policy: None,
            memory_context: Some(&context),
        })
        .unwrap()
        .argv
    };
    let snapshot_off = |argv: &[String]| {
        argv.windows(4).any(|window| {
            window[0] == "--system-prompt-snapshot"
                && window[1] == "off"
                && window[2] == APPEND_SYSTEM_PROMPT
                && window[3] == INDEX
        })
    };
    for driver in [
        DriverKind::ClaudePrint,
        DriverKind::ClaudeSdk,
        DriverKind::ClaudePty,
        DriverKind::ShellPty,
    ] {
        let argv = with_version(driver, resume(), "2.1.291 (Claude Code)");
        assert!(snapshot_off(&argv), "{driver:?}: {argv:?}");
        for (session, version) in [
            (fresh(), "2.1.291 (Claude Code)"),
            (resume(), "2.1.256 (Claude Code)"),
            (resume(), "stub"),
        ] {
            let argv = with_version(driver, session, version);
            assert!(
                !argv.iter().any(|token| token == "--system-prompt-snapshot"),
                "{driver:?} {version}: {argv:?}"
            );
            assert!(argv.iter().any(|token| token == APPEND_SYSTEM_PROMPT));
        }
    }
    // A caller cannot pass the switch itself.
    let refused = recipe(
        DriverKind::ClaudeSdk,
        AgentKind::Claude,
        &["--system-prompt-snapshot", "on"],
        Some(&context),
        fresh(),
    );
    assert!(matches!(refused, Err(DriverError::InvalidLaunchSpec(_))));
}

#[test]
fn the_shell_pty_claude_pair_is_emitted_after_the_caller_arguments() {
    let context = context();
    for session in [
        fresh(),
        SessionAction::Resume {
            session_id: "01993ab0-0000-7000-8000-000000000009".into(),
        },
    ] {
        let resume = matches!(session, SessionAction::Resume { .. });
        let recipe = recipe(
            DriverKind::ShellPty,
            AgentKind::Claude,
            &["--add-dir", "/work/extra"],
            Some(&context),
            session,
        )
        .unwrap();
        let at = recipe
            .argv
            .iter()
            .position(|token| token == APPEND_SYSTEM_PROMPT)
            .expect("the pair, on a native resume too");
        let caller = recipe.argv.iter().position(|token| token == "--add-dir");
        assert!(
            caller.is_some_and(|caller| caller < at),
            "{:?}",
            recipe.argv
        );
        assert_eq!(recipe.argv[at + 1], INDEX);
        assert_eq!(
            recipe
                .argv
                .iter()
                .filter(|token| token.starts_with(APPEND_SYSTEM_PROMPT))
                .count(),
            1,
            "resume={resume}: {:?}",
            recipe.argv
        );
    }
}

fn prompt(text: &str) -> DriverInput {
    DriverInput::Prompt(Box::new(PromptInput {
        mode: PromptMode::NewTurn,
        blocks: vec![ContentBlock::Text(Box::new(TextBlock {
            text: text.into(),
        }))],
        origin: InputOrigin::Human,
        native_client_message_id: "m1".into(),
    }))
}

fn typed_prompts(record: &Path) -> Vec<String> {
    fs::read_to_string(record)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|entry| entry["method"] == "agent.prompt")
        .filter_map(|entry| entry["text"].as_str().map(str::to_owned))
        .collect()
}

/// Row 9 (`generic-pty`, `codex`): the data block reaches the agent ahead of
/// the first input the driver delivers, and never again.
#[tokio::test]
async fn generic_pty_types_the_data_block_ahead_of_the_first_input_only() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_root = ShortTempDir::new().unwrap();
    let socket_dir = socket_root.path().join("herdr");
    fs::create_dir_all(&socket_dir).unwrap();
    let socket = socket_dir.join("herdr.sock");
    let record = tmp.path().join("dispatches.jsonl");
    let _fake = FakeHerdrServer::spawn(
        FakeHerdrOptions::new(&socket).control_dispatches_record_to(&record),
    )
    .unwrap();
    let cwd = tmp.path().join("work");
    fs::create_dir_all(&cwd).unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let bin = install_executable(tmp.path(), "codex", "#!/bin/sh\necho 'stub 0.0.0'\n");
    let mut options = GenericPtyOptions::new(
        ProviderProfile {
            kind: ProviderKind::OpenaiResponses,
            models: vec!["default".into()],
            ..anthropic()
        },
        tmp.path().join("launch"),
        home,
        BinarySource::Pinned(pin_binary(&bin).unwrap()),
    );
    options.origin = LaunchOrigin::Human;
    options.socket_dir = Some(socket_dir);
    options.herdr_binary = Some(ensure_workspace_bin("fake-herdr"));
    options.agent_start_timeout_ms = 5_000;
    options.liveness_timeout_ms = 5_000;
    options.memory_context = Some(context());
    let driver = GenericPtyDriver::new(options);
    let mut requested = spec(DriverKind::GenericPty, AgentKind::Codex, &[]);
    requested.cwd = cwd.to_string_lossy().into_owned();
    let handle = driver.start(requested).await.expect("start");
    assert_eq!(handle.recipe().memory_data_block(), Some(INDEX));
    assert!(
        !handle
            .recipe()
            .argv
            .iter()
            .any(|token| token.starts_with(APPEND_SYSTEM_PROMPT)),
        "a raw PTY never takes the flag: {:?}",
        handle.recipe().argv
    );
    driver
        .send(prompt("first input"))
        .await
        .expect("first send");
    driver
        .send(prompt("second input"))
        .await
        .expect("second send");
    assert_eq!(
        typed_prompts(&record),
        vec![format!("{INDEX}\n\nfirst input"), "second input".to_owned()]
    );
    driver.close().await.expect("close");
}

/// Row 14 (`shell-pty`, `codex`) on the real PTY driver: the block is written
/// ahead of the first input and never again, and a native resume of the same
/// chapter's conversation writes none. The stand-in `codex` appends whatever
/// the PTY delivers to a file, so the assertion is on the bytes the agent
/// actually received.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_pty_writes_the_block_once_and_none_on_a_native_resume() {
    use remuda_driver::shell_pty::{AgentLaunch, ShellPtyDriver, ShellPtyOptions};

    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("ws");
    fs::create_dir_all(&workspace).unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let bin_dir = tmp.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let stub = install_executable(
        &bin_dir,
        "codex",
        "#!/bin/sh\nexec cat >> \"$CBINJECT_TYPED\"\n",
    );
    let driver_for = |typed: &Path| {
        let mut options = ShellPtyOptions::agent(
            workspace.clone(),
            AgentKind::Codex,
            AgentLaunch {
                profile: Box::new(ProviderProfile {
                    kind: ProviderKind::OpenaiResponses,
                    ..anthropic()
                }),
                launch_dir: tmp.path().join("launch"),
                native_home: home.clone(),
                binary: Some(stub.clone()),
                origin: LaunchOrigin::Human,
                native_home_managed: true,
                settings_overlay: None,
            },
        );
        options.emulator = false;
        options.extra_env.insert(
            "CBINJECT_TYPED".into(),
            typed.to_string_lossy().into_owned(),
        );
        options.memory_context = Some(context());
        ShellPtyDriver::new(options)
    };
    let mut requested = spec(DriverKind::ShellPty, AgentKind::Codex, &[]);
    requested.cwd = workspace.to_string_lossy().into_owned();
    let read_until = |path: &Path, needle: &str| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let text = fs::read_to_string(path).unwrap_or_default();
            if text.contains(needle) || std::time::Instant::now() > deadline {
                return text;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    };

    // A fresh session: the block, then the first input; the second input bare.
    let typed = tmp.path().join("fresh.txt");
    let fresh = driver_for(&typed);
    let handle = Driver::start(&fresh, requested.clone())
        .await
        .expect("start");
    assert_eq!(handle.recipe().memory_data_block(), Some(INDEX));
    fresh.send(prompt("first input")).await.expect("first send");
    fresh
        .send(prompt("second input"))
        .await
        .expect("second send");
    let text = read_until(&typed, "second input");
    assert!(text.trim_start().starts_with("<remuda-memory"), "{text:?}");
    assert_eq!(text.matches("<remuda-memory").count(), 1, "{text:?}");
    let block_end = text.find("</remuda-memory>").expect("closed block");
    let first = text.find("first input").expect("first input");
    assert!(block_end < first && first < text.find("second input").unwrap());
    Driver::close(&fresh).await.expect("close");

    // A native resume of that conversation: no block at all.
    let typed = tmp.path().join("resumed.txt");
    let resumed = driver_for(&typed);
    let handle = Driver::start_resumed(
        &resumed,
        requested.clone(),
        "01993ab0-0000-7000-8000-000000000009".into(),
    )
    .await
    .expect("resume");
    assert_eq!(handle.recipe().memory, None);
    resumed
        .send(prompt("after resume"))
        .await
        .expect("send after resume");
    let text = read_until(&typed, "after resume");
    assert!(text.contains("after resume"), "{text:?}");
    assert!(!text.contains("<remuda-memory"), "{text:?}");
    Driver::close(&resumed).await.expect("close");
}
