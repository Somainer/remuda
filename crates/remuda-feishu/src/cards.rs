//! Card JSON 2.0 templates and structural validation (create-lark-card skill).

use remuda_protocol::{
    DecisionEffect, Interaction, InteractionKind, InteractionRequest, QuestionField, QuestionInput,
};
use serde_json::{Map, Value, json};

use crate::error::Error;

/// Markdown `element_id` reserved for CardKit streaming updates.
pub const PROGRESS_ELEMENT_ID: &str = "progress_md";

/// Official CardKit per-card write cap (Hz). Adapter should stay well below this.
pub const CARDKIT_MAX_HZ: u32 = 10;

/// Per-element cap on agent-controlled markdown in progress/completion cards (F12).
///
/// Feishu's own card limit is ~30 KB for the whole payload; this bounds the one
/// element journal text reaches so a long agent turn cannot push the card over it.
pub const MAX_CARD_TEXT_BYTES: usize = 4096;

/// Truncate agent-controlled text to [`MAX_CARD_TEXT_BYTES`] on a char boundary.
///
/// Cards render this as markdown, so an over-long turn would otherwise either blow
/// the payload limit or push the actual content out of view.
#[must_use]
pub fn clamp_card_text(text: &str) -> String {
    if text.len() <= MAX_CARD_TEXT_BYTES {
        return text.to_string();
    }
    const NOTE: &str = "\n\n… truncated; open the PWA for the full output.";
    let budget = MAX_CARD_TEXT_BYTES - NOTE.len();
    let mut end = budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{NOTE}", &text[..end])
}

/// Reserved CardKit streaming handle. Live HTTP is M2-06; v1 progress cards are static.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardKitStream {
    /// Entity id from `POST /cardkit/v1/cards`, when created.
    pub card_id: Option<String>,
    /// Markdown element to PUT full-text content into.
    pub element_id: String,
    /// Strictly monotonic sequence for element content updates.
    pub sequence: u64,
    /// Must not exceed [`CARDKIT_MAX_HZ`].
    pub max_hz: u32,
}

impl Default for CardKitStream {
    fn default() -> Self {
        Self {
            card_id: None,
            element_id: PROGRESS_ELEMENT_ID.into(),
            sequence: 0,
            max_hz: CARDKIT_MAX_HZ,
        }
    }
}

impl CardKitStream {
    /// Config fragment that would enable streaming_mode. Not applied to static v1 cards.
    #[must_use]
    pub fn streaming_config() -> Value {
        json!({
            "streaming_mode": true,
            "streaming_config": {
                "print_strategy": "fast",
                "print_frequency_ms": { "default": 300 }
            }
        })
    }

    /// Next full-text element update (prefix-stable text types; otherwise redraws).
    pub fn update_element(&mut self, card_id: &str, content: &str) -> CardKitOp {
        self.sequence = self.sequence.saturating_add(1);
        CardKitOp::UpdateElement {
            card_id: card_id.to_string(),
            element_id: self.element_id.clone(),
            content: content.to_string(),
            sequence: self.sequence,
        }
    }

    /// End streaming so the card is forwardable (must not stay in `[生成中...]`).
    #[must_use]
    pub fn disable_streaming(card_id: &str) -> CardKitOp {
        CardKitOp::PatchSettings {
            card_id: card_id.to_string(),
            streaming_mode: false,
        }
    }
}

/// Planned CardKit call. This crate does not perform the HTTP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardKitOp {
    /// `PUT .../elements/:id/content` with full text and monotonic `sequence`.
    UpdateElement {
        /// CardKit entity id.
        card_id: String,
        /// Target `element_id`.
        element_id: String,
        /// Full replacement text.
        content: String,
        /// Monotonic sequence.
        sequence: u64,
    },
    /// `PATCH .../cards/:id/settings`.
    PatchSettings {
        /// CardKit entity id.
        card_id: String,
        /// `config.streaming_mode`.
        streaming_mode: bool,
    },
}

