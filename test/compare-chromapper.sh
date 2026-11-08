#!/bin/sh
# Runs the user's ChroMapper (native Linux build, read-only) in podman containers on Xvfb with
# software OpenGL, PropPreview's scripted open and the BslProbe plugin (test/chromapper-probe).
#
#   test/compare-chromapper.sh <jobs.tsv> <maps folder> <out folder> [parallel]
#
# Each ChroMapper renders in software and can use several GB: every container is capped at
# CM_MEMORY (default 4g, no swap), parallel defaults to 2, and it won't start with less than
# CM_MIN_FREE_GB (default 12) of memory available. Each container also gets CM_CPUS cores
# (default 4) and that many software-render threads. Keep <out folder> on disk, not in /tmp.
#
# jobs.tsv lines: name<TAB>map folder (inside /maps)<TAB>difficulty<TAB>beats csv
# Per job, <out>/<name>/ gets environment.json, lights.jsonl and <beat>.png pose renders.
# Your ChroMapper settings are copied into each container; the real ones are not touched.
# CM_HIDE_NOTES=1 hides notes and walls (for comparing environment renders); PROBE_FULL=1 also exports
# components, textures and per-snapshot shader state (for the renderer port).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
jobs=$1; maps=$2; out=$3; par=${4:-2}
mkdir -p "$out"
# podman needs absolute paths (a relative one is taken as a volume name).
jobs=$(realpath "$jobs"); maps=$(realpath "$maps"); out=$(realpath "$out")
mem="${CM_MEMORY:-4g}"
avail=$(awk '/MemAvailable/ {print int($2 / 1048576)}' /proc/meminfo)
need=${CM_MIN_FREE_GB:-12}
if [ "$avail" -lt "$need" ]; then
    echo "Only ${avail} GB of memory available (need ${need}); not starting." >&2
    exit 3
fi
cm="${CHROMAPPER_DIR:-$HOME/Applications/chromapper}"
cfg="$HOME/.config/unity3d/BinaryElement/ChroMapper"
run="$out/.run"; mkdir -p "$out" "$run"

podman build -q -t bs-light-timeline-test -f "$here/Containerfile" "$here" >/dev/null
podman build -q -t bs-light-timeline-cm -f "$here/Containerfile.chromapper" "$here" >/dev/null
(cd "$here/chromapper-probe" && dotnet build -c Release -o out -p:ChroMapperDir="$cm" >/dev/null)
rm -rf "$run/plugins"; mkdir -p "$run/plugins"
cp "$cm"/Plugins/*.dll "$run/plugins/"; cp "$here/chromapper-probe/out/ChroMapperProbe.dll" "$run/plugins/"

split -n "l/$par" -d "$jobs" "$run/jobs."
i=0
for part in "$run"/jobs.*; do
    home="$run/home$i"; rm -rf "$home"; mkdir -p "$home/.config/unity3d/BinaryElement/ChroMapper"
    python3 -I - "$cfg/ChroMapperSettings.json" "$home/.config/unity3d/BinaryElement/ChroMapper/ChroMapperSettings.json" <<'PY'
import json, os, sys
s = json.load(open(sys.argv[1]))
s.update(BeatSaberInstallation="/bs", LastLoadedMap="", DiscordRPCEnabled="False")
if os.environ.get("CM_HIDE_NOTES"):  # reference renders of the environment alone
    s.update(Load_Notes="False", Load_Obstacles="False")
json.dump(s, open(sys.argv[2], "w"), indent=2)
PY
    podman rm -f "bslt-cmp$i" >/dev/null 2>&1 || true
    podman run -d --name "bslt-cmp$i" --memory "$mem" --memory-swap "$mem" \
        --cpus "${CM_CPUS:-4}" -e LP_NUM_THREADS="${CM_CPUS:-4}" \
        -v "$cm:/cm:ro,Z" -v "$run/plugins:/cm/Plugins:ro,Z" -v "$maps:/maps:ro" \
        -v "$out:/out:Z" -v "$home:/tmp/home:Z" -v "$here:/test:ro,Z" -v "$part:/jobs.tsv:ro,Z" \
        -e PROBE_FULL="${PROBE_FULL:-}" -e HOME=/tmp/home -e DISPLAY=:99 localhost/bs-light-timeline-cm sh /test/cm-worker.sh /jobs.tsv >/dev/null
    i=$((i + 1))
done
for c in $(seq 0 $((i - 1))); do podman wait "bslt-cmp$c" >/dev/null; podman logs "bslt-cmp$c" > "$run/worker$c.log" 2>&1; podman rm "bslt-cmp$c" >/dev/null; done
echo "done: $(grep -h '^ok' "$run"/worker*.log | wc -l) ok, $(grep -h '^FAIL' "$run"/worker*.log | wc -l) failed"
