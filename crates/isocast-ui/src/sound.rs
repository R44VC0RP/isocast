//! The editor's sound while it plays (#94): the program's sound of each
//! clip (what the edited file has), from the playhead on, through the
//! setup's monitoring device (`audio.monitoring_device`).
//!
//! An AVPlayer of its own plays an AVComposition of the clips' program
//! audio, started with `setRate:time:atHostTime:` so a moment of the
//! timeline is heard when its picture is on screen: the player takes care
//! of the device's latency, the window's (a drawn frame shows a couple of
//! refreshes after the display link's time for it) is [`PICTURE_LATENCY`]. Faster than real time
//! it plays pitch-corrected (spectral); backwards it is silent. It is a
//! player of the app's own, never the engine's monitoring output, so a
//! recording's monitoring carries on as it was. Dropping a [`Sound`] stops
//! it.

use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::time::Duration;
use std::time::Instant;

/// How long after the moment the display link gives it a played picture
/// is on screen: the frames the window's Metal layer keeps queued, about
/// two refreshes at 60 Hz (measured in a real window on isocast-m2-b: the
/// sound was 37 ms early without it, 20.5 ms with 17 ms; see
/// docs/notes.md). ISOCAST_AV_OFFSET_MS overrides it.
#[cfg(target_os = "macos")]
const PICTURE_LATENCY: Duration = Duration::from_millis(37);

/// A stretch of the timeline's sound: from `start` to `end` on the
/// timeline, `file_seconds` into `path` at `start` (none: no sound there).
#[cfg_attr(
    not(target_os = "macos"),
    expect(dead_code, reason = "editor audio playback is not implemented yet")
)]
pub struct Stretch {
    pub path: Option<PathBuf>,
    pub start: f64,
    pub end: f64,
    pub file_seconds: f64,
}

/// What a [`Sound`] plays: `stretches` from timeline second `from`, which
/// was the playhead at `origin`, at `speed` times real time.
#[cfg_attr(
    not(target_os = "macos"),
    expect(dead_code, reason = "editor audio playback is not implemented yet")
)]
pub struct Plan {
    pub stretches: Vec<Stretch>,
    pub from: f64,
    pub origin: Instant,
    pub speed: f64,
    /// `audio.monitoring_device`: "default", or a device's id or name.
    pub device: String,
}

/// The sound playing; dropping it stops it.
pub struct Sound {
    _stop: std::sync::mpsc::Sender<()>,
}

impl Sound {
    /// Start playing `plan` on a thread of its own (opening the files
    /// takes a moment; it starts in step with the playhead regardless).
    #[cfg(target_os = "macos")]
    pub fn start(plan: Plan) -> Option<Self> {
        if plan.speed <= 0.0 || plan.stretches.iter().all(|s| s.path.is_none()) {
            return None;
        }
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let spawned = std::thread::Builder::new()
            .name("editor-sound".into())
            .spawn(move || {
                mac::play(&plan, &stopped);
            });
        match spawned {
            Ok(_) => Some(Self { _stop: stop }),
            Err(error) => {
                eprintln!("sound: couldn’t start its thread: {error}");
                None
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    pub fn start(_plan: Plan) -> Option<Self> {
        // No playback backend: don't allocate a waiting thread and pretend
        // it is playing sound. The editor's optional sound stays absent.
        None
    }
}

/// The picture latency, or ISOCAST_AV_OFFSET_MS's.
#[cfg(target_os = "macos")]
fn picture_latency() -> Duration {
    std::env::var("ISOCAST_AV_OFFSET_MS")
        .ok()
        .and_then(|ms| ms.trim().parse::<f64>().ok())
        .map_or(PICTURE_LATENCY, |ms| {
            Duration::from_secs_f64(ms.max(0.0) / 1000.0)
        })
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{CString, c_void};
    use std::ptr::{self, NonNull};
    use std::sync::mpsc::{Receiver, RecvTimeoutError};
    use std::time::{Duration, Instant};

    use objc2::encode::{Encode, Encoding, RefEncode};
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, Bool};
    use objc2::{class, msg_send};
    use objc2_core_foundation::{CFRetained, CFString};

    use super::Plan;
    use crate::refresh::mac::{host_clock, ticks};

    #[link(name = "AVFoundation", kind = "framework")]
    unsafe extern "C" {
        static AVAudioTimePitchAlgorithmSpectral: *const AnyObject;
    }

    /// CoreMedia's CMTime.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CMTime {
        value: i64,
        timescale: i32,
        flags: u32,
        epoch: i64,
    }

    // SAFETY: the layout above, which is CMTime's ("{?=qiIq}").
    unsafe impl Encode for CMTime {
        const ENCODING: Encoding = Encoding::Struct(
            "?",
            &[i64::ENCODING, i32::ENCODING, u32::ENCODING, i64::ENCODING],
        );
    }
    // SAFETY: a pointer to the struct above.
    unsafe impl RefEncode for CMTime {
        const ENCODING_REF: Encoding = Encoding::Pointer(&Self::ENCODING);
    }

    /// CoreMedia's CMTimeRange.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CMTimeRange {
        start: CMTime,
        duration: CMTime,
    }

