# Sourced by remote.sh after `start`: the engine is up, Terminal runs
# cli.command (which records a take) and the app shows it all. Uses
# remote.sh's helpers (cua, window_of, grab, frame, front, menu, log).

waited=0
while [ ! -e "$S/demo.done" ] && [ $waited -lt 60 ]; do sleep 1; waited=$((waited + 1)); done
log "the CLI demo ran (${waited}s)"
sleep 3

front Terminal cli.command
grab cli Terminal cli.command

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
menu Isocast View "Studio Mode"

# Settings, on its output page.
menu Isocast File "Settings…"
sleep 2
swift "$S/cursor.swift"
grab settings Isocast
w="$(window_of Isocast)"
cua press_key "{\"pid\":${w#* },\"window_id\":${w% *},\"key\":\"escape\",\"session\":\"$(sid)\"}" >/dev/null

# Stop, then the take's files in Finder: each recording has its own
# project folder under the recordings folder; open it, in the list view,
# without the sidebar (it would show the test Mac's own favourites).
sleep 2
"$isocast" replay stop >/dev/null 2>&1
"$isocast" stop >/dev/null
sleep 2
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
