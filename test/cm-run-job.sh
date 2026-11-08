#!/bin/sh
# Inside the ChroMapper comparison container: opens one map in ChroMapper with the probe plugin
# and PropPreview's scripted run, and collects the probe output + pose screenshots in /out/<name>.
# usage: cm-run-job.sh <name> <map folder> <difficulty> <beats csv> [pose]
set -u
name=$1; src=$2; diff=$3; beats=$4; pose=${5:-"0,2.833,-1.667,-8,0,0,70,0.15,5000"}
work=/work/$name; out=/out/$name
rm -rf "$work" "$out"; mkdir -p "$work" "$out"
# Work on a copy (ChroMapper may write next to the map).
for f in "$src"/*; do case "$f" in */autosaves) ;; *) cp -r "$f" "$work/" ;; esac; done
poses=$(echo "$beats" | tr ',' '\n' | sed "s|.*|$pose|" | paste -sd ';')
last=$(echo "$beats" | tr ',' '\n' | tail -1)
cd /cm && LIBGL_ALWAYS_SOFTWARE=1 PROBE_OUT="$out" PROPPREVIEW_OPEN="$work" PROPPREVIEW_DIFF="$diff" \
    PROPPREVIEW_SHOTS="$beats" PROPPREVIEW_OUT="$out" PROPPREVIEW_MODE=Playing PROPPREVIEW_POSES="$poses" \
    PROPPREVIEW_SIZE=1280x420 timeout 150 /cm/ChroMapper -force-glcore -logFile "$out/player.log" >/dev/null 2>&1 &
pid=$!
i=0
while [ $i -lt 140 ]; do
    [ -f "$out/$last.png" ] && { sleep 3; break; }
    kill -0 $pid 2>/dev/null || break
    sleep 1; i=$((i + 1))
done
kill $pid 2>/dev/null; sleep 1; kill -9 $pid 2>/dev/null
rm -rf "$work"
[ -f "$out/environment.json" ] && echo "ok $name" || echo "FAIL $name"
