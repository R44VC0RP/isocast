//! Read-only, non-prompting macOS diagnostics. No capture devices are opened.
use objc2::rc::autoreleasepool;
use objc2_foundation::NSProcessInfo;
use serde_json::{Value, json};

use std::sync::atomic::{AtomicU64, Ordering};
static DEVICE_CHANGES: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// Notifications make idle inventory collection free. A once/minute fallback
/// catches supported-format changes that drivers fail to notify about.
pub struct DeviceChanges {
    observers: Vec<
        objc2::rc::Retained<objc2::runtime::ProtocolObject<dyn objc2_foundation::NSObjectProtocol>>,
    >,
    audio: objc2_core_audio::AudioObjectPropertyAddress,
    last: [u64; 4],
}
impl Default for DeviceChanges {
    fn default() -> Self {
        use objc2_av_foundation::{
            AVCaptureDeviceWasConnectedNotification, AVCaptureDeviceWasDisconnectedNotification,
        };
        use objc2_foundation::{NSNotification, NSNotificationCenter};
        let center = NSNotificationCenter::defaultCenter();
        let mut observers = Vec::new();
        for (i, name) in [unsafe { AVCaptureDeviceWasConnectedNotification }, unsafe {
            AVCaptureDeviceWasDisconnectedNotification
        }]
        .into_iter()
        .enumerate()
        {
            let callback = block2::RcBlock::new(move |_: std::ptr::NonNull<NSNotification>| {
                DEVICE_CHANGES[i].fetch_add(1, Ordering::Relaxed);
            });
            observers.push(unsafe {
                center.addObserverForName_object_queue_usingBlock(Some(name), None, None, &callback)
            });
        }
        let mut audio = objc2_core_audio::AudioObjectPropertyAddress {
            mSelector: objc2_core_audio::kAudioHardwarePropertyDevices,
            mScope: objc2_core_audio::kAudioObjectPropertyScopeGlobal,
            mElement: 0,
        };
        let status = unsafe {
            objc2_core_audio::AudioObjectAddPropertyListener(
                1,
                std::ptr::NonNull::from(&mut audio),
                Some(audio_devices_changed),
                std::ptr::null_mut(),
            )
        };
        let display = unsafe {
            objc2_core_graphics::CGDisplayRegisterReconfigurationCallback(
                Some(displays_changed),
                std::ptr::null_mut(),
            )
        }
        .0;
        isocast_diagnostics::emit(
            "info",
            "devices",
            "device observers registered",
            json!({"core_audio_status":status,"core_graphics_status":display}),
        );
        Self {
            observers,
            audio,
            last: [0; 4],
        }
    }
}
impl DeviceChanges {
    pub fn poll(&mut self) -> Option<Value> {
        let current = std::array::from_fn(|i| DEVICE_CHANGES[i].load(Ordering::Relaxed));
        if current == self.last {
            return None;
        }
        let change = json!({"camera_connected":current[0]-self.last[0],"camera_disconnected":current[1]-self.last[1],"audio_topology":current[2]-self.last[2],"display_configuration":current[3]-self.last[3]});
        self.last = current;
        Some(change)
    }
}
impl Drop for DeviceChanges {
    fn drop(&mut self) {
        crate::camera::unwatch(&self.observers);
        unsafe {
            objc2_core_audio::AudioObjectRemovePropertyListener(
                1,
                std::ptr::NonNull::from(&mut self.audio),
                Some(audio_devices_changed),
                std::ptr::null_mut(),
            );
            objc2_core_graphics::CGDisplayRemoveReconfigurationCallback(
                Some(displays_changed),
                std::ptr::null_mut(),
            );
        }
    }
}
unsafe extern "C-unwind" fn audio_devices_changed(
    _: u32,
    _: u32,
    _: std::ptr::NonNull<objc2_core_audio::AudioObjectPropertyAddress>,
    _: *mut c_void,
) -> i32 {
    DEVICE_CHANGES[2].fetch_add(1, Ordering::Relaxed);
    0
}
unsafe extern "C-unwind" fn displays_changed(
    _: u32,
    _: objc2_core_graphics::CGDisplayChangeSummaryFlags,
    _: *mut c_void,
) {
    DEVICE_CHANGES[3].fetch_add(1, Ordering::Relaxed);
}
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_void};
use std::mem::{MaybeUninit, size_of};
use std::path::Path;
use std::time::Instant;

