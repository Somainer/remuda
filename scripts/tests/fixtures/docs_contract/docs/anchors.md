# Source anchors

- Symbol on the anchor line: `alpha_fn` at `crates/demo/src/lib.rs:3`.
- Symbol within three lines: `alpha_fn` at `crates/demo/src/lib.rs:6`.
- Symbol far from the anchor: `alpha_fn` at `crates/demo/src/lib.rs:11`.
- Only a path, so no symbol is inferred: `crates/demo/src/lib.rs:9`.
- Missing source file: `crates/demo/src/nope.rs:1`.
- Anchor beyond the end of the file: `crates/demo/src/lib.rs:900`.
- Shorthand resolved from the earlier full path: `lib.rs:3`.
