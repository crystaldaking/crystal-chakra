#!/usr/bin/env bash
# Build the chakra-lsp-test image (tools/Dockerfile.lsp) and run Chakra's test
# suite inside it with every language server available on PATH (issue #123).
#
# Without arguments this runs the full workspace suite including real-provider
# tests:
#
#   cargo test --locked --workspace -- --include-ignored
#
# Any arguments replace the default tail of that command, so a selective
# provider run looks like:
#
#   ./tools/run_lsp_tests.sh -p chakra-provider-gopls -- --ignored
#
# The repository is mounted read-write at /workspace; build artifacts go to
# named volumes (chakra-lsp-target, chakra-lsp-cargo-registry,
# chakra-lsp-cargo-git) so reruns stay incremental and the host target/ is
# never touched by the root-owned container.
set -euo pipefail

IMAGE=chakra-lsp-test
TARGET_VOLUME=chakra-lsp-target
REGISTRY_VOLUME=chakra-lsp-cargo-registry
GIT_VOLUME=chakra-lsp-cargo-git

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

docker build -t "$IMAGE" -f "$REPO_ROOT/tools/Dockerfile.lsp" "$REPO_ROOT/tools"

# Initialize the target volume without copying the host's target/ into it:
# Docker populates a fresh named volume from whatever the container currently
# sees at the mount point, and at /workspace/target that is the host-built
# tree. Seeding a marker file first keeps the volume "initialized" and empty.
if ! docker run --rm -v "$TARGET_VOLUME:/seed" "$IMAGE" test -f /seed/.keep 2>/dev/null; then
    docker run --rm -v "$TARGET_VOLUME:/seed" "$IMAGE" touch /seed/.keep
fi

if [ $# -eq 0 ]; then
    set -- --workspace -- --include-ignored
fi

TTY_OPTS=()
if [ -t 0 ] && [ -t 1 ]; then
    TTY_OPTS=(-t)
fi

docker run --rm --init "${TTY_OPTS[@]}" \
    -v "$REPO_ROOT:/workspace" \
    -v "$TARGET_VOLUME:/workspace/target" \
    -v "$REGISTRY_VOLUME:/usr/local/cargo/registry" \
    -v "$GIT_VOLUME:/usr/local/cargo/git" \
    "$IMAGE" cargo test --locked "$@"
