use super::*;
use isocast_protocol::projector::{
    CloseParams, Frame, MultiviewLayout, OpenParams, Projector, ProjectorView, Projectors,
};

#[derive(clap::Subcommand)]
pub enum Command {
    /// Open a projector: the program, studio mode's preview, a scene, a
    /// video source or a multiview grid, full screen on a display or in a
    /// window. The app shows it (and opens it when it next starts if it
    /// isn't running). Opening a view that is open already moves it.
    Open {
        /// program, preview, scene, source or multiview.
        #[arg(value_parser = clap::builder::PossibleValuesParser::new(ProjectorView::NAMES))]
        view: String,
        /// The scene's id (view scene) or the video source's id (view
        /// source).
        id: Option<String>,
        /// Full screen on this display: `main`, a display's id or name
        /// (`isocast devices`).
        #[arg(long, conflicts_with = "window")]
        display: Option<String>,
        /// Full screen on the main display (or --display).
        #[arg(long, conflicts_with = "window")]
        fullscreen: bool,
        /// In a window (where this view was last, unless --frame).
        #[arg(long)]
        window: bool,
        /// The window's place: X,Y,WIDTH,HEIGHT in points from the main
        /// display's top left.
        #[arg(long, value_name = "X,Y,W,H")]
        frame: Option<String>,
        /// The multiview's grid: top-8 (the default), 3x3 or 2x2.
        #[arg(long, value_parser = clap::builder::PossibleValuesParser::new(MultiviewLayout::NAMES))]
        layout: Option<String>,
        /// The canvas: main (the default), vertical (the setup's vertical
        /// canvas) or a [[canvases]] id (`isocast canvas list`): its
        /// program, preview or a scene as it arranges it.
        #[arg(long)]
        canvas: Option<String>,
        /// A multiview's last tile shows the vertical canvas's program
        /// instead of a scene (--vertical=false takes it back out).
        #[arg(long, num_args = 0..=1, default_missing_value = "true", value_name = "BOOL")]
        vertical: Option<bool>,
        /// A multiview's last tiles show these extra canvases' programs
        /// instead of scenes: ids, comma-separated (`isocast canvas
        /// list`); --canvases= takes them back out.
        #[arg(long, value_name = "ID,…", num_args = 0..=1, default_missing_value = "")]
        canvases: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Close a projector by id (`isocast projector list`), or all.
    Close {
        id: Option<String>,
        #[arg(long, conflicts_with = "id")]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// The open projectors, where each is, and the displays.
    List {
        #[arg(long)]
        json: bool,
    },
}

fn parse_frame(text: &str) -> Result<Frame> {
    let parts: Vec<f64> = text
        .split(',')
        .map(|part| part.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .with_context(|| format!("--frame takes X,Y,WIDTH,HEIGHT in points, not {text:?}"))?;
    let [x, y, width, height] = parts[..] else {
        bail!("--frame takes four numbers, X,Y,WIDTH,HEIGHT, not {text:?}");
    };
    Ok(Frame {
        x,
        y,
        width,
        height,
    })
}

pub fn run(command: Command) -> Result<ExitCode> {
    match command {
        Command::Open {
            view,
            id,
            display,
            fullscreen,
            window,
            frame,
            layout,
            canvas,
            vertical,
            canvases,
            json,
        } => {
            let view: ProjectorView = serde_json::from_value(serde_json::json!(view))?;
            let layout = layout
                .map(|layout| serde_json::from_value(serde_json::json!(layout)))
                .transpose()?;
            let (scene, source) = match view {
                ProjectorView::Scene => (
                    Some(id.context("give the scene's id: isocast projector open scene ID")?),
                    None,
                ),
                ProjectorView::Source => (
                    None,
                    Some(
                        id.context("give the video source's id: isocast projector open source ID")?,
                    ),
                ),
                _ if id.is_some() => bail!("only scene and source projectors take an id"),
                _ => (None, None),
            };
            let params = OpenParams {
                view,
                scene,
                source,
                canvas,
                layout,
                vertical,
                canvases: canvases.map(|list| {
                    list.split(',')
                        .map(str::trim)
                        .filter(|id| !id.is_empty())
                        .map(str::to_string)
                        .collect()
                }),
                fullscreen: match (fullscreen || display.is_some(), window) {
                    (true, _) => Some(true),
                    (false, true) => Some(false),
                    (false, false) => None,
                },
                display,
                frame: frame.as_deref().map(parse_frame).transpose()?,
            };
            let projector: Projector =
                Client::connect_or_launch()?.call(Method::ProjectorOpen(Box::new(params)))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&projector)?);
            } else {
                print_one(&projector);
                if !projector.shown {
                    println!("  (shown by the Isocast app; it opens it when it starts)");
                }
            }
        }
        Command::Close { id, all, json } => {
            if id.is_none() && !all {
                bail!("give a projector's id (isocast projector list) or --all");
            }
            let list: Projectors =
                Client::connect_or_launch()?.call(Method::ProjectorClose(CloseParams {
                    id,
                    all,
                    reason: None,
                }))?;
            print_list(&list, json)?;
        }
        Command::List { json } => {
            let list: Projectors = Client::connect_or_launch()?.call(Method::ProjectorList)?;
            print_list(&list, json)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_list(list: &Projectors, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(list)?);
        return Ok(());
    }
    if list.projectors.is_empty() {
        println!("No projectors open.");
    }
    for projector in &list.projectors {
        print_one(projector);
    }
    if !list.displays.is_empty() {
        println!("Displays:");
        for display in &list.displays {
            println!(
                "  {} {:?} {}x{}{}",
                display.id,
                display.name,
                display.width,
                display.height,
                if display.main { " (main)" } else { "" }
            );
        }
    }
    Ok(())
}

pub fn print_one(projector: &Projector) {
    let what = match projector.view {
        ProjectorView::Program => "program".to_string(),
        ProjectorView::Preview => "preview".to_string(),
        ProjectorView::Scene => format!("scene {}", projector.scene.as_deref().unwrap_or("?")),
        ProjectorView::Source => {
            format!("source {}", projector.source.as_deref().unwrap_or("?"))
        }
        ProjectorView::Multiview => {
            let tiles: Vec<String> = projector
                .tile_canvases()
                .iter()
                .map(|c| c.to_string())
                .collect();
            format!(
                "multiview {}{}",
                projector.layout.unwrap_or_default().name(),
                if tiles.is_empty() {
                    String::new()
                } else {
                    format!(
                        " with the {} program{}",
                        tiles.join(", "),
                        if tiles.len() == 1 { "" } else { "s" }
                    )
                }
            )
        }
    };
    let what = if projector.canvas_id().is_main() {
        what
    } else {
        format!("{} {what}", projector.canvas)
    };
    let placement = &projector.placement;
    let place = match (placement.fullscreen, &placement.display, placement.frame) {
        (true, Some(display), _) => format!("full screen on {display}"),
        (true, None, _) => "full screen".to_string(),
        (false, _, Some(frame)) => format!(
            "window at {},{} {}x{}",
            frame.x, frame.y, frame.width, frame.height
        ),
        (false, _, None) => "window".to_string(),
    };
    println!(
        "{} {what} ({}): {place}{}",
        projector.id,
        projector.canvas,
        if projector.shown { "" } else { ", not shown" }
    );
}
