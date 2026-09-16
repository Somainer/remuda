//! Claude envelope UUIDs identify events, not content. Correlate each native
//! message and block index; assistant records may contain only one finished block.

use super::*;
use remuda_protocol::MessageOrigin;
use std::collections::{BTreeMap, HashSet};

#[cfg(test)]
#[path = "claude_print_stream_tests.rs"]
mod tests;

#[derive(Default)]
pub(super) struct StreamState {
    active: HashMap<Option<String>, String>,
    blocks: HashMap<(Option<String>, String), BTreeMap<u32, Block>>,
    snapshots: HashSet<String>,
}

struct Block {
    id: Id,
    revision: u64,
    value: Value,
    input_json: String,
    closed: bool,
    reconciled: bool,
    phase: MessagePhase,
}

impl Block {
    fn kind(&self) -> &str {
        self.value["type"].as_str().unwrap_or("")
    }

    fn emit(
        &mut self,
        mapper: &mut Mapper,
        native: &str,
        index: u32,
        parent: Option<Id>,
        operation: MutationOperation,
        delta: Option<&str>,
    ) -> DriverResult<Observation> {
        let base_revision = (self.revision > 0).then_some(U64(self.revision));
        self.revision += 1;
        let mutation = NodeMutation {
            node_id: self.id.clone(),
            revision: U64(self.revision),
            operation,
            base_revision,
        };
        let status = if self.closed {
            ContentStatus::Complete
        } else {
            ContentStatus::Streaming
        };
        let body = match self.kind() {
            "text" => ObservationPayload::Message(Box::new(MessagePayload {
                mutation,
                message_id: self.id.clone(),
                role: MessageRole::Assistant,
                phase: self.phase,
                blocks: vec![ContentBlock::Text(Box::new(TextBlock {
                    text: delta
                        .unwrap_or_else(|| self.value["text"].as_str().unwrap_or(""))
                        .into(),
                }))],
                target_block: Some(0),
                parent_tool_call_id: parent,
                native_origin: known_or_unknown(Some(native)),
                // Assistant text is never an injection.
                origin: Some(MessageOrigin::Human),
                command_id: None,
            prompt_mode: None,
                status,
            })),
            "thinking" | "redacted_thinking" => {
                ObservationPayload::Thought(Box::new(ThoughtPayload {
                    mutation,
                    thought_id: self.id.clone(),
                    representation: if self.kind() == "redacted_thinking" {
                        ThoughtRepresentation::Redacted
                    } else {
                        ThoughtRepresentation::Text
                    },
                    text: delta
                        .or_else(|| self.value["thinking"].as_str())
                        .map(str::to_owned),
                    part_index: index,
                    status,
                }))
            }
            _ => {
                let name = self.value["name"].as_str().unwrap_or("");
                ObservationPayload::ToolCall(Box::new(ToolCallPayload {
                    mutation,
                    tool_call_id: self.id.clone(),
                    parent_tool_call_id: parent,
                    tool_name: known_or_unknown(Some(name)),
                    display_title: known_or_unknown(Some(name)),
                    category: tool_category(name),
                    input: if self.closed {
                        self.value.get("input").cloned().map_or_else(
                            || unknown("missing-input"),
                            |value| Knowledge::Known { value },
                        )
                    } else {
                        unknown("streaming-input")
                    },
                    input_text_delta: delta.map(str::to_owned),
                    state: ToolCallState::Proposed,
                    executor: Knowledge::NotApplicable,
                }))
            }
        };
        mapper.observation(Completeness::Structured, NativeRequestKey::None, body)
    }
}

fn new_block(mapper: &mut Mapper, value: Value) -> DriverResult<Block> {
    let id = if value["type"] == "tool_use" {
        mapper.ids.tool(value["id"].as_str().unwrap_or(""))?
    } else {
        Id::new("obj")?
    };
    Ok(Block {
        id,
        revision: 0,
        value,
        input_json: String::new(),
        closed: false,
        reconciled: false,
        phase: MessagePhase::Final,
    })
}

fn parent_id(mapper: &mut Mapper, parent: Option<&str>) -> DriverResult<Option<Id>> {
    parent.map(|id| mapper.ids.tool(id)).transpose()
}