pub fn encoder_metrics() -> Value {
    crate::stream::diagnostics()
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
}
#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPSCopyPowerSourcesInfo() -> *const c_void;
    fn IOPSGetProvidingPowerSourceType(info: *const c_void) -> *const c_void;
    fn IOServiceMatching(name: *const i8) -> *mut c_void;
    fn IOServiceGetMatchingService(port: u32, matching: *mut c_void) -> u32;
    fn IORegistryEntryCreateIterator(
        entry: u32,
        plane: *const i8,
        options: u32,
        iterator: *mut u32,
    ) -> i32;
    fn IOObjectConformsTo(object: u32, class: *const i8) -> bool;
    fn IOIteratorNext(iterator: u32) -> u32;
    fn IOObjectRelease(object: u32) -> i32;
    fn IORegistryEntryCreateCFProperty(
        entry: u32,
        key: *const c_void,
        allocator: *const c_void,
        options: u32,
    ) -> *const c_void;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: *const c_void);
    fn CFStringCreateWithCString(
        allocator: *const c_void,
        string: *const i8,
        encoding: u32,
    ) -> *const c_void;
    fn CFGetTypeID(value: *const c_void) -> usize;
    fn CFArrayGetTypeID() -> usize;
    fn CFDictionaryGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFArrayGetCount(array: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
    fn CFDictionaryGetValue(dict: *const c_void, key: *const c_void) -> *const c_void;
    fn CFNumberGetValue(number: *const c_void, kind: isize, value: *mut c_void) -> bool;
    fn CFStringGetCString(
        value: *const c_void,
        buffer: *mut i8,
        size: isize,
        encoding: u32,
    ) -> bool;
}

pub fn permissions() -> Value {
    json!({"capture": crate::devices::permissions().map(|p|serde_json::to_value(p).unwrap_or_default()).unwrap_or_else(|e|json!({"error":e.to_string()})),
        "accessibility":unsafe { AXIsProcessTrusted() },"accessibility_scope":"this process; no request made"})
}

pub fn inventory() -> Value {
    autoreleasepool(|_| {
        crate::devices::inventory()
            .map(|d| serde_json::to_value(d).unwrap_or_default())
            .unwrap_or_else(|e| json!({"error":e.to_string()}))
    })
}

