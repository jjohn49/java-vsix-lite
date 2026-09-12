#!/usr/bin/env bash
# Builds the comparison image and runs the baseline: one container per
# flavor, then one post-processing pass over all flavors' reports. Repeats
# the whole thing N times and aggregates, so results carry a sample size and
# a dispersion rather than being one anecdote.
#
# Usage: tools/vscode-compare/run.sh [--runs N] [--no-cache]
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../.." && pwd)"
out_dir="${script_dir}/out"
image="jvl-vscode-compare:latest"

runs=1
build_args=()
while [ $# -gt 0 ]; do
  case "$1" in
    --runs)
      runs="${2:?--runs needs a count}"
      shift 2
      ;;
    --runs=*)
      runs="${1#*=}"
      shift
      ;;
    *)
      build_args+=("$1")
      shift
      ;;
  esac
done
case "${runs}" in
  ''|*[!0-9]*|0) echo "--runs must be a positive integer, got '${runs}'" >&2; exit 2 ;;
esac

mkdir -p "${out_dir}"
# Every bind mount of the output directory below carries `:z`. On an
# SELinux-enforcing host (Fedora, RHEL) a container may not write to a
# directory that still has its host label, so every report write fails with
# EACCES — which looks exactly like a crashed launch, and the retry loop
# gives up after three identical failures. `:z` has Docker relabel the
# directory for container use; shared rather than private (`:Z`) because
# each run directory is written by one container and then read by the
# post-processing ones. Where SELinux is off (Docker Desktop's and colima's
# VMs, most Ubuntu hosts) Docker ignores it.

# Builds natively for the host's architecture (amd64 or arm64): the
# Dockerfile detects the target arch at build time and pins matching Node
# and VS Code downloads for it. Running VS Code's Electron runtime under
# cross-arch QEMU emulation is not just slow but memory-prohibitive in
# practice, so this deliberately never forces a foreign --platform.
#
# Built once, before any measuring: every repetition must run the identical
# image, and a rebuild mid-sequence would silently change the thing under
# test.
docker build "${build_args[@]+"${build_args[@]}"}" -f "${script_dir}/Dockerfile" -t "${image}" "${repo_root}"

# Seconds of quiet time each container waits after installing its extension
# before it starts sampling and runs the probes, so container boot, Xvfb
# startup, and the VSIX install stay out of the measured window. Set
# JVL_COMPARE_SETTLE_SECONDS=0 for fast iteration on probe values alone.
settle_seconds="${JVL_COMPARE_SETTLE_SECONDS:-30}"

# Record what the numbers were produced on. Resource figures are meaningless
# without it, and `docker info` reports the CPU and memory the containers
# actually get, which on a Linux server is the host but on a Mac is the
# virtual machine Docker runs inside.
image_id="$(docker image inspect --format '{{.Id}}' "${image}")"
docker_ncpu="$(docker info --format '{{.NCPU}}' 2>/dev/null || echo unknown)"
docker_mem="$(docker info --format '{{.MemTotal}}' 2>/dev/null || echo unknown)"
docker_version="$(docker version --format '{{.Server.Version}}' 2>/dev/null || echo unknown)"
cat > "${out_dir}/environment.json" <<EOF
{
  "host": "$(uname -srm)",
  "dockerServerVersion": "${docker_version}",
  "dockerCpus": "${docker_ncpu}",
  "dockerMemoryBytes": "${docker_mem}",
  "imageId": "${image_id}",
  "settleSeconds": ${settle_seconds},
  "runs": ${runs},
  "startedAt": "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
}
EOF

all_flavors=(none ours redhat)

for run in $(seq 1 "${runs}"); do
  run_dir="$(printf '%s/run-%02d' "${out_dir}" "${run}")"
  rm -rf "${run_dir}"
  mkdir -p "${run_dir}"

  # Rotate which flavor goes first. Flavors run sequentially, so whoever
  # runs last inherits a host that has been busy for several minutes —
  # warmer caches, possibly a hotter CPU and lower turbo headroom. Over a
  # multiple of three runs every flavor occupies every slot equally, so that
  # drift cancels instead of accruing to one plugin.
  flavors=()
  for i in 0 1 2; do
    flavors+=("${all_flavors[$(( (i + run - 1) % 3 ))]}")
  done
  echo "=== run ${run}/${runs} (order: ${flavors[*]})"

  attempts_json=""
  for flavor in "${flavors[@]}"; do
    echo "--- container: ${flavor} (settling ${settle_seconds}s before sampling)"
    attempt=1
    # VS Code's Electron occasionally dies with SIGSEGV during startup — it
    # has been observed in this harness and passed immediately on retry,
    # with nothing plugin-specific about it. A crashed launch produces no
    # report at all, so retry rather than throwing away the whole run. The
    # count is recorded: a flavor that needs retries often is itself a
    # finding, and silently absorbing that would hide it.
    until docker run --rm \
      --name "jvl-vscode-compare-${flavor}" \
      --shm-size=2g \
      -e "JVL_COMPARE_FLAVOR=${flavor}" \
      -e "JVL_COMPARE_SETTLE_SECONDS=${settle_seconds}" \
      -v "${run_dir}:/out:z" \
      "${image}"; do
      if [ "${attempt}" -ge 3 ]; then
        echo "flavor ${flavor} failed 3 times; giving up" >&2
        exit 1
      fi
      attempt=$((attempt + 1))
      echo "--- flavor ${flavor} failed; retrying (attempt ${attempt}/3)" >&2
    done
    attempts_json="${attempts_json}${attempts_json:+, }\"${flavor}\": ${attempt}"
  done

  cat > "${run_dir}/run-meta.json" <<EOF
{
  "run": ${run},
  "flavorOrder": ["${flavors[0]}", "${flavors[1]}", "${flavors[2]}"],
  "attempts": { ${attempts_json} },
  "endedAt": "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
}
EOF

  # Post-processing, in its own throwaway container so no measured run is
  # perturbed by it: diff the reports, then render the CPU/memory timeline.
  # Reuses the image's pinned Node — no extra host dependency.
  docker run --rm -v "${run_dir}:/out:z" --entrypoint node "${image}" \
    /work/tools/vscode-compare/harness/compareReports.js /out
  docker run --rm -v "${run_dir}:/out:z" --entrypoint node "${image}" \
    /work/tools/vscode-compare/harness/renderTimeline.js
done

# Aggregate every run into one report carrying n, median, and spread.
docker run --rm -v "${out_dir}:/out:z" --entrypoint node "${image}" \
  /work/tools/vscode-compare/harness/aggregate.js /out

echo
echo "output in ${out_dir}:"
ls -1 "${out_dir}"
