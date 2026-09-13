//! Handwritten fake-herdr fixtures: no model calls or personal terminal data.
use remuda_driver::{
    BinarySource, ClaudePtyDriver, ClaudePtyOptions, Delegation, Driver, GenericPtyDriver,
    GenericPtyOptions, ProviderHealth, ProviderKind, ProviderProfile, RunHandle, pin_binary,
};
use remuda_protocol::*;
use remuda_testing::{
    FakeHerdrOptions, FakeHerdrScript, FakeHerdrServer, ensure_workspace_bin, install_executable,
};
use std::{collections::BTreeMap, time::Duration};

async fn requested(handle: &mut RunHandle) -> Interaction {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let observation = handle.recv().await.expect("observation stream");
            if let ObservationPayload::InteractionRequested(payload) = observation.body {
                assert_eq!(observation.completeness, Completeness::ScreenDerived);
                return payload.interaction;
            }
        }
    })
    .await
    .expect("blocked interaction")
}

fn answer(interaction: &Interaction, option: &str) -> InteractionAnswer {
    match &interaction.request {
        InteractionRequest::Approval(request) => {
            InteractionAnswer::Approval(Box::new(ApprovalAnswer {
                option_id: option.into(),
                input_digest: request.input_digest.clone(),
            }))
        }
        InteractionRequest::Question(request) => {
            InteractionAnswer::Question(Box::new(QuestionAnswer {
                answers: BTreeMap::from([(
                    request.fields[0].id.clone(),
                    QuestionFieldAnswer {
                        option_ids: if request.fields[0].input == QuestionInput::Text {
                            vec![]
                        } else {
                            vec![option.into()]
                        },
                        text: (request.fields[0].input == QuestionInput::Text)
                            .then(|| option.into()),
                    },
                )]),
            }))
        }
        _ => panic!("PTY request kind"),
    }
}

#[tokio::test]
async fn all_pty_kinds_block_answer_once_and_settle_on_idle() {
    for (kind, script, selected, expected_keys) in [
        (AgentKind::Codex, FakeHerdrScript::Approval, "y", "y enter"),
        (
            AgentKind::Grok,
            FakeHerdrScript::Question,
            "2",
            "down enter",
        ),
        (AgentKind::Agy, FakeHerdrScript::Continue, "enter", "enter"),
        (
            AgentKind::Claude,
            FakeHerdrScript::TextQuestion,
            "demo",
            "d e m o enter",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let socket_dir = dir.path().join("herdr");
        std::fs::create_dir_all(&socket_dir).unwrap();
        let mut fake_options = FakeHerdrOptions::new(socket_dir.join("herdr.sock"));
        fake_options.script = script;
        let _fake = FakeHerdrServer::spawn(fake_options).unwrap();
        let bin = install_executable(dir.path(), "native-stub", "#!/bin/sh\necho 'stub 1.0'\n");
        let binary = BinarySource::Pinned(pin_binary(&bin).unwrap());
        let profile = ProviderProfile {
            id: Id::new("pvp").unwrap(),
            kind: ProviderKind::Anthropic,
            base_url: String::new(),
            delegation: Delegation::None,
            secret_ref: None,
            models: vec!["default".into()],
            health: ProviderHealth::Healthy,
        };
        let mut spec: InstanceSpec =
            serde_json::from_str(include_str!("fixtures/instance-spec.json")).unwrap();
        spec.kind = kind;
        spec.cwd = dir.path().to_string_lossy().into_owned();
        spec.driver = if kind == AgentKind::Claude {
            DriverKind::ClaudePty
        } else {
            DriverKind::GenericPty
        };
        let fake_bin = ensure_workspace_bin("fake-herdr");
        let driver: Box<dyn Driver> = if kind == AgentKind::Claude {
            let mut options = ClaudePtyOptions::new(
                profile,
                dir.path().join("launch"),
                dir.path().join("home"),
                binary,
            );
            options.socket_dir = Some(socket_dir.clone());
            options.herdr_binary = Some(fake_bin);
            Box::new(ClaudePtyDriver::new(options))
        } else {
            let mut options = GenericPtyOptions::new(
                profile,
                dir.path().join("launch"),
                dir.path().join("home"),
                binary,
            );
            options.socket_dir = Some(socket_dir.clone());
            options.herdr_binary = Some(fake_bin);
            options.liveness_timeout_ms = 2000;
            Box::new(GenericPtyDriver::new(options))
        };
        let mut handle = driver.start(spec).await.unwrap();
        let interaction = requested(&mut handle).await;
        assert_eq!(interaction.carrier, InteractionCarrier::NativeTty);
        assert!(interaction.answerable && interaction.blocking);
        // Repeated polling cannot create duplicate cards for the same prompt.
        tokio::time::sleep(Duration::from_millis(550)).await;
        let reply = answer(&interaction, selected);
        driver
            .respond_interaction(interaction.meta.id.clone(), reply.clone())
            .await
            .unwrap();
        assert!(
            driver
                .respond_interaction(interaction.meta.id.clone(), reply)
                .await
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                let observation = handle.recv().await.unwrap();
                assert!(
                    !matches!(
                        observation.body,
                        ObservationPayload::InteractionRequested(_)
                    ),
                    "duplicate blocked card"
                );
                if let ObservationPayload::Lifecycle(payload) = observation.body
                    && let LifecyclePayload::Entity(entity) = payload.as_ref()
                    && let LifecycleEntity::Interaction(settled) = &entity.entity_value
                {
                    assert_eq!(settled.meta.id, interaction.meta.id);
                    if settled.state == InteractionState::AnswerCommitted {
                        continue;
                    }
                    assert_eq!(settled.state, InteractionState::Resolved);
                    assert_eq!(settled.delivery, DeliveryState::Written);
                    assert_eq!(entity.reason_code, "native-cleared");
                    break;
                }
            }
        })
        .await
        .unwrap();
        let client = remuda_herdr::Client::connect(socket_dir.join("herdr.sock"));
        let agent = handle.ack().native_ids.get("agentName").unwrap();
        assert_eq!(
            client.agent_get(agent).await.unwrap().agent.agent_status,
            remuda_herdr::AgentStatus::Idle
        );
        let screen = client
            .agent_read(remuda_herdr::AgentReadParams {
                target: agent.clone(),
                source: remuda_herdr::ReadSource::Visible,
                lines: Some(32),
                format: remuda_herdr::ReadFormat::Text,
                strip_ansi: true,
            })
            .await
            .unwrap();
        assert!(
            screen.text().contains(&format!("KEYS {expected_keys}")),
            "{}",
            screen.text()
        );
        driver.close().await.unwrap();
    }
}

