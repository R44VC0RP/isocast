//! Command-line options: none for a normal launch, or snapshot mode.

use std::path::PathBuf;

pub const HELP: &str = "\
Isocast: the desktop app for the Isocast engine (isocastd).

Usage: Isocast [--snapshot OUT.png [--size WxH] [--scale 1|2] [--wait-ms MS]
                [--route NAME] [--eval JS]]

Options:
  --snapshot OUT.png  Render the interface off screen, save it as a PNG and
                      quit, so agents can see it.
  --size WxH          Window size in points for the snapshot (1440x900).
  --scale N           Pixels per point in the PNG, 1 or 2 (2).
  --wait-ms MS        How long to wait after the interface is ready (1500).
  --route NAME        Open a view first, like `settings` or `onboarding`.
  --eval JS           Run this script in the interface once it is ready,
                      before waiting (to select an item, open a menu...).

Environment:
  ISOCAST_UI_DIR      Serve the interface from this folder instead of the
                      copy built in (for fast iteration).
  ISOCAST_SOCKET      The daemon's socket, as for the `isocast` command.
  ISOCAST_DAEMON      The isocastd to launch when none is running.
";

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub snapshot: Option<Snapshot>,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub scale: u32,
    pub wait_ms: u64,
    pub route: Option<String>,
    pub eval: Option<String>,
}

pub enum Parsed {
    Run(Options),
    Help,
}

impl Options {
    pub fn parse(args: impl Iterator<Item = String>) -> Result<Parsed, String> {
        let mut args = args.peekable();
        let mut path = None;
        let (mut width, mut height, mut scale, mut wait_ms) = (1440, 900, 2, 1500);
        let (mut route, mut eval) = (None, None);
        let mut snapshot_only = Vec::new();
        while let Some(arg) = args.next() {
            let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
            match arg.as_str() {
                "-h" | "--help" => return Ok(Parsed::Help),
                "--snapshot" => path = Some(PathBuf::from(value("--snapshot")?)),
                "--size" => {
                    let size = value("--size")?;
                    let parsed = size
                        .split_once('x')
                        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                        .filter(|&(w, h): &(u32, u32)| {
                            (320..=8192).contains(&w) && (240..=8192).contains(&h)
                        });
                    (width, height) = parsed.ok_or_else(|| {
                        format!("--size {size:?}: expected WIDTHxHEIGHT in points, like 1440x900")
                    })?;
                    snapshot_only.push("--size");
                }
                "--scale" => {
                    let text = value("--scale")?;
                    scale = match text.as_str() {
                        "1" => 1,
                        "2" => 2,
                        _ => return Err(format!("--scale {text:?}: use 1 or 2")),
                    };
                    snapshot_only.push("--scale");
                }
                "--wait-ms" => {
                    let text = value("--wait-ms")?;
                    wait_ms = text
                        .parse()
                        .map_err(|_| format!("--wait-ms {text:?}: expected milliseconds"))?;
                    snapshot_only.push("--wait-ms");
                }
                "--route" => {
                    route = Some(value("--route")?);
                    snapshot_only.push("--route");
                }
                "--eval" => {
                    eval = Some(value("--eval")?);
                    snapshot_only.push("--eval");
                }
                // macOS passes this to apps launched from Finder.
                _ if arg.starts_with("-psn_") => {}
                _ => {
                    return Err(format!(
                        "unknown option {arg:?}; options: --snapshot, --size, --scale, \
                         --wait-ms, --route, --eval, --help"
                    ));
                }
            }
        }
        let snapshot = match path {
            Some(path) => Some(Snapshot {
                path,
                width,
                height,
                scale,
                wait_ms,
                route,
                eval,
            }),
            None if !snapshot_only.is_empty() => {
                return Err(format!(
                    "{} only apply with --snapshot",
                    snapshot_only.join(", ")
                ));
            }
            None => None,
        };
        Ok(Parsed::Run(Options { snapshot }))
    }
}
