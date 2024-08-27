# Dockerfile for cross compiling rqbit binaries from scratch inside docker.
FROM --platform=$BUILDPLATFORM tonistiigi/xx AS xx

FROM --platform=$BUILDPLATFORM rust:alpine AS builder
COPY --from=xx / /

RUN apk update && apk add clang lld npm python3 perl make

ARG TARGETPLATFORM
RUN xx-apk add musl-dev

COPY / /src/
WORKDIR /src/

RUN --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/usr/local/cargo/registry/cache \
    --mount=type=cache,target=/usr/local/cargo/registry/index \
    find /usr/local/cargo/git/db && \
    find /usr/local/cargo/registry && \
    cargo fetch -vv

RUN --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/usr/local/cargo/registry/cache \
    --mount=type=cache,target=/usr/local/cargo/registry/index \
    --mount=type=cache,target=/src/target \
    find /src/target && \
    xx-cargo build && \
    mv target/$(xx-cargo --print-target-triple)/debug/libtestssl.rlib /bin/testssl

FROM scratch

COPY --from=builder /bin/testssl /bin/testssl
