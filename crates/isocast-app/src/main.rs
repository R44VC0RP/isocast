//! Isocast, the desktop app: a native macOS window hosting the web
//! interface (`ui/`, embedded in the binary), a thin client of `isocastd`.
//! Everything the interface does goes through the daemon's method table,
//! the same one the CLI and MCP use.
//!
//! `Isocast --snapshot out.png` renders the interface off screen and saves
//! it as a PNG, so agents can see it; `--help` lists the options.

#[cfg(target_os = "macos")]
mod assets;
#[cfg(target_os = "macos")]
mod daemon;
#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
mod options;

use std::process::ExitCode;

#[cfg(target_os = "macos")]
fn main() -> ExitCode {
    let options = match options::Options::parse(std::env::args().skip(1)) {
        Ok(options::Parsed::Run(options)) => options,
        Ok(options::Parsed::Help) => {
            print!("{}", options::HELP);
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("Isocast: {message}\n\n{}", options::HELP);
            return ExitCode::FAILURE;
        }
    };
    mac::run(options)
}

#[cfg(not(target_os = "macos"))]
fn main() -> ExitCode {
    eprintln!(
        "The Isocast app runs on macOS only for now; the `isocast` command line and its MCP \
         server control the same engine."
    );
    ExitCode::FAILURE
}
