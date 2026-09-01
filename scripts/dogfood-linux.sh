#!/usr/bin/env bash
# The dogfood: anneal builds and unit-tests itself, under `--require-enforced`
# (Linux namespaces) — in the same privileged container the Linux sandbox
# tests use, because hosted Ubuntu's bwrap fails Anneal's namespace probe.
#
# The repo is mounted read-only except `.anneal/`, which is bind-mounted
# read-write from the runner's checkout — so the transportable half of the
# store (`store/`) can be persisted across CI runs via actions/cache and the
# machine-bound half (`local/`) stays ephemeral, exactly the anneal-store
# layout's contract.
#
# Environment:
#   ANNEAL_LINUX_SANDBOX_COMMAND  the anneal invocation to run (default:
#                                 test --base origin/master --require-enforced)
#   ANNEAL_TOOLCHAIN_MANIFEST     the Nix-built toolchain manifest path
#   ANNEAL_BIN                    the anneal binary path *inside the container*
#                                 (the repo is mounted at /work); built on the
#                                 host first with the nix dev shell — `nix build
#                                 .#anneal` fetches crate tarballs inside nix
#                                 builds and is at the mercy of the CDN, while
#                                 dev-shell cargo fetches like every other lane
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
image="${ANNEAL_LINUX_SANDBOX_IMAGE:-anneal-linux-sandbox-test}"
command="${ANNEAL_LINUX_SANDBOX_COMMAND:-test --base origin/master --require-enforced}"
anneal_bin="${ANNEAL_BIN:-target/release/anneal}"
manifest="${ANNEAL_TOOLCHAIN_MANIFEST:?set ANNEAL_TOOLCHAIN_MANIFEST}"

mkdir -p "$repo/.anneal"

docker build \
  -f "$repo/docker/linux-sandbox.Dockerfile" \
  -t "$image" \
  "$repo/docker" >&2

# `docker run` returns anneal's exit code (failures propagate); --privileged
# is what makes the bwrap namespace probe pass. /nix is read-only so the
# sandbox mounts toolchain closures from the manifest's store paths.
docker run \
  --rm \
  --privileged \
  -v "$repo:/work:ro" \
  -v "$repo/.anneal:/work/.anneal" \
  -v /nix:/nix:ro \
  -e "ANNEAL_TOOLCHAIN_MANIFEST=$manifest" \
  -w /work \
  "$image" \
  "./$anneal_bin" $command
