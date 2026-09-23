//! `AskUserQuestion` hook payload → [`QuestionRequest`] card, and the answer
//! mapping for both directions.
//!
//! Measured live against claude 2.1.272 (evidence `ask-user-question-1.md`):
//!
//! - The TUI's question form is a `PermissionRequest` hook whose
//!   `tool_name` is `AskUserQuestion`; `tool_input.questions[]` carries
//!   `question` / `header` / `options[{label, description}]` / `multiSelect`.
//! - A hook answers it with the ordinary nested permission decision and an
//!   `updatedInput` that is the original tool input plus an `answers` object.
//!   The answers object is keyed by the **question text** (not the header):
//!   a single-select answer is the chosen label string, a multi-select answer
//!   is an array of label strings (a `", "`-joined string is also accepted by
//!   the harness, but the array is the shape the TUI produces), and free text
//!   is any string not present in the options.
//!
//! Option ids on the card are therefore the labels themselves — the same
//! convention the claude-control path already uses
//! (`remuda_driver::claude_print::question_request`) — so an answer needs no
//! label lookup table to travel back. Field ids are `q0`, `q1`, … in payload
//! order, matching that same path.

use crate::decision::{HookDecision, PermissionRequestEvent};
use crate::event::HookEvent;
use remuda_protocol::{
    ActorRef, ActorType, CommandId, CommittedAnswer, EntityMeta, Id, InstanceId, Interaction,
    InteractionAnswer, InteractionCarrier, InteractionKind, InteractionRequest,
    InteractionRequestKey, InteractionState, Knowledge, NativeRequestKey, QuestionAnswer,
    QuestionField, QuestionFieldAnswer, QuestionInput, QuestionOption, QuestionRequest, Timestamp,
    U64,
};
use serde_json::Value;
use std::collections::BTreeMap;

/// Tool name the harness uses for its question form.
pub const ASK_USER_QUESTION: &str = "AskUserQuestion";

/// True when a permission request is actually the question form.
#[must_use]
pub fn is_ask_user_question(request: &PermissionRequestEvent) -> bool {
    request.tool_name == ASK_USER_QUESTION
}

/// Read the question request carried by either hook event that carries one.
///
/// - `PermissionRequest` is the shape default/manual modes raise (evidence
///   `ask-user-question-1.md`).
/// - In **auto** permission mode the harness raises the question as a
///   `PreToolUse` instead and sends no `PermissionRequest` at all when that
///   hook answers (measured on `claude` 2.1.277, evidence
///   `askq-pretooluse-1.md`). Its payload is field-compatible: same
///   `tool_input.questions[]`; `tool_use_id` is present here (it is absent on
///   `PermissionRequest`), and there are no permission suggestions.
///
/// Returns `None` for anything else, so a `PreToolUse` for an ordinary tool
/// can never be turned into a question card.
#[must_use]
pub fn question_request_from_event(event: &HookEvent) -> Option<PermissionRequestEvent> {
    match event.name.as_str() {
        "PermissionRequest" => {
            PermissionRequestEvent::from_event(event).filter(is_ask_user_question)
        }
        "PreToolUse" if event.text("tool_name") == Some(ASK_USER_QUESTION) => {
            Some(PermissionRequestEvent {
                tool_name: ASK_USER_QUESTION.to_owned(),
                tool_input: event
                    .payload
                    .get("tool_input")
                    .cloned()
                    .unwrap_or(Value::Null),
                suggestions: Vec::new(),
                tool_use_id: event.text("tool_use_id").map(ToOwned::to_owned),
                permission_mode: event.text("permission_mode").map(ToOwned::to_owned),
            })
        }
        _ => None,
    }
}

