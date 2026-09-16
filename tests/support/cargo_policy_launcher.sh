#!/bin/sh
# Trusted fake rustup; no instructor source or Cargo execution.
test "$1" = run && test "$2" = pinned && test "$3" = /trusted/cargo &&
test "$4" = run && test "$5" = --message-format=json && test "$6" = --locked && test "$#" = 6 &&
test "$RUSTC" = /trusted/rustc && test "$RUSTDOC" = /trusted/rustdoc &&
test "$CARGO" = /trusted/cargo && test "$RUSTUP_TOOLCHAIN" = pinned &&
test "$RUSTUP_AUTO_INSTALL" = 0 && test -z "${CARGO_NET_OFFLINE+x}" &&
test "${RUSTC_WRAPPER}x" = x && test "${RUSTC_WORKSPACE_WRAPPER}x" = x &&
test "${CARGO_ENCODED_RUSTFLAGS}x" = x && test "${CARGO_ENCODED_RUSTDOCFLAGS}x" = x &&
test -z "${RUSTFLAGS+x}${RUSTDOCFLAGS+x}${RUSTFMT+x}${CARGO_BUILD_RUSTC+x}${CARGO_TARGET_FAKE_RUNNER+x}${CARGO_REGISTRY_TOKEN+x}${RUSTUP_LOG+x}${LD_PRELOAD+x}${CLIPPY_ARGS+x}${UNRELATED_SECRET+x}" &&
! read -r input
