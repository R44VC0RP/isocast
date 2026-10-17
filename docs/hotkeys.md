# Profile hotkeys

The daemon owns global hotkeys. They work with every Isocast window closed
or another app frontmost; firing one never activates Isocast. Bindings live
in `profiles/<id>.hotkeys.json`, travel with profile duplicate/import/export, and
are replaced when the profile switches. Scene-collection switches keep the
profile's bindings and resolve their arguments against the new collection.

Existing profiles have **no bindings**. Reading/saving one doesn't add a
hotkeys section or create an empty sidecar. Defaults are opt-in and merge
without replacing custom binding ids or occupied chords:

```sh
isocast hotkeys defaults
isocast hotkeys list --json
isocast hotkeys set record f13 recording.toggle
isocast hotkeys set camera 'ctrl+alt+cmd+2' scene.switch --args '{"scene":"camera"}'
isocast hotkeys set talk f14 audio.push_to_talk --args '{"source":"mic"}'
isocast hotkeys unset record
isocast hotkeys actions --json
isocast hotkeys input media_play_pause --hold-ms 500 --json
```

`set` replaces one binding (its first argument is its stable id); `unset`
removes it and rejects missing ids with the available choices. Use
`hotkeys defaults --replace` only to replace the entire set intentionally.
Every command supports `--json`. `actions --json` includes the
catalogue, strict argument schemas, labels, groups, availability, hold
behavior, key names and suggested defaults. MCP exposes `hotkeys_get`,
`hotkeys_set`, `hotkeys_actions`, and `hotkeys_input` on the same method table.

## Key syntax

A chord is modifiers followed by **one** key, separated with
`+`. Names are case-insensitive; saved chords have lowercase modifiers in
`ctrl+alt+shift+cmd` order. Duplicate modifiers, spaces and unknown names are
errors. These are **physical US key positions**, independent of keyboard
layout (not the characters an input method would type).

Printable/navigation keys and F1–F12 require **Control or Command**. Shift,
Option, or Shift+Option alone are text input (capitals, symbols, dead keys),
so they cannot be grabbed globally. Only F13–F20, keypad and media keys may
be bare.

Known system/app shortcuts are refused too: Command(+Shift) letters, digits,
editing/navigation keys and common punctuation shortcuts (Find, new tab,
reload, screenshots, preferences, zoom and window switching), and Control
(+Shift) letters/navigation used by editors and terminals (including Ctrl-C).
Known qualified system chords such as Ctrl-Cmd-F/Q/Space and Option-Cmd
Force Quit, Dock, clipboard, Find and reload shortcuts are also refused.
Unused combinations remain available, for example `cmd+f8`, `ctrl+f10`,
`ctrl+alt+r`, `ctrl+cmd+r`, and `ctrl+shift+1`. Other OS-owned shortcuts
report a native registration warning. The same policy disables unsafe
hand-edited bindings at load time; it cannot know every app's custom keys.

- Modifiers: `ctrl`, `alt` (Option), `shift`, `cmd` (Command).
- Keys: `a`–`z`, `0`–`9`, `f1`–`f20`, including Stream Deck's F13–F20.
- Navigation: `space`, `return`, `tab`, `escape`, `backspace` (⌫), `delete`
  (forward delete), `left`, `right`, `up`, `down`, `home`, `end`, `pageup`,
  `pagedown`.
- Punctuation uses names: `minus`, `equal`, `left_bracket`, `right_bracket`,
  `backslash`, `semicolon`, `quote`, `comma`, `period`, `slash`, `grave`.
- Numeric keypad: `numpad_0`–`numpad_9`, `numpad_decimal`, `numpad_multiply`,
  `numpad_plus`, `numpad_clear`, `numpad_divide`, `numpad_enter`,
  `numpad_minus`, `numpad_equal`. These differ from the main keyboard's keys.

Media keys: `media_play_pause`, `media_next`, `media_previous`,
`media_volume_up`, `media_volume_down`, `media_mute`. They may also have
modifiers. Brightness, Fn alone, Caps Lock and key sequences are not
supported. On a Mac keyboard, the system's “Use F1, F2, etc. as standard
function keys” setting decides whether its top row produces an F-key or a
media key. Stream Deck should send the actual F13–F20 key codes.

