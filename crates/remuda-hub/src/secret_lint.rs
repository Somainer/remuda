//! The secret lint: one set of candidate shapes for the gate scan, the memory
//! write lint and `remuda brief lint` (main-agent.md §12.5).
//!
//! The shapes are `scripts/ci/secret-shapes.json`, embedded here at compile
//! time and loaded by `scripts/ci/secret-scan.py` at run time, so there is no
//! second copy to drift. The patterns use lookbehind, which Python `re` and
//! `fancy-regex` both accept. The redaction rules (placeholders, dummies,
//! masked tails, the minimum payload length) are ported from the scanner line
//! for line; `scripts/ci/secret-shapes-corpus.txt` is tested by both sides,
//! so a shape one side flags and the other passes fails a suite.
//!
//! This is a lint on credentials only. It never looks at what a text says
//! (D-060 R2 rejects content-policy lints), and it does not apply the
//! repository's private-token denylist: that list keeps internal host and
//! user names out of a public repository, and the same names are ordinary
//! facts in the owner's own memory.

use fancy_regex::Regex;
use serde::Deserialize;
use std::sync::OnceLock;

/// The shared shapes file.
const SHAPES_JSON: &str = include_str!("../../../scripts/ci/secret-shapes.json");

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShapesFile {
    rules: Vec<RuleFile>,
    dummy_part: String,
    placeholder: String,
    mask: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuleFile {
    name: String,
    pattern: String,
    min_payload: usize,
}

struct Rule {
    name: String,
    pattern: Regex,
    min_payload: usize,
}

struct Shapes {
    rules: Vec<Rule>,
    dummy_part: Regex,
    placeholder: Regex,
    mask: Vec<char>,
}

fn shapes() -> &'static Shapes {
    static SHAPES: OnceLock<Shapes> = OnceLock::new();
    SHAPES.get_or_init(|| {
        let file: ShapesFile =
            serde_json::from_str(SHAPES_JSON).expect("scripts/ci/secret-shapes.json parses");
        let compile = |pattern: &str| {
            Regex::new(pattern).unwrap_or_else(|error| {
                panic!("secret-shapes.json pattern {pattern:?} does not compile: {error}")
            })
        };
        Shapes {
            rules: file
                .rules
                .into_iter()
                .map(|rule| Rule {
                    pattern: compile(&rule.pattern),
                    name: rule.name,
                    min_payload: rule.min_payload,
                })
                .collect(),
            dummy_part: compile(&file.dummy_part),
            placeholder: compile(&file.placeholder),
            mask: file.mask.chars().collect(),
        }
    })
}

/// Python's `str.isspace`: Unicode whitespace plus the four separators
/// (U+001C..U+001F) Rust does not count.
fn py_space(ch: char) -> bool {
    ch.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&ch)
}

/// Python's `str.splitlines` boundaries (`\r\n` counts once).
fn lines(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        match rest.char_indices().find(|(_, ch)| {
            matches!(
                ch,
                '\n' | '\r'
                    | '\u{0b}'
                    | '\u{0c}'
                    | '\u{1c}'
                    | '\u{1d}'
                    | '\u{1e}'
                    | '\u{85}'
                    | '\u{2028}'
                    | '\u{2029}'
            )
        }) {
            Some((at, ch)) => {
                let line = &rest[..at];
                let mut next = at + ch.len_utf8();
                if ch == '\r' && rest[next..].starts_with('\n') {
                    next += 1;
                }
                rest = &rest[next..];
                Some(line)
            }
            None => {
                let line = rest;
                rest = "";
                Some(line)
            }
        }
    })
}

/// `payload.strip().strip("\"'")`.
fn body_of(payload: &str) -> &str {
    payload.trim_matches(py_space).trim_matches(['"', '\''])
}

fn matches(regex: &Regex, text: &str) -> bool {
    // A pattern that cannot finish counts as a match: the lint fails closed.
    regex.is_match(text).unwrap_or(true)
}

/// secret-scan.py `is_redacted`.
fn is_redacted(shapes: &Shapes, whole: &str, payload: &str) -> bool {
    let body = body_of(payload);
    if body.is_empty() || matches(&shapes.placeholder, body) || matches(&shapes.dummy_part, body) {
        return true;
    }
    let masked = |ch: char| shapes.mask.contains(&ch);
    let visible: String = whole.chars().take(4).collect();
    let rest: Vec<char> = whole.chars().skip(4).collect();
    if !rest.is_empty() && rest.iter().all(|&ch| masked(ch) || py_space(ch)) {
        return true;
    }
    if body.chars().count() <= 4 && body.chars().all(|ch| masked(ch) || ch.is_alphanumeric()) {
        // four-or-fewer chars: truncated/redacted unless it looks random-long
        if body.chars().any(masked) || visible.ends_with(['*', 'x', 'X']) {
            return true;
        }
    }
    false
}

/// Push the rule name (its literal needle, never the matched text) of every
/// flagged candidate on one line.
fn line_hits<'s>(shapes: &'s Shapes, line: &str, out: &mut Vec<&'s str>) {
    for rule in &shapes.rules {
        for captures in rule.pattern.captures_iter(line) {
            let Ok(captures) = captures else {
                // Fail closed, as in `matches`.
                out.push(&rule.name);
                continue;
            };
            let whole = captures.get(0).map_or("", |m| m.as_str());
            let payload = captures.get(1).map_or(whole, |m| m.as_str());
            if is_redacted(shapes, whole, payload) {
                continue;
            }
            if body_of(payload).chars().count() >= rule.min_payload {
                out.push(&rule.name);
            }
        }
    }
}