/// Render the card that matches an Interaction kind.
pub fn render_interaction_card(interaction: &Interaction, ticket_id: &str) -> Result<Value, Error> {
    if interaction.carrier == remuda_protocol::InteractionCarrier::NativeTty {
        let (mut card, excerpt) = match &interaction.request {
            InteractionRequest::Approval(request) => {
                let buttons = if interaction.answerable {
                    request
                        .options
                        .iter()
                        .map(|option| {
                            callback_button(&option.label, "default", ticket_id, &option.id)
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                let card = json!({"schema":"2.0", "config":{"compact_width":false,"update_multi":true,"enable_forward":false},
                    "header":{"template":"orange", "title":plain_text(&request.title)},
                    "body":{"elements":[button_row(buttons)]}});
                (card, request.description.clone())
            }
            InteractionRequest::Question(request) if interaction.answerable => (
                render_question_card(ticket_id, &request.title, &request.fields)?,
                request
                    .fields
                    .iter()
                    .filter_map(|field| field.description.as_deref())
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => return Err(Error::UnsupportedInteraction),
        };
        if let Some(elements) = card
            .pointer_mut("/body/elements")
            .and_then(Value::as_array_mut)
        {
            elements.insert(0, json!({"tag":"markdown", "content":format!("Terminal prompt (reply sends keys):\n```text\n{}\n```", excerpt.replace("```", "~~~"))}));
        }
        validate_card(&card)?;
        return Ok(card);
    }
    match &interaction.request {
        InteractionRequest::Approval(req) => render_approval_card(
            ticket_id,
            &req.title,
            &req.description,
            req.options.iter().map(|o| o.effect),
        ),
        InteractionRequest::Question(req) => {
            render_question_card(ticket_id, &req.title, &req.fields)
        }
        InteractionRequest::PlanReview(_) | InteractionRequest::Elicitation(_) => {
            Err(Error::UnsupportedInteraction)
        }
    }
}

/// Approval card: Allow / Deny / Allow once. `behaviors.callback.value = {tid, a}`.
pub fn render_approval_card(
    ticket_id: &str,
    title: &str,
    description: &str,
    effects: impl IntoIterator<Item = DecisionEffect>,
) -> Result<Value, Error> {
    let wanted: Vec<DecisionEffect> = effects.into_iter().collect();
    let mut buttons = Vec::new();
    for (label, a, style, effect) in [
        ("Allow", "allow", "primary", DecisionEffect::AllowSession),
        ("Deny", "deny", "danger", DecisionEffect::Deny),
        ("Allow once", "once", "default", DecisionEffect::AllowOnce),
    ] {
        if wanted.contains(&effect) || wanted.is_empty() {
            buttons.push(callback_button(label, style, ticket_id, a));
        }
    }
    let card = json!({
        "schema": "2.0",
        "config": {
            "compact_width": false,
            "update_multi": true,
            "enable_forward": false
        },
        "header": {
            "template": "orange",
            "title": plain_text(title)
        },
        "body": {
            "elements": [
                {
                    "tag": "markdown",
                    "content": description,
                    "text_align": "left"
                },
                {
                    "tag": "collapsible_panel",
                    "expanded": false,
                    "header": {
                        "title": plain_text("Risk details"),
                        "expanded_title": plain_text("Hide details")
                    },
                    "elements": [
                        {
                            "tag": "markdown",
                            "content": "Allow grants this tool for the session. Allow once grants a single use. Deny rejects the request. Bot paths never bypass permissions.",
                            "text_align": "left"
                        }
                    ]
                },
                button_row(buttons)
            ]
        }
    });
    validate_card(&card)?;
    Ok(card)
}

/// AskUserQuestion: native `form` + a single submit (no per-option callbacks).
pub fn render_question_card(
    ticket_id: &str,
    title: &str,
    fields: &[QuestionField],
) -> Result<Value, Error> {
    let mut elements = Vec::new();
    for field in fields {
        elements.extend(question_field_element(field)?);
    }
    elements.push(json!({
        "tag": "button",
        "type": "primary",
        "text": plain_text("Submit"),
        "name": "submit",
        "action_type": "form_submit",
        "form_action_type": "submit",
        "form_name": "ask",
        "behaviors": [{
            "type": "callback",
            "value": { "tid": ticket_id, "a": "submit" }
        }]
    }));
    let card = json!({
        "schema": "2.0",
        "config": {
            "compact_width": false,
            "update_multi": true,
            "enable_forward": false
        },
        "header": {
            "template": "blue",
            "title": plain_text(title)
        },
        "body": {
            "elements": [{
                "tag": "form",
                "name": "ask",
                "elements": elements
            }]
        }
    });
    validate_card(&card)?;
    Ok(card)
}

/// Static progress card. CardKit streaming is [`CardKitStream`], not this renderer.
///
/// `tool` and `summary` are agent-controlled, so the body is capped and the card is
/// not forwardable (F12) — a markdown link in agent output must not become a
/// clickable, shareable message in the owner's Feishu.
pub fn render_progress_card(
    title: &str,
    tool: &str,
    summary: &str,
    elapsed_secs: u64,
) -> Result<Value, Error> {
    let content = clamp_card_text(&format!(
        "**Tool:** {tool}\n\n{summary}\n\nElapsed: {elapsed_secs}s"
    ));
    let card = json!({
        "schema": "2.0",
        "config": {
            "compact_width": false,
            "update_multi": true,
            "streaming_mode": false,
            "enable_forward": false
        },
        "header": {
            "template": "blue",
            "title": plain_text(title)
        },
        "body": {
            "elements": [{
                "tag": "markdown",
                "element_id": PROGRESS_ELEMENT_ID,
                "content": content,
                "text_align": "left"
            }]
        }
    });
    validate_card(&card)?;
    Ok(card)
}

/// Terminal success / failure card. Long bodies should go out as `--file`.
///
/// `conclusion` is agent-controlled: capped and non-forwardable, as for progress.
pub fn render_completion_card(title: &str, conclusion: &str, ok: bool) -> Result<Value, Error> {
    let template = if ok { "green" } else { "red" };
    let card = json!({
        "schema": "2.0",
        "config": {
            "compact_width": false,
            "update_multi": true,
            "streaming_mode": false,
            "enable_forward": false
        },
        "header": {
            "template": template,
            "title": plain_text(title)
        },
        "body": {
            "elements": [{
                "tag": "markdown",
                "content": clamp_card_text(conclusion),
                "text_align": "left"
            }]
        }
    });
    validate_card(&card)?;
    Ok(card)
}

/// Replacement after a successful first answer (native resolution may still be pending).
pub fn render_recorded_card(title: &str) -> Result<Value, Error> {
    let card = json!({
        "schema": "2.0",
        "config": {
            "compact_width": false,
            "update_multi": true
        },
        "header": {
            "template": "wathet",
            "title": plain_text(title)
        },
        "body": {
            "elements": [{
                "tag": "markdown",
                "content": "Answer recorded. Waiting for the native runtime to confirm.",
                "text_align": "left"
            }]
        }
    });
    validate_card(&card)?;
    Ok(card)
}

/// Replacement after the 10–15 min runtime deadline.
pub fn render_expired_card(title: &str) -> Result<Value, Error> {
    let card = json!({
        "schema": "2.0",
        "config": {
            "compact_width": false,
            "update_multi": true
        },
        "header": {
            "template": "grey",
            "title": plain_text(title)
        },
        "body": {
            "elements": [{
                "tag": "markdown",
                "content": "This request expired. The runtime will deny or cancel it; open the PWA if you still need to act.",
                "text_align": "left"
            }]
        }
    });
    validate_card(&card)?;
    Ok(card)
}

/// Structural Card JSON 2.0 checks used by the create-lark-card skill.
///
/// Rejects `msg_type` wrappers, image URLs, missing `schema: "2.0"`, forms without
/// a single submit, and callback buttons whose `value` lacks `{tid, a}`.
pub fn validate_card(card: &Value) -> Result<(), Error> {
    let obj = card
        .as_object()
        .ok_or_else(|| Error::Card("root must be a JSON object".into()))?;
    if obj.contains_key("msg_type") || obj.contains_key("card") {
        return Err(Error::Card(
            "do not wrap Card JSON in msg_type/interactive envelopes".into(),
        ));
    }
    if obj.get("schema").and_then(Value::as_str) != Some("2.0") {
        return Err(Error::Card("schema must be \"2.0\"".into()));
    }
    let config = obj
        .get("config")
        .and_then(Value::as_object)
        .ok_or_else(|| Error::Card("config.compact_width is required".into()))?;
    if !config.get("compact_width").is_some_and(Value::is_boolean) {
        return Err(Error::Card("config.compact_width must be a boolean".into()));
    }
    if let Some(mode) = config.get("streaming_mode")
        && !mode.is_boolean()
    {
        return Err(Error::Card(
            "config.streaming_mode must be a boolean".into(),
        ));
    }
    if let Some(header) = obj.get("header") {
        let title = header
            .get("title")
            .ok_or_else(|| Error::Card("header.title is required when header is set".into()))?;
        if title.get("tag").and_then(Value::as_str) != Some("plain_text") {
            return Err(Error::Card("header.title.tag must be plain_text".into()));
        }
    }
    let elements = obj
        .get("body")
        .and_then(|b| b.get("elements"))
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Card("body.elements must be an array".into()))?;
    for (i, el) in elements.iter().enumerate() {
        walk_element(el, &format!("body.elements[{i}]"))?;
    }
    Ok(())
}

fn walk_element(el: &Value, path: &str) -> Result<(), Error> {
    let obj = el
        .as_object()
        .ok_or_else(|| Error::Card(format!("{path} must be an object")))?;
    let tag = obj.get("tag").and_then(Value::as_str).unwrap_or("");
    match tag {
        "img" => {
            if obj.contains_key("src") || obj.contains_key("url") || obj.contains_key("img_url") {
                return Err(Error::Card(format!(
                    "{path}: img must use img_key, never a URL"
                )));
            }
            if !obj.contains_key("img_key") {
                return Err(Error::Card(format!("{path}: img requires img_key")));
            }
        }
        "button" => validate_button(obj, path)?,
        "form" => validate_form(obj, path)?,
        "column_set" => {
            let cols = obj
                .get("columns")
                .and_then(Value::as_array)
                .ok_or_else(|| Error::Card(format!("{path}.columns required")))?;
            for (i, col) in cols.iter().enumerate() {
                if let Some(els) = col.get("elements").and_then(Value::as_array) {
                    for (j, child) in els.iter().enumerate() {
                        walk_element(child, &format!("{path}.columns[{i}].elements[{j}]"))?;
                    }
                }
            }
        }
        "collapsible_panel" => {
            if let Some(els) = obj.get("elements").and_then(Value::as_array) {
                for (i, child) in els.iter().enumerate() {
                    walk_element(child, &format!("{path}.elements[{i}]"))?;
                }
            }
        }
        "select_static" | "multi_select_static" | "input" => {
            if obj
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .is_empty()
            {
                return Err(Error::Card(format!("{path}.name is required")));
            }
            if obj.get("behaviors").is_some() {
                return Err(Error::Card(format!(
                    "{path}: form fields must not fire per-change callbacks"
                )));
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_button(obj: &Map<String, Value>, path: &str) -> Result<(), Error> {
    let behaviors = obj.get("behaviors").and_then(Value::as_array);
    let is_submit = obj.get("form_action_type").and_then(Value::as_str) == Some("submit")
        || obj.get("action_type").and_then(Value::as_str) == Some("form_submit");
    if is_submit {
        return Ok(());
    }
    let Some(behaviors) = behaviors else {
        return Err(Error::Card(format!(
            "{path}: non-submit buttons need behaviors"
        )));
    };
    for (i, b) in behaviors.iter().enumerate() {
        if b.get("type").and_then(Value::as_str) != Some("callback") {
            continue;
        }
        let value = b
            .get("value")
            .and_then(Value::as_object)
            .ok_or_else(|| Error::Card(format!("{path}.behaviors[{i}].value must be an object")))?;
        if value
            .get("tid")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
            || value
                .get("a")
                .and_then(Value::as_str)
                .unwrap_or("")
                .is_empty()
        {
            return Err(Error::Card(format!(
                "{path}.behaviors[{i}].value must contain tid and a"
            )));
        }
    }
    Ok(())
}

fn validate_form(obj: &Map<String, Value>, path: &str) -> Result<(), Error> {
    if obj
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .is_empty()
    {
        return Err(Error::Card(format!("{path}.name is required")));
    }
    let elements = obj
        .get("elements")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Card(format!("{path}.elements required")))?;
    let mut submits = 0usize;
    for (i, child) in elements.iter().enumerate() {
        walk_element(child, &format!("{path}.elements[{i}]"))?;
        if child.get("tag").and_then(Value::as_str) == Some("button")
            && (child.get("form_action_type").and_then(Value::as_str) == Some("submit")
                || child.get("action_type").and_then(Value::as_str) == Some("form_submit"))
        {
            submits += 1;
        }
    }
    if submits != 1 {
        return Err(Error::Card(format!(
            "{path} must contain exactly one form_submit button (found {submits})"
        )));
    }
    Ok(())
}

fn question_field_element(field: &QuestionField) -> Result<Vec<Value>, Error> {
    let select = |multi: bool| {
        json!({
            "tag": if multi { "multi_select_static" } else { "select_static" },
            "name": field.id,
            "required": field.required,
            "placeholder": plain_text(&field.title),
            "options": field.options.iter().map(|o| {
                // Feishu select options have no description slot; append it to
                // the label so the card still shows what the TUI shows.
                let text = match o.description.as_deref() {
                    Some(description) if !description.is_empty() => {
                        format!("{} — {}", o.label, description)
                    }
                    _ => o.label.clone(),
                };
                json!({ "text": plain_text(&text), "value": o.id })
            }).collect::<Vec<_>>(),
        })
    };
    let element = match field.input {
        QuestionInput::Text => {
            let input_type = if field.sensitive { "password" } else { "text" };
            json!({
                "tag": "input",
                "name": field.id,
                "required": field.required,
                "input_type": input_type,
                "label": plain_text(&field.title),
                "placeholder": plain_text(field.description.as_deref().unwrap_or("")),
            })
        }
        QuestionInput::SingleSelect => select(false),
        QuestionInput::MultiSelect => select(true),
    };
    let mut elements = vec![element];
    // AskUserQuestion always offers "Type something" in the TUI; mirror it as a
    // second input which the answer encoder prefers over the select value.
    if field.allow_free_text && field.input != QuestionInput::Text {
        elements.push(json!({
            "tag": "input",
            "name": free_text_name(&field.id),
            "required": false,
            "input_type": if field.sensitive { "password" } else { "text" },
            "label": plain_text(&format!("{} · 其他（Type something，可留空）", field.title)),
            "placeholder": plain_text("填写后优先于上方选择"),
        }));
    }
    Ok(elements)
}

/// Form name of a question field's optional free-text companion input.
pub(crate) fn free_text_name(field_id: &str) -> String {
    format!("{field_id}__free")
}

fn callback_button(label: &str, style: &str, tid: &str, a: &str) -> Value {
    json!({
        "tag": "button",
        "type": style,
        "text": plain_text(label),
        "name": a,
        "behaviors": [{
            "type": "callback",
            "value": { "tid": tid, "a": a }
        }]
    })
}

fn button_row(buttons: Vec<Value>) -> Value {
    let columns: Vec<Value> = buttons
        .into_iter()
        .map(|b| {
            json!({
                "tag": "column",
                "width": "weighted",
                "weight": 1,
                "elements": [b]
            })
        })
        .collect();
    json!({
        "tag": "column_set",
        "background_style": "default",
        "flex_mode": "flow",
        "horizontal_spacing": "8px",
        "columns": columns
    })
}

fn plain_text(content: &str) -> Value {
    json!({
        "tag": "plain_text",
        "content": content,
        "text_align": "left"
    })
}

/// Kind of card a pending Interaction should use.
#[must_use]
pub fn card_kind(kind: InteractionKind) -> Option<&'static str> {
    match kind {
        InteractionKind::Approval => Some("approval"),
        InteractionKind::Question => Some("question"),
        InteractionKind::PlanReview | InteractionKind::Elicitation => None,
    }
}
