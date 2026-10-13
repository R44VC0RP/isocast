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

## Export

```sh
isocast timeline export                     # waits until it is done
isocast timeline export --background        # returns at once
isocast watch --kinds export_progress,export_finished
```

Export joins the clips into one program file and one file per ISO source
that every clip has, next to the first take (or in `--folder`), with an
`.export.json` manifest. Video is not re-encoded, and audio and video stay
in sync at every join. You can keep recording while an export runs.

Timelines are kept across restarts. Over MCP the same steps are
`new_timeline`, `retake`, `get_timeline`, `remove_clip`, `move_clip` and
`export_timeline` ([mcp-tools.md](mcp-tools.md)). Note that `move_clip`'s
`index` counts from 0, while the CLI's position counts from 1.

Trimming, splitting a take and removing silence come later.
