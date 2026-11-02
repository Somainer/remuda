# Review rubric

How an independent reviewer judges a delivery. The review request fixes the
output shape; this document fixes the judgment behind it. Read the diff before
you read the report.

## Review the code, not the report

The worker's completion line, its test names and its proof files are claims.
- List the delivery's commits (`git log --oneline <base>..<tip>`) and its
  footprint (`git diff --stat <base>..<tip>`). A diff wider than the files the
  task named is itself a reason to look closely.
- Read the diff hunk by hunk. For every acceptance criterion, find the code or
  test that implements it and cite `file:line`.
- Tests are evidence only when they exercise the production path. A test that
  drives a stub proves the stub and the test, not the delivery; compare the
  stub with the component it replaces before calling the criterion MET.
- The review is read-only. Do not build, run tests, start services or write
  files. If a claim can only be settled by execution, see "Claims you cannot
  verify".

## What a finding must contain

Every finding states all four parts, or it is not a finding:

- **file:line** in the delivery, not a range that merely looks suspicious;
- **trigger**: the concrete input, command, identity, ordering or timing that
  reaches the defect;
- **consequence**: what then goes wrong, observably — wrong output, a lost
  obligation, a stale row, a cross-owner write, a crash;
- **fix**: a specific change, not "handle this".

Trace the whole call path yourself before writing it down: the arguments the
caller actually sends, what any wrapper does to the response after it arrives,
and whether the state needed for the failure is reachable at all. An apparent
hole in a helper that no production caller reaches is not a defect.

Findings are per defect, not per wording. Do not inflate one problem by
restating it under several titles, and do not report the same issue as both a
criterion failure and a separate defect.

## Severity

- **high** — a stated acceptance criterion or invariant fails, or reachable
  input causes wrong output, data loss, a lost or duplicated obligation, a
  cross-identity or authority write, or a security breach. Landing it ships
  the defect.
- **medium** — a real defect with a concrete trigger, but bounded: one path,
  recoverable, or not user-visible in normal operation; or a stated ruling
  violated without breaking an acceptance criterion.
- **low** — real but minor: a test that proves the wrong thing, a comment that
  states something false, an edge the product already tolerates.

Severity follows consequence and reachability, not how hard the code was to
read or how confident you feel. "Probably fine on the common path" does not
lower it; "I could not construct a failure" does.

## Ownership and durable state

Most serious defects in a stateful client are state that outlives its owner.
For every map, queue, timer or obligation the delivery adds or changes:

- **Who owns it?** Write down the identity key — epoch, session, instance,
  device, journal generation. Check that the whole key is compared, not a
  stable prefix of it. Ask what happens when a *different* identity, session or
  page adopts the same state without that key changing.
- **Is retirement durable?** A "settled", "done" or "retired" transition must
  be backed by an awaited write whose failure keeps the obligation. A
  fire-and-forget patch whose rejection is unhandled loses recovery.
- **Does a read write shared state?** A read path that inserts into history, a
  dedupe set or another consumer's input changes behaviour beyond the read.
  When a ruling forbids such writes, test each write against the rule
  literally; a comment calling the write harmless is not an exemption.
- **Can the pass make progress?** Bounded retries need a bound that shrinks, a
  cursor that advances only over inspected data, and a terminal state. A pass
  that can repeat the same window forever, or advance past data it never saw,
  is a finding.

## What not to report

The landing gate already checks these. Do not spend findings on them, and do
not reject on them:

- formatting and lint (`cargo fmt`, `cargo check`, `cargo clippy -D warnings`);
- the secret scan, tunnel scan and host-wide-kill audit;
- the documentation contract (links, anchors, command references);
- the Rust and web test suites, builds, typecheck and browser end-to-end runs.

These are gate results, not review findings. Mention one only when the brief
asked you to verify that evidence, and then as evidence or CANNOT TELL. A
worker's claimed check line is not proof that the check passed.

Also out of scope:

- **style and preference** — naming, comment wording, file layout, a different
  but working design. Report a comment only when it asserts something false
  that would mislead the next change, and then at low.
- **speculation** — "might race", "could be slow", "may be uncovered", with no
  sequence and no consequence.
- **pre-existing issues** in code the delivery did not change, unless the
  delivery makes them reachable or worse; name the change that does.
- **scope you invent** — a requirement the brief never stated is not a missed
  criterion.

## Claims you cannot verify

A read-only review cannot run code. When a claim depends on execution, write
**CANNOT TELL**, state what you read and what would settle it. CANNOT TELL is
not a defect and must not drive the verdict. Do not infer MET from a
plausible test name, and do not infer NOT MET from the absence of a report.

## Verdict

- **REJECT** when a verified high exists, or when a verified medium leaves a
  stated acceptance criterion NOT MET.
- **ACCEPT** otherwise: every criterion is MET or CANNOT TELL, and any
  remaining findings are medium or low and outside the acceptance criteria.

The verdict follows the verified findings only. A medium or low finding that
does not break an acceptance criterion is recorded and does not block.
CANNOT TELL never becomes REJECT. Put the verdict on the first line as
`VERDICT: ACCEPT` or `VERDICT: REJECT`.
