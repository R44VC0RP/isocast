# Echo Show 8 (2025, madeleine) — arrival probe plan

**Goal:** test whether the 2025 Echo Show 8 (model MD6NE4, FCC 2A4DH-6274,
Azma15/AZ3 Pro, Vega OS) can be made to run our own software with mic/camera
access — using the levers found in the 1571 GPL source drop and the Gen 3 /
Show 15 hardware-forensics playbook. Probes are ordered by destructiveness so
the Amazon return window stays usable as long as possible.

**Ground truth from tonight's research** (all traced to artifacts in
`research_notes/Echo Show 8 2025 hackability/` and the session temp dir):

- adbd ships on the device but is gated: systemd `ExecCondition=/sbin/fosflags.sh`
  (production builds only), and the binary itself stays non-root unless
  `/etc/os-release` contains `BUILD_VARIANT="userdebug"`. Ethernet ADB (port
  5555) additionally needs `/var/lib/adb/eth_enabled.conf` to exist with `1`.
  `vda` is byte-for-byte adb — stock platform-tools work if the daemon comes up.
- UART console is enabled in the device tree: `console=ttyS0,921600n8`,
  earlycon @ 0xb2010000.
- U-Boot exists and has a fastboot trigger wired to the volume-down button
  ("fastboot gpio, only for uboot purpose" in the DTS).
- Kernel 6.1.70, and the published defconfig has `CONFIG_KEXEC=y`, no module
  signing, no dm-verity — softer than Gen 3 (kexec compiled out), but the
  shipped config must be verified on hardware.
- swupdate handles OTA with A/B slots (`copyA`/`copyB`), signed images
  (production root CA in the source drop), a mongoose web server on :8080
  when enabled, and auto-runs updates from inserted USB sticks
  (`swupdate-usb.rules`).
- Gen 3 / Show 15 playbook: fastboot enumerates but blocks unlock; UART boot
  log leaks everything, autoboot interruptible but all commands blocked; eMMC
  read off-chip via needle probes + EasyJTAG (1-bit SDR, 1 MHz, 1.8 V IO).

---

## Stage 0 — before/while it ships (already done)

- [x] Source-tarball analysis: gates, OTA, kernel config (see
      `source_release_1571_findings.md`).
- [x] FCC internal/external photos downloaded + sha-verified (see
      `fcc_and_rhodes_methodology.md`); full-res page images extracted for
      visual review — **look at these before it arrives**: board layout, any
      debug pads, whether a USB data port exists on the back I/O.
- [ ] Order equipment (below) if committing to Stage 3.

## Stage 1 — sealed-box probing (no disassembly; fully returnable)

1. Confirm the unit: model **MD6NE4** on the label, FCC ID **2A4DH-6274**.
2. Put it on an **isolated network** (guest VLAN, no internet) so no OTA
   lands mid-probe; keep it there for all probing.
3. Let it finish first-boot setup far enough to be on Wi-Fi (or Ethernet if
   it has a port). Then LAN recon:
   - `nmap -sV -p- <ip>` — watch for **5555** (adbd/vda), **8080**
     (swupdate mongoose), anything unexpected; `avahi-browse -al` for mDNS
     services (avahi + dnsmasq are in the image).
4. Inspect the back I/O for any USB data port (not just power). If one exists:
   - Plug into a Linux box, `lsusb` during normal boot — record everything.
   - **Hold Vol-Down while powering on** (the DTS fastboot GPIO) and watch for
     a download/fastboot enumeration (Gen 3 showed 18d1:0d02 "USB download
     gadget"). If it appears: `fastboot getvar all` (leaks partition table,
     slot info, secure/unlocked flags). Expect unlock commands blocked —
     still pure recon. Do NOT hold Vol-Up+Vol-Down together (factory reset).
5. USB-stick probe: insert a FAT32 stick containing a junk `.swu` file —
   the udev rule starts swupdate-usb; observe (offline network) whether it
   parses/rejects it. Harmless, returnable.

