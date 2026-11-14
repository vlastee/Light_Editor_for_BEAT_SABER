#!/bin/sh
# Headless UI test: runs the release build on Xvfb in podman with the maps mounted read-only.
# usage: test/run-ui-test.sh <CustomLevels folder> <output folder> [steps script, default ui-steps.sh]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
maps=$(realpath "$1"); out=$2; mkdir -p "$out"; out=$(realpath "$out"); steps=$(realpath "${3:-$here/ui-steps.sh}")
mkdir -p "$out"
podman build -q -t bs-light-timeline-test -f "$here/Containerfile" "$here" >/dev/null
podman rm -f bslt-ui >/dev/null 2>&1 || true
envs="$HOME/.local/share/bs-light-timeline/environments"
env_mount=""
[ -d "$envs" ] && env_mount="-v $envs:/tmp/home/.local/share/bs-light-timeline/environments:ro,Z"
# shellcheck disable=SC2086
podman run -d --name bslt-ui $env_mount \
    -v "$here/../target/release/bs-light-timeline:/app/bslt:ro,Z" \
    -v "$maps:/maps:ro" -v "$out:/out:Z" -v "$steps:/ui-steps.sh:ro,Z" \
    -e HOME=/tmp/home -e DISPLAY=:99 -e LIBGL_ALWAYS_SOFTWARE=1 \
    localhost/bs-light-timeline-test sleep infinity >/dev/null
podman exec bslt-ui sh -c 'mkdir -p /work /tmp/home; (Xvfb :99 -screen 0 1600x900x24 >/tmp/xvfb.log 2>&1 &); sleep 1'
podman exec bslt-ui sh /ui-steps.sh
podman rm -f bslt-ui >/dev/null