#[tokio::test]
async fn same_kind_instances_keep_prompt_and_key_ownership() {
    for kind in [AgentKind::Codex, AgentKind::Claude] {
        let dir = tempfile::tempdir().unwrap();
        let socket_dir = dir.path().join("herdr");
        std::fs::create_dir_all(&socket_dir).unwrap();
        let mut fake_options = FakeHerdrOptions::new(socket_dir.join("herdr.sock"));
        fake_options.script = FakeHerdrScript::Approval;
        let _fake = FakeHerdrServer::spawn(fake_options).unwrap();
        let bin = install_executable(dir.path(), "native-stub", "#!/bin/sh\necho 'stub 1.0'\n");
        let mut spec: InstanceSpec =
            serde_json::from_str(include_str!("fixtures/instance-spec.json")).unwrap();
        spec.kind = kind;
        spec.cwd = dir.path().to_string_lossy().into_owned();
        spec.driver = if kind == AgentKind::Claude {
            DriverKind::ClaudePty
        } else {
            DriverKind::GenericPty
        };
        let mut runs = Vec::new();
        for index in 0..2 {
            let profile = ProviderProfile {
                id: Id::new("pvp").unwrap(),
                kind: ProviderKind::Anthropic,
                base_url: String::new(),
                delegation: Delegation::None,
                secret_ref: None,
                models: vec!["default".into()],
                health: ProviderHealth::Healthy,
            };
            let launch = dir.path().join(format!("launch-{index}"));
            let home = dir.path().join(format!("home-{index}"));
            let binary = BinarySource::Pinned(pin_binary(&bin).unwrap());
            let driver: Box<dyn Driver> = if kind == AgentKind::Claude {
                let mut options = ClaudePtyOptions::new(profile, launch, home, binary);
                options.socket_dir = Some(socket_dir.clone());
                options.herdr_binary = Some(ensure_workspace_bin("fake-herdr"));
                Box::new(ClaudePtyDriver::new(options))
            } else {
                let mut options = GenericPtyOptions::new(profile, launch, home, binary);
                options.socket_dir = Some(socket_dir.clone());
                options.herdr_binary = Some(ensure_workspace_bin("fake-herdr"));
                options.liveness_timeout_ms = 2000;
                Box::new(GenericPtyDriver::new(options))
            };
            let mut handle = driver.start(spec.clone()).await.unwrap();
            let interaction = requested(&mut handle).await;
            runs.push((driver, handle, interaction));
        }
        let [
            (first, first_run, first_prompt),
            (second, second_run, second_prompt),
        ] = runs.as_slice()
        else {
            unreachable!()
        };
        assert_ne!(
            first_run.ack().native_ids["agentName"],
            second_run.ack().native_ids["agentName"]
        );
        assert_ne!(first_prompt.meta.id, second_prompt.meta.id);
        assert!(
            first
                .respond_interaction(second_prompt.meta.id.clone(), answer(second_prompt, "y"))
                .await
                .is_err()
        );
        first
            .respond_interaction(first_prompt.meta.id.clone(), answer(first_prompt, "y"))
            .await
            .unwrap();
        let client = remuda_herdr::Client::connect(socket_dir.join("herdr.sock"));
        assert_eq!(
            client
                .agent_get(&first_run.ack().native_ids["paneId"])
                .await
                .unwrap()
                .agent
                .agent_status,
            remuda_herdr::AgentStatus::Idle
        );
        assert_eq!(
            client
                .agent_get(&second_run.ack().native_ids["paneId"])
                .await
                .unwrap()
                .agent
                .agent_status,
            remuda_herdr::AgentStatus::Blocked
        );
        second
            .respond_interaction(second_prompt.meta.id.clone(), answer(second_prompt, "n"))
            .await
            .unwrap();
        assert_eq!(
            client
                .agent_get(&second_run.ack().native_ids["paneId"])
                .await
                .unwrap()
                .agent
                .agent_status,
            remuda_herdr::AgentStatus::Idle
        );
        first.close().await.unwrap();
        second.close().await.unwrap();
    }
}

