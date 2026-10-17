# Timelines

A timeline puts takes in order and joins them into one video, like a clip
bar: record take 1, then take 2 lands after it; retake a bad one; remove or
reorder takes; export one file at the end.

## Record into a timeline

```sh
isocast timeline new "My video"   # make one and make it current
isocast start                     # take 1
isocast stop                      # take 1 lands on the timeline
isocast start                     # take 2
isocast split                     # take 2 ends here, take 3 starts at once
isocast stop
```

While a timeline is current, every take that finishes (by `stop` or by a
`split`) is added to its end. `isocast timeline close` stops that; takes are
then only recorded, as without timelines.

## Retake

`isocast retake` records the last take again:

- not recording: it takes the last clip off the timeline and starts
  recording;
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
newest; a project without one gets a timeline of its takes in order), so
from then on takes are added to it, as with `timeline open`.

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
