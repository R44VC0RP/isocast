# Timelines

A timeline puts takes in order and joins them into one video, like a clip
bar: record take 1, then take 2 lands after it; retake a bad one; remove or
reorder takes; export one file at the end.

## Where a recording goes

By default every recording starts a new project folder with a timeline of
its own: record three times and you get three projects, each with one take.
To add a recording to an existing project instead, choose its timeline as
the **record target**; the take is written into the folder of the
timeline's last take and lands at the end of it ("take 4 of My Video").

```sh
isocast record-target             # where the next recording goes
isocast record-target my-video    # the next recording is take N+1 of my-video
isocast record-target new         # back to a new project each time (the default)
isocast start --into my-video     # this recording only
isocast start --new-project       # this recording only, whatever the choice
```

The choice is kept across restarts and shown in `isocast status` ("Next
recording: …") and in the app: next to Start Recording in the Controls dock
(a row under it, "New project" or "Take 4 · My Video", or in a short dock a
square button beside it) with a menu of the newest projects and the one
open in the editor, in the status bar, and in the editor as **Record Next
Take Here**. When a recording stops, a toast says where it went ("Saved as
take 2 of My Video", "Saved as a new project"). The obs-websocket
`StartRecord` and the `recording.start` hotkey follow the same choice.
Over MCP: `record_target`, and `start_recording`'s `new_project` and `into`.

A recording keeps its timeline to the end: its splits and its retakes stay
on it, and choosing another target while it records is for the next
recording. When its first take begins, its timeline becomes current (the
one the editor shows and the timeline commands act on).

```sh
isocast timeline new "My video"   # make one, make it current and the record target
isocast start                     # take 1 (in a new project folder)
isocast stop --wait               # take 1 lands on the timeline
isocast start                     # take 2, in the same folder
isocast split                     # take 2 ends here, take 3 starts at once
isocast stop --wait
```

`isocast timeline open ID` and opening a project (`isocast project open`)
only make a timeline current; they never change where recordings go.
`isocast timeline close` makes none current, and if it was the record
target, recordings start new projects again.

Before 0.0.8 the current timeline took every recording until it was closed.
A current timeline from then is migrated once: it stays current (the editor
shows it), but recordings start new projects again, and `status` (and the
app, with a warning sign next to Start Recording) says so until you choose:
`isocast record-target ID` keeps adding to it.

## Retake

`isocast retake` records the last take again:

- not recording: it takes the current timeline's last clip off and starts
  recording into that timeline (and that take's folder);
- recording: it drops the take in progress from the timeline and begins a
  new take about a quarter second later, without stopping any device.

The files of a take that was taken off are never deleted.

## Edit the order

```sh
isocast timeline show             # clips in order (c1, c2, ...), with their files
isocast timeline remove c2        # take a clip off (its files stay)
isocast timeline move c3 1        # move a clip to position 1 (the first)
isocast timeline list             # every timeline, and which is current
isocast timeline open ID          # make another one current
```

## Edit: trim, split, delete and angles

In the app, editing has its own window: **Window › Editor** (⌘E), or a
clip's **Open in Editor** in the Create view, opens "Isocast Editor —
<timeline>" beside the studio, which it never changes. It shows the angle
at the playhead over the timeline with a track for the program and each
ISO source. Drag a clip's ends to trim; S splits at the playhead; I and O
mark a span that Delete takes out; 1 shows the program and 2–9 the ISO
tracks over the span (or the clip); J, K and L play back, stop and play
forward (press again for 2× and 4×); arrows step a frame (⇧ ten); ⌘Z and
⇧⌘Z undo and redo. Playing forward, you hear the program's sound (what the
edited file has) in step with the picture, through the monitoring device
(Settings › Output, `audio.monitoring_device`); at 2× and 4× it is
pitch-corrected, backwards it is silent. It is the app's own player, so a
recording's monitoring carries on unchanged. While a recording, a stream
or the replay buffer takes in system audio (or an Isocast app's audio), the
editor plays silently, so they don't pick it up; its sound comes back when
they stop. The picture follows the display's refresh. A clip whose file is
missing shows "Offline". **Export** opens a menu: Export, or Export with
Vertical (9:16) (see below). The same edits from the CLI and MCP:

Every clip covers part of its take, from its in point to its out point on
the take's own clock (the whole take when it lands). The edits only change
the timeline, never a take's files, and each is one step that
`isocast timeline undo` takes back (and `redo` puts back):

```sh
isocast timeline trim c1 --in 1.25 --out 7.5   # a clip's in and out points (seconds into its take)
isocast timeline split 10                      # cut the clip at 10 s on the timeline in two
isocast timeline delete 12 13.5                # take 12–13.5 s out and close the gap (ripple)
isocast timeline angle camera 3 5              # the edited video shows the camera ISO from 3 to 5 s
isocast timeline angle program 3 5             # and back to the program
isocast timeline crop 3 5 --preset 9:16         # show a centred 9:16 part of the picture from 3 to 5 s
isocast timeline crop 3 5 --rect 0.25,0,0.5,1  # or any part: x,y,width,height as fractions of the picture
isocast timeline crop 3 5 --clear              # the whole picture again
isocast timeline roll c1 -0.5                  # move the cut after c1 half a second earlier (a roll)
isocast timeline undo                          # take back the last edit; `redo` puts it back
```

Times on the timeline round to the nearest frame of the program of the clip
there, so cuts always fall between two frames. An angle is any ISO source
every clip in the span recorded (the error lists them); clips across a
span's ends are split there. Undo goes back through the edits (trim, split,
delete, angle, remove, move) since the last take was recorded: a take added
by recording or `retake` starts a new history.

A crop is the part of a clip's picture the edited video shows: a rectangle
in fractions of the picture (from its top-left), kept on its 2-pixel grid and
at least 32 pixels each way. It belongs to the clip and applies to whichever
angle the clip shows; presets (16:9, 9:16, 1:1, 4:5) give the largest
rectangle of that shape inside the picture (or inside `--rect`), centred.
Trim drags the clip's end and moves everything after it (ripple); a roll
moves the cut between two clips and keeps the timeline's length.

## Export

```sh
isocast timeline export                     # waits until it is done
isocast timeline export --background        # returns at once
isocast timeline export --vertical          # plus a 9:16 edited file
isocast watch --kinds export_progress,export_finished
```

Export joins the clips into one program file and one file per ISO source
that every clip has, each cut to the clips' in and out points, next to the
first take (or in `--folder`), with an `.export.json` manifest. When a clip
shows another angle than the program, it also writes `<name> EDITED.mov`:
each clip's angle as its picture, the program's sound throughout, in the
program's format. A crop does too: the edited file shows each cropped clip's
crop scaled to fit the program's size (black bars where its shape differs),
encoded again with the program's codec, while uncropped clips are copied as
always. The program and ISO files are never cropped: they stay whole, so a
crop never loses a picture.

With `--vertical` (`vertical` on `timeline_export`) it also writes
`<name> VERTICAL.mov`, the edited picture at 9:16: the program's size on
its side (1080x1920 for a 1920x1080 program), each clip's angle or crop
fitted into it, so a 9:16 crop fills the frame and an uncropped clip shows
with black bars above and below. Every frame of it is encoded again. A
program already taller than wide (or square) has nothing to turn on its
side: the vertical file is skipped (`skipped` says why) and EDITED is the
one to use. Takes recorded with a vertical canvas also export `<name>
VERTICAL-PROGRAM.mov`, the vertical program as it was recorded: that is the
better 9:16 file (composed for 9:16, never scaled or encoded again);
VERTICAL is derived from the main program's picture. `edited` and
`vertical` are reserved source ids (the export's labels), as `program` is;
a setup from before 0.0.7 with a source by either id loads with it renamed
(`edited-2`, `vertical-2`, …, its scenes, recording lists and hotkeys
following), saved at once, with a note in `status` (`setup_file.notes`).

Cuts are frame-accurate. Video is copied as recorded, except where a clip
starts between two keyframes (recordings have one every second): the frame
on screen at the in point and the frames up to the next keyframe are decoded
and encoded again with the same codec (`reencoded_frames` in the manifest
counts them). An angle recorded in another size, frame rate or bit depth
than the program (a 10-bit ISO file in an 8-bit program's edited file) is
encoded again whole, fitted to the program's size, in its format; so is a
take recorded at another HEVC bit depth than the first take's (the setting
changed between takes) in the program or ISO file it joins. Audio is cut
to the sample and cross-faded over 10 ms at every join, so joins never
click, and audio and video stay in sync at every join. You can keep
recording while an export runs. Each file has one video sample description:
frames encoded again carry their parameter sets in-band at every keyframe,
so HEVC exports play in Chrome as well as Safari, QuickTime and ffmpeg.

## Export to Final Cut Pro, Premiere Pro or Resolve

In the editor, **File › Export › For Final Cut Pro… / For Premiere Pro… /
For DaVinci Resolve… / OpenTimelineIO…** writes the timeline as a project
that editor opens. The same from the CLI and MCP (`export_timeline_nle`):

```sh
isocast timeline export --nle fcpxml                 # Final Cut Pro (FCPXML 1.10)
isocast timeline export --nle xmeml                  # Premiere Pro (Final Cut Pro 7 XML)
isocast timeline export --nle xmeml --app resolve    # DaVinci Resolve
isocast timeline export --nle otio                   # OpenTimelineIO
isocast timeline export --nle fcpxml --output ~/Desktop/Talk.fcpxml
```

The project points at the takes' own program and ISO files: nothing is
encoded, copied or changed, and the only file written is the project
(next to the first take, or in `--folder`, numbered so nothing is
overwritten; or exactly `--output`). Every ISO source has its own video
track, the program's is on top, and each clip's stretch of every file is
on its track, lined up from the take manifests (each file's start
offset), so the angles stay in sync and another angle is a click away.
At each clip only its angle's track is on: the sequence plays what the
EDITED file shows. Cuts, trims, rolls and deletes are the clips' in and out
points; a crop is a crop and a transform on the clip (on all its angles).
Markers come along. The program's audio tracks are on; each ISO file's
audio is on its own tracks, there but off (the EDITED file has the
program's sound). The sequence runs at the first clip's program frame
rate and size, from timecode 00:00:00:00.

DaVinci Resolve (21.1, free) renders the xmeml (`--app resolve`) frame
for frame as Isocast's EDITED file, with the same sound. Resolve also
opens the other two, with less:

- FCPXML: Resolve takes each clip's picture and sound together (it
  ignores FCPXML's `srcEnable`), so the program's sound plays twice and
  the uncropped program shows around a crop of another shape. Final Cut
  Pro reads the split as written.
- OpenTimelineIO has no crop or audio-track choice: crops are left out
  (they are in each clip's `metadata.isocast`).

Where the formats fall short:

- FCPXML has no tracks: the ISO angles are lanes 1…n with the program
  above them, and each file's audio is one clip on a lane below (a file
  with several audio tracks keeps them together).
- Media paths are absolute in every format (the editors relink a moved
  folder by file name): Resolve imported no OpenTimelineIO clip whose path
  was relative or a `file://` URL.
- `--app` matters for xmeml only: Premiere counts a picture's scale in its
  own pixels and Resolve from its own fit to the frame, and Resolve reads
  xmeml audio a channel at a time, so its file has a track per channel (a
  stereo pair panned left and right).

Timelines are kept across restarts. Over MCP the same steps are
`new_timeline`, `retake`, `get_timeline`, `remove_clip`, `move_clip`,
`trim_clip`, `split_clip`, `delete_range`, `roll_cut`, `set_angle`,
`crop_clip`, `undo_edit`,
`redo_edit` and `export_timeline` ([mcp-tools.md](mcp-tools.md)). Note that
`move_clip`'s `index` counts from 0, while the CLI's position counts from 1.

## Project files (.isoprj)

Every recording session writes into its own project folder, and every
project folder holds a visible `<Project name>.isoprj`. Double-click it
in the Finder (or **File › Open…**, ⌘O, or **File › Open Recent**) and
Isocast opens the editor on it, beside the studio, even while recording.
The same from the CLI and MCP:

```sh
isocast project open "~/Movies/Isocast/2026-10-16 10-00-00 Talk/Talk.isoprj"
isocast project open "~/Movies/Isocast/2026-10-16 10-00-00 Talk"   # the folder works too
isocast project recent            # File › Open Recent
isocast timeline relink c2 /Volumes/Backup/Talk   # an offline clip's take is there now
```

Over MCP: `open_project`, `recent_projects` and `relink_clip`.

The file is JSON: the project's id, its takes' manifests, and every
timeline that uses its takes with all their edits (clip order, trims,
splits, angles, crops, the undo history). Paths in it are relative to the file,
so the folder can be copied or moved anywhere, to another disk or another
Mac, and it opens with the same edits. A project opened from somewhere new
has its timelines taken in (under another id if one by that id is already
here); a take that isn't where its timeline says is shown as offline, and
`relink` (or the app's "Relink…" when it says takes are offline)
points it at where it is now. Its name
follows the project's: renaming the project (or its folder) renames the
file.

A project file opened outside its project's folder (one mailed alone,
in Downloads) opens read-only: its timelines are taken in, but nothing is
written into that folder and it doesn't become a project.

Opening a project makes one of its timelines current (the one named with
`--timeline`, else the current one if it is the project's, else its
newest; a project without one gets a timeline of its takes in order), as
with `timeline open`. It doesn't change where the next recording goes:
choose it with **Record Next Take Here** in the editor (or `isocast
record-target ID`).

The file is written by the engine in the background, a moment after a
change, never into a folder while a recording writes into it (it is
brought up to date when the recording stops), and always atomically, so
a crash leaves the previous version, never half of one. The engine keeps
its own copy of every timeline (in its timelines folder) as before; the
project file is what travels with the folder.

## Open the editor after recording

Settings › Output › **Open Editor After Recording** (the setup's
`recording.open_editor`, off unless set; `isocast project auto-open
on|off`, or MCP `apply_change` with `{"recording": {"open_editor": true}}`):
when a recording stops, the editor opens on that take's project and
timeline in its own window. A split doesn't open it. It never takes the
keyboard: the app isn't brought forward, and while a stream runs the
editor opens behind the studio window, which stays in front.

Removing silence comes later.