#[tokio::test]
async fn claude_trust_is_auto_answered_only_when_node_enabled_it() {
    for enabled in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let socket_dir = dir.path().join("herdr");
        std::fs::create_dir_all(&socket_dir).unwrap();
        let mut fake_options = FakeHerdrOptions::new(socket_dir.join("herdr.sock"));
        fake_options.script = FakeHerdrScript::Trust;
        let _fake = FakeHerdrServer::spawn(fake_options).unwrap();
        let bin = install_executable(dir.path(), "native-stub", "#!/bin/sh\necho 'stub 1.0'\n");
        let profile = ProviderProfile {
            id: Id::new("pvp").unwrap(),
            kind: ProviderKind::Anthropic,
            base_url: String::new(),
            delegation: Delegation::None,
            secret_ref: None,
            models: vec!["default".into()],
            health: ProviderHealth::Healthy,
        };
        let mut options = ClaudePtyOptions::new(
            profile,
            dir.path().join("launch"),
            dir.path().join("home"),
            BinarySource::Pinned(pin_binary(&bin).unwrap()),
        );
        options.socket_dir = Some(socket_dir.clone());
        options.herdr_binary = Some(ensure_workspace_bin("fake-herdr"));
        options.auto_trust_registered_workspace = enabled;
        let driver = ClaudePtyDriver::new(options);
        let mut spec: InstanceSpec =
            serde_json::from_str(include_str!("fixtures/instance-spec.json")).unwrap();
        spec.driver = DriverKind::ClaudePty;
        spec.cwd = dir.path().to_string_lossy().into_owned();
        let mut handle = driver.start(spec).await.unwrap();
        let interaction = requested(&mut handle).await;
        assert!(interaction.blocking);
        let client = remuda_herdr::Client::connect(socket_dir.join("herdr.sock"));
        let pane = handle.ack().native_ids["paneId"].clone();
        if enabled {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let observation = handle.recv().await.unwrap();
                    if let ObservationPayload::Lifecycle(payload) = observation.body
                        && let LifecyclePayload::Native(native) = *payload
                        && native.native_name == "trust-dialog"
                    {
                        assert!(matches!(native.status, Knowledge::Known { value }
                            if value == "trust-dialog auto-accepted (registered workspace)"));
                        break;
                    }
                }
            })
            .await
            .unwrap();
            assert!(matches!(
                driver.wait_control().await,
                Err(remuda_driver::DriverError::ControlUnavailable)
            ));
            std::fs::write(
                dir.path().join("launch/session-meta.json"),
                serde_json::json!({
                    "session_id": "fixture-session",
                    "transcript_path": dir.path().join("transcript.jsonl"),
                    "hook_event_name": "SessionStart",
                })
                .to_string(),
            )
            .unwrap();
            tokio::time::timeout(Duration::from_secs(3), async {
                while driver.wait_control().await.is_err() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
        } else {
            assert!(matches!(
                driver.wait_control().await,
                Err(remuda_driver::DriverError::ControlUnavailable)
            ));
        }
        let screen = client
            .agent_read(remuda_herdr::AgentReadParams {
                target: pane,
                source: remuda_herdr::ReadSource::Visible,
                lines: Some(32),
                format: remuda_herdr::ReadFormat::Text,
                strip_ansi: true,
            })
            .await
            .unwrap();
        assert_eq!(
            screen.text().matches("KEYS down enter").count(),
            usize::from(enabled)
        );
        driver.close().await.unwrap();
    }
}
