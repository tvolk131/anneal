#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
image="${ANNEAL_LINUX_SANDBOX_IMAGE:-anneal-linux-sandbox-test}"
command="${ANNEAL_LINUX_SANDBOX_COMMAND:-cargo test -p anneal-exec --lib sandbox::tests -- --nocapture && cargo test -p anneal-exec --test linux_sandbox -- --nocapture && cargo test -p anneal-exec --test sandbox_fds -- --nocapture}"

docker build \
  -f "$repo/docker/linux-sandbox.Dockerfile" \
  -t "$image" \
  "$repo/docker"

docker_args=(
  --rm
  --privileged
  -v "$repo:/work:ro"
  -v anneal-linux-sandbox-target:/target
  -v anneal-linux-sandbox-cargo-registry:/usr/local/cargo/registry
  -v anneal-linux-sandbox-cargo-git:/usr/local/cargo/git
  -e CARGO_TARGET_DIR=/target
  -w /work
)

if [[ -n "${ANNEAL_TOOLCHAIN_MANIFEST:-}" || "${ANNEAL_LINUX_TEST_PATH:-}" == *"/nix/"* ]]; then
  docker_args+=(-v /nix:/nix:ro)
fi

if [[ -n "${ANNEAL_TOOLCHAIN_MANIFEST:-}" ]]; then
  docker_args+=(
    -e "ANNEAL_TOOLCHAIN_MANIFEST=$ANNEAL_TOOLCHAIN_MANIFEST"
  )
fi

if [[ -n "${ANNEAL_LINUX_TEST_PATH:-}" ]]; then
  docker_args+=(-e "PATH=$ANNEAL_LINUX_TEST_PATH")
fi

docker run "${docker_args[@]}" \
  "$image" \
  bash -c "$command"