pub fn power() -> Value {
    autoreleasepool(|_| {
        let process = NSProcessInfo::processInfo();
        let mut source = None;
        // IOPS owns info; the type is borrowed from it until CFRelease.
        unsafe {
            let info = IOPSCopyPowerSourcesInfo();
            if !info.is_null() {
                let kind = IOPSGetProvidingPowerSourceType(info);
                let mut buffer = [0i8; 128];
                if !kind.is_null()
                    && CFStringGetCString(
                        kind,
                        buffer.as_mut_ptr(),
                        buffer.len() as isize,
                        0x08000100,
                    )
                {
                    source = Some(
                        CStr::from_ptr(buffer.as_ptr())
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
                CFRelease(info);
            }
        }
        json!({"thermal_state":process.thermalState().0,"thermal_states":["nominal","fair","serious","critical"],
            "low_power_mode":process.isLowPowerModeEnabled(),"source":source})
    })
}

fn usage(pid: i32) -> Option<libc::rusage_info_v4> {
    let mut value = MaybeUninit::<libc::rusage_info_v4>::zeroed();
    // libproc writes exactly flavor 4's public structure, not a pointer.
    let result = unsafe { libc::proc_pid_rusage(pid, 4, value.as_mut_ptr().cast()) };
    (result == 0).then(|| unsafe { value.assume_init() })
}

pub fn peer(stream: &std::os::unix::net::UnixStream) -> Value {
    use std::os::fd::AsRawFd;
    let mut pid: libc::pid_t = 0;
    let mut size = size_of::<libc::pid_t>() as libc::socklen_t;
    // LOCAL_PEERPID (0x002), SOL_LOCAL (0): public sys/un.h.
    if unsafe { libc::getsockopt(stream.as_raw_fd(), 0, 2, (&raw mut pid).cast(), &mut size) } == 0
    {
        json!({"pid":pid,"executable":process_name(pid).and_then(|p|Path::new(&p).file_name().map(|s|s.to_string_lossy().into_owned()))})
    } else {
        json!({"pid":null,"reason":std::io::Error::last_os_error().to_string()})
    }
}

fn process_name(pid: i32) -> Option<String> {
    let mut buffer = [0i8; 4096];
    let n = unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    (n > 0).then(|| {
        unsafe { CStr::from_ptr(buffer.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    })
}

fn pids() -> Vec<i32> {
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let mut pids = vec![0i32; (count.max(0) as usize + 64).min(65536)];
    let n = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr().cast(),
            (pids.len() * size_of::<i32>()) as i32,
        )
    };
    pids.truncate(n.max(0) as usize);
    pids
}

fn is_vt_service(pid: i32) -> bool {
    let mut name = [0i8; 64];
    if unsafe { libc::proc_name(pid, name.as_mut_ptr().cast(), name.len() as u32) } <= 0 {
        return false;
    }
    matches!(
        unsafe { CStr::from_ptr(name.as_ptr()) }.to_bytes(),
        b"VTEncoderXPCService" | b"VTDecoderXPCService"
    )
}

#[derive(Default)]
pub struct Sampler {
    previous: HashMap<i32, (u64, u64, u64)>,
    at: Option<Instant>,
    vt: Vec<i32>,
    scans: u64,
    gpu: GpuMeter,
}

impl Sampler {
    pub fn sample(&mut self, folder: &Path) -> Value {
        let now = Instant::now();
        let seconds = self.at.map(|t| now.duration_since(t).as_secs_f64());
        self.at = Some(now);
        // Discover services every 5 s, read already known PIDs every second.
        if self.scans.is_multiple_of(5) {
            self.vt = pids()
                .into_iter()
                .filter(|pid| is_vt_service(*pid))
                .collect();
            self.gpu.discover();
        }
        self.scans += 1;
        let pid = std::process::id() as i32;
        let process = self.process(pid, seconds);
        let relay_pid = isocast_diagnostics::native::pid().map(|p| p as i32);
        let relay = relay_pid.map(|p| self.process(p, seconds));
        let total_cpu = process["cpu_percent_one_core"]
            .as_f64()
            .and_then(|cpu| match &relay {
                Some(r) => r["cpu_percent_one_core"].as_f64().map(|other| cpu + other),
                None => Some(cpu),
            });
        let vt = self
            .vt
            .clone()
            .into_iter()
            .map(|pid| self.process(pid, seconds))
            .collect::<Vec<_>>();
        self.previous
            .retain(|p, _| *p == pid || Some(*p) == relay_pid || self.vt.contains(p));
        let mut task = MaybeUninit::<libc::proc_taskinfo>::zeroed();
        let threads = unsafe {
            (libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTASKINFO,
                0,
                task.as_mut_ptr().cast(),
                size_of::<libc::proc_taskinfo>() as i32,
            ) == size_of::<libc::proc_taskinfo>() as i32)
                .then(|| task.assume_init().pti_threadnum)
        };
        let fd_bytes =
            unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        // The size-only query is an allocation estimate (45 slots for a real
        // three-FD process), not the number of open files. Count returned rows.
        let open_files = if fd_bytes > 0 {
            let mut fds = vec![
                MaybeUninit::<libc::proc_fdinfo>::zeroed();
                fd_bytes as usize / size_of::<libc::proc_fdinfo>() + 16
            ];
            let bytes = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDLISTFDS,
                    0,
                    fds.as_mut_ptr().cast(),
                    (fds.len() * size_of::<libc::proc_fdinfo>()) as i32,
                )
            };
            (bytes > 0).then_some(bytes as usize / size_of::<libc::proc_fdinfo>())
        } else {
            None
        };
        let disk = crate::volume(folder).map(|v|json!({"free_bytes":v.free_bytes,"file_system":v.file_system,"network":v.network})).unwrap_or_else(|e|json!({"error":e.to_string()}));
        json!({"process":process,"native_relay":relay,"daemon_total_cpu_percent_one_core":total_cpu,"threads":threads,"open_files":open_files,
            "vt_services":vt,"vt_scope":"system-wide; service attribution to a client is not available through libproc",
            "gpu_time_ns":self.gpu.total(),"gpu_scope":"AGX accumulatedGPUTime for this PID; null when driver counters are unavailable",
            "disk":disk,"power":power()})
    }

    fn process(&mut self, pid: i32, seconds: Option<f64>) -> Value {
        let Some(r) = usage(pid) else {
            self.previous.remove(&pid);
            return json!({"pid":pid,"unavailable":true});
        };
        // proc_pid_rusage CPU values are Mach ticks, unlike getrusage/timeval
        // and CLOCK_THREAD_CPUTIME_ID. Apple silicon commonly uses 125/3 ns.
        let ticks = r.ri_user_time.saturating_add(r.ri_system_time);
        let Some(cpu) = mach_nanoseconds(ticks) else {
            return json!({"pid":pid,"cpu_mach_ticks":ticks,"cpu_ns":null,"cpu_percent_one_core":null,"reason":"Mach timebase unavailable"});
        };
        let previous = self.previous.insert(
            pid,
            (r.ri_proc_start_abstime, cpu, r.ri_diskio_byteswritten),
        );
        let delta = previous
            .filter(|p| p.0 == r.ri_proc_start_abstime && cpu >= p.1)
            .zip(seconds.filter(|s| *s > 0.0));
        json!({"pid":pid,"start_abstime":r.ri_proc_start_abstime,"cpu_percent_one_core":delta.map(|(p,s)|(cpu-p.1) as f64/1e9/s*100.0),
            "footprint_bytes":r.ri_phys_footprint,"rss_bytes":r.ri_resident_size,"cpu_ns":cpu,"instructions":r.ri_instructions,
            "disk_bytes_written":r.ri_diskio_byteswritten,"write_bytes_per_second":delta.map(|(p,s)|r.ri_diskio_byteswritten.saturating_sub(p.2) as f64/s)})
    }
}

