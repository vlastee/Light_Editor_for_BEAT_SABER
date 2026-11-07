#!/bin/sh
# Headless UI test: runs the release build on Xvfb in podman with the maps mounted read-only.
# usage: test/run-ui-test.sh <CustomLevels folder> <output folder>
set -eu
here=$(cd "$(dirname "$0")" && pwd)
maps=$1; out=$2
mkdir -p "$out"
podman build -q -t bs-light-timeline-test -f "$here/Containerfile" "$here" >/dev/null
podman rm -f bslt-ui >/dev/null 2>&1 || true
podman run -d --name bslt-ui \
    -v "$here/../target/release/bs-light-timeline:/app/bslt:ro,Z" \
    -v "$maps:/maps:ro" -v "$out:/out:Z" -v "$here/ui-steps.sh:/ui-steps.sh:ro,Z" \
    -e HOME=/tmp/home -e DISPLAY=:99 -e LIBGL_ALWAYS_SOFTWARE=1 \
    localhost/bs-light-timeline-test sleep infinity >/dev/null
podman exec bslt-ui sh -c 'mkdir -p /work /tmp/home; (Xvfb :99 -screen 0 1600x900x24 >/tmp/xvfb.log 2>&1 &); sleep 1'
podman exec bslt-ui sh /ui-steps.sh
podman rm -f bslt-ui >/dev/null
