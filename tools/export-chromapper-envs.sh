#!/bin/sh
# Exports every environment from your installed ChroMapper (native Linux build) for the preview:
# for each environment it generates a tiny map, opens it in ChroMapper inside podman (Xvfb,
# software OpenGL; see test/compare-chromapper.sh) with the BslProbe plugin, and saves the
# loaded scene's meshes, materials, textures, lights (lane + map light IDs), particle systems, fog
# lights, lane settings and colors, for the editor's renderer (a port of ChroMapper's BloomFog).
# Needs podman, dotnet (to build the probe) and ChroMapper-PropPreview in ChroMapper's Plugins.
# The results stay in ~/.local/share/bs-light-timeline/environments (ChroMapper is GPL-2.0).
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
data="${XDG_DATA_HOME:-$HOME/.local/share}/bs-light-timeline"
cm="${CHROMAPPER_DIR:-$HOME/Applications/chromapper}"
# On disk, next to the results (/tmp is often RAM-backed).
mkdir -p "$data/tmp"
work=$(mktemp -d "$data/tmp/export.XXXXXX")
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/maps" "$work/out" "$data/environments"
gen() {
python3 -I - "$work" "$@" <<'PY'
import json, os, sys, wave
work, envs = sys.argv[1], sys.argv[2:]
jobs = []
for env in envs:
    d = os.path.join(work, "maps", env); os.makedirs(d)
    json.dump({"_version": "2.1.0", "_songName": env, "_songAuthorName": "probe", "_levelAuthorName": "probe",
               "_beatsPerMinute": 120, "_songFilename": "song.wav", "_coverImageFilename": "",
               "_environmentName": env, "_difficultyBeatmapSets": [{"_beatmapCharacteristicName": "Standard",
               "_difficultyBeatmaps": [{"_difficulty": "Expert", "_difficultyRank": 7,
               "_beatmapFilename": "ExpertStandard.dat", "_noteJumpMovementSpeed": 16}]}]},
              open(os.path.join(d, "Info.dat"), "w"))
    json.dump({"version": "3.3.0", "basicBeatmapEvents": [], "colorNotes": []}, open(os.path.join(d, "ExpertStandard.dat"), "w"))
    w = wave.open(os.path.join(d, "song.wav"), "wb"); w.setnchannels(1); w.setsampwidth(2); w.setframerate(22050)
    w.writeframes(b"\0\0" * 22050 * 20); w.close()
    jobs.append(f"env_{env}\t/maps/{env}\tExpert\t4")
open(os.path.join(work, "jobs.tsv"), "w").write("\n".join(jobs) + "\n")
PY
}
# First run: DefaultEnvironment, which also lists every environment this ChroMapper has.
export PROBE_FULL=1
gen DefaultEnvironment
"$here/test/compare-chromapper.sh" "$work/jobs.tsv" "$work/maps" "$work/out" 1
envs=$(grep -v '^DefaultEnvironment$' "$work/out/env_DefaultEnvironment/environments.txt" || true)
if [ -n "$envs" ]; then
    # shellcheck disable=SC2086
    gen $envs
    "$here/test/compare-chromapper.sh" "$work/jobs.tsv" "$work/maps" "$work/out" "${PARALLEL:-2}"
fi
n=0
for d in "$work"/out/env_*; do
    [ -f "$d/environment.json" ] || continue
    name=${d##*/env_}
    cp "$d/environment.json" "$data/environments/$name.json"
    # Extras next to it: fog lights, component settings and textures.
    rm -rf "$data/environments/$name"; mkdir -p "$data/environments/$name"
    for f in bloomfog.json components.json; do [ -f "$d/$f" ] && cp "$d/$f" "$data/environments/$name/"; done
    [ -d "$d/textures" ] && cp -r "$d/textures" "$data/environments/$name/"
    n=$((n + 1))
done
echo "Exported $n environments to $data/environments"