fn mach_nanoseconds(ticks: u64) -> Option<u64> {
    static BASE: std::sync::OnceLock<Option<(u32, u32)>> = std::sync::OnceLock::new();
    let (numer, denom) = (*BASE.get_or_init(|| {
        let mut base =
            std::mem::MaybeUninit::<mach2::mach_time::mach_timebase_info_data_t>::zeroed();
        if unsafe { mach2::mach_time::mach_timebase_info(base.as_mut_ptr()) } != 0 {
            return None;
        }
        let base = unsafe { base.assume_init() };
        (base.numer > 0 && base.denom > 0).then_some((base.numer, base.denom))
    }))?;
    Some(
        ((u128::from(ticks) * u128::from(numer)) / u128::from(denom)).min(u128::from(u64::MAX))
            as u64,
    )
}

/// Cache only this daemon's GPU registry clients; no subprocess per sample.
#[derive(Default)]
struct GpuMeter {
    clients: Vec<u32>,
}
impl Drop for GpuMeter {
    fn drop(&mut self) {
        for client in &self.clients {
            unsafe {
                IOObjectRelease(*client);
            }
        }
    }
}
impl GpuMeter {
    fn discover(&mut self) {
        let mut clients = Vec::new();
        unsafe {
            let key = CFStringCreateWithCString(
                std::ptr::null(),
                c"IOUserClientCreator".as_ptr(),
                0x08000100,
            );
            let mut iterator = 0;
            let gpu = IOServiceGetMatchingService(0, IOServiceMatching(c"AGXAccelerator".as_ptr()));
            if gpu != 0
                && IORegistryEntryCreateIterator(gpu, c"IOService".as_ptr(), 1, &mut iterator) == 0
            {
                loop {
                    let entry = IOIteratorNext(iterator);
                    if entry == 0 {
                        break;
                    }
                    if !IOObjectConformsTo(entry, c"AGXDeviceUserClient".as_ptr()) {
                        IOObjectRelease(entry);
                        continue;
                    }
                    let creator = IORegistryEntryCreateCFProperty(entry, key, std::ptr::null(), 0);
                    let mut buffer = [0i8; 512];
                    let ours = !creator.is_null()
                        && CFStringGetCString(
                            creator,
                            buffer.as_mut_ptr(),
                            buffer.len() as isize,
                            0x08000100,
                        )
                        && CStr::from_ptr(buffer.as_ptr())
                            .to_bytes()
                            .starts_with(format!("pid {},", std::process::id()).as_bytes());
                    if !creator.is_null() {
                        CFRelease(creator);
                    }
                    if ours {
                        clients.push(entry);
                    } else {
                        IOObjectRelease(entry);
                    }
                }
                IOObjectRelease(iterator);
            }
            if gpu != 0 {
                IOObjectRelease(gpu);
            }
            CFRelease(key);
        }
        for old in std::mem::replace(&mut self.clients, clients) {
            unsafe {
                IOObjectRelease(old);
            }
        }
    }
    fn total(&self) -> Option<u64> {
        let mut total = 0u64;
        let mut found = false;
        unsafe {
            let usage_key =
                CFStringCreateWithCString(std::ptr::null(), c"AppUsage".as_ptr(), 0x08000100);
            let time_key = CFStringCreateWithCString(
                std::ptr::null(),
                c"accumulatedGPUTime".as_ptr(),
                0x08000100,
            );
            for client in &self.clients {
                let usage =
                    IORegistryEntryCreateCFProperty(*client, usage_key, std::ptr::null(), 0);
                if usage.is_null() {
                    continue;
                }
                if CFGetTypeID(usage) == CFArrayGetTypeID() {
                    for i in 0..CFArrayGetCount(usage) {
                        let entry = CFArrayGetValueAtIndex(usage, i);
                        if CFGetTypeID(entry) != CFDictionaryGetTypeID() {
                            continue;
                        }
                        let number = CFDictionaryGetValue(entry, time_key);
                        let mut value = 0i64;
                        if !number.is_null()
                            && CFGetTypeID(number) == CFNumberGetTypeID()
                            && CFNumberGetValue(number, 4, (&raw mut value).cast())
                        {
                            total = total.saturating_add(value.max(0) as u64);
                            found = true;
                        }
                    }
                }
                CFRelease(usage);
            }
            CFRelease(usage_key);
            CFRelease(time_key);
        }
        found.then_some(total)
    }
}

