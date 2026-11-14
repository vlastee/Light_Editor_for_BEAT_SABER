#!/bin/sh
# Map browser steps for run-ui-test.sh: opens it with Ctrl+O, searches, opens a map with Enter.
set -u
first=$(ls /maps | head -1)
(/app/bslt "/maps/$first" >/tmp/app.log 2>&1 &)
sleep 4
xdotool search --name "Light Timeline" windowfocus --sync
x() { xdotool "$@"; sleep 0.8; }
ctrl() { xdotool keydown ctrl; sleep 0.15; xdotool key "$1"; sleep 0.15; xdotool keyup ctrl; sleep 1; }
shot() { import -window root "/out/$1.png"; }
ctrl o
sleep 2
shot b1-browser
x type --delay 60 "bruises"
sleep 1
shot b2-search
x key Return
sleep 3
shot b3-opened
ctrl o
sleep 1
x key Escape
shot b4-closed
tail -3 /tmp/app.log
