# Primary-source inspection: Echo_Show_8_Source_Release_1571.tar.gz (2026-10-12)

Inspected directly (coordinator, not subagent): pulled Amazon's S3 source index via
the OpenCode Browser (amazon.com help page nodeId=201626480 →
fireos-audio-src.s3.amazonaws.com), streamed the 13.8 GB tarball twice without
storing it (listing pass + selective extraction), and read the kernel Makefile,
defconfig, madeleine device trees, adbd service config, and audio-stack README.

## Attribution to the 2025 (4th gen) model — confirmed

Naming scheme across the S3 index decodes the generations:

- `Echo_Show_8_src-6.5.x.x` = Gen 1 (crown, Fire OS 6)
- `Echo_Show_8_2nd_Gen_src-7.5.x.x` = Gen 2 (athena)
- `Echo_Show_8_3rd_Gen_src-7.5.x.x` = Gen 3 (rhodes)
- `Echo_Show_8_Source_Release_1571.tar.gz` = 2025 4th gen — new-style name, no
  Fire OS version number, and the same "1571" release train as
  `Echo_Show_11_Source_Release_1571.tar.gz` (Show 11 exists only as a 2025
  device). Inside, the DTS names it directly: madeleine (2025 Show 8 codename)
  on the Azma15 SoC.

Tarball: 13,851,078,000 bytes, Last-Modified 2026-09-26, ~197k files, root
dir `Echo_Show_8/`.

## What the release reveals (first public look at 2025 hardware)

- **SoC**: in-house Amazon "Azma15" (= AZ3 Pro). Tree also contains azma11 and
  azma13 predecessors, and an `azma15-rhodes-proto.dts` — Gen 3 (rhodes) was
  prototyped on Azma silicon before production switched to Amlogic.
- **OS**: Yocto-built Linux. glibc 2.35, systemd 250, SWUpdate 2022.12 for OTA,
  mainline **Linux 6.1.70** ("Curry Ramen").
- **Mic array**: standard ASoC path. Capture codecs = TI TLV320ADC5140 +
  TLV320ADC5120 and Everest ES7210 + ES7243 mic ADCs, feeding an Amazon HiFi
  DSP (HiFi1/HiFi5 cores, "voice-dsp-pcm" module) loaded via OP-TEE rproc.
  The audio-sw README documents on-device PCM nodes: ASR stream (16 kHz/1ch)
  and **raw DC stream (96 kHz/16ch/32-bit)** — the mic array is a normal ALSA
  capture device; the lock against third-party audio access is in the
  userspace/app layer, not the driver layer.
- **Camera**: OmniVision **OV13858** (13.8 MP) on MIPI CSI-2, standard V4L2
  binding (`compatible = "ovti,ov13858"`), driver in out-of-tree camera-kmods
  (upstream-style), libcamera 202209 in userland. Same story: standard V4L2.
- **Speakers**: TI TAS5815M (woofer + full-range) + Awinic AW85806 amps.
- **Other hardware in DTS**: Novatek NT36xxx touch + Truly NT36523 panel
  (rotation 270), SGM37604S backlight, Silego SLG4A46953V CPLD privacy circuit
  (SW_MUTE_DEVICE, 3s-hold → power event), Zigbee radio over SPI
  ("mediatek,ZBcontrol"), TCS3400 ALS + TSL2772 proximity, dual MMC
  (eMMC HS400 + SDIO for WiFi), two USB dwc3 ports — **usb0 in peripheral
  (gadget) mode**, uart_dw0 (console) + uart_dw4 both enabled.
- **Serial console enabled by default**: bootargs `console=ttyS0,921600n8
  earlycon=uart8250,mmio32,0xb2010000`.
- **U-Boot fastboot GPIO**: madeleine.dtsi volume-down button node carries
  `/* fastboot gpio, only for uboot purpose */`. Whether it is
  signature-enforced on shipped hardware is unknown (U-Boot source is NOT in
  the release).
