# Sourced by remote.sh after `start`: the engine is up, Terminal runs
# cli.command (which records a take) and the app shows it all. Uses
# remote.sh's helpers (cua, window_of, grab, frame, front, menu, log).

waited=0
while [ ! -e "$S/demo.done" ] && [ $waited -lt 60 ]; do sleep 1; waited=$((waited + 1)); done
log "the CLI demo ran (${waited}s)"
sleep 3

front Terminal cli.command
grab cli Terminal cli.command

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
cua press_key "{\"pid\":${w#* },\"window_id\":${w% *},\"key\":\"escape\",\"session\":\"$session\"}" >/dev/null

# Stop, then the take's files in Finder: File > Show Recordings reveals the
# folder in its parent; open it, in the list view.
sleep 2
"$isocast" stop >/dev/null
sleep 2
menu Isocast File "Show Recordings"
sleep 3
w="$(window_of Finder Shared)"
[ -n "$w" ] && cua hotkey "{\"pid\":${w#* },\"window_id\":${w% *},\"keys\":[\"cmd\",\"down\"],\"session\":\"$session\"}" >/dev/null
sleep 2
menu Finder View "as List" || log "couldn't switch Finder to the list view"
frame Finder 60 80 840 400 Recordings
front Finder Recordings
grab recordings Finder Recordings

# What was recorded, for the log.
for f in /Users/Shared/Recordings/*.mov; do
    command -v ffprobe >/dev/null || break
    log "$(basename "$f"): $(ffprobe -v error -show_entries stream=codec_name,width,height,r_frame_rate -show_entries format=duration -of compact=p=0:nk=1 "$f" | tr '\n' ' ')"
done
