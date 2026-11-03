//! One WASAPI exclude stream, never a subtraction or unfiltered fallback.

use super::*;
use windows::Win32::Media::Audio::PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE;

const OPTIONS: &str = "use exclude=[] for no named exclusion, or use application_audio sources for the apps you want to record";

fn named_root<T>(selector: &str, roots: Vec<T>, denied: Option<u32>) -> Result<T> {
    ensure!(
        denied.is_none(),
        "system_audio.exclude[0]: cannot verify every running instance of {selector:?} (access denied); {OPTIONS}"
    );
    let mut roots = roots.into_iter();
    match (roots.next(), roots.next()) {
        (Some(root), None) => Ok(root),
        (None, _) => bail!(
            "system_audio.exclude[0]: {selector:?} must be running to identify its process tree; {OPTIONS}"
        ),
        _ => bail!(
            "system_audio.exclude[0]: {selector:?} and Isocast do not fit in one process tree rooted at either target. WASAPI can exclude just one tree and Isocast must always be excluded; {OPTIONS}"
        ),
    }
}

struct Plan {
    pid: u32,
    // The recorder's lifetime already contains the capture.
    root: Option<Owned<HANDLE>>,
    description: String,
}

fn plan(exclude: &[String]) -> Result<Plan> {
    let own = std::process::id();
    let selector = match exclude {
        [] => None,
        [selector] => Some(selector),
        _ => bail!(
            "system_audio.exclude: expected at most one named app in the same tree as Isocast. WASAPI cannot exclude multiple independent trees; {OPTIONS}"
        ),
    };
    supported().map_err(|e| {
        anyhow!("system_audio.exclude: {e:#}; use exclude=[] with monitoring off for endpoint loopback, or upgrade Windows")
    })?;
    let (pid, root, description) = if let Some(selector) = selector {
        let apps = audio_apps::audio_apps().context(
            "system_audio.exclude[0]: could not list audio apps; use exclude=[] for no named exclusion",
        )?;
        let identity = audio_apps::resolve(selector, &apps)
            .map_err(|e| anyhow!("system_audio.exclude[0]: {e:#}; {OPTIONS}"))?;
        let (roots, denied) = matching_roots(&identity, Some(own)).context("system_audio.exclude[0]: could not verify process trees; use exclude=[] for no named exclusion")?;
        let (pid, root) = named_root(selector, roots, denied)?;
        (
            pid,
            root,
            format!(
                "Isocast and {identity:?} (current tree, PID {pid}; later independent instances are not excluded)"
            ),
        )
    } else {
        (
            own,
            None,
            format!("the Isocast engine process tree (PID {own}); no other apps excluded"),
        )
    };
    Ok(Plan {
        pid,
        root,
        description: format!("All render endpoints except {description}"),
    })
}

pub(crate) fn probe_system_audio(exclude: &[String]) -> Result<String> {
    Ok(plan(exclude)?.description)
}

pub(in crate::audio) fn capture_desktop(
    exclude: &[String],
    sink: AudioSink,
    thread_start: fn(),
) -> Result<(AudioCapture, u32)> {
    let exclude = exclude.to_vec();
    let device = AudioDevice {
        id: "process-loopback:desktop".into(),
        name: "Filtered Windows desktop audio".into(),
        input: false,
        default: false,
        channels: 2,
        sample_rate: SAMPLE_RATE,
    };
    process_capture(device, sink, move |state, sink, started, running| {
        let _apt = Apartment::new()?;
        let plan = plan(&exclude)?;
        thread_start();
        let capture = activate(plan.pid, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE, state)
            .context("system_audio.exclude: could not activate filtered desktop audio; unfiltered capture is not a fallback")?;
        unsafe { capture.client.Start() }
            .context("system_audio.exclude: start process loopback")?;
        isocast_diagnostics::diagnostic!("info"; "Windows system audio opened: mode=process-loopback-exclude");
        let process = plan.root.as_ref().map(|handle| **handle);
        let _ = started.send(Ok(()));
        *running = true;
        capture.run(state, sink, process)?;
        ensure!(
            state.stopping.load(Ordering::Acquire),
            "system_audio.exclude: excluded tree root exited; reopen capture with a running tree or use exclude=[]"
        );
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclude_exactly_one_tree_never_a_common_launcher() {
        let edges = [(1, 0), (10, 1), (20, 10), (21, 20), (30, 1)];
        let plan = |ids: &[u32]| {
            named_root(
                "Tone",
                lineage_roots(&edges, ids, 100, |pid| Some(u64::from(pid))),
                None,
            )
        };
        assert_eq!(plan(&[10, 20]).unwrap(), 10);
        assert_eq!(plan(&[20, 21]).unwrap(), 20);
        assert_eq!(plan(&[20]).unwrap(), 20);
        for ids in [&[20, 30][..], &[10, 20, 30], &[]] {
            let error = plan(ids).unwrap_err().to_string();
            assert!(
                error.contains("system_audio.exclude[0]")
                    && error.contains("exclude=[]")
                    && error.contains("application_audio"),
                "{error}"
            );
        }
        assert!(named_root("Tone", vec![20], Some(30)).is_err());
        let error = probe_system_audio(&["A".into(), "B".into()])
            .unwrap_err()
            .to_string();
        assert!(error.contains("at most one") && error.contains("exclude=[]"));
        assert!(
            probe_system_audio(&[])
                .unwrap()
                .contains("no other apps excluded")
        );
    }
}