/// The rule names of every secret candidate in `text`, in scan order (line
/// by line, then rule order), one per match. Empty when the text is clean.
/// The result never contains any of the matched text.
#[must_use]
pub fn secret_rules(text: &str) -> Vec<&'static str> {
    let shapes = shapes();
    let mut out = Vec::new();
    for line in lines(text) {
        line_hits(shapes, line, &mut out);
    }
    out
}

/// The first rule `text` trips, if any.
#[must_use]
pub fn first_secret(text: &str) -> Option<&'static str> {
    secret_rules(text).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CORPUS: &str = include_str!("../../../scripts/ci/secret-shapes-corpus.txt");

    /// (expected flagged, text) per corpus line, `<<NAME>>` replaced by the
    /// rule's needle, exactly as `scripts/tests/test_secret_shapes.py` does.
    fn corpus() -> Vec<(bool, String)> {
        let names: Vec<&str> = shapes().rules.iter().map(|r| r.name.as_str()).collect();
        let mut out = Vec::new();
        for raw in CORPUS.lines() {
            if raw.trim().is_empty() || raw.starts_with('#') {
                continue;
            }
            let (sign, text) = raw.split_at(2);
            let flagged = match sign {
                "+ " => true,
                "- " => false,
                other => panic!("corpus line starts with {other:?}: {raw}"),
            };
            let mut line = String::new();
            let mut rest = text;
            while let Some(open) = rest.find("<<") {
                line.push_str(&rest[..open]);
                let close = rest[open..].find(">>").expect("closed needle") + open;
                let name = &rest[open + 2..close];
                assert!(names.contains(&name), "unknown rule {name:?} in corpus");
                line.push_str(name);
                rest = &rest[close + 2..];
            }
            line.push_str(rest);
            out.push((flagged, line));
        }
        out
    }

    #[test]
    fn the_corpus_verdicts_match_the_scanner() {
        let cases = corpus();
        assert!(cases.len() > 20);
        for (flagged, text) in cases {
            assert_eq!(!secret_rules(&text).is_empty(), flagged, "{text}");
        }
    }

    #[test]
    fn every_rule_has_a_flagged_corpus_line() {
        let mut hit: Vec<&str> = corpus()
            .iter()
            .filter(|(flagged, _)| *flagged)
            .flat_map(|(_, text)| secret_rules(text))
            .collect();
        hit.sort_unstable();
        hit.dedup();
        let mut names: Vec<&str> = shapes().rules.iter().map(|r| r.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(hit, names);
    }

    #[test]
    fn lines_split_like_python() {
        let split: Vec<&str> = lines("a\r\nb\rc\nd\u{2028}e\u{85}f").collect();
        assert_eq!(split, ["a", "b", "c", "d", "e", "f"]);
        assert_eq!(lines("one\n").collect::<Vec<_>>(), ["one"]);
        assert_eq!(lines("").count(), 0);
    }

    #[test]
    fn a_secret_on_a_later_line_is_found_by_rule_name_only() {
        let names: Vec<&str> = shapes().rules.iter().map(|r| r.name.as_str()).collect();
        let (_, secret) = corpus()
            .into_iter()
            .find(|(flagged, _)| *flagged)
            .expect("a flagged line");
        let text = format!("first line\n\n{secret}\nlast");
        let rules = secret_rules(&text);
        assert!(!rules.is_empty());
        assert!(rules.iter().all(|rule| names.contains(rule)), "{rules:?}");
        assert_eq!(first_secret("plain text\nmore plain text"), None);
    }

    /// Generated near-misses and hits, judged by both implementations: the
    /// scanner itself runs (python3, as the gate does) over the same lines.
    #[test]
    fn generated_lines_get_the_scanner_verdict() {
        use std::io::Write;
        let needles: Vec<String> = shapes()
            .rules
            .iter()
            .map(|rule| {
                if rule.name.contains(' ') && rule.name.starts_with("BEGIN") {
                    format!("-----{}-----", rule.name)
                } else {
                    rule.name.clone()
                }
            })
            .collect();
        let alphabet: Vec<char> = "abcXYZ019_-=:'\" ./*x<>${}…•()[]\tQ".chars().collect();
        let before = ["", " ", "a", "_", "(", "=", "my ", "\"", "x"];
        let between = ["", " = ", ": ", "=", "'", "\"", " "];
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = |bound: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % bound as u64) as usize
        };
        let mut lines_in = Vec::new();
        for _ in 0..4000 {
            let mut line = String::from(before[next(before.len())]);
            line.push_str(&needles[next(needles.len())]);
            line.push_str(between[next(between.len())]);
            for _ in 0..next(34) {
                line.push(alphabet[next(alphabet.len())]);
            }
            lines_in.push(line);
        }
        let script = r#"
import importlib.util, json, sys
spec = importlib.util.spec_from_file_location("scan", sys.argv[1])
scan = importlib.util.module_from_spec(spec)
spec.loader.exec_module(scan)
for line in json.load(sys.stdin):
    print(json.dumps([name for name, _ in scan.secret_hits(line)]))
"#;
        let scanner = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/ci/secret-scan.py"
        );
        let mut child = std::process::Command::new("python3")
            .args(["-B", "-c", script, scanner])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("python3 runs the gate scanner");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(serde_json::to_string(&lines_in).unwrap().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let verdicts: Vec<Vec<String>> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(verdicts.len(), lines_in.len());
        let mut flagged = 0;
        for (line, python) in lines_in.iter().zip(verdicts) {
            assert_eq!(secret_rules(line), python, "{line:?}");
            flagged += usize::from(!python.is_empty());
        }
        // Both outcomes are exercised, not only one.
        assert!(flagged > 200 && flagged < 3800, "{flagged} of 4000 flagged");
    }
}