- **adbd ships, flag-gated**: android-tools 5.1.1.r37 with patches ("non-root
  adbd", "adbd-Allow-adbd-to-be-ran-as-root", "non-root-adbd-shipmode"), run as
  a systemd service gated by `ExecCondition=/sbin/fosflags.sh` — ship mode
  leaves it off. This is the `vda` daemon's ground truth.
- **Published defconfig**: no CONFIG_DM_VERITY, no MODULE_SIG, no IMA — but the
  shipped kernel config may differ from the compliance drop. OP-TEE =y.
  USB_CONFIGFS_F_FS=y (FunctionFS, the adbd transport).

## Implications for hackability

1. **Nothing here lets you flash the device** — boot chain (U-Boot, TEE
   firmware, signing) is not in the release and remains the wall.
2. **It makes any future break far more tractable than Gen 3's blind effort**:
   complete kernel source + device trees for this exact board, and the mic
   array and camera are standard ALSA/V4L2 devices. A root-level break (adbd
   gate, UART, or eMMC chip-off) would find full driver support for a custom
   userspace — mic and camera included.
3. **Concrete probes the tree hands to researchers**: the fosflags.sh adbd
   gate, the U-Boot fastboot GPIO (volume-down at boot), the always-on UART
   console, and the HS400 eMMC (chip-off).
4. OTA via SWUpdate; no dm-verity in the published defconfig suggests partition
   integrity may rely on secure boot + A/B updates rather than verified
   rootfs — unverified on real hardware.

## Deep-dive addendum (2026-10-12 overnight, third streaming pass)

### adbd gate mechanics — fully decoded

The android-tools recipe (5.1.1.r37, Yocto) installs `android-tools-adbd.service`
with ExecStart=/usr/bin/adbd, plus drop-ins:

- `adbd-service-override.conf` (always): terminal env for adb shells.
- `adbd-service-fosflags-override.conf` (`[Service] ExecCondition=/sbin/fosflags.sh`)
  — installed ONLY when the build is NOT `variant-debug` (recipe bbappend:
  `bb.utils.contains("DISTRO_FEATURES","variant-debug","false","true",d)`).
  So production units gate adbd on a fosflags.sh exit code; debug builds run
  adbd unconditionally.

In-adbd gates (patch 0020 `non-root-adbd`, runtime check `is_debug_build()`):
- adbd reads **/etc/os-release** on the device; `BUILD_VARIANT="userdebug"`
  → keeps root; anything else → drops privileges (non-root adbd, groups
  GID_LOG=113, GID_FR=259 factory reset, GID_SHIPMODE=519 shipmode).
- patch 0021 mk: `ALLOW_ADBD_ROOT` CFLAG is commented out → privilege-drop
  logic compiled in on production.
- patch 0008 ("Allow adbd to be ran as root", `should_drop_privileges` return 0)
  exists in the tree but is inert without ALLOW_ADBD_ROOT.

Network ADB (patch 0001-adb-over-ethernet, by an Amazon engineer, 2022):
listens on TCP 5555 only if **/var/lib/adb/eth_enabled.conf** exists with 1 —
a second plain-file gate.

Other: `exec-bash-as-shell.patch` (shells are /bin/bash), `0013-adb-enable-reboot`
(adb reboot → /sbin/reboot -f), `0022` payload 32KB. Smack label
`System::Privileged` on the service (Smack LSM, not SELinux — the selinux
extensions were patched OUT).

**Attack-chain implication:** every adbd gate on a production unit is either a
file on the filesystem (os-release, eth_enabled.conf) or a flag read by
/sbin/fosflags.sh (mechanism unpublished; on Fire OS Echo devices the analogous
fosflags lived in a partition writable via fastboot — `fastboot oem flags 61`
enabled ADB/root on the Gen 1 jailbreak). If fosflags are similarly stored on
eMMC, a single off-chip eMMC write could satisfy all gates → root ADB shell
over USB (usb0 peripheral mode is enabled in the DTS) or LAN (port 5555).

### Kernel config posture (published defconfig — shipped config may differ)

From `arch/arm64/configs/defconfig` in the release:
- **CONFIG_KEXEC=y and CONFIG_KEXEC_FILE=y** — kexec compiled IN. Gen 3
  (rhodes) reportedly had kexec compiled out. If root is gained on madeleine,
  `kexec -l` loads an unsigned custom kernel at runtime, defeating fuse-locked
  secure boot without touching the boot chain. (KEXEC_FILE verification flags
  not visible in defconfig.)
- **No CONFIG_MODULE_SIG, no CONFIG_DM_VERITY, no IMA/EVM/lockdown** in the
  published defconfig; MODULE_UNLOAD=y, USER_NS=y, BPF_SYSCALL + BPF_JIT=y,
  DEBUG_FS=y, KASLR (RANDOMIZE_BASE)=y, FTRACE not set.
- Caveat: this is the GPL compliance defconfig; the shipped kernel may enable
  module signing/verity/Smack. Verify on hardware via /proc/config.gz or a
  dumped boot image.

### OTA / update mechanism (swupdate 2022.12)

- **A/B slots**: `09-swupdate-args` reads `slot_suffix=_a` from /proc/cmdline
  and targets the opposite copy (`-q stable,copyB` / `copyA`). Slot selection
  is made by the bootloader (U-Boot) via kernel cmdline.
- **Signed images enforced**: CONFIG_SIGNED_IMAGES=y (OpenSSL), CA at
  `/etc/sys_ota_root_ca.pem`; the production OTA root CA cert
  ("OTA root CA (prod)", Amazon/Orpheus, EC 2021-2041) is included in the
  release as `root_ca_production.pem` — usable to verify any captured .swu.
- **Local web server**: swupdate's mongoose listens on **port 8080**
  (`10-mongoose-args`: `-r /www -p 8080`) when enabled — a LAN HTTP surface
  worth port-scanning on a real unit (uploads still signature-checked).
- **USB update path**: `swupdate-usb.rules` udev rule starts
  `swupdate-usb@<dev>.service` when a USB storage device with a filesystem is
  inserted — an update/recovery-from-USB-stick path exists (still signed).
- Runs as user `ota`, PartOf=aceotad.service (Amazon's Vega OTA daemon, "ace"
  middleware), tmpfs /run/swupdate with SMACK64=System.

### Misc

- mkbootimg/fastboot signing scripts (genkey.sh, signfile.sh, p12topem.sh)
  ship in android-tools — image-signing tooling, keys obviously not included.
- No SSH server anywhere in the release (libssh is a client library; avahi's
  ssh.service is an mDNS example file).

## Caveats

- GPL-compliance drop ≠ shipped image; configs/firmware may differ on-device.
- U-Boot, OP-TEE payload, DSP firmware blobs, and Vega userspace (VPKG/Kepler,
  fosflags.sh itself) are not in the tarball.
- The listing and extracted files remain in the session temp dir
  (opencode/show8_1571_listing.txt, opencode/show8_extract/) for follow-up.