## Suggested defaults

Opt-in defaults avoid common macOS commands. They don't start a stream.

| Chord | Action |
|---|---|
| Control–Option–Command–R | Toggle recording |
| Control–Option–Command–S | Split take |
| Control–Option–Command–M | Add a marker labelled “Marker” |
| Control–Option–Command–D | Toggle Studio Mode |
| Control–Option–Command–Return | Take preview to program |

Another app can still own a chord. `hotkeys list` names each native
registration failure; choose a different chord or use a focused client.

## Actions and arguments

All action ids are stable; keys can be reassigned without changing an id.
The catalogue is the authoritative schema. Arguments not listed are
rejected, including typos inside a transition.

| Action ids | Arguments |
|---|---|
| `recording.start`, `.stop`, `.toggle`, `.split` | `{}` |
| `recording.marker` | `{"label":"Intro"}` |
| `streaming.start`, `.stop`, `.toggle` | `{}`; use the profile's configured service |
| `studio.toggle`, `transition.take` | `{}` |
| `scene.switch`, `scene.preview` | `{"scene":"scene-id"}` |
| `scene.switch_index`, `scene.preview_index` | `{"index":1}` (one-based current scene order) |
| `item.show`, `.hide`, `.toggle` | `{"scene":"scene-id","item":"item-id"}` |
| `audio.mute`, `.unmute`, `.toggle`, `.push_to_talk`, `.push_to_mute` | `{"source":"audio-source-id"}` |
| `transition.set`, `.take_with` | `{"transition":{"kind":"fade","duration_ms":300}}` |
| `program.screenshot` | `{"path":"/absolute/new-file.png"}` |
| `profile.switch`, `collection.switch` | `{"id":"library-id"}` |

### Timeline actions

These dispatch the existing timeline methods, with the same safety checks:

| Action | Arguments |
|---|---|
| `timeline.new` | `{}` or `{"name":"My video"}`; creates and selects it |
| `timeline.open` | `{"timeline":"timeline-id"}` |
| `timeline.close` | `{}`; finished takes stop appending to a timeline |
| `timeline.retake` | `{}`; optional `folder` and `seconds` as on `retake` |
| `timeline.marker` | `{"label":"Intro"}`; alias of `recording.marker` on the current recording take, not a standalone timeline marker |
| `timeline.remove` | `{"clip":"c2"}`; optional `timeline`, else current |
| `timeline.move` | `{"clip":"c3","index":0}`; optional `timeline` |
| `timeline.export` | `{}` or `{"timeline":"timeline-id","folder":"/absolute/output","vertical":true}` |

Move positions are **zero-based**, matching `timeline_move` (the separate
`isocast timeline move` command presents one-based positions). Original
reserved bindings' `id` argument remains an alias for `clip` on remove/move,
and for `timeline` on export; new bindings should use the canonical names.
Retake works both idle and during a recording, retaining replaced takes'
files. Remove also keeps the files. Exports run asynchronously: watch
`export_progress` / `export_finished` for completion, not just `hotkey_fired`.

```sh
isocast hotkeys set retake 'ctrl+alt+cmd+t' timeline.retake
isocast hotkeys set new-timeline 'ctrl+alt+cmd+n' timeline.new --args '{"name":"My video"}'
isocast hotkeys set export 'ctrl+alt+cmd+e' timeline.export
```

### Action behavior

`scene.switch` selects the preview when Studio Mode is on; otherwise it
changes program. The daemon's preview being present means Studio Mode is
on. Disabling Studio Mode clears preview and emits `preview_cleared`, never
an empty scene id. Items are found recursively by
their scene-wide id (or the source id for legacy items without an id).
Source/scene/library references are checked **when fired**, not when bound:
a profile may be used with several collections. A missing target reports
the argument field and the available ids, without changing anything.

