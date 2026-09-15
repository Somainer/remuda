//! Workflow script parsing: `export const meta` and the `agent(…)` call sites.
//!
//! Verified against the script copies the harness writes to
//! `<session>/workflows/scripts/<name>-<runId>.js` (claude 2.1.221 spike,
//! `docs/design/evidence/workflow-progress-signals-1.md` §1.1/§1.4):
//!
//! ```js
//! export const meta = {
//!   name: 'spike-wf-1',
//!   description: '…',
//!   phases: [{ title: 'Alpha' }, { title: 'Beta' }],
//! }
//! phase('Alpha')
//! await agent('Reply with PONG.', { label: 'beta:one', phase: 'Beta' })
//! ```
//!
//! The parser is deliberately a tiny quote-aware scanner rather than a JS
//! engine: it recognizes *static* shape and nothing else. A prompt that is
//! concatenated, templated with interpolation, or built in a loop is not a
//! literal, and the producer then degrades that member to its agent id with no
//! phase — it never invents a label/phase it could not prove (evidence §1.4,
//! card decision 6).

/// One recognized `agent('<prompt>', { label, phase })` call, in source order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCallSite {
    /// The prompt the agent is spawned with, when the first argument is one
    /// static string literal. This is the join key against the first user
    /// record of `agent-<id>.jsonl`.
    pub prompt: Option<String>,
    /// `label` from the options object literal.
    pub label: Option<String>,
    /// `phase` from the options object literal.
    pub phase: Option<String>,
}

/// The static facts recoverable from a workflow script copy.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkflowScript {
    /// `meta.name`.
    pub name: Option<String>,
    /// `meta.description`.
    pub description: Option<String>,
    /// `meta.phases[].title` in declared order.
    pub phases: Vec<String>,
    /// Every statically visible `agent(…)` call in source order.
    pub calls: Vec<AgentCallSite>,
    /// True when the member total cannot be counted from source alone — a
    /// dynamic prompt or a loop construct around an `agent()` call.
    pub total_unknown: bool,
}

impl WorkflowScript {
    /// Parse a script copy. Absent/unparseable fields stay `None`; this never
    /// fails — an unrecognized script parses to [`WorkflowScript::default`].
    #[must_use]
    pub fn parse(source: &str) -> Self {
        let tokens = scan(source);
        let mut parsed = Self::default();

        if let Some(body) = meta_object(&tokens) {
            parsed.name = object_string_field(&tokens, body, "name");
            parsed.description = object_string_field(&tokens, body, "description");
            parsed.phases = phase_titles(&tokens, body);
        }

        let static_calls = agent_calls(&tokens);
        parsed.total_unknown = static_calls.iter().any(|call| call.prompt.is_none())
            || contains_dynamic_control(&tokens);
        parsed.calls = static_calls;
        parsed
    }

    /// Match the first user prompt of an `agent-<id>.jsonl` transcript to a
    /// call site. `used` records prompts already claimed by an earlier agent
    /// (each call site joins at most one agent; retries share the same key).
    ///
    /// Returns the matched call's label/phase when the prompt equals a static
    /// literal call site. Dynamic prompts never match.
    #[must_use]
    pub fn match_prompt(
        &self,
        prompt: &str,
        used: &std::collections::HashSet<usize>,
    ) -> Option<MatchedCall> {
        let first_unused = self
            .calls
            .iter()
            .enumerate()
            .find(|(index, call)| !used.contains(index) && call.prompt.as_deref() == Some(prompt))
            .map(|(index, _)| index);
        let index = first_unused.or_else(|| {
            // Duplicate literal already claimed (e.g. a retried identical
            // call): fall back to the first call with this prompt.
            self.calls
                .iter()
                .position(|call| call.prompt.as_deref() == Some(prompt))
        })?;
        let call = &self.calls[index];
        Some(MatchedCall {
            index,
            label: call.label.clone(),
            phase: call.phase.clone(),
        })
    }
}

/// A call site joined to a spawned agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedCall {
    /// Index of the matched call in [`WorkflowScript::calls`].
    pub index: usize,
    /// The call's `label`, when present.
    pub label: Option<String>,
    /// The call's `phase`, when present.
    pub phase: Option<String>,
}

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    /// A string literal (single/double quote, or interpolation-free
    /// template); carries the *decoded* value.
    Str(String),
    /// A template literal containing `${ … }` — not static.
    DynamicTemplate,
    /// An identifier or keyword.
    Word(String),
    /// One punctuation character (braces/brackens/parens/comma/colon/…).
    Punct(char),
}

