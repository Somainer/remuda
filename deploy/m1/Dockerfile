# Hub image from a pre-built linux/amd64 static musl `remuda` binary.
# Compiling rustc/cc under qemu linux/amd64 on Apple Silicon has segfaulted
# (vite 139, ring cc SIGSEGV). The operator builds musl with cargo-zigbuild,
# then this stage only copies the binary into distroless/static.
#
#   docker buildx build --platform linux/amd64 -f deploy/m1/Dockerfile \
#     --output type=docker,dest=deploy/out/remuda-hub-<sha>.tar \
#     deploy/out

FROM gcr.io/distroless/static-debian12:nonroot
COPY remuda-linux-musl /usr/local/bin/remuda
COPY NOTICE LICENSE /usr/local/share/remuda/
USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/remuda"]
CMD ["hub"]