/// Build the question card for an `AskUserQuestion` permission request.
pub fn question_interaction(
    request: &PermissionRequestEvent,
    context: &crate::approval::ApprovalContext,
) -> Result<Interaction, remuda_protocol::WireValueError> {
    Ok(Interaction {
        meta: EntityMeta {
            id: context.interaction_id.clone(),
            revision: U64(1),
            created_at: context.now.clone(),
            updated_at: context.now.clone(),
        },
        instance_id: context.instance_id.clone(),
        run_id: Some(context.run_id.clone()),
        host_id: context.host_id.clone(),
        kind: InteractionKind::Question,
        request_key: InteractionRequestKey {
            native: NativeRequestKey::Hook {
                invocation_id: context.interaction_id.as_id().clone(),
            },
            process_generation: U64(1),
            run_generation: Some(U64(1)),
            connection_epoch: Id::new("epoch")?,
        },
        request_version: U64(1),
        state: InteractionState::Pending,
        blocking: true,
        answerable: true,
        carrier: InteractionCarrier::HarnessHook,
        request: InteractionRequest::Question(Box::new(question_request(request))),
        deadline: Knowledge::Known {
            value: context.deadline.clone(),
        },
        deadline_source: remuda_protocol::DeadlineSource::RuntimePolicy,
        answer: Knowledge::Unknown {
            reason: "pending".into(),
            evidence_event_ids: Vec::new(),
        },
        delivery: remuda_protocol::DeliveryState::NotSent,
        resolution: Knowledge::Unknown {
            reason: "pending".into(),
            evidence_event_ids: Vec::new(),
        },
    })
}

/// Translate `tool_input.questions[]` into protocol question fields.
#[must_use]
pub fn question_request(request: &PermissionRequestEvent) -> QuestionRequest {
    let questions = request
        .tool_input
        .get("questions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let fields = questions
        .iter()
        .enumerate()
        .map(|(index, question)| {
            let title = question
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or("question")
                .to_owned();
            let options = question
                .get("options")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|option| {
                    option.get("label").and_then(Value::as_str).map(|label| {
                        QuestionOption {
                            // The label is the id: it is also the exact value
                            // the answers object carries back to the harness.
                            id: label.to_owned(),
                            label: label.to_owned(),
                            description: option
                                .get("description")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned),
                        }
                    })
                })
                .collect();
            let multi = question
                .get("multiSelect")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            QuestionField {
                id: format!("q{index}"),
                title,
                // The header is the tab title in the TUI; the protocol field
                // description is where the control carrier puts it too.
                description: question
                    .get("header")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                input: if multi {
                    QuestionInput::MultiSelect
                } else {
                    QuestionInput::SingleSelect
                },
                required: true,
                options,
                // The TUI always offers "Type something", on both radios and
                // checkboxes; the card must offer it as well.
                allow_free_text: true,
                sensitive: false,
            }
        })
        .collect();
    QuestionRequest {
        title: ASK_USER_QUESTION.into(),
        fields,
    }
}

/// Turn a card answer into the hook decision the harness applies.
///
/// `None` only when an answer names a field or option the request never
/// offered; the caller denies rather than guessing. An entirely empty answer
/// is the card's explicit Deny/Cancel.
#[must_use]
pub fn question_decision(
    request: &PermissionRequestEvent,
    answer: &QuestionAnswer,
) -> Option<HookDecision> {
    if answer.answers.is_empty() {
        return Some(HookDecision::Deny {
            message: "The user declined to answer the question".to_owned(),
        });
    }
    let questions = request
        .tool_input
        .get("questions")
        .and_then(Value::as_array)?;
    let mut answers = serde_json::Map::new();
    for (field_id, field) in &answer.answers {
        let index = field_id.strip_prefix('q')?.parse::<usize>().ok()?;
        let question = questions.get(index)?;
        let key = question.get("question")?.as_str()?;
        let labels: Vec<&str> = question
            .get("options")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|option| option.get("label").and_then(Value::as_str))
            .collect();
        let text = field
            .text
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty());
        let value = if let Some(text) = text {
            Value::String(text.to_owned())
        } else if question.get("multiSelect").and_then(Value::as_bool) == Some(true) {
            if field
                .option_ids
                .iter()
                .any(|id| !labels.contains(&id.as_str()))
            {
                return None;
            }
            Value::Array(
                field
                    .option_ids
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            )
        } else {
            let label = field.option_ids.first()?;
            if !labels.contains(&label.as_str()) {
                return None;
            }
            Value::String(label.clone())
        };
        answers.insert(key.to_owned(), value);
    }
    let mut updated = request.tool_input.clone();
    if let Some(object) = updated.as_object_mut() {
        object.insert("answers".into(), Value::Object(answers));
    } else {
        return None;
    }
    Some(HookDecision::Allow {
        updated_input: Some(updated),
        updated_permissions: Vec::new(),
    })
}