    // SAFETY: the layout above ("{?={?=qiIq}{?=qiIq}}").
    unsafe impl Encode for CMTimeRange {
        const ENCODING: Encoding = Encoding::Struct("?", &[CMTime::ENCODING, CMTime::ENCODING]);
    }

    #[link(name = "CoreMedia", kind = "framework")]
    unsafe extern "C" {
        fn CMTimeMakeWithSeconds(seconds: f64, preferred_timescale: i32) -> CMTime;
        fn CMTimeGetSeconds(time: CMTime) -> f64;
        fn CMClockMakeHostTimeFromSystemUnits(host_time: u64) -> CMTime;
    }

    /// Core Audio's property address.
    #[repr(C)]
    struct Address {
        selector: u32,
        scope: u32,
        element: u32,
    }

    #[link(name = "CoreAudio", kind = "framework")]
    unsafe extern "C" {
        fn AudioObjectGetPropertyDataSize(
            object: u32,
            address: *const Address,
            qualifier_size: u32,
            qualifier: *const c_void,
            size: *mut u32,
        ) -> i32;
        fn AudioObjectGetPropertyData(
            object: u32,
            address: *const Address,
            qualifier_size: u32,
            qualifier: *const c_void,
            size: *mut u32,
            data: *mut c_void,
        ) -> i32;
    }

