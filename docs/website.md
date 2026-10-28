# isocast.com

The site is one static page in `website/public/`: hand-written HTML, CSS and
ES modules, with no framework and no build step, served as is from the domain
root. `website/` around it holds only the deploy config.

## Hosting

isocast.com is a Cloudflare Worker with static assets and no code of its own:
`isocast-site`, on the work (anoma.ly) account, in the zone isocast.com, which
has Always Use HTTPS on (http answers 301 to https). It serves isocast.com and
www.isocast.com. The config, in `website/`:

- `cloudflare.config.ts`: the Worker's name, compatibility date, domains and
  observability, and how assets are served: `notFoundHandling: "404-page"`
  (a missing path gets `404.html` with status 404) and
  `htmlHandling: "auto-trailing-slash"`;
- `wrangler.config.ts`: the assets directory, `public/`;
- `package.json` and `package-lock.json`: the pinned `cf` and `wrangler`
  that deploy it, with the two commands below.

Only `public/` is uploaded. `node_modules/` and the build output
`.cloudflare/` stay out of git (`website/.gitignore`) and out of the upload.

To deploy, from `website/`, with the `cf` CLI's `work` profile signed in:

```sh
npm ci            # once, and after package-lock.json changes
npm run check     # cf deploy --dry-run --profile work: builds, uploads nothing
npm run deploy    # cf deploy --profile work
```

The check builds `.cloudflare/output/`, whose `assets/` is an exact copy of
`public/`, and ends with "--dry-run: exiting now.". Deploy from main, after
the change has landed.

## Files the site serves

Everything in `website/public/`:

- `index.html`, `404.html`, `robots.txt`, `sitemap.xml`, `site.webmanifest`,
  `favicon.ico` and `_headers` (response headers, which Cloudflare applies
  and doesn't serve as a file);
- `css/site.css`;
- `js/main.js`, `js/hero.js`, `js/files.js`, `js/slab.js`, `js/spring.js`,
  `js/take.js`;
- `fonts/isocast-sans.woff2` with `fonts/LICENSE-mona-sans.txt` (the font is a
  renamed subset of Mona Sans, see `fonts/README.md`);
- `img/` (icons, `favicon.svg`, `og.jpg`);
- `media/take-<hash>/` (the hero's loops, posters and mic peaks).

`_headers` sets a Content-Security-Policy that allows only the site's own
files (no inline scripts or `style` attributes: keep it that way, or the
policy has to change), `nosniff`, a referrer policy and framing denial. HTML,
CSS and JS are revalidated on every load because their names don't change;
`media/` is cached for a year as immutable, which is why each cut of the take
gets a new folder.

## Preview

The LaunchAgent `dev.isocast.website` serves the website worktree's
`website/public/` (`.claude/worktrees/website/website/public`) on
127.0.0.1:8790, published on the tailnet as https://isocast-site.i.vogel.dev
(OpenCan). It applies no `_headers` and has no 404 page; those show on the
deployed site (`curl -sI https://isocast.com/nope`).

## A new release

The page names the version once: `data-version` on the meta line in
`public/index.html`. CSS writes "Version X.Y.Z" from it and `js/main.js`
points both Download links at
`releases/download/vX.Y.Z/Isocast-X.Y.Z-arm64.dmg` (without scripts they open
the latest release's page). After a release is published, bump that
attribute, check the DMG link, and deploy once it's on main (docs/releasing.md,
step 6). Check the page's claims against the release notes at the same time.

## The hero's take

The hero's slabs play one take of Isocast's own files, cut to a short silent
loop: `program`, `screen` and `camera` as video, `mic` as a waveform drawn
from peaks of the program's audio. `LAYERS` in `website/public/js/take.js` is the one
place to change what each layer shows (a looping video, a still image, or the
peaks); `TAKE` there holds the loop's start, length and the take's total
length, which the timecodes use.

### What to record

The current loops (from 2026-10-22) show an older version of this page on
the screen, with its old copy erased (see "Cutting the loops"), which leaves
the screen's left side empty. To replace them, record a new Isocast take:

- The program plus ISO files for the screen and the camera, at 60 fps, with
  the mic in the program. At least 20 s; the loop is cut from a 3 to 4 s
  stretch of it.
- On the screen: one app filling the display, such as the Isocast window or a
  terminal running `isocast status`, not a browser and not this website (a
  recording of the site puts the site's own text and animation inside the
  loop). Turn on Do Not Disturb, and keep the menu bar, Dock, notifications,
  other windows and any private text out of the part of the screen that will
  be cropped in.
- Somewhere in the take, 4 s or more where the screen barely changes while
  you talk to the camera: that's the stretch the loop is cut from, since a
  loop can only be seamless where the screen holds still.
- Hand over the take folder; nothing is published from the original files.

### Cutting the loops

Copy the take's files somewhere private, choose the loop's start `S` (s) and
length `L`, measure the crops (the page area of the screen, at 1080p for the
program and the same window doubled for a 4K screen file), then run the
commands below.

The 2026-10-22 take recorded an older version of this site, so its screen
shows the old headline, line, Download button, version and diagram title.
The hero must never show copy that disagrees with the page, so `ERASE_*`
paints those regions over with the surrounding background (ffmpeg's
`delogo`, which interpolates from the region's edges; the page holds still
over the loop, so the fill is steady). A take that doesn't show the site
needs no erasing: leave both empty.

```zsh
TAKE=/path/to/take-copy          # program.mov, screen.mov, camera.mov
S=20.1667 L=3.4 X=0.4            # start, length, crossfade at the seam
PROG_CROP="crop=1424:801:474:225," SCR_CROP="crop=2848:1602:948:450,"
ERASE_PROG="delogo=x=494:y=284:w=374:h=282,delogo=x=678:y=854:w=150:h=26,"
ERASE_SCR="delogo=x=988:y=568:w=748:h=564,delogo=x=1356:y=1708:w=300:h=52,"
OUT=$(mktemp -d)

loop() { # name input crop width h264-crf tune vp9-crf
  local g="[0:v]${3}scale=${4}:$(($4 * 9 / 16)):flags=lanczos,fps=60,setpts=PTS-STARTPTS,split=3[a][b][c];"
  g+="[a]trim=start=${X}:end=${L},setpts=PTS-STARTPTS[main];"
  g+="[b]trim=start=${L}:end=$((L + X)),setpts=PTS-STARTPTS[tail];"
  g+="[c]trim=start=0:end=${X},setpts=PTS-STARTPTS[head];"
  g+="[tail][head]xfade=transition=fade:duration=${X}:offset=0[x];[x][main]concat=n=2:v=1:a=0,format=yuv420p[v]"
  ffmpeg -v error -y -ss $S -t $((L + X + 0.2)) -i "$2" -filter_complex "$g" -map "[v]" -an \
    -c:v libx264 -crf 8 -preset fast -g 60 "$OUT/.$1-$4.mov"
  ffmpeg -v error -y -i "$OUT/.$1-$4.mov" -an -c:v libx264 -preset veryslow -profile:v high -crf $5 \
    -tune $6 -g 30 -keyint_min 30 -sc_threshold 0 -pix_fmt yuv420p -movflags +faststart "$OUT/$1-$4.mp4"
  ffmpeg -v error -y -i "$OUT/.$1-$4.mov" -an -c:v libvpx-vp9 -b:v 0 -crf $7 -row-mt 1 \
    -deadline good -cpu-used 1 -g 30 -pix_fmt yuv420p "$OUT/$1-$4.webm"
  ffmpeg -v error -y -i "$OUT/.$1-$4.mov" -frames:v 1 -update 1 "$OUT/.$1-$4.png"
  cwebp -quiet -q 82 -m 6 "$OUT/.$1-$4.png" -o "$OUT/$1-$4.webp"
  rm "$OUT/.$1-$4.mov" "$OUT/.$1-$4.png"
}
for W in 960 640; do
  loop program "$TAKE/program.mov" "$ERASE_PROG$PROG_CROP" $W 20 film 29
  loop screen  "$TAKE/screen.mov"  "$ERASE_SCR$SCR_CROP"   $W 19 animation 28
  loop camera  "$TAKE/camera.mov"  ""           $W 21 film 30
done

# Mic peaks: 4 per frame over the loop (with the same crossfade) and 30 a
# second over the whole take. Only numbers are published, never audio.
uv run --no-project --with numpy python - "$TAKE/program.mov" "$OUT/mic-peaks.json" $S $L $X <<'PY'
import json, subprocess, sys
import numpy as np
src, out, S, L, X = sys.argv[1], sys.argv[2], *map(float, sys.argv[3:])
pcm = lambda *a: np.abs(np.frombuffer(subprocess.run(["ffmpeg", "-v", "error", *a, "-i", src, "-ac", "1",
      "-ar", "48000", "-f", "f32le", "-"], capture_output=True, check=True).stdout, np.float32))
a, full = pcm("-ss", str(S), "-t", str(L + X + 0.1)), pcm()
peak = lambda x, i0, n, b: np.array([x[i0 + k * b:i0 + (k + 1) * b].max() for k in range(n)])
n, nx = round(L * 240), round(X * 240)
loop = peak(a, 0, n, 200)
w = np.arange(nx) / nx
loop[:nx] = peak(a, round(L * 48000), nx, 200) * (1 - w) + loop[:nx] * w
ref = np.percentile(loop, 99.5)
q = lambda v: np.clip(np.round(v / ref * 100), 0, 100).astype(int).tolist()
json.dump({"start": S, "length": L, "total": round(len(full) / 48000, 3), "perSecond": 240, "peaks": q(loop),
           "overviewPerSecond": 30, "overview": q(peak(full, 0, len(full) // 1600, 1600))},
          open(out, "w"), separators=(",", ":"))
PY

DIR=website/public/media/take-$(cat "$OUT"/* | shasum -a 256 | cut -c1-8)
mkdir -p "$DIR" && cp "$OUT"/* "$DIR"/
```

Then point `DIR` in `js/take.js` at the new folder, set `TAKE`, delete the old
folder, and check before publishing:

- the first, middle and last frame of every loop, zoomed, show nothing
  private;
- the loop's seam is invisible and the three videos stay in sync (the page's
  `window.__isocast.sample()` measures frame times);
- each mp4 is under 600 KB at 960 wide, with no audio stream
  (`ffprobe -show_streams`).
