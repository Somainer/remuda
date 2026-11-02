# Review rubric

How an independent reviewer judges a delivery. The review request fixes the
output shape; this document fixes the judgment behind it. Read the diff before
you read the report.

## Review the code, not the report

The worker's completion line, its test names and its proof files are claims.
- List the delivery's commits (`git log --oneline <base>..<tip>`) and its
  footprint (`git diff --stat <base>..<tip>`). A diff wider than the files the
  task named is itself a reason to look closely.
- Read the diff hunk by hunk. A criterion is MET only when you traced the
  exact path it names, including failure and a different identity, session or
  timing; cite `file:line`. A helper that exists is not a criterion that works.
- Tests are evidence only when they exercise the production path. A test that
  drives a stub proves the stub; compare the stub with the component it
  replaces. Check the unit, encoding and serialized field behind any window or
  cap it reimplements — if the difference changes which input the cap cuts,
  the regression is not testing production.
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

Trace the whole call path: the arguments actually sent, what any wrapper does
to the response, and whether the state the failure needs is reachable. An
apparent hole in a helper no production caller reaches is not a defect. Report
each defect once, not also as a separate criterion failure.

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

Severity follows consequence and reachability, not difficulty or confidence.
"Probably fine on the common path" does not lower it.

## Common traps in stateful code

For every map, queue, timer or obligation the delivery adds or changes, write
down its identity key — epoch, session, instance, device, generation — and
check the whole key, not a stable prefix. Then check these shapes:

1. **A guard on the wrong path.** A retirement or logout path may clear state
   while a successful re-auth as a different identity never runs it. Enumerate
   every identity-changing path — logout, failed re-auth, same-device and
   different-device re-auth, reconnect, page takeover — and verify the fence on
   the path the criterion names. An epoch bumped elsewhere proves nothing.
2. **"Not recorded" in a comment, but recorded through a shared structure.**
   A read path appends to shared history, cache or dedupe state and claims the
   ids stay out. Follow the structure to every consumer; the write reaches
   them without an explicit registration call. A "harmless" comment is not an
   exemption.
3. **Final in memory, not on disk.** `void write(...)`, a missing `await`, or a
   logging-only catch makes a "done" or "settled" transition provisional. If
   the write fails, the obligation must remain and be retried; a success-only
   test does not cover this.
4. **A double measures a different quantity.** A stub counting characters,
   code units or JSON text where production counts UTF-8 payload bytes (or
   another field) does not reproduce a cap. Find the input where they disagree
   before dismissing it.
5. **A cursor claims what it did not read.** Compute the range actually sent —
   both bounds — and its size against the cap before alleging a slice lost
   rows. A slice only loses rows when the page is wider than it.

Bounded retries also need a shrinking bound, a cursor that advances only over
inspected data, and a terminal state; a pass that can repeat the same window
forever is a finding.

## What not to report

The landing gate already checks these. Do not spend findings on them, and do
not reject on them:

- formatting and lint (`cargo fmt`, `cargo check`, `cargo clippy -D warnings`);
- the secret, tunnel and host-wide-kill scans;
- the documentation contract;
- the Rust and web suites, builds, typecheck and browser end-to-end runs.

These are gate results. Mention one only when the brief asked you to verify
that evidence, and then as evidence or CANNOT TELL. A worker's claimed check
line is not proof that the check passed.

Also out of scope:

- **style and preference** — naming, comment wording, layout, a different but
  working design. Report a false or misleading comment at low.
- **speculation** — "might race", "could be slow", with no sequence or
  consequence.
- **pre-existing issues** in unchanged code, unless the delivery makes them
  reachable or worse; name the change that does.
- **invented scope** — a requirement the brief never stated is not a missed
  criterion.

## Claims you cannot verify

A read-only review cannot run code. When a claim depends on execution, write
**CANNOT TELL**, state what you read and what would settle it. CANNOT TELL is
not a defect and must not drive the verdict. Do not infer MET from a
plausible test name, and do not infer NOT MET from the absence of a report.

## Verdict

Before the verdict, name the line that enforces each stated invariant and the
alternate path that could bypass it. A happy-path check, or the code's own
comment about itself, is not a check.

- **REJECT** when a verified high exists, or when a verified medium leaves a
  stated acceptance criterion NOT MET.
- **ACCEPT** otherwise: every criterion is MET or CANNOT TELL, and any
  remaining findings are medium or low and outside the acceptance criteria.

The verdict follows the verified findings only. A medium or low finding that
does not break an acceptance criterion is recorded and does not block.
CANNOT TELL never becomes REJECT. Put the verdict on the first line as
`VERDICT: ACCEPT` or `VERDICT: REJECT`.