/// Rebuild a card answer from the harness's own `answers` object — the answer
/// given in the TUI, observed on `PostToolUse` after the dialog closes there.
///
/// A string matching an offered label is an option choice; any other string is
/// free text. An array is a set of option choices. Returns `None` when the
/// payload carries no questions this request asked.
#[must_use]
pub fn answer_from_harness(
    questions: &Value,
    raw_answers: &Value,
) -> Option<BTreeMap<String, QuestionFieldAnswer>> {
    let questions = questions.as_array()?;
    let raw_answers = raw_answers.as_object()?;
    let mut answers = BTreeMap::new();
    for (index, question) in questions.iter().enumerate() {
        let Some(key) = question.get("question").and_then(Value::as_str) else {
            continue;
        };
        let Some(value) = raw_answers.get(key) else {
            continue;
        };
        let labels: Vec<&str> = question
            .get("options")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|option| option.get("label").and_then(Value::as_str))
            .collect();
        let multi = question.get("multiSelect").and_then(Value::as_bool) == Some(true);
        let field = match value {
            Value::String(text) => {
                if labels.contains(&text.as_str()) {
                    QuestionFieldAnswer {
                        option_ids: vec![text.clone()],
                        text: None,
                    }
                } else if multi {
                    // The harness accepts a multi answer either as an array or
                    // as a ", "-joined string (both measured on 2.1.272); only
                    // treat the split as choices when every piece is a label,
                    // otherwise it is free text.
                    let pieces: Vec<&str> = text.split(", ").collect();
                    if pieces.len() > 1 && pieces.iter().all(|label| labels.contains(label)) {
                        QuestionFieldAnswer {
                            option_ids: pieces.into_iter().map(ToOwned::to_owned).collect(),
                            text: None,
                        }
                    } else {
                        QuestionFieldAnswer {
                            option_ids: Vec::new(),
                            text: Some(text.clone()),
                        }
                    }
                } else {
                    QuestionFieldAnswer {
                        option_ids: Vec::new(),
                        text: Some(text.clone()),
                    }
                }
            }
            Value::Array(values) => QuestionFieldAnswer {
                option_ids: values
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|label| labels.contains(label))
                    .map(ToOwned::to_owned)
                    .collect(),
                text: None,
            },
            _ => continue,
        };
        answers.insert(format!("q{index}"), field);
    }
    (!answers.is_empty()).then_some(answers)
}

/// The actor stamped on an interaction the human answered in the agent's own
/// terminal: a human on this instance, with no device id.
#[must_use]
pub fn terminal_actor(instance_id: InstanceId) -> ActorRef {
    ActorRef {
        principal_id: Id::new("prn").expect("prn is a registered prefix"),
        actor_type: ActorType::Human,
        device_id: None,
        instance_id: Some(instance_id),
    }
}

