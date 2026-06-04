#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
image="${ANNEAL_LINUX_SANDBOX_IMAGE:-anneal-linux-sandbox-test}"

docker build \
  -f "$repo/docker/linux-sandbox.Dockerfile" \
  -t "$image" \
  "$repo/docker"

docker run --rm --privileged \
  -v "$repo:/work:ro" \
  -v anneal-linux-sandbox-target:/work/target \
  -v anneal-linux-sandbox-cargo-registry:/usr/local/cargo/registry \
  -v anneal-linux-sandbox-cargo-git:/usr/local/cargo/git \
  -w /work \
  "$image" \
  bash -c 'cargo test -p anneal-exec --lib sandbox::tests -- --nocapture && cargo test -p anneal-exec --test linux_sandbox -- --nocapture && cargo test -p anneal-exec --test sandbox_fds -- --nocapture'
