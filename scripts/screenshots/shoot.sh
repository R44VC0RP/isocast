# Sourced by remote.sh after `start`: the engine is up, Terminal runs
# cli.command (which records a take) and the app shows it all. Uses
# remote.sh's helpers (cua, window_of, grab, frame, front, menu, log).

waited=0
while [ ! -e "$S/demo.done" ] && [ $waited -lt 60 ]; do sleep 1; waited=$((waited + 1)); done
log "the CLI demo ran (${waited}s)"
sleep 3

# The status output has outgrown 32 rows (the machine profile, the project and
# next-recording lines): a taller window for this one shot shows the marker
# above it, then the window goes back to the size the Screen scene shows.
frame Terminal 32 60 896 620 cli.command
sleep 2
front Terminal cli.command
grab cli Terminal cli.command
frame Terminal 32 60 896 504 cli.command
sleep 2

# The replay buffer running beside the take (its button lit).
"$isocast" replay start >/dev/null || log "couldn't start the replay buffer"
sleep 2
front Isocast
grab app Isocast

# Studio Mode with the intro staged in preview.
menu Isocast View "Studio Mode"
"$isocast" preview intro >/dev/null
sleep 3
swift "$S/cursor.swift"
grab studio Isocast

# The multiview (top-8) in a window over the app on the virtual display,
# with tally: the intro in preview (green), the Screen scene on air (red).
set -- $(cat "$S/vd.log")
"$isocast" projector open multiview --layout top-8 --window --frame "${3%.*},30,$window_w,$window_h" >/dev/null ||
    log "couldn't open the multiview"
sleep 4
front Isocast Multiview
grab multiview Isocast Multiview
"$isocast" projector close --all >/dev/null
sleep 1
menu Isocast View "Studio Mode"

# Settings, on its output page.
menu Isocast File "Settings…"
sleep 2
swift "$S/cursor.swift"
grab settings Isocast
w="$(window_of Isocast)"
cua press_key "{\"pid\":${w#* },\"window_id\":${w% *},\"key\":\"escape\",\"session\":\"$(sid)\"}" >/dev/null


# Stop the take.
sleep 2
"$isocast" replay stop >/dev/null 2>&1
"$isocast" stop --wait >/dev/null
sleep 2

# A vertical (9:16) canvas, once the take has stopped (it can't be added
# while recording): the Screen scene arranged for it (the terminal
# on top, the slides below, the lower third over them), its monitor beside
# the main one. Taken back out afterwards, so the other shots don't show it.
revision() { "$isocast" get --json | python3 -c 'import json, sys; print(json.load(sys.stdin)["revision"])'; }
"$isocast" apply --revision "$(revision)" --patch '{"vertical":{"width":1080,"height":1920},"scenes":[{"id":"screen","vertical":{"items":[
{"source":"terminal","rect":{"x":0,"y":0,"width":1080,"height":608},"fit":"cover"},
{"id":"slides-vertical","source":"slides","rect":{"x":0,"y":608,"width":1080,"height":1312},"fit":"cover"},
{"id":"lower-third","rect":{"x":40,"y":1640,"width":760,"height":104},"style":{"background":"#101418e6","radius":14},"children":[
 {"id":"title","rect":{"x":28,"y":14,"width":700,"height":44},"content":{"kind":"text","text":"Building a recorder in Rust","size":36,"weight":600,"font":"Helvetica Neue"}},
 {"id":"subtitle","rect":{"x":28,"y":60,"width":700,"height":30},"content":{"kind":"text","text":"Episode 12 · scenes, ISO files and agents","size":24,"color":"#f2b84b","font":"Helvetica Neue"}}]}]}}]}' >/dev/null ||
    log "couldn't add the vertical canvas"
sleep 4
front Isocast
grab vertical Isocast
"$isocast" apply --revision "$(revision)" --patch '{"vertical":null,"scenes":[{"id":"screen","vertical":null}]}' >/dev/null ||
    log "couldn't remove the vertical canvas"

# The take's files in Finder: each recording has its own project folder
# under the recordings folder; open it, in the list view, without the
# sidebar (it would show the test Mac's own favourites).
project="$(ls -d /Users/Shared/Recordings/*/ | head -1)"
project="${project%/}"
open "$project"
sleep 3
title="$(basename "$project")"
menu Finder View "as List" || log "couldn't switch Finder to the list view"
menu Finder View "Hide Sidebar" || log "the sidebar was hidden already"
frame Finder 60 80 840 400 "$title"
front Finder "$title"
grab recordings Finder "$title"

# What was recorded, for the log.
for f in "$project"/*.mov; do
    command -v ffprobe >/dev/null || break
    log "$(basename "$f"): $(ffprobe -v error -show_entries stream=codec_name,width,height,r_frame_rate -show_entries format=duration -of compact=p=0:nk=1 "$f" | tr '\n' ' ')"
done