pub(super) fn map_stream(
    mapper: &mut Mapper,
    frame: &StreamEventMessage,
) -> DriverResult<Vec<Observation>> {
    let event = &frame.event;
    let kind = event["type"].as_str().unwrap_or("");
    if kind == "message_start" {
        if let Some(id) = event.pointer("/message/id").and_then(Value::as_str) {
            mapper
                .stream
                .active
                .insert(frame.parent_tool_use_id.clone(), id.into());
        }
        return Ok(Vec::new());
    }
    let Some(native) = mapper.stream.active.get(&frame.parent_tool_use_id).cloned() else {
        // Missing native identity cannot be repaired using a per-event UUID.
        return mapper.opaque(kind, OpaqueReason::UnmappedFields, event);
    };
    let parent = parent_id(mapper, frame.parent_tool_use_id.as_deref())?;
    let key = (frame.parent_tool_use_id.clone(), native.clone());
    let mut blocks = mapper.stream.blocks.remove(&key).unwrap_or_default();
    let mut out = Vec::new();
    let index = event["index"].as_u64().and_then(|n| u32::try_from(n).ok());
    if kind == "content_block_start" {
        if let Some(index) = index {
            let value = event["content_block"].clone();
            if matches!(
                value["type"].as_str(),
                Some("text" | "thinking" | "redacted_thinking" | "tool_use")
            ) {
                let mut block = new_block(mapper, value)?;
                // Empty text/thought starts have no visible content yet.
                if block.kind() == "tool_use"
                    || block.kind() == "redacted_thinking"
                    || block.value["text"].as_str().is_some_and(|s| !s.is_empty())
                    || block.value["thinking"]
                        .as_str()
                        .is_some_and(|s| !s.is_empty())
                {
                    out.push(block.emit(
                        mapper,
                        &native,
                        index,
                        parent.clone(),
                        MutationOperation::Open,
                        None,
                    )?);
                }
                blocks.insert(index, block);
            } else {
                out.extend(mapper.opaque(kind, OpaqueReason::UnmappedFields, event)?);
            }
        }
    } else if kind == "content_block_delta" {
        if let Some((index, block)) = index.and_then(|i| blocks.get_mut(&i).map(|b| (i, b))) {
            let field = match (block.kind(), event["delta"]["type"].as_str()) {
                ("text", Some("text_delta")) => Some("text"),
                ("thinking", Some("thinking_delta")) => Some("thinking"),
                ("tool_use", Some("input_json_delta")) => Some("partial_json"),
                _ => None,
            };
            if let Some((field, delta)) = field
                .and_then(|f| event["delta"][f].as_str().map(|d| (f, d)))
                .filter(|(_, d)| !d.is_empty())
            {
                if field == "partial_json" {
                    block.input_json.push_str(delta);
                } else {
                    let mut text = block.value[field].as_str().unwrap_or("").to_owned();
                    text.push_str(delta);
                    block.value[field] = Value::String(text);
                }
                let op = if block.revision == 0 {
                    MutationOperation::Open
                } else {
                    MutationOperation::Append
                };
                out.push(block.emit(mapper, &native, index, parent.clone(), op, Some(delta))?);
            }
        } else {
            out.extend(mapper.opaque(kind, OpaqueReason::UnmappedFields, event)?);
        }
    } else if kind == "content_block_stop" || kind == "message_stop" {
        for (&i, block) in &mut blocks {
            if (kind == "message_stop" || index == Some(i)) && !block.closed {
                block.closed = true;
                if block.kind() == "tool_use" && !block.input_json.is_empty() {
                    if let Ok(input) = serde_json::from_str::<Value>(&block.input_json) {
                        block.value["input"] = input;
                    } else {
                        if let Some(value) = block.value.as_object_mut() {
                            value.remove("input");
                        }
                    }
                }
                if block.revision == 0 {
                    out.push(block.emit(
                        mapper,
                        &native,
                        i,
                        parent.clone(),
                        MutationOperation::Open,
                        None,
                    )?);
                }
                out.push(block.emit(
                    mapper,
                    &native,
                    i,
                    parent.clone(),
                    MutationOperation::Close,
                    None,
                )?);
            }
        }
        if kind == "message_stop" {
            mapper.stream.active.remove(&frame.parent_tool_use_id);
        }
    }
    mapper.stream.blocks.insert(key, blocks);
    Ok(out)
}

pub(super) fn map_assistant(
    mapper: &mut Mapper,
    msg: &AssistantMessage,
) -> DriverResult<Vec<Observation>> {
    if msg
        .uuid
        .as_ref()
        .is_some_and(|id| mapper.stream.snapshots.contains(id))
    {
        return Ok(Vec::new());
    }
    let native = msg.message["id"].as_str().or(msg.uuid.as_deref());
    let Some(native) = native else {
        return mapper.opaque("assistant", OpaqueReason::UnmappedFields, &msg.message);
    };
    let content = msg.message["content"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let parent = parent_id(mapper, msg.parent_tool_use_id.as_deref())?;
    let key = (msg.parent_tool_use_id.clone(), native.to_owned());
    let mut blocks = mapper.stream.blocks.remove(&key).unwrap_or_default();
    let mut out = Vec::new();
    let has_tool = content.iter().any(|v| v["type"] == "tool_use")
        || blocks.values().any(|b| b.kind() == "tool_use");
    for value in content {
        let kind = value["type"].as_str().unwrap_or("");
        if !matches!(kind, "text" | "thinking" | "redacted_thinking" | "tool_use") {
            out.extend(mapper.opaque(kind, OpaqueReason::UnmappedFields, &value)?);
            continue;
        }
        // Claude emits singleton assistant records for successive blocks of the
        // same message.id. Their array offset is not the native stream index.
        let index = blocks
            .iter()
            .find(|(_, b)| {
                b.kind() == kind
                    && if kind == "tool_use" {
                        b.value["id"] == value["id"]
                    } else {
                        !b.reconciled
                    }
            })
            .map(|(&i, _)| i)
            .unwrap_or_else(|| blocks.last_key_value().map_or(0, |(i, _)| i + 1));
        let block = match blocks.entry(index) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(new_block(mapper, value.clone())?)
            }
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
        };
        block.value = value;
        block.closed = true;
        block.reconciled = true;
        block.phase = if has_tool {
            MessagePhase::Commentary
        } else {
            MessagePhase::Final
        };
        let op = if block.revision == 0 {
            MutationOperation::Open
        } else {
            MutationOperation::Replace
        };
        out.push(block.emit(mapper, native, index, parent.clone(), op, None)?);
        out.push(block.emit(
            mapper,
            native,
            index,
            parent.clone(),
            MutationOperation::Close,
            None,
        )?);
    }
    mapper.stream.blocks.insert(key, blocks);
    if let Some(id) = &msg.uuid {
        mapper.stream.snapshots.insert(id.clone());
    }
    Ok(out)
}
