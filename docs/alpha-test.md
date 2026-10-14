# Alpha test: the daily-driver checklist

PLAN A.7. The alpha is done when Isocast survives a real session on your
own Mac with real devices. This is the run sheet: about two and a half
hours, most of it the recording running on its own.

## 1. Install (5 minutes)

```sh
cd ~/dev/isocast
cargo build --release
scripts/sign-dev.sh target/release/isocastd   # the engine, keeps your permissions
scripts/app-dev.sh --release                  # the app, as ~/Applications/Isocast.app
./target/release/isocast restart
open ~/Applications/Isocast.app
```

Optional, for Zoom/Meet: `scripts/vcam-install.sh` installs the virtual
camera (it asks for your password, then an Approve click in System
Settings).

## 2. Before you record (5 minutes)

```sh
./target/release/isocast doctor   # permissions, encoders, devices, disk
./target/release/isocast tune     # does this Mac keep up with your setup?
```

Fix anything `doctor` flags. `tune` should say your setup fits.

## 3. The soak (2 hours)

Use your real setup: camera, microphone, display or window capture. Record
the program plus ISO files, and stream at the same time to a destination
you're happy to go live on.

While it runs, do what a normal session does, spread over the two hours:

- switch scenes, and take one live from Studio Mode;
- pause, then resume (Controls dock or your hotkey);
- add a marker; split once (`isocast split`);
- in View > Create, record two short takes, retake one, drag to reorder,
  export;
- leave it alone for a long stretch.

Watch the LCD's DROPPED counter and the status bar. Both should stay at 0.

## 4. After you stop

The REC details popover lists every file with frames, dropped and gaps.
To check the files themselves:

```sh
cd "<the take's folder>"
for f in *.mov *.mp4; do echo "$f"; ffprobe -v error -show_entries stream=codec_type,codec_name,width,height,r_frame_rate,duration -of compact "$f"; done
```

In every file the video and audio durations should match, and the
program's should match the time you recorded (minus pauses).

## 5. Crash recovery (10 minutes)

1. Start a recording, wait 30 s, then `pkill -9 isocastd`. Reopen the app:
   it says the take was interrupted, and the files play up to within about
   two seconds of the kill.
2. Same again, but pause first, then kill while paused. The note says the
   recoverable length is unknown; the files play up to about the last two
   seconds before the pause. Making this crash-proof is PLAN G.3b (after
   the alpha week).
3. Quit the app while recording: it asks before stopping.

## 6. When something's wrong

Help > Report a Bug… builds a report: a GitHub issue body and a zip of logs
and system details, with stream keys and tokens removed. Review the
preview, then press **Send Report**: it files the issue on GitHub for you
and uploads the zip privately (only maintainers can download it), then
offers Open Issue. Nothing is sent until you press Send Report. **Save Zip
Only** keeps the report on your Mac instead. If sending fails, the zip is
saved and Open GitHub Issue lets you file it by hand.

## What passes

- 0 dropped frames over two hours on your Mac, in every file and the
  stream;
- every file plays and its audio and video stay in sync;
- the crash tests recover what they say they do;
- the workflows above work without surprises (anything that surprised
  you is a bug report).