fn sysctl(name: &str) -> Value {
    let name = CString::new(name).unwrap();
    let mut buffer = [0u8; 512];
    let mut size = buffer.len();
    if unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Value::Null;
    }
    if name.as_bytes() == b"hw.memsize" && size == 8 {
        json!(u64::from_ne_bytes(buffer[..8].try_into().unwrap()))
    } else if size == 4 {
        json!(u32::from_ne_bytes(buffer[..4].try_into().unwrap()))
    } else {
        json!(String::from_utf8_lossy(&buffer[..size]).trim_end_matches('\0'))
    }
}

/// Bounded read-only utilities, only when a person requests a bundle. Readers
/// drain both pipes concurrently so a verbose system_profiler cannot deadlock.
fn command(program: &str, args: &[&str]) -> Value {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    else {
        return json!({"unavailable":program});
    };
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut b = String::new();
        let _ = (&mut stdout).take(2 << 20).read_to_string(&mut b);
        b
    });
    let err = std::thread::spawn(move || {
        let mut b = String::new();
        let _ = (&mut stderr).take(64 << 10).read_to_string(&mut b);
        b
    });
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        if let Ok(Some(status)) = child.try_wait() {
            break Some(status.code());
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let out = out.join().unwrap_or_default();
    let err = err.join().unwrap_or_default();
    json!({"exit":status,"output":serde_json::from_str::<Value>(&out).unwrap_or(Value::String(out)),"stderr":err})
}