/// Stamp a fresh interaction entity with a terminal answer and the resolved
/// state, ready to emit as an entity lifecycle observation.
#[must_use]
pub fn resolved_in_terminal(
    mut interaction: Interaction,
    answers: BTreeMap<String, QuestionFieldAnswer>,
    now: Timestamp,
    command_id: CommandId,
) -> Interaction {
    interaction.meta.revision.0 += 1;
    interaction.meta.updated_at = now.clone();
    interaction.state = InteractionState::Resolved;
    interaction.blocking = false;
    interaction.answerable = false;
    interaction.delivery = remuda_protocol::DeliveryState::Confirmed;
    interaction.answer = Knowledge::Known {
        value: CommittedAnswer {
            command_id,
            actor: terminal_actor(interaction.instance_id.clone()),
            value: InteractionAnswer::Question(Box::new(QuestionAnswer { answers })),
            committed_at: now,
        },
    };
    interaction.resolution = Knowledge::Known {
        value: remuda_protocol::InteractionResolution {
            reason: remuda_protocol::InteractionResolutionReason::Answered,
            event_ids: Vec::new(),
        },
    };
    interaction
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::ApprovalContext;
    use remuda_protocol::{HostId, InteractionId, RunId};

    fn recorded() -> PermissionRequestEvent {
        let raw = include_str!("../fixtures/askuser/permission-request.json");
        let event = crate::event::HookEvent {
            name: "PermissionRequest".into(),
            ppid: 4242,
            payload: serde_json::from_str(raw).unwrap(),
        };
        PermissionRequestEvent::from_event(&event).expect("a recorded permission request")
    }

    /// The recorded auto-mode PreToolUse (claude 2.1.277,
    /// evidence askq-pretooluse-1).
    fn recorded_pretooluse() -> crate::event::HookEvent {
        let raw = include_str!("../fixtures/askuser/pretooluse-auto.json");
        crate::event::HookEvent {
            name: "PreToolUse".into(),
            ppid: 4242,
            payload: serde_json::from_str(raw).unwrap(),
        }
    }

    fn context() -> ApprovalContext {
        ApprovalContext {
            instance_id: InstanceId::new(),
            host_id: HostId::new(),
            run_id: RunId::new(),
            interaction_id: InteractionId::new(),
            now: Timestamp::try_from("2026-09-16T00:00:00.000Z".to_owned()).unwrap(),
            deadline: Timestamp::try_from("2026-09-16T00:15:00.000Z".to_owned()).unwrap(),
        }
    }

    #[test]
    fn the_recorded_request_builds_a_two_field_question_card() {
        let interaction = question_interaction(&recorded(), &context()).unwrap();
        assert_eq!(interaction.kind, InteractionKind::Question);
        assert_eq!(interaction.carrier, InteractionCarrier::HarnessHook);
        let InteractionRequest::Question(request) = &interaction.request else {
            panic!("expected a question");
        };
        assert_eq!(request.fields.len(), 2);
        assert_eq!(request.fields[0].id, "q0");
        assert_eq!(request.fields[0].title, "接下来这一步你想怎么走？");
        assert_eq!(request.fields[0].description.as_deref(), Some("下一步"));
        assert_eq!(request.fields[0].input, QuestionInput::SingleSelect);
        assert_eq!(request.fields[0].options.len(), 3);
        assert_eq!(request.fields[0].options[0].id, "继续排查");
        assert!(request.fields[0].allow_free_text);
        assert_eq!(request.fields[1].input, QuestionInput::MultiSelect);
        assert_eq!(request.fields[1].options.len(), 2);
    }

    #[test]
    fn a_card_answer_becomes_updated_input_answers_keyed_by_question_text() {
        let event = recorded();
        let answer = QuestionAnswer {
            answers: BTreeMap::from([
                (
                    "q0".into(),
                    QuestionFieldAnswer {
                        option_ids: vec!["回到GravityDB".into()],
                        text: None,
                    },
                ),
                (
                    "q1".into(),
                    QuestionFieldAnswer {
                        option_ids: vec!["保存端口".into(), "保存环境变量".into()],
                        text: None,
                    },
                ),
            ]),
        };
        let HookDecision::Allow {
            updated_input,
            updated_permissions,
        } = question_decision(&event, &answer).expect("a decision")
        else {
            panic!("expected allow");
        };
        assert!(updated_permissions.is_empty());
        let input = updated_input.expect("updated input");
        assert_eq!(
            input["answers"]["接下来这一步你想怎么走？"],
            serde_json::json!("回到GravityDB")
        );
        assert_eq!(
            input["answers"]["需要把哪些内容保存到记忆里？"],
            serde_json::json!(["保存端口", "保存环境变量"])
        );
        // The original questions ride along verbatim.
        assert!(input["questions"].is_array());
    }

    #[test]
    fn free_text_travels_verbatim_even_when_it_is_not_an_option() {
        let event = recorded();
        let answer = QuestionAnswer {
            answers: BTreeMap::from([(
                "q0".into(),
                QuestionFieldAnswer {
                    option_ids: vec![],
                    text: Some("我想先喝杯茶".into()),
                },
            )]),
        };
        let decision = question_decision(&event, &answer).expect("a decision");
        let HookDecision::Allow { updated_input, .. } = decision else {
            panic!("expected allow");
        };
        assert_eq!(
            updated_input.expect("input")["answers"]["接下来这一步你想怎么走？"],
            serde_json::json!("我想先喝杯茶")
        );
    }

    #[test]
    fn an_empty_answer_is_a_deny_not_an_allow_with_blank_answers() {
        let decision = question_decision(
            &recorded(),
            &QuestionAnswer {
                answers: BTreeMap::new(),
            },
        )
        .expect("a decision");
        assert!(matches!(decision, HookDecision::Deny { .. }));
    }

    #[test]
    fn an_answer_naming_an_unknown_option_is_refused() {
        let answer = QuestionAnswer {
            answers: BTreeMap::from([(
                "q0".into(),
                QuestionFieldAnswer {
                    option_ids: vec!["不存在的选项".into()],
                    text: None,
                },
            )]),
        };
        assert!(question_decision(&recorded(), &answer).is_none());
    }

    #[test]
    fn terminal_answers_are_reconstructed_from_the_post_tool_use_payload() {
        let raw = include_str!("../fixtures/askuser/post-tool-use.json");
        let post: Value = serde_json::from_str(raw).unwrap();
        let answers = answer_from_harness(
            &post["tool_input"]["questions"],
            &post["tool_response"]["answers"],
        )
        .expect("answers");
        assert_eq!(answers["q0"].option_ids, vec!["继续排查".to_owned()]);
        assert_eq!(answers["q0"].text, None);
        assert_eq!(
            answers["q1"].option_ids,
            vec!["保存端口".to_owned(), "保存环境变量".to_owned()]
        );
    }

    #[test]
    fn a_terminal_free_text_answer_is_reconstructed_as_text() {
        let questions = serde_json::json!([
            {"question": "去哪？", "options": [{"label": "家"}, {"label": "公司"}]}
        ]);
        let answers = serde_json::json!({"去哪？": "月球"});
        let mapped = answer_from_harness(&questions, &answers).expect("answers");
        assert!(mapped["q0"].option_ids.is_empty());
        assert_eq!(mapped["q0"].text.as_deref(), Some("月球"));
    }

    #[test]
    fn the_recorded_auto_mode_pretooluse_is_read_as_a_question() {
        // The c-askq shape: auto mode sends no PermissionRequest; the
        // PreToolUse carries the same questions payload plus the tool_use_id
        // that PermissionRequest lacks.
        let request = question_request_from_event(&recorded_pretooluse())
            .expect("the auto-mode PreToolUse is a question request");
        assert_eq!(request.tool_name, ASK_USER_QUESTION);
        assert_eq!(
            request.tool_use_id.as_deref(),
            Some("call_00000000000000000000000001")
        );
        assert_eq!(request.permission_mode.as_deref(), Some("auto"));
        assert!(request.suggestions.is_empty());
        assert_eq!(request.tool_input["questions"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn the_recorded_auto_mode_pretooluse_builds_the_same_card_shape() {
        let request = question_request_from_event(&recorded_pretooluse()).expect("request");
        let card = question_request(&request);
        assert_eq!(card.fields.len(), 1);
        assert_eq!(card.fields[0].title, "Tea or coffee?");
        assert_eq!(card.fields[0].description.as_deref(), Some("Beverage"));
        assert_eq!(card.fields[0].input, QuestionInput::SingleSelect);
        assert_eq!(card.fields[0].options.len(), 2);
        assert_eq!(card.fields[0].options[0].id, "Tea");
    }

    #[test]
    fn an_auto_mode_card_answer_round_trips_through_the_pretooluse_input() {
        let request = question_request_from_event(&recorded_pretooluse()).expect("request");
        let decision = question_decision(
            &request,
            &QuestionAnswer {
                answers: BTreeMap::from([(
                    "q0".into(),
                    QuestionFieldAnswer {
                        option_ids: vec!["Coffee".into()],
                        text: None,
                    },
                )]),
            },
        )
        .expect("a decision");
        let HookDecision::Allow { updated_input, .. } = decision else {
            panic!("expected allow");
        };
        assert_eq!(
            updated_input.expect("input")["answers"]["Tea or coffee?"],
            serde_json::json!("Coffee")
        );
    }

    #[test]
    fn only_askuserquestion_pretooluse_events_become_question_requests() {
        for (name, tool) in [
            ("PreToolUse", "Bash"),
            ("PreToolUse", "Write"),
            ("PostToolUse", ASK_USER_QUESTION),
            ("Notification", ASK_USER_QUESTION),
        ] {
            let event = HookEvent {
                name: name.into(),
                ppid: 1,
                payload: serde_json::json!({"tool_name": tool, "tool_input": {}}),
            };
            assert!(
                question_request_from_event(&event).is_none(),
                "{name}/{tool} must not become a question"
            );
        }
    }

    #[test]
    fn permission_request_and_pretooluse_for_one_call_have_equal_questions() {
        // The dedup key relies on the paired events carrying byte-identical
        // question arrays (measured 2.1.277); assert it on the recorded pair.
        let pre = question_request_from_event(&recorded_pretooluse()).expect("pre");
        let raw = include_str!("../fixtures/askuser/posttooluse-auto.json");
        let post = HookEvent {
            name: "PostToolUse".into(),
            ppid: 1,
            payload: serde_json::from_str(raw).unwrap(),
        };
        // The PostToolUse echoes the same questions the card was built from.
        assert_eq!(
            pre.tool_input["questions"],
            post.payload["tool_input"]["questions"]
        );
    }
}