#[derive(Clone, Copy)]
struct Span {
    start: usize,
    end: usize,
}

fn scan(source: &str) -> Vec<(Tok, Span)> {
    let chars: Vec<char> = source.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let start = i;
        let ch = chars[i];
        match ch {
            '/' if i + 1 < chars.len() && chars[i + 1] == '/' => {
                i += 2;
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i = (i + 2).min(chars.len());
            }
            '\'' | '"' => {
                let quote = ch;
                i += 1;
                let mut value = String::new();
                let mut closed = false;
                while i < chars.len() {
                    let c = chars[i];
                    if c == '\\' {
                        if let Some((decoded, next)) = decode_escape(&chars, i) {
                            value.push_str(&decoded);
                            i = next;
                            continue;
                        }
                        value.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    if c == quote {
                        i += 1;
                        closed = true;
                        break;
                    }
                    if c == '\n' {
                        // Unterminated string; bail out of the literal.
                        break;
                    }
                    value.push(c);
                    i += 1;
                }
                out.push((
                    if closed {
                        Tok::Str(value)
                    } else {
                        Tok::DynamicTemplate
                    },
                    Span { start, end: i },
                ));
            }
            '`' => {
                i += 1;
                let mut value = String::new();
                let mut interpolation = false;
                let mut closed = false;
                while i < chars.len() {
                    let c = chars[i];
                    if c == '\\' {
                        if let Some((decoded, next)) = decode_escape(&chars, i) {
                            value.push_str(&decoded);
                            i = next;
                            continue;
                        }
                        value.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    if c == '`' {
                        i += 1;
                        closed = true;
                        break;
                    }
                    if c == '$' && i + 1 < chars.len() && chars[i + 1] == '{' {
                        interpolation = true;
                        i += 2;
                        let mut depth = 1;
                        while i < chars.len() && depth > 0 {
                            match chars[i] {
                                '{' => depth += 1,
                                '}' => depth -= 1,
                                _ => {}
                            }
                            i += 1;
                        }
                        continue;
                    }
                    value.push(c);
                    i += 1;
                }
                let _ = value;
                out.push((
                    if interpolation || !closed {
                        Tok::DynamicTemplate
                    } else {
                        Tok::Str(value)
                    },
                    Span { start, end: i },
                ));
            }
            c if c.is_alphabetic() || c == '_' || c == '$' => {
                i += 1;
                while i < chars.len() {
                    let c = chars[i];
                    if c.is_alphanumeric() || c == '_' || c == '$' {
                        i += 1;
                    } else {
                        break;
                    }
                }
                let word: String = chars[start..i].iter().collect();
                out.push((Tok::Word(word), Span { start, end: i }));
            }
            c if c.is_whitespace() => {
                i += 1;
            }
            c => {
                out.push((
                    Tok::Punct(c),
                    Span {
                        start,
                        end: start + 1,
                    },
                ));
                i += 1;
            }
        }
    }
    out
}

/// Decode one escape at the backslash position. Returns the decoded text and
/// the index just past the escape.
fn decode_escape(chars: &[char], at: usize) -> Option<(String, usize)> {
    let next = *chars.get(at + 1)?;
    let simple = match next {
        'n' => Some('\n'),
        'r' => Some('\r'),
        't' => Some('\t'),
        'b' => Some('\u{0008}'),
        'f' => Some('\u{000C}'),
        'v' => Some('\u{000B}'),
        '0' => Some('\0'),
        '\'' => Some('\''),
        '"' => Some('"'),
        '`' => Some('`'),
        '\\' => Some('\\'),
        '/' => Some('/'),
        '\n' => Some('\u{200B}'), // line continuation: contributes nothing
        _ => None,
    };
    if let Some(decoded) = simple {
        if decoded == '\u{200B}' {
            return Some((String::new(), at + 2));
        }
        return Some((decoded.to_string(), at + 2));
    }
    if next == 'x' {
        let hex: String = chars.get(at + 2..at + 4)?.iter().collect();
        let value = u32::from_str_radix(&hex, 16).ok()?;
        return Some((char::from_u32(value)?.to_string(), at + 4));
    }
    if next == 'u' {
        if chars.get(at + 2) == Some(&'{') {
            let mut end = at + 3;
            while end < chars.len() && chars[end] != '}' {
                end += 1;
            }
            let hex: String = chars.get(at + 3..end)?.iter().collect();
            let value = u32::from_str_radix(&hex, 16).ok()?;
            return Some((char::from_u32(value)?.to_string(), end + 1));
        }
        let hex: String = chars.get(at + 2..at + 6)?.iter().collect();
        let value = u32::from_str_radix(&hex, 16).ok()?;
        return Some((char::from_u32(value)?.to_string(), at + 6));
    }
    // Unknown escape: JS keeps the literal char.
    Some((next.to_string(), at + 2))
}

// ---------------------------------------------------------------------------
// Token-level matching
// ---------------------------------------------------------------------------

/// Find the `export const meta = { … }` object's token span (inside the full
/// token list), brace-matched and string-safe.
fn meta_object(tokens: &[(Tok, Span)]) -> Option<Span> {
    let words: Vec<&str> = tokens
        .iter()
        .filter_map(|(tok, _)| match tok {
            Tok::Word(w) => Some(w.as_str()),
            _ => None,
        })
        .collect();
    let _ = words;
    for index in 0..tokens.len() {
        if let Tok::Word(word) = &tokens[index].0
            && word == "meta"
        {
            let export_const = index >= 2
                && matches!(&tokens[index - 2].0, Tok::Word(w) if w == "export")
                && matches!(&tokens[index - 1].0, Tok::Word(w) if w == "const");
            let after = tokens.get(index + 1).map(|(tok, _)| tok);
            if export_const
                && matches!(after, Some(Tok::Punct('=')))
                && let Some(open) = tokens.get(index + 2..).and_then(|rest| {
                    rest.iter()
                        .position(|(tok, _)| matches!(tok, Tok::Punct('{')))
                })
            {
                let brace = index + 2 + open;
                if let Some(close) = match_brace(tokens, brace, '{', '}') {
                    return Some(Span {
                        start: brace,
                        end: close,
                    });
                }
            }
        }
    }
    None
}

/// Index of the matching close for an open `(`/`{`/`[` at `open`.
fn match_brace(tokens: &[(Tok, Span)], open: usize, opener: char, closer: char) -> Option<usize> {
    let mut depth = 0;
    for (index, (tok, _)) in tokens.iter().enumerate().skip(open) {
        match tok {
            Tok::Punct(c) if *c == opener => depth += 1,
            Tok::Punct(c) if *c == closer => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

/// Read `key: '<literal>'` directly inside an object span. The literal must be
/// the whole value: a string immediately followed by `,` or `}`. A
/// concatenation (`label: 'a' + who`) is not static.
fn object_string_field(tokens: &[(Tok, Span)], object: Span, key: &str) -> Option<String> {
    let field = object_key(tokens, object, key)?;
    let value_at = field + 2;
    let value = tokens.get(value_at).map(|(tok, _)| tok)?;
    let followed_by_terminator = matches!(
        tokens.get(value_at + 1).map(|(tok, _)| tok),
        Some(Tok::Punct(',')) | Some(Tok::Punct('}')) | None
    );
    match (value, followed_by_terminator) {
        (Tok::Str(value), true) => Some(value.clone()),
        _ => None,
    }
}

/// Position of a `key` token directly inside an object (not a nested one).
fn object_key(tokens: &[(Tok, Span)], object: Span, key: &str) -> Option<usize> {
    let mut depth = 0;
    for index in (object.start + 1)..object.end {
        let (tok, _) = &tokens[index];
        match tok {
            Tok::Punct('{') | Tok::Punct('[') | Tok::Punct('(') => depth += 1,
            Tok::Punct('}') | Tok::Punct(']') | Tok::Punct(')') => depth -= 1,
            Tok::Word(word) if depth == 0 && word == key => {
                if matches!(tokens.get(index + 1).map(|(t, _)| t), Some(Tok::Punct(':'))) {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

/// Read `phases: [{ title: '…' }]` inside the meta object.
fn phase_titles(tokens: &[(Tok, Span)], meta: Span) -> Vec<String> {
    let Some(field) = object_key(tokens, meta, "phases") else {
        return Vec::new();
    };
    let Some(open) = tokens
        .get(field + 2..)
        .and_then(|rest| {
            rest.iter()
                .position(|(tok, _)| matches!(tok, Tok::Punct('[')))
        })
        .map(|offset| field + 2 + offset)
    else {
        return Vec::new();
    };
    let Some(close) = match_brace(tokens, open, '[', ']') else {
        return Vec::new();
    };
    let mut titles = Vec::new();
    let mut index = open + 1;
    while index < close {
        if matches!(&tokens[index].0, Tok::Punct('{'))
            && let Some(object_close) = match_brace(tokens, index, '{', '}')
        {
            let object = Span {
                start: index,
                end: object_close,
            };
            if let Some(title) = object_string_field(tokens, object, "title") {
                titles.push(title);
            }
            index = object_close + 1;
            continue;
        }
        index += 1;
    }
    titles
}

/// Every `agent(<prompt>[, { label, phase }])` call in source order.
fn agent_calls(tokens: &[(Tok, Span)]) -> Vec<AgentCallSite> {
    let mut calls = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let is_agent = matches!(&tokens[index].0, Tok::Word(w) if w == "agent")
            && matches!(
                tokens.get(index + 1).map(|(tok, _)| tok),
                Some(Tok::Punct('('))
            );
        if !is_agent {
            index += 1;
            continue;
        }
        let open = index + 1;
        let Some(close) = match_brace(tokens, open, '(', ')') else {
            index += 1;
            continue;
        };
        // The prompt is static only when the whole first argument is one
        // string literal: `agent('x', …)` / `agent('x')`. A concatenation or
        // template interpolation yields `None`.
        let prompt = match (
            tokens.get(open + 1).map(|(tok, _)| tok),
            tokens.get(open + 2).map(|(tok, _)| tok),
        ) {
            (Some(Tok::Str(value)), Some(Tok::Punct(',')) | Some(Tok::Punct(')'))) => {
                Some(value.clone())
            }
            _ => None,
        };
        let (mut label, mut phase) = (None, None);
        // Options object: the first `{` at depth 1 inside the call.
        for inner in (open + 1)..close {
            if matches!(&tokens[inner].0, Tok::Punct('{'))
                && let Some(object_close) = match_brace(tokens, inner, '{', '}')
                && object_close <= close
            {
                let object = Span {
                    start: inner,
                    end: object_close,
                };
                label = object_string_field(tokens, object, "label");
                phase = object_string_field(tokens, object, "phase");
                break;
            }
        }
        calls.push(AgentCallSite {
            prompt,
            label,
            phase,
        });
        index = close + 1;
    }
    calls
}

/// Loops around agent calls mean the static call count is not the member
/// total. Keywords seen in code (strings and comments are already stripped).
fn contains_dynamic_control(tokens: &[(Tok, Span)]) -> bool {
    tokens.iter().any(|(tok, _)| {
        matches!(
            tok,
            Tok::Word(word) if word == "for" || word == "while"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPIKE: &str = include_str!("../../tests/fixtures/workflow/scripts/spike-wf-1.js");

    #[test]
    fn parses_the_recorded_spike_script() {
        let script = WorkflowScript::parse(SPIKE);
        assert_eq!(script.name.as_deref(), Some("spike-wf-1"));
        assert_eq!(
            script.description.as_deref(),
            Some("r-ux-w two-phase signals spike")
        );
        assert_eq!(script.phases, vec!["Alpha", "Beta"]);
        assert_eq!(script.calls.len(), 4);
        assert!(!script.total_unknown);
        let first = &script.calls[0];
        assert_eq!(
            first.prompt.as_deref(),
            Some(
                "Run this exact shell command and nothing else: echo hello-alpha-1; sleep 2; echo done-alpha-1"
            )
        );
        assert_eq!(first.label.as_deref(), Some("alpha:one"));
        assert_eq!(first.phase.as_deref(), Some("Alpha"));
        let pong = script
            .calls
            .iter()
            .find(|call| call.label.as_deref() == Some("beta:one"))
            .unwrap();
        assert_eq!(
            pong.prompt.as_deref(),
            Some("Reply with exactly the single word PONG and nothing else.")
        );
    }

    #[test]
    fn matches_an_agent_prompt_to_its_call_site() {
        let script = WorkflowScript::parse(SPIKE);
        let mut used = std::collections::HashSet::new();
        let matched = script
            .match_prompt(
                "Reply with exactly the single word PONG and nothing else.",
                &used,
            )
            .expect("literal prompt matches");
        assert_eq!(matched.label.as_deref(), Some("beta:one"));
        assert_eq!(matched.phase.as_deref(), Some("Beta"));
        used.insert(matched.index);
        // First alpha agent matches in source order.
        let prompt = "Run this exact shell command and nothing else: echo hello-alpha-1; sleep 2; echo done-alpha-1";
        let one = script.match_prompt(prompt, &used).unwrap();
        assert_eq!(one.label.as_deref(), Some("alpha:one"));
        used.insert(one.index);
        let two = script.match_prompt(
            "Run this exact shell command and nothing else: echo hello-alpha-2; sleep 3; echo done-alpha-2",
            &used,
        )
        .unwrap();
        assert_eq!(two.label.as_deref(), Some("alpha:two"));
    }

    #[test]
    fn an_unmatched_prompt_returns_nothing() {
        let script = WorkflowScript::parse(SPIKE);
        let matched = script.match_prompt(
            "do something completely different",
            &std::collections::HashSet::new(),
        );
        assert_eq!(matched, None);
    }

    #[test]
    fn dynamic_prompts_degrade_without_a_phase() {
        let source = r#"
export const meta = {
  name: 'dyn',
  description: 'dynamic prompts',
  phases: [{ title: 'Only' }],
}
const items = ['a', 'b']
const r = await parallel(items.map((item) =>
  () => agent(`Review ${item}`, { label: 'review:' + item, phase: 'Only' })))
return r
"#;
        let script = WorkflowScript::parse(source);
        assert_eq!(script.phases, vec!["Only"]);
        assert_eq!(script.calls.len(), 1);
        assert_eq!(
            script.calls[0].prompt, None,
            "template interpolation is not a literal"
        );
        assert!(
            script.total_unknown,
            "a .map call site makes the total unknown"
        );
        assert_eq!(
            script.match_prompt("Review a", &std::collections::HashSet::new()),
            None,
            "a dynamic call site never joins"
        );
    }

    #[test]
    fn concatenated_prompts_and_labels_are_not_static() {
        let source = r#"
export const meta = { name: 'c', description: 'd', phases: [{ title: 'P' }] }
const who = 'world'
await agent('hello ' + who, { label: 'lbl-' + who, phase: 'P' })
"#;
        let script = WorkflowScript::parse(source);
        assert_eq!(script.calls.len(), 1);
        assert_eq!(script.calls[0].prompt, None);
        // The literal options fields are not falsely read as joined.
        assert_eq!(script.calls[0].label, None);
        assert_eq!(script.calls[0].phase.as_deref(), Some("P"));
    }

    #[test]
    fn a_static_template_literal_is_a_literal() {
        let source = r#"
export const meta = { name: 't', description: 'd' }
await agent(`plain template`, {})
"#;
        let script = WorkflowScript::parse(source);
        assert_eq!(script.calls[0].prompt.as_deref(), Some("plain template"));
        assert!(!script.total_unknown);
    }

    #[test]
    fn loops_force_an_unknown_total_even_with_literal_prompts() {
        let source = r#"
export const meta = { name: 'loop', description: 'd', phases: [{ title: 'P' }] }
for (const n of [1, 2, 3]) {
  await agent('do the same thing', { label: 'x', phase: 'P' })
}
"#;
        let script = WorkflowScript::parse(source);
        assert_eq!(script.calls.len(), 1);
        assert!(script.calls[0].prompt.is_some());
        assert!(script.total_unknown);
    }

    #[test]
    fn escapes_are_decoded_like_the_harness_writes_them() {
        let source = r#"
export const meta = { name: 'e', description: 'd' }
await agent('line one\nline two\tend', {})
"#;
        let script = WorkflowScript::parse(source);
        assert_eq!(
            script.calls[0].prompt.as_deref(),
            Some("line one\nline two\tend")
        );
    }

    #[test]
    fn braces_inside_strings_do_not_end_the_meta_object() {
        let source = r#"
export const meta = {
  name: 'b',
  description: 'a } brace { inside',
  phases: [{ title: 'Alpha' }, { title: 'Beta' }],
}
await agent('x', { label: 'l', phase: 'Beta' })
"#;
        let script = WorkflowScript::parse(source);
        assert_eq!(script.name.as_deref(), Some("b"));
        assert_eq!(script.description.as_deref(), Some("a } brace { inside"));
        assert_eq!(script.phases, vec!["Alpha", "Beta"]);
        assert_eq!(script.calls[0].phase.as_deref(), Some("Beta"));
    }

    #[test]
    fn garbage_parses_to_default() {
        let script = WorkflowScript::parse("not a workflow at all");
        assert_eq!(script, WorkflowScript::default());
    }

    #[test]
    fn duplicate_literal_claims_each_call_site_once() {
        let source = r#"
export const meta = { name: 'dup', description: 'd', phases: [{ title: 'P' }] }
await parallel([
  () => agent('same prompt', { label: 'one', phase: 'P' }),
  () => agent('same prompt', { label: 'two', phase: 'P' }),
])
"#;
        let script = WorkflowScript::parse(source);
        let mut used = std::collections::HashSet::new();
        let first = script.match_prompt("same prompt", &used).unwrap();
        assert_eq!(first.label.as_deref(), Some("one"));
        used.insert(first.index);
        let second = script.match_prompt("same prompt", &used).unwrap();
        assert_eq!(second.label.as_deref(), Some("two"));
    }
}