**Decision point:** if 5555 or adbd/USB enumerates and a shell is reachable,
stop — document it (that alone is a first). Expected: everything closed.

## Stage 2 — open the case (still returnable if no soldering)

6. Pry open (plastic tools; fabric-backed shell), photograph the MLB both
   sides, compare with the FCC internal-photo PDFs (temp dir: `fcc_6274_*`).
7. Locate UART pads (Gen 3 convention: labeled solder pads near the board
   edge; `uart_dw0`). Connect with **needle probes / pogo pins** — no solder:
   - 3.3 V USB-UART adapter, **921600 8N1**.
   - Capture full boot log: confirms shipped kernel cmdline
     (`slot_suffix`, `buildvariant`, verity flags), U-Boot version/lockdown,
     whether "Hit Enter to stop autoboot" exists and whether commands are
     blocked (expect blocked, like Gen 3).
8. Multimeter continuity-map eMMC test points (CLK/CMD/D0-7/VCC/VCCQ) against
   the FCC photos — do not write anything.

**Decision point — return or commit:** everything so far is reversible; if
the boot log says the shipped kernel really has kexec + no verity + no module
signing, that materially raises the value of committing. Cutting shields or
soldering = point of no return for returns.

## Stage 3 — off-chip eMMC (committed; the real shot)

9. In-circuit first (needles on test points, EasyJTAG: 1-bit SDR, 1 MHz,
   1.8 V IO): dump the full 8 GB. If signal integrity fails, chip-off the
   BGA eMMC with hot air, read in socket.
10. From the dump, map partitions (expect A/B copies; a nvcfg/misc-like
    area for flags; look for where `/sbin/fosflags.sh` reads from — compare
    with Gen 3's idme/misc conventions).
11. Write experiments (target the **inactive** slot first so a bad edit
    doesn't kill the boot chain):
    - `/etc/os-release` → `BUILD_VARIANT="userdebug"` (root adbd if it runs)
    - create `/var/lib/adb/eth_enabled.conf` containing `1` (LAN adbd :5555)
    - set whatever fosflags bit mirrors Fire OS's ADB-enable flag
      (Gen 1 used `fastboot oem flags 61` — same bit family)
12. Boot the edited slot with USB (or LAN) attached:
    - `adb devices` → if it shows: `adb shell whoami` (root or app_user).
13. With any shell: pivot_root into a Debian/Alpine rootfs like Gen 3 — all
    drivers are standard ALSA/V4L2 (mic array = 16-ch/96 kHz PCM, camera =
    ov13858 V4L2). With root: check `/proc/config.gz`; if KEXEC=y as
    published, a custom kernel is loadable at runtime — the boot-chain
    lockstops mattering.

## Equipment list (Stage 2+3)

- USB-UART 3.3 V adapter (CP2102 or FTDI) + 0.5 mm needle probes / pogo pins
- Multimeter
- EasyJTAG Plus (or Medusa Pro) with eMMC adapters; RT80H-style reader as a
  cheaper alternative for chip-off
- Hot-air rework station + flux (only if in-circuit reads fail)
- Linux box: platform-tools (adb/fastboot), `screen`/`minicom`, `nmap`,
  `avahi-utils`
- Precision pry tools + Torx/Phillix drivers

## Risks and notes

- OTAs can change any of this — keep the device offline while probing.
- The FCC unit was an EVT build; retail boards may differ slightly from the
  photos (pad locations especially — verify on the real board).
- Nothing here is a published result for this model; every probe is
  first-of-its-kind and worth writing up even when it fails (the absence
  data is itself new — nobody has reported ANY of this for madeleine).
- Legal/safety: this is your own hardware, personal research; keep Amazon
  account disentangled (fresh Alexa setup or none) to avoid cloud-side
  surprises during experiments.