Push-to-talk temporarily **unmutes** while held; push-to-mute temporarily
**mutes**. On release the source returns to its configured state, including
any changes made by another client during the hold. For talk-only-while-held,
configure the source muted first. Simultaneous push-to-mute wins over
push-to-talk. Holds are memory-only: no setup revision or file write, and
they are released on rebinding, profile/collection switching or focused
client disconnect. They affect the normal source mix/filter paths, just as
the source's mute control does; raw, unfiltered ISOs retain their raw audio.
Key repeat never retriggers an action; both press and release edges are used.
When native and focused input report the same chord's press within 100 ms,
they count as one press in either arrival order (also if the first path has
already sent its release). One release or focused-client disconnect ends a
paired hold. Distinct presses on the same path remain distinct, including
rapid marker presses. The time is captured on arrival, so a slow recording
action cannot make queued events look simultaneous.
Distinct press/release pairs remain distinct commands, including recording
toggle: there is no time-based debounce that would silently drop intentional
commands. Use dedicated `recording.start` / `.stop` bindings when preferred.

Output actions retain their normal safety checks. Switching profiles or
collections during recording/streaming is refused. Screenshots use
create-new semantics and never overwrite a file (choose a fresh path for
each screenshot). Failed actions emit a warning and a `hotkey_fired` event
with `error`; successful ones have `error: null`. Events include binding id,
action id, `source` (`global` or `focused`) and `phase` (`press` or `release`).

`recording.pause` and `recording.resume` call the `pause` / `resume`
methods; `recording.pause_toggle` resumes when the recording is paused and
pauses otherwise (read from the recording's own clock, as `status` reports
`live.paused`). All take `{}`. They act only on a running recording; invalid
state or a duplicate pause emits a warning and a `hotkey_fired` error.

The hotkey worker queues distinct press/release pairs and executes actions
serially, waiting for each cut to finish. Two quick `pause_toggle` presses
therefore pause and resume on successive cut boundaries; the second press
can be queued while the first cut is pending. An input reply with
`queued: true` confirms queueing, not action completion; watch `hotkey_fired` for the
outcome. Direct concurrent `pause` / `resume` RPCs can instead be refused
while a cut is pending. Cut steps are 50 ms when every file is 60 fps,
100 ms with a 30 fps file, and up to 1 s with odd rates. The streaming output
is never paused. See [getting-started.md](getting-started.md) for what a
pause does to the files.

`timeline.clear` is still reserved because there is no matching method;
remove individual clips or create a new timeline instead. Its catalogue entry
and firing error state the reason.

## API and client integration

`hotkeys_get` returns current profile id, setup revision, usable `bindings`,
lossless `configured` data (including disabled/unknown entries), `warnings`,
native registration results, held-source overrides, and Studio Mode state.
`pending` says a native registration update is in progress; focused input
returns a retryable explanation during it rather than waiting for the native
event loop. A queued registration that times out is cancelled, so it cannot
install global grabs later while claiming to be focused-only.
`hotkeys_set` accepts **exactly one** of `bindings` (replace everything) or
`patch` (replace/remove by binding id), plus an optional `revision` for
optimistic concurrency. At most 256 bindings per profile. Duplicate chords
are rejected atomically, listing conflict groups; modifier order and case
do not evade that check. Diagnostics are bounded to 8 KiB, with representative
ids for large groups. The 256-binding limit is checked first. An OS-owned
chord is saved with a per-binding registration warning, permitting focused
fallback.

### Loading and version compatibility

Applying a whole setup that **omits** `hotkeys` preserves the current
profile's configuration and says so in the plan/apply notes. Explicit
`hotkeys: {}` in a whole setup, `hotkeys_set` with `bindings: {}`, or a
merge patch with `hotkeys: null` clears it durably.

Hotkeys are non-critical metadata: invalid shapes, unknown fields/actions,
unsafe keys and duplicate chords never prevent the setup loading or a
recording starting. Invalid entries are disabled with warnings; every member
of a duplicate group is disabled. Unknown data is retained losslessly for a
newer daemon, not silently discarded. `hotkeys_set` remains strict and
atomic; fix/remove invalid entries or explicitly replace the whole map.

