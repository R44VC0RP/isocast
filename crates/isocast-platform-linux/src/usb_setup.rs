//! One stream setup at a time per USB device.
//!
//! A webcam's camera and microphone are interfaces of one USB device. Some
//! firmware (Logitech BRIO, 046d:085e) drops off the bus when a V4L2
//! STREAMON and a USB audio start reach it together, so setups on one
//! device are serialized; streaming stays concurrent.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;

/// Longest wait for another setup on the device, so a hung one can't push
/// this source past its open deadline.
const WAIT: Duration = Duration::from_secs(2);

static BUSY: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());
static FREED: Condvar = Condvar::new();

/// The USB device (its sysfs directory) behind a class device's `device`
/// link, such as `/sys/class/video4linux/video2/device`. `None` for non-USB
/// devices.
fn usb_device(device_link: &Path) -> Option<PathBuf> {
    // The link names the USB interface (`1-1:1.0`); its parent is the device.
    let interface = std::fs::canonicalize(device_link).ok()?;
    let device = interface.parent()?;
    device
        .join("busnum")
        .is_file()
        .then(|| device.to_path_buf())
}

/// The USB device of a V4L2 node such as `/dev/video2`.
pub(crate) fn video_node_device(node: &Path) -> Option<PathBuf> {
    let name = node.file_name()?.to_str()?;
    usb_device(
        &Path::new("/sys/class/video4linux")
            .join(name)
            .join("device"),
    )
}

/// The USB device of ALSA card `card`.
pub(crate) fn sound_card_device(card: u32) -> Option<PathBuf> {
    usb_device(&PathBuf::from(format!(
        "/sys/class/sound/card{card}/device"
    )))
}

/// Held while one stream sets up on a USB device; dropping it lets the next.
pub(crate) struct Setup {
    device: Option<PathBuf>,
}

/// Wait (at most [`WAIT`]) until no other stream is setting up on `device`,
/// then claim it. `None` (not a USB device) never waits.
pub(crate) fn begin(device: Option<PathBuf>) -> Setup {
    begin_within(device, WAIT)
}

fn begin_within(device: Option<PathBuf>, wait: Duration) -> Setup {
    let Some(device) = device else {
        return Setup { device: None };
    };
    let busy = BUSY.lock().unwrap_or_else(PoisonError::into_inner);
    let (mut busy, waited) = FREED
        .wait_timeout_while(busy, wait, |busy| busy.contains(&device))
        .unwrap_or_else(PoisonError::into_inner);
    if waited.timed_out() {
        drop(busy);
        isocast_diagnostics::output::deferred(
            "warn",
            module_path!(),
            format!(
                "USB device {} still setting up another stream after {} ms; starting this one anyway",
                device.display(),
                wait.as_millis()
            ),
        );
        // Not claimed: the holder's drop must not be undone by ours.
        return Setup { device: None };
    }
    busy.insert(device.clone());
    Setup {
        device: Some(device),
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        if let Some(device) = self.device.take() {
            BUSY.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&device);
            FREED.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Instant;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("isocast-usb-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn interfaces_resolve_to_their_usb_device() {
        let root = scratch("sysfs");
        let usb = root.join("devices/usb1/1-1");
        std::fs::create_dir_all(usb.join("1-1:1.0")).unwrap();
        std::fs::create_dir_all(usb.join("1-1:1.3")).unwrap();
        std::fs::write(usb.join("busnum"), "1\n").unwrap();
        let platform = root.join("devices/platform/isp");
        std::fs::create_dir_all(&platform).unwrap();
        let class = root.join("class");
        std::fs::create_dir_all(class.join("video2")).unwrap();
        std::fs::create_dir_all(class.join("card4")).unwrap();
        std::fs::create_dir_all(class.join("video1")).unwrap();
        std::os::unix::fs::symlink(usb.join("1-1:1.0"), class.join("video2/device")).unwrap();
        std::os::unix::fs::symlink(usb.join("1-1:1.3"), class.join("card4/device")).unwrap();
        std::os::unix::fs::symlink(&platform, class.join("video1/device")).unwrap();

        let camera = usb_device(&class.join("video2/device"));
        let microphone = usb_device(&class.join("card4/device"));
        assert_eq!(camera, Some(std::fs::canonicalize(&usb).unwrap()));
        assert_eq!(camera, microphone);
        // A platform camera (Apple's ISP) has no USB device: never waits.
        assert_eq!(usb_device(&class.join("video1/device")), None);
        assert_eq!(usb_device(&class.join("missing/device")), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn setups_on_one_device_run_one_at_a_time() {
        let device = PathBuf::from("/test/usb1/1-1");
        let inside = Arc::new(AtomicU32::new(0));
        let most = Arc::new(AtomicU32::new(0));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let (device, inside, most) = (device.clone(), inside.clone(), most.clone());
                std::thread::spawn(move || {
                    let _setup = begin_within(Some(device), Duration::from_secs(10));
                    let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(20));
                    inside.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(most.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn other_devices_and_non_usb_sources_do_not_wait() {
        let _a = begin_within(Some(PathBuf::from("/test/usb2/2-1")), Duration::ZERO);
        let started = Instant::now();
        let _b = begin_within(
            Some(PathBuf::from("/test/usb2/2-2")),
            Duration::from_secs(5),
        );
        let _c = begin_within(None, Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_stuck_setup_delays_the_next_only_up_to_the_bound() {
        let device = PathBuf::from("/test/usb3/3-1");
        let held = begin_within(Some(device.clone()), Duration::ZERO);
        let started = Instant::now();
        drop(begin_within(
            Some(device.clone()),
            Duration::from_millis(50),
        ));
        assert!(started.elapsed() >= Duration::from_millis(50));
        // The late one didn't claim the device, so dropping it left it held.
        let started = Instant::now();
        drop(begin_within(
            Some(device.clone()),
            Duration::from_millis(50),
        ));
        assert!(started.elapsed() >= Duration::from_millis(50));
        drop(held);
        let started = Instant::now();
        let _next = begin_within(Some(device), Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
