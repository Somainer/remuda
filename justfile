build:
    cargo build --workspace --locked

test:
    cargo test --workspace --locked

lint:
    cargo clippy --workspace --all-targets --locked -- -D warnings
    cargo fmt --all -- --check

web-install:
    cd web && pnpm install

web-build:
    cd web && pnpm build

dev:
    cargo run --locked -p remuda -- dev

# Linux Node/Hub artifacts via cargo-zigbuild (https://github.com/rust-cross/cargo-zigbuild).
# macOS: install zig 0.13.x on PATH, then `cargo install cargo-zigbuild`.
# Debian 10 / glibc 2.28 is the Node compatibility floor (plan §13.6).
# musl static is the smoke default; gnu.2.28 is the fallback if a host
# cannot run musl (record the reason, do not claim glibc compatibility
# from the target triple alone).

linux-musl:
    rustup target add x86_64-unknown-linux-musl
    PATH="$HOME/.local/bin:$PATH" cargo zigbuild --locked --release -p remuda --target x86_64-unknown-linux-musl
    file target/x86_64-unknown-linux-musl/release/remuda

linux-gnu:
    rustup target add x86_64-unknown-linux-gnu
    PATH="$HOME/.local/bin:$PATH" cargo zigbuild --locked --release -p remuda --target x86_64-unknown-linux-gnu.2.28
    file target/x86_64-unknown-linux-gnu/release/remuda