The profile TOML keeps its original v1 settings, so both pre-G.5 and
original G.5 daemons can read/write it and leave the sidecar alone.
Original G.5 **inline** hotkeys are read without changing any file or backup;
they take precedence over a sidecar until the next explicit profile write
(such as a rename, settings edit or hotkey update). That write saves the
sidecar before removing the inline field and keeps the original TOML as
`.bak`. Merely listing, getting, switching or loading a profile never performs
this migration. Before downgrading an inline profile, write it once with the
new daemon. A malformed
sidecar is preserved on unrelated saves and only an explicit update replaces
it (also keeping `.bak`). Older daemons simply do not run these bindings.
Exports/imports through the current API still carry inline `hotkeys` to
transfer the profile as one document; use the new daemon for those exports.

Deleting a profile moves its sidecar and backup with it. If an old daemon
deleted only the TOML, creating a profile with that id archives the orphaned
sidecar/backup under `deleted/profiles/orphan …` before writing the new
profile. Blank profiles inherit no old bindings; duplicates/imports receive
only the bindings of the supplied profile.

If an older daemon already parked an inline profile as `.broken.toml`, that
archive remains untouched: import the desired archived profile with the new
daemon rather than guessing which old settings to restore.

```json
{"method":"hotkeys_set","params":{"patch":{"record":{"keys":"f13","action":"recording.toggle","args":{}},"old-binding":null}}}
```

Settings > Hotkeys edits the current profile and records physical key chords.
The native app's focused-key bridge forwards `hotkeys_input` with
`{"keys":"media_play_pause","phase":"press"}` and then `release`, on
the **same persistent socket**. Send releases on window blur too. Closing
that socket releases its holds. The shared Rust client's `hotkey_input`
helper preserves this lifetime. Globally registered chords are accepted too:
`global: true` means registration succeeded, not that the OS delivered this
press. The first delivery fires immediately; the other path's matching press
within 100 ms is suppressed by the shared edge tracker. `queued: true` means
accepted for that processing, not a promise of a second action/event.
MCP's one-shot focused input produces
a pulse; it isn't a persistent keyboard client for holding push-to-talk.
Clients can read `hotkeys_get.studio_mode` after a `hotkey_fired` event to
present the daemon's state.

For automation on macOS, Cua's **desktop-targeted** hotkey route reaches
Carbon; its **window-targeted, foreground** route can deliver the chord to
the app without a Carbon callback, despite both reporting `global_input`.
The focused bridge covers that case inside Isocast. A window-targeted press
into Finder cannot be observed by Isocast's local bridge; test unfocused
hotkeys with the desktop target while Finder is frontmost. This distinction
was reproduced on macOS 27 using the unchanged QA daemon; desktop delivery
also succeeded in a minimal Carbon receiver. A physical-keyboard check remains useful for keyboard layout,
Stream Deck and media-key behavior; no permission is added to intercept keys.
`ISOCAST_DEBUG_KEYS=1` on the daemon logs registration results and native
Carbon edges; on the app it logs forwarded key edges and API replies. Use
these only while diagnosing input (the app log contains typed chords).

## macOS permissions

Ordinary keys use Carbon `RegisterEventHotKey` with exclusive registration
and both `kEventHotKeyPressed` / `kEventHotKeyReleased`. **No Accessibility
or Input Monitoring permission is needed**, including for push-to-talk.
The daemon runs AppKit's event pump with activation prohibited (no window,
Dock icon or focus change). The callback queues work; output start/stop
never blocks the native event thread. The bounded queue cancels holds if
overloaded, rather than dropping a release and leaving a source unmuted.

Quit Apple Events (including logout/shutdown and Activity Monitor Quit),
signals and API shutdown all finish recordings, stop the engine and flush
state before exiting. AppKit defers termination while that work runs; a
recording-stop timeout is not permission to abandon an active take.

Carbon doesn't register media keys. Those use a **listen-only session event
tap** for system-defined media events, requiring Input Monitoring for the
daemon's signed app. Isocast checks existing permission without prompting.
When denied, `hotkeys_get` and warning events explain how to enable it in
System Settings > Privacy & Security > Input Monitoring and restart the
daemon. Ordinary global keys continue working, and media bindings remain
available through the focused-client API. The passive tap does not suppress
macOS's media/volume behavior. Accessibility isn't requested as a workaround.

Global capture on other platforms currently reports focused-only status.
