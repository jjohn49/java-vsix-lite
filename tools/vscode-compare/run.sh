#!/usr/bin/env bash
# Builds the comparison image and runs the baseline: one container per
# flavor, then one post-processing pass over both flavors' reports.
# Usage: tools/vscode-compare/run.sh [--no-cache]
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../.." && pwd)"
out_dir="${script_dir}/out"
image="jvl-vscode-compare:latest"

mkdir -p "${out_dir}"
# Builds natively for the host's architecture (amd64 or arm64): the
# Dockerfile detects the target arch at build time and pins matching Node
# and VS Code downloads for it. Running VS Code's Electron runtime under
# cross-arch QEMU emulation is not just slow but memory-prohibitive in
# practice, so this deliberately never forces a foreign --platform.
docker build "$@" -f "${script_dir}/Dockerfile" -t "${image}" "${repo_root}"

# Seconds of quiet time each container waits after installing its extension
# before it starts sampling and runs the probes, so container boot, Xvfb
# startup, and the VSIX install stay out of the measured window. Set
# JVL_COMPARE_SETTLE_SECONDS=0 for fast iteration on probe values alone.
settle_seconds="${JVL_COMPARE_SETTLE_SECONDS:-30}"

# One pristine container per flavor, run one after the other from the exact
# same image with the exact same fixture, limits, settle period, and
# scripted edits — the only difference between them is which VSIX gets
# installed. Separate containers mean neither flavor inherits the other's
# page cache, JIT state, JVM warmth, or memory pressure, and the cgroup
# stats each one samples describe that flavor alone.
# `none` first: it installs no extension at all and establishes what the
# container, VS Code, and the probe suite itself cost before any plugin is
# in the picture.
# VS Code's Electron occasionally dies with SIGSEGV during startup — it has
# been observed once in this harness and passed immediately on retry, with
# nothing plugin-specific about it. A crashed launch produces no report at
# all, so retry rather than throwing away the whole multi-container run.
attempts=3
for flavor in none ours redhat; do
  echo "=== container: ${flavor} (settling ${settle_seconds}s before sampling)"
  attempt=1
  until docker run --rm \
    --name "jvl-vscode-compare-${flavor}" \
    --shm-size=2g \
    -e "JVL_COMPARE_FLAVOR=${flavor}" \
    -e "JVL_COMPARE_SETTLE_SECONDS=${settle_seconds}" \
    -v "${out_dir}:/out" \
    "${image}"; do
    if [ "${attempt}" -ge "${attempts}" ]; then
      echo "flavor ${flavor} failed ${attempts} times; giving up" >&2
      exit 1
    fi
    attempt=$((attempt + 1))
    echo "--- flavor ${flavor} failed; retrying (attempt ${attempt}/${attempts})" >&2
  done
done

# Post-processing, in its own throwaway container so no measured run is
# perturbed by it: diff the reports, then render the CPU/memory timeline.
# Reuses the image's pinned Node — no extra host dependency.
docker run --rm \
  -v "${out_dir}:/out" \
  --entrypoint node \
  "${image}" \
  /work/tools/vscode-compare/harness/compareReports.js /out

docker run --rm \
  -v "${out_dir}:/out" \
  --entrypoint node \
  "${image}" \
  /work/tools/vscode-compare/harness/renderTimeline.js

echo
echo "reports written to ${out_dir}:"
ls -1 "${out_dir}"
