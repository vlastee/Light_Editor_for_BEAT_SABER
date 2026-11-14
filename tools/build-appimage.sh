#!/bin/sh
# Builds dist/Beat_Saber_Light_Timeline-x86_64.AppImage from a release build.
#
#   tools/build-appimage.sh [runtime]
#
# An AppImage is the AppImage type-2 runtime followed by a squashfs of the app. The runtime is
# taken from, in order: the argument, $APPIMAGE_RUNTIME, ~/.cache/bs-light-timeline/runtime-x86_64,
# or copied from any type-2 AppImage in ~/Applications (cached for next time). Needs mksquashfs
# with zstd. The binary links the system's libasound and loads Vulkan/X11/Wayland at run time.
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
cache="${XDG_CACHE_HOME:-$HOME/.cache}/bs-light-timeline"
runtime=${1:-${APPIMAGE_RUNTIME:-$cache/runtime-x86_64}}
if [ ! -f "$runtime" ]; then
    mkdir -p "$cache"
    for a in "$HOME"/Applications/*.AppImage "$HOME"/Applications/*.appimage; do
        [ -x "$a" ] || continue
        off=$(timeout 10 "$a" --appimage-offset 2>/dev/null || true)
        case "$off" in ''|*[!0-9]*) continue ;; esac
        if timeout 10 "$a" --appimage-version 2>&1 | grep -q type2-runtime; then
            head -c "$off" "$a" > "$cache/runtime-x86_64"
            echo "Using the AppImage runtime from $a"
            break
        fi
    done
    runtime="$cache/runtime-x86_64"
    [ -s "$runtime" ] || { echo "No AppImage runtime found: pass one (github.com/AppImage/type2-runtime releases)." >&2; exit 1; }
fi

cd "$here"
cargo build --release
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
app=$(mktemp -d "${TMPDIR:-/tmp}/bslt-appdir.XXXXXX")
trap 'rm -rf "$app"' EXIT
mkdir -p "$app/usr/bin" "$app/usr/share/applications" "$app/usr/share/icons/hicolor/256x256/apps"
cp target/release/bs-light-timeline "$app/usr/bin/"
strip "$app/usr/bin/bs-light-timeline" 2>/dev/null || true
cp assets/icon-256.png "$app/bs-light-timeline.png"
cp assets/icon-256.png "$app/usr/share/icons/hicolor/256x256/apps/bs-light-timeline.png"
ln -s bs-light-timeline.png "$app/.DirIcon"
cat > "$app/bs-light-timeline.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Beat Saber Light Timeline
Comment=Blender-style light timeline editor for Beat Saber maps
Exec=bs-light-timeline %f
Icon=bs-light-timeline
Categories=AudioVideo;Audio;
StartupWMClass=bs-light-timeline
Terminal=false
X-AppImage-Version=$version
DESKTOP
cp "$app/bs-light-timeline.desktop" "$app/usr/share/applications/"
cat > "$app/AppRun" <<'RUN'
#!/bin/sh
here=$(dirname "$(readlink -f "$0")")
exec "$here/usr/bin/bs-light-timeline" "$@"
RUN
chmod +x "$app/AppRun"

mkdir -p dist
out="dist/Beat_Saber_Light_Timeline-$version-x86_64.AppImage"
rm -f "$out" "$app.sqfs"
mksquashfs "$app" "$app.sqfs" -root-owned -noappend -comp zstd -quiet
cat "$runtime" "$app.sqfs" > "$out"
rm -f "$app.sqfs"
chmod +x "$out"
echo "Built $out ($(du -h "$out" | cut -f1))"
