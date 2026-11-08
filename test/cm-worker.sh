#!/bin/sh
# Inside the container: sets up the display and fake Beat Saber folder, then runs each job line
# ("name<TAB>map folder<TAB>difficulty<TAB>beats") from the given file.
set -u
mkdir -p "/bs/Beat Saber_Data" /work
ln -sfn /maps "/bs/Beat Saber_Data/CustomLevels"; ln -sfn /maps "/bs/Beat Saber_Data/CustomWIPLevels"
(Xvfb :99 -screen 0 1280x720x24 >/dev/null 2>&1 &); sleep 1
while IFS="$(printf '\t')" read -r name map diff beats; do
    [ -n "$name" ] || continue
    [ -f "/out/$name/lights.jsonl" ] && [ -f "/out/$name/environment.json" ] && { echo "skip $name"; continue; }
    sh /test/cm-run-job.sh "$name" "$map" "$diff" "$beats"
done < "$1"
