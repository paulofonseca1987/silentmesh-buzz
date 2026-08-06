#!/usr/bin/env bash
# Point `openssl-sys` at a usable OpenSSL when the system has no development
# headers. Source this (do not execute it) before any cargo command that
# builds dev-dependencies.
#
# Why this is needed at all: `openssl-sys` enters the workspace only through
# dev-dependencies, so `cargo check` succeeds while `cargo test` and
# `cargo clippy --all-targets` fail — which is to say, exactly the commands
# the quality gates run. On a machine without `libssl-dev` (and without root
# to install it) every gate died in ~2s with a confusing
# "Failed to find OpenSSL development headers", and the documented fix lived
# in a contributor's shell profile rather than in the repo. A gate that only
# runs for people who already know the trick is not a gate.
#
# Deliberately a no-op in three cases, so it can never make things worse:
#   - OPENSSL_DIR already set — the caller's choice wins.
#   - System headers present — the normal path, including CI runners.
#   - No Homebrew OpenSSL found — leave the variable unset so cargo reports
#     the real error. Exporting an empty OPENSSL_DIR would be worse than
#     silence: openssl-sys would treat "" as a prefix and fail obscurely.
#
# The version is resolved, never pinned: any `brew install` can bump
# openssl@3 out from under a hardcoded path and break every gate with a
# build failure that names neither brew nor the upgrade.

if [ -z "${OPENSSL_DIR:-}" ] && [ ! -e /usr/include/openssl/opensslv.h ]; then
  _openssl_candidate=$(ls -d \
    /home/linuxbrew/.linuxbrew/Cellar/openssl@3/* \
    /opt/homebrew/Cellar/openssl@3/* \
    /usr/local/Cellar/openssl@3/* \
    2>/dev/null | sort -V | tail -1 || true)
  if [ -n "${_openssl_candidate}" ]; then
    export OPENSSL_DIR="${_openssl_candidate}"
  fi
  unset _openssl_candidate
fi
