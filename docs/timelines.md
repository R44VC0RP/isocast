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
⇧⌘Z undo and redo. The same edits from the CLI and MCP:

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
isocast timeline undo                          # take back the last edit; `redo` puts it back
```

Times on the timeline round to the nearest frame of the program of the clip
there, so cuts always fall between two frames. An angle is any ISO source
every clip in the span recorded (the error lists them); clips across a
span's ends are split there. Undo goes back through the edits (trim, split,
delete, angle, remove, move) since the last take was recorded: a take added
by recording or `retake` starts a new history.

## Export

```sh
isocast timeline export                     # waits until it is done
isocast timeline export --background        # returns at once
isocast watch --kinds export_progress,export_finished
```

Export joins the clips into one program file and one file per ISO source
that every clip has, each cut to the clips' in and out points, next to the
first take (or in `--folder`), with an `.export.json` manifest. When a clip
shows another angle than the program, it also writes `<name> EDITED.mov`:
each clip's angle as its picture, the program's sound throughout, in the
program's format.

Cuts are frame-accurate. Video is copied as recorded, except where a clip
starts between two keyframes (recordings have one every second): the frame
on screen at the in point and the frames up to the next keyframe are decoded
and encoded again with the same codec (`reencoded_frames` in the manifest
counts them). An angle recorded in another size or frame rate than the
program is encoded again whole, fitted to the program's size. Audio is cut
to the sample and cross-faded over 10 ms at every join, so joins never
click, and audio and video stay in sync at every join. You can keep
recording while an export runs.

Timelines are kept across restarts. Over MCP the same steps are
`new_timeline`, `retake`, `get_timeline`, `remove_clip`, `move_clip`,
`trim_clip`, `split_clip`, `delete_range`, `set_angle`, `undo_edit`,
`redo_edit` and `export_timeline` ([mcp-tools.md](mcp-tools.md)). Note that
`move_clip`'s `index` counts from 0, while the CLI's position counts from 1.

Removing silence comes later.
