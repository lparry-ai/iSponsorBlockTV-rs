# syntax=docker/dockerfile:1
# Drop-in replacement for the Python image: same /app/data volume, same env vars,
# same entrypoint behaviour (no args = start, `--setup`/`--setup-cli` or `setup` = configure).
#
# Build:  docker buildx build --platform linux/amd64,linux/arm64 -t isponsorblocktv-rs .
# The builder runs on the host's architecture and cross-compiles a static musl binary
# for each target (via tonistiigi/xx), so multi-arch builds don't need QEMU emulation.

FROM --platform=$BUILDPLATFORM tonistiigi/xx:1.6.1 AS xx

FROM --platform=$BUILDPLATFORM rust:1.99-alpine AS builder
COPY --from=xx / /
RUN apk add --no-cache clang lld ca-certificates
ARG TARGETPLATFORM
RUN xx-apk add --no-cache musl-dev gcc

WORKDIR /src
# clang defaults the armv6 musl target to an older ISA than ring's assembly needs.
ENV CFLAGS_arm_unknown_linux_musleabi="-march=armv6" \
    CFLAGS_arm_unknown_linux_musleabihf="-march=armv6"
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/build,id=isponsorblocktv-target-$TARGETPLATFORM \
    xx-cargo build --release --locked --target-dir /src/build && \
    cp "/src/build/$(xx-cargo --print-target-triple)/release/isponsorblocktv" /isponsorblocktv && \
    xx-verify --static /isponsorblocktv

FROM scratch
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=builder /isponsorblocktv /app/isponsorblocktv
ENV iSPBTV_docker=True \
    iSPBTV_data_dir=data \
    SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
WORKDIR /app
VOLUME /app/data
ENTRYPOINT ["/app/isponsorblocktv"]