pub fn environment(folder: &Path) -> Value {
    let mut display_ids = [0u32; 32];
    let mut display_count = 0u32;
    let display_colors = if unsafe {
        objc2_core_graphics::CGGetActiveDisplayList(
            32,
            display_ids.as_mut_ptr(),
            &mut display_count,
        )
    }
    .0 == 0
    {
        display_ids[..display_count as usize]
            .iter()
            .map(|id| {
                let space = objc2_core_graphics::CGDisplayCopyColorSpace(*id);
                let name =
                    objc2_core_graphics::CGColorSpace::name(Some(&space)).map(|s| s.to_string());
                json!({"id":crate::devices::display_uuid(*id),"cg_id":id,"color_space":name})
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let others = pids()
        .into_iter()
        .filter_map(|pid| {
            let path = process_name(pid)?;
            [
                "OBS.app/",
                "ScreenFlow.app/",
                "QuickTime Player.app/",
                "Camtasia",
                "isocast",
                // The release engine (#83).
                "Isocast Engine",
            ]
            .iter()
            .any(|s| path.contains(s))
            .then(|| json!({"pid":pid,"executable":Path::new(&path).file_name().map(|s|s.to_string_lossy().into_owned())}))
        })
        .collect::<Vec<_>>();
    let app = std::env::current_exe().unwrap_or_default();
    let signature = command("/usr/bin/codesign", &["-dvv", app.to_str().unwrap_or("")]);
    let signing = json!({"executable":app,"exit":signature["exit"],"details":signature["stderr"].as_str().unwrap_or("").lines().filter(|line|["Identifier=","Format=","CodeDirectory ","CDHash=","Signature=","Runtime Version="].iter().any(|p|line.starts_with(p))).collect::<Vec<_>>(),"identity_names_omitted":true});
    json!({"macos":command("/usr/bin/sw_vers",&[]),"hardware":{"model":sysctl("hw.model"),"chip":sysctl("machdep.cpu.brand_string"),"cores":sysctl("hw.logicalcpu"),"physical_cores":sysctl("hw.physicalcpu"),"memory_bytes":sysctl("hw.memsize")},
        "gpu_displays":command("/usr/sbin/system_profiler",&["SPDisplaysDataType","-json"]),"display_color_spaces":display_colors,"power":power(),
        "disks":volumes(folder),"recording_volume":crate::volume(folder).map(|v|json!({"free_bytes":v.free_bytes,"file_system":v.file_system,"network":v.network})).unwrap_or_else(|e|json!({"error":e.to_string()})),
        "locale":{"LANG":std::env::var("LANG").ok(),"LC_ALL":std::env::var("LC_ALL").ok(),"system":command("/usr/bin/defaults",&["read","-g","AppleLocale"])},
        "signing":signing,"other_capture_processes":others})
}

fn volumes(recording: &Path) -> Value {
    let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if count < 0 {
        return json!({"error":std::io::Error::last_os_error().to_string()});
    }
    let mut stats = vec![MaybeUninit::<libc::statfs>::zeroed(); count as usize + 16];
    let count = unsafe {
        libc::getfsstat(
            stats.as_mut_ptr().cast(),
            (stats.len() * size_of::<libc::statfs>()) as i32,
            libc::MNT_NOWAIT,
        )
    };
    let mut rows=stats.iter().take((count.max(0) as usize).min(stats.len())).map(|s| {
        let s=unsafe {s.assume_init_ref()};
        json!({"mount":unsafe {CStr::from_ptr(s.f_mntonname.as_ptr())}.to_string_lossy(),
            "file_system":unsafe {CStr::from_ptr(s.f_fstypename.as_ptr())}.to_string_lossy(),
            "free_bytes":s.f_bavail.saturating_mul(s.f_bsize as u64),"total_bytes":s.f_blocks.saturating_mul(s.f_bsize as u64),
            "network":s.f_flags & libc::MNT_LOCAL as u32==0})
    }).collect::<Vec<_>>();
    let recording = recording
        .canonicalize()
        .unwrap_or_else(|_| recording.to_owned());
    let target = rows
        .iter()
        .filter_map(|v| v["mount"].as_str())
        .filter(|m| recording.starts_with(m))
        .max_by_key(|m| m.len())
        .map(String::from);
    rows.retain(|v| v["mount"] == "/" || v["mount"].as_str() == target.as_deref());
    json!(rows)
}