    const fn fourcc(code: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*code)
    }

    /// AVPlayerItemStatus.
    const READY: isize = 1;
    const FAILED: isize = 2;

    fn ns_string(text: &str) -> Option<Retained<AnyObject>> {
        let text = CString::new(text).ok()?;
        // SAFETY: a documented class method with a valid C string.
        unsafe { msg_send![class!(NSString), stringWithUTF8String: text.as_ptr()] }
    }

    fn seconds(value: f64) -> CMTime {
        // SAFETY: a pure CoreMedia function.
        unsafe { CMTimeMakeWithSeconds(value, 48_000) }
    }

    /// Play `plan` until `stop` says so (or its sender is dropped).
    pub fn play(plan: &Plan, stop: &Receiver<()>) {
        let player = objc2::rc::autoreleasepool(|_| {
            // SAFETY: AVFoundation's documented methods on valid objects.
            unsafe { start(plan) }
        });
        let Some((player, _assets)) = player else {
            return;
        };
        while let Err(RecvTimeoutError::Timeout) = stop.recv_timeout(Duration::from_secs(1)) {}
        objc2::rc::autoreleasepool(|_| {
            // SAFETY: as above.
            unsafe {
                let _: () = msg_send![&*player, pause];
                let _: () =
                    msg_send![&*player, replaceCurrentItemWithPlayerItem: ptr::null::<AnyObject>()];
            }
        });
    }

    /// Build the composition and the player, wait until it can play, and
    /// start it in step with the playhead.
    unsafe fn start(plan: &Plan) -> Option<(Retained<AnyObject>, Vec<Retained<AnyObject>>)> {
        // SAFETY: as in `play`.
        unsafe {
            let composition: Retained<AnyObject> =
                msg_send![class!(AVMutableComposition), composition];
            let audio = ns_string("soun")?;
            let track: Option<Retained<AnyObject>> = msg_send![
                &*composition,
                addMutableTrackWithMediaType: &*audio,
                preferredTrackID: 0i32
            ];
            let track = track?;
            let mut any = false;
            // The files, alive as long as the player.
            let mut assets = Vec::new();
            for stretch in &plan.stretches {
                let at = seconds((stretch.start - plan.from).max(0.0));
                let length = (stretch.end - stretch.start).max(0.0);
                let Some(path) = &stretch.path else { continue };
                let Some((asset, source)) = audio_track(path) else {
                    eprintln!("sound: no sound in {}", path.display());
                    continue;
                };
                // Within the file's sound (the rest stays silent).
                let range: CMTimeRange = msg_send![&*source, timeRange];
                let file_end = CMTimeGetSeconds(range.start) + CMTimeGetSeconds(range.duration);
                let from = stretch.file_seconds.max(0.0);
                let skip = from - stretch.file_seconds;
                let length = (length - skip).min(file_end - from);
                if length <= 0.0 {
                    continue;
                }
                let at = match skip > 0.0 {
                    true => seconds((stretch.start - plan.from).max(0.0) + skip),
                    false => at,
                };
                let range = CMTimeRange {
                    start: seconds(from),
                    duration: seconds(length),
                };
                let mut error: *mut AnyObject = ptr::null_mut();
                let inserted: Bool = msg_send![
                    &*track,
                    insertTimeRange: range,
                    ofTrack: &*source,
                    atTime: at,
                    error: &mut error
                ];
                assets.push(asset);
                if inserted.as_bool() {
                    any = true;
                } else {
                    let why: Option<Retained<AnyObject>> = match error.is_null() {
                        true => None,
                        false => msg_send![&*error, localizedDescription],
                    };
                    let why = why.map(|w| {
                        let text: *const std::ffi::c_char = msg_send![&*w, UTF8String];
                        std::ffi::CStr::from_ptr(text)
                            .to_string_lossy()
                            .into_owned()
                    });
                    eprintln!(
                        "sound: couldn’t use the sound of {}: {}",
                        path.display(),
                        why.unwrap_or_default()
                    );
                }
            }
            if !any {
                return None;
            }
            let item: Retained<AnyObject> =
                msg_send![class!(AVPlayerItem), playerItemWithAsset: &*composition];
            if !AVAudioTimePitchAlgorithmSpectral.is_null() {
                let _: () = msg_send![&*item, setAudioTimePitchAlgorithm: AVAudioTimePitchAlgorithmSpectral];
            }
            let player: Retained<AnyObject> =
                msg_send![class!(AVPlayer), playerWithPlayerItem: &*item];
            let _: () = msg_send![&*player, setAutomaticallyWaitsToMinimizeStalling: Bool::NO];
            if let Some(uid) = device_uid(&plan.device) {
                let uid = ns_string(&uid)?;
                let _: () = msg_send![&*player, setAudioOutputDeviceUniqueID: &*uid];
            }
            // Ready (it opens the files), within a second.
            let until = Instant::now() + Duration::from_secs(2);
            loop {
                let status: isize = msg_send![&*item, status];
                if status == READY {
                    break;
                }
                if status == FAILED || Instant::now() > until {
                    eprintln!("sound: the player couldn’t start (status {status})");
                    return None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            // A moment from now, the sound of the picture on screen then:
            // the playhead's then, a picture latency ago.
            let now = Instant::now();
            let (host_now, _, _) = host_clock();
            let lead = Duration::from_millis(40);
            let when = host_now + ticks(lead);
            let since = (now + lead)
                .saturating_duration_since(plan.origin)
                .saturating_sub(super::picture_latency());
            let item_time = since.as_secs_f64() * plan.speed;
            let _: () = msg_send![
                &*player,
                setRate: plan.speed as f32,
                time: seconds(item_time),
                atHostTime: CMClockMakeHostTimeFromSystemUnits(when)
            ];
            if std::env::var_os("ISOCAST_PREVIEW_STATS").is_some() {
                eprintln!(
                    "sound: {:.3} s of the timeline at {:.1} ms from now ({}x)",
                    plan.from + item_time,
                    lead.as_secs_f64() * 1000.0,
                    plan.speed
                );
            }
            Some((player, assets))
        }
    }

    /// The file at `path` and its first sound track (the track needs its
    /// asset alive).
    unsafe fn audio_track(
        path: &std::path::Path,
    ) -> Option<(Retained<AnyObject>, Retained<AnyObject>)> {
        // SAFETY: as in `play`.
        unsafe {
            let path = ns_string(path.to_str()?)?;
            let url: Option<Retained<AnyObject>> =
                msg_send![class!(NSURL), fileURLWithPath: &*path];
            let asset: Option<Retained<AnyObject>> = msg_send![
                class!(AVURLAsset),
                URLAssetWithURL: &*url?,
                options: ptr::null::<AnyObject>()
            ];
            let asset = asset?;
            let audio = ns_string("soun")?;
            let tracks: Option<Retained<AnyObject>> =
                msg_send![&*asset, tracksWithMediaType: &*audio];
            let track: Option<Retained<AnyObject>> = msg_send![&*tracks?, firstObject];
            Some((asset, track?))
        }
    }

    /// The Core Audio UID of the output `query` names ("default": none, the
    /// player follows the system output), matched as the engine matches
    /// its monitoring device: id, name, name ignoring case, or the
    /// shortest name containing it.
    fn device_uid(query: &str) -> Option<String> {
        if query == "default" {
            return None;
        }
        let devices: Vec<(String, String)> = devices()
            .into_iter()
            .filter(|device| has_output(*device))
            .filter_map(|device| Some((string(device, b"uid ")?, string(device, b"lnam")?)))
            .collect();
        let lower = query.to_lowercase();
        let found = devices
            .iter()
            .find(|(uid, name)| uid == query || name == query)
            .or_else(|| {
                devices
                    .iter()
                    .find(|(_, name)| name.to_lowercase() == lower)
            })
            .or_else(|| {
                devices
                    .iter()
                    .filter(|(_, name)| name.to_lowercase().contains(&lower))
                    .min_by_key(|(_, name)| name.len())
            });
        if found.is_none() {
            eprintln!("sound: no output matches {query:?}; playing on the system output");
        }
        found.map(|(uid, _)| uid.clone())
    }

    fn address(selector: &[u8; 4], scope: &[u8; 4]) -> Address {
        Address {
            selector: fourcc(selector),
            scope: fourcc(scope),
            element: 0,
        }
    }

    /// The size of a property, if it has one.
    fn size(object: u32, address: &Address) -> Option<u32> {
        let mut size = 0u32;
        // SAFETY: a valid address and out pointer.
        let status =
            unsafe { AudioObjectGetPropertyDataSize(object, address, 0, ptr::null(), &mut size) };
        (status == 0).then_some(size)
    }

    fn devices() -> Vec<u32> {
        let address = address(b"dev#", b"glob");
        let Some(mut bytes) = size(1, &address) else {
            return Vec::new();
        };
        let mut ids = vec![0u32; bytes as usize / 4];
        // SAFETY: `ids` holds `bytes` bytes.
        let status = unsafe {
            AudioObjectGetPropertyData(
                1,
                &address,
                0,
                ptr::null(),
                &mut bytes,
                ids.as_mut_ptr().cast(),
            )
        };
        if status != 0 {
            return Vec::new();
        }
        ids.truncate(bytes as usize / 4);
        ids
    }

    fn has_output(device: u32) -> bool {
        size(device, &address(b"stm#", b"outp")).is_some_and(|bytes| bytes > 0)
    }

    /// A string property of `device` (Core Audio hands over a retained
    /// CFString).
    fn string(device: u32, selector: &[u8; 4]) -> Option<String> {
        let address = address(selector, b"glob");
        let mut value: *const CFString = ptr::null();
        let mut bytes = size_of::<*const CFString>() as u32;
        // SAFETY: a pointer-sized out value.
        let status = unsafe {
            AudioObjectGetPropertyData(
                device,
                &address,
                0,
                ptr::null(),
                &mut bytes,
                (&mut value as *mut *const CFString).cast(),
            )
        };
        if status != 0 {
            return None;
        }
        // SAFETY: Core Audio returned a +1 CFString.
        let string = unsafe { CFRetained::from_raw(NonNull::new(value.cast_mut())?) };
        Some(string.to_string())
    }
}
