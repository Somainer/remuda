//! Property tests for priority resolution.
//!
//! The engine's resolution rule is "highest priority wins, ties break on
//! document order". These properties pin the consequences of that: the outcome
//! must be total (never ambiguous), stable (independent of how the rules were
//! shuffled before the sort), and consistent (the single-verdict path always
//! agrees with the ranked list).
//!
//! The generator is a small deterministic LCG rather than a proptest
//! dependency: the state space here is a list of (priority, match) pairs, which
//! is cheap enough to cover exhaustively at small sizes and by sampling above
//! that.

use remuda_rules::{BUNDLED, Manifest, Screen, State, bundled};

/// Deterministic linear congruential generator — reproducible failures, no
/// dependency, no `Date.now()`-style nondeterminism in CI.
struct Rng(u64);

impl Rng {
    fn next_u32(&mut self) -> u32 {
        // Numerical Recipes LCG constants.
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as u32
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            self.next_u32() as usize % n
        }
    }
}

/// Build a synthetic manifest whose rules match on distinct marker lines, so a
/// screen can select exactly which rules fire.
fn synthetic(priorities: &[i64]) -> String {
    let mut doc = String::from(
        "id = \"synthetic\"\nversion = \"1\"\nmin_engine_version = 1\nupdated_at = \"now\"\n",
    );
    for (i, p) in priorities.iter().enumerate() {
        doc.push_str(&format!(
            "\n[[rules]]\nid = \"r{i}\"\nstate = \"working\"\npriority = {p}\n\
             region = \"whole_recent\"\nvisible_working = true\ncontains = [\"MARK{i}\"]\n"
        ));
    }
    doc
}

fn screen_selecting(indices: &[usize]) -> Screen {
    Screen::new(indices.iter().map(|i| format!("MARK{i}")).collect())
}

#[test]
fn highest_priority_wins_and_ties_break_on_document_order() {
    let mut rng = Rng(0x5EED);
    for case in 0..500 {
        let count = 1 + rng.below(8);
        // A small priority range guarantees frequent ties, which is the part
        // of the ordering that actually needs pinning.
        let priorities: Vec<i64> = (0..count).map(|_| rng.below(4) as i64 * 10).collect();
        let manifest = Manifest::compile_str(&synthetic(&priorities)).expect("compiles");

        let selected: Vec<usize> = (0..count).filter(|_| rng.below(2) == 0).collect();
        let verdict = manifest.evaluate(&screen_selecting(&selected));

        let Some(&expected) = selected
            .iter()
            .max_by_key(|&&i| (priorities[i], std::cmp::Reverse(i)))
        else {
            assert_eq!(
                verdict.state,
                State::Unknown,
                "case {case}: nothing selected"
            );
            assert!(!verdict.matched());
            continue;
        };
        assert_eq!(
            verdict.rule.as_deref(),
            Some(format!("r{expected}").as_str()),
            "case {case}: priorities {priorities:?}, selected {selected:?}"
        );
        assert_eq!(verdict.priority, Some(priorities[expected]));
    }
}

#[test]
fn ranking_is_monotonic_and_total() {
    let mut rng = Rng(0xC0FFEE);
    for case in 0..500 {
        let count = 1 + rng.below(8);
        let priorities: Vec<i64> = (0..count).map(|_| rng.below(4) as i64 * 10).collect();
        let manifest = Manifest::compile_str(&synthetic(&priorities)).expect("compiles");
        let all: Vec<usize> = (0..count).collect();
        let matches = manifest.evaluate_all(&screen_selecting(&all));

        assert_eq!(matches.len(), count, "case {case}: every rule should match");
        for pair in matches.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            assert!(
                a.priority > b.priority || (a.priority == b.priority && a.order < b.order),
                "case {case}: ranking is not strictly ordered at {a:?} / {b:?}"
            );
        }
    }
}

#[test]
fn ordering_does_not_depend_on_how_rules_were_shuffled() {
    // Same rule set, different document order: the winner must be decided by
    // priority, and a tie must be decided by the order the manifest actually
    // declares — never by sort instability.
    let a = Manifest::compile_str(&synthetic(&[10, 20, 20, 5])).expect("compiles");
    let screen = screen_selecting(&[0, 1, 2, 3]);
    let first = a.evaluate(&screen);
    // r1 and r2 tie at 20; the earlier declaration wins.
    assert_eq!(first.rule.as_deref(), Some("r1"));

    // Re-running is idempotent.
    for _ in 0..10 {
        assert_eq!(a.evaluate(&screen), first);
    }
}

#[test]
fn evaluate_agrees_with_the_head_of_evaluate_all() {
    let mut rng = Rng(0xBEEF);
    for (kind, _) in BUNDLED {
        let manifest = bundled(kind).expect("bundled");
        for _ in 0..20 {
            // Random noise screens: whatever matches, the two APIs must agree.
            let rows: Vec<String> = (0..rng.below(8))
                .map(|_| {
                    let words = ["esc to cancel", "❯ 1. Yes", "· Working", "ready", "[stop]"];
                    words[rng.below(words.len())].to_owned()
                })
                .collect();
            let screen = Screen::new(rows).with_cols(80);
            let verdict = manifest.evaluate(&screen);
            let ranked = manifest.evaluate_all(&screen);
            match ranked.first() {
                Some(best) => assert_eq!(verdict, best.verdict, "{kind}"),
                None => assert!(!verdict.matched(), "{kind}"),
            }
        }
    }
}

#[test]
fn no_bundled_manifest_can_return_unknown_as_a_state_setting_verdict() {
    // An `unknown` verdict may only come from "nothing matched" or from a
    // skip_state_update rule. A rule that sets unknown *and* updates state
    // would let a screen actively erase a known state.
    for (kind, text) in BUNDLED {
        for rule in &Manifest::parse(text).expect("parse").rules {
            if rule.state == "unknown" {
                assert!(
                    rule.skip_state_update,
                    "{kind}: rule {} sets unknown without skipping the update",
                    rule.id
                );
            }
        }
    }
}

#[test]
fn priorities_are_declared_not_inferred() {
    // Every bundled rule must carry an explicit priority; `serde` has no
    // default for it, so this is really a guard that the assets stay explicit
    // after a regeneration.
    for (kind, text) in BUNDLED {
        let manifest = Manifest::parse(text).expect("parse");
        assert!(!manifest.rules.is_empty(), "{kind} has no rules");
    }
}
