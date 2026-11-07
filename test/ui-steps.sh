#!/bin/sh
# Drives the app inside the test container (see run-ui-test.sh). Writes screenshots to /out.
set -u
restart() {
    pkill -x bslt; sleep 0.5
    rm -rf /work/kemare /tmp/home/.config
    cp -r "/maps/DANCE RMX - KEMARE" /work/kemare
    (/app/bslt /work/kemare >/tmp/app.log 2>&1 &)
    sleep 4
    # No window manager in the container: give the window keyboard focus explicitly.
    xdotool search --name "Light Timeline" windowfocus --sync
}
x() { xdotool "$@"; sleep 0.6; }
# Hold the modifier long enough for winit to see it (fast xdotool combos can lose Ctrl).
ctrl() { xdotool keydown ctrl; sleep 0.15; xdotool key "$1"; sleep 0.15; xdotool keyup ctrl; sleep 0.6; }
shot() { import -window root "/out/$1.png"; }
file=/work/kemare/ExpertPlusStandard.dat

restart
x mousemove 720 218 click --repeat 2 --delay 90 1        # add key on Left Lasers ~beat 12
x mousemove 1355 340 click 1                             # palette swatch (panel widget)
xdotool mousemove 1424 233 mousedown 1; sleep 0.2
for X in 1440 1460 1480; do xdotool mousemove $X 233; sleep 0.1; done
x mouseup 1                                              # drag brightness slider
ctrl s                                             # save right after using panel widgets
shot 06-saved
echo "backups: $(ls /work/kemare | grep -c timeline-backup)"
echo "saved key: $(grep -o '{"b":12[.0-9]*,"et":2,[^}]*}[^}]*}' $file | head -1)"

x key Delete                                             # delete the selected key
ctrl s
echo "after delete: $(grep -c '"b":12.25,"et":2' $file) matching events"
ctrl z                                             # undo the delete
shot 07a-after-ctrl-z
ctrl s
echo "after undo: $(grep -c '"b":12.25,"et":2' $file) matching events"
shot 07-undo
ctrl z; ctrl z; ctrl z; ctrl z                           # undo past the start of history
shot 08-nothing-to-undo
