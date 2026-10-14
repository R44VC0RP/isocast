# FCC exhibits, rhodes methodology, and vda (2026-10-12 overnight)

## FCC exhibits for the 2025 Echo Show 8 (MD6NE4, FCC ID 2A4DH-6274)

All pulled through the OpenCode Browser (fccid.io + apps.fcc.gov redirect), PDFs
saved in the session temp dir, sha256 verified against fccid.io metadata where
published:

| File | Doc ID | Size | sha256 |
|---|---|---|---|
| fcc_6274_intpho1.pdf | 8637509 | 5,599,578 B | `ec720196513234d7a3009cd8b279a8e3e156422d56d244f04ea29baac4a38502` (matches fccid.io) |
| fcc_6274_intpho2.pdf | 8637510 | 1,821,633 B | not published by fccid.io for this doc |
| fcc_6274_extpho.pdf | 8637508 | 1,083,962 B | " |
| fcc_6274_manual.pdf | 8637511 | 165,571 B | " |
| fcc_7824_intpho.pdf (Show 11) | 8642156 | 6,008,201 B | " |
| fcc_7824_extpho.pdf (Show 11) | 8642154 | 928,281 B | " |

The confidentiality period on 2A4DH-6274 expired 2026-03-29 — these exhibits
have been public for ~6 months but were not indexed by any of the Echo-hacking
coverage surveyed earlier ("no FCC internal-photo set has been extracted" was
the prior state of knowledge).

Extracted per-page images (pypdf) + 3x-upscaled copies + macOS Vision OCR
results are in the temp dir (`fcc_6274_*_img*`, `fcc_7824_*_img*`,
`ocr_*.txt`). OCR findings are limited: the photos are wide/angled shots where
chip silkscreen is too small for reliable OCR; readable items were mostly
photo rulers, the power adapter label (LA24BN, 18.0V/1.67A 30W), and the
notable **"NOT FOR SALE EVT G1"** stamp on the adapter — the FCC test unit was
an EVT build (matching `madeleine-evt.dts` in the source drop). Repeated
markings worth a human look: "TO_AMP" (p13/p14), "2502 KZ", "54982683",
"09000153826R2". **The PDFs themselves need human/visual review** — this
model cannot read images, and the submode vision-model bridge was broken
tonight (ctx.catalog error). Full-res page images are ready for inspection.

## Gen 3 / Show 15 hardware-root methodology (jcrasselt/amazon-echo-show-15)

The DFRWS 2024 repo documents the entire procedure for the sibling Amlogic
Pop1 platform (hoya = Show 15; rhodes = Show 8 Gen 3):

1. **Fastboot via buttons**: Power + Vol-down ~4 s over the micro-USB service
   port → enumerates as "USB download gadget" (18d1:0d02, Android Fastboot,
   manufacturer Amlogic). `fastboot getvar all` works and leaks the partition
   table, serial, `secure: yes`, `unlocked: yes`(!), but every unlock command
   returns `FAILblocked command`. WARNING: Power + Vol-up + Vol-down =
   factory reset.
2. **UART**: solder pads on the MLB, needle probes, 3.3 V, 921600 8N1 (same
   baud as madeleine's DTS console). Boot log prints the entire chain
   (BL2 → BL2E → BL2X → BL31/ATOS TEE → AOCPU FreeRTOS → U-Boot 2019.01 →
   "Image AUTHENTICATED with PRODUCTION certificate"). "Hit Enter key to stop
   autoboot" IS interruptible, but every U-Boot shell command is blocked
   ("I'm sorry, but command X is blocked").
3. **eMMC**: SanDisk iNAND 7250 (SDINBDG4-16G, BGA-153) under a shield.
   In-circuit read via needle probes on test points (CMD accessible at the
   side of an SMD resistor) or chip-off. EasyJTAG settings: EasyJTAG2
   E-Socket pinout, 1 MHz CLK (wire-length limited), 1.8 V IO, 1-bit SDR.
   Partition table dumped and mapped (bootloader/reserved/nvcfg/tee/boot/
   recovery/logo/misc/cri_data/vendor/odm/system/product/cache/data +
   boot0/boot1/rpmb). The unverified 8 MB `odm` partition is where the
   Gen 3 root plants its setuid payload.
4. Misc: mic boards and camera board are on separate flex/PCBs and can be
   disconnected without boot complaints (relevant for hardware repurposing).

## vda == adb (RobusTest, 2026-08-31)

Field note verified from https://robustest.com/blog/the-one-line-fork:
- `vda` ("Vega Device Adapter") is a one-line fork of adb: product name string
  changed; wire protocol, Server Version 41, `~/.android/adbkey` RSA auth, and
  all verbs (shell/push/pull/forward/pair/tcpip) identical.
- Stock Google platform-tools adb drives a Vega Fire TV Stick directly
  (`adb shell whoami` → `app_user`), no SDK needed.
- On Fire TV, vda is gated behind developer-mode enrollment; the Echo Show
  Vega units ship the same adbd (per the 1571 source drop) but gate it with
  `/sbin/fosflags.sh` + `/etc/os-release` BUILD_VARIANT as documented in
  source_release_1571_findings.md.

## Transfer assessment to madeleine (2025 Show 8)

- UART: high confidence — console enabled in DTS (ttyS0 @921600, earlycon at
  0xb2010000). Pads need locating on the MLB (FCC photos / live board).
- Fastboot GPIO: the madeleine DTS volume-down node carries
  `/* fastboot gpio, only for uboot purpose */` — high confidence a U-Boot
  fastboot/download mode exists; expect blocked commands like hoya. Unknown
  whether the retail shell exposes a USB data port at all (Show 15 had
  micro-USB; the 2025 Show 8's back I/O must be checked on the real unit —
  the ExtPho PDF is the place to look).
- eMMC: 8 GB eMMC per specs; rhodes-family technique should transfer
  mechanically (needles/EasyJTAG), but the Vega partition layout is unknown —
  expect A/B copies (swupdate copyA/copyB) and different gate targets:
  fosflags storage (likely an nvcfg/misc-like partition), /etc/os-release,
  /var/lib/adb/eth_enabled.conf. RPMB exists but `rpmb-status="0"` in the DTS
  hints it is not the primary lock.
- Once root: everything needed for a custom userspace is standard
  (ALSA/V4L2), and the published defconfig has KEXEC=y — verify the shipped
  config from a UART boot log / dumped boot image before counting on it.
