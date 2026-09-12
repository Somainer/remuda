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
