# Hub image. Multi-stage: Web assets → locked Cargo build → distroless non-root.
#
# Production (M3-05 / scripts/ci/container.sh) must pin every FROM by sha256.
# Floating tags here are a bootstrap stand-in so `docker build` works before
# that gate exists. Do not publish an image built from unpinned bases.
#
# Build from the repository root:
#   docker build -f deploy/Dockerfile --build-arg REMUDA_GIT_SHA=$(git rev-parse HEAD) .

ARG NODE_IMAGE=node:24-bookworm-slim
ARG RUST_IMAGE=rust:1.94.1-bookworm
ARG DISTROLESS_IMAGE=gcr.io/distroless/cc-debian12:nonroot

# Native linux/amd64 CI (no qemu). Operator laptops should use
# deploy/m1/Dockerfile with a cargo-zigbuild musl binary instead.

FROM ${NODE_IMAGE} AS web-build
WORKDIR /src/web
COPY web/package.json web/pnpm-lock.yaml web/.npmrc ./
RUN corepack enable \
    && corepack prepare pnpm@9.15.9 --activate \
    && pnpm install --frozen-lockfile
COPY web/ ./
RUN pnpm build

FROM ${RUST_IMAGE} AS rust-build
WORKDIR /src
ARG REMUDA_GIT_SHA=unknown
ENV REMUDA_GIT_SHA=${REMUDA_GIT_SHA}
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY --from=web-build /src/web/dist ./web/dist
RUN cargo build --locked --release --bin remuda

FROM ${DISTROLESS_IMAGE}
COPY --from=rust-build /src/target/release/remuda /usr/local/bin/remuda
COPY NOTICE LICENSE /usr/local/share/remuda/
USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/remuda"]
CMD ["hub"]
