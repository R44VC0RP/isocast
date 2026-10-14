# Echo Show 8 (all generations) — hardware and firmware architecture

Scope: Amazon Echo Show 8 Gen 1 (2019), Gen 2 (2021), Gen 3 (2023), as foundation for assessing custom-Linux viability. All facts tagged per generation. Current as of October 2026. (For completeness: a Gen 4 "2025 release" exists — AZ3 Pro, 8.7" — but is out of scope.)

---

## 1. SoC / memory / storage per generation

### Takeaway
The three generations use three entirely different silicon platforms: Gen 1 is MediaTek MT8163 (quad Cortex-A53, Mali-450), Gen 2 is MediaTek MT8183 (octa-core), Gen 3 is a custom Amazon-branded Amlogic part ("P0P1-R" / PopcornA "Pop1") with the AZ2 neural engine. This is the single most important fact for hacking, because the only known software unlock (amonet) works exclusively on the MT8163 generation.

### Cited Findings

**Gen 1 (2019) — model C7H6N3, product/build model AEOCW, codename `crown`**
- SoC: MediaTek MT8163 quad-core — [Wikipedia comparison table](https://en.wikipedia.org/wiki/Amazon_Echo_Show); [Pocket-lint Show 8 2019 vs 2021](https://www.pocket-lint.com/new-amazon-echo-show-8-vs-old-echo-show-8-differences-compared) ("the older model has a four-core processor in the MediaTek 8163")
- FCC ID 2ARO5-7879, granted 2019-09-25, "2T2R 802.11abgn/ac w/DFS, Bluetooth; DC 5.25V/1A input" — [WikiDevi Amazon Echo Devices](https://wikidevi.wi-cat.ru/Amazon_Echo_Devices)
- Codename `crown`, product AEOCW, firmware package `com.amazon.crown.android.os` — [FTVDB crown firmware history](https://ftvdb.com/echo/firmware/com.amazon.crown.android.os); "THIS IS ONLY for the 1st gen Echo Show 8 (crown) released in 2019 (C7H6N3)" — [XDA crown unlock thread](https://xdaforums.com/t/unlock-root-twrp-unbrick-amazon-echo-show-8-1st-gen-2019-crown.4766687/)
- CPU quad-core ARM Cortex-A53, GPU ARM Mali-450 MP4, RAM 2 GB (vs 1 GB on Echo Show 5), storage 8 GB eMMC — [community LineageOS install guide (winmutt/self-2026)](https://github.com/winmutt/self-2026/blob/master/LINEAGEOS_INSTALLATION_GUIDE.md)
- Physical RAM is 2 GB but stock/unlocked configurations differ: amonet v2.0.0 changelog "Unlocked the extra 1 GB of RAM, giving us a total of 2 GB of RAM!" — while an earlier Q&A in the same thread states "there's currently no way to keep the full 2 GB of RAM after unlocking the device… we're basically flashing 1st-gen Echo Show 5 firmware onto… the 1st-gen Echo Show 8. This happens because the only vulnerable bootloader available comes from a demo unit of the 1st-gen Echo Show 5" — [XDA crown unlock thread](https://xdaforums.com/t/unlock-root-twrp-unbrick-amazon-echo-show-8-1st-gen-2019-crown.4766687/) (the v2.0.0 changelog post-dates and supersedes the Q&A)
- Wi-Fi/BT are MediaTek MT76x8-family parts (separate GPL drops exist for `mt76x8-wifi` and `mt76x8-bt`) — [XDA crown LineageOS thread](https://xdaforums.com/t/rom-unofficial-11-crown-lineageos-18-1-for-the-amazon-echo-show-8-2019.4766709/)
- No published Amazon spec for RAM/storage on Gen 1; the above comes from community teardown of the running device, not an FCC photo dump.

**Gen 2 (2021) — model A8H3N2, build model AEOAT, codename `athena`**
- SoC: MediaTek MT8183, 8-core — [B&H Photo spec sheet](https://www.bhphotovideo.com/c/product/1642766-REG/amazon_b084dcjksl_echo_show_8_2g.html); [Pocket-lint Gen2 vs Gen3](https://www.pocket-lint.com/echo-show-8-3rd-generation-vs-echo-show-8-2nd-generation); [Android Central comparison](https://www.androidcentral.com/accessories/smart-home/amazon-echo-show-8-3rd-gen-vs-echo-show-8-2nd-gen)
- TechInsights deep-dive (comparing Gen 3 to Gen 2): Gen 2 has 2 GB DDR3 SDRAM (vs LPDDR4 in Gen 3); TechInsights labels the Gen 2 predecessor "C7H6N3", which conflicts with Amazon/WikiDevi/XDA (C7H6N3 = Gen 1; A8H3N2 = Gen 2) — [TechInsights Echo Show 8 3rd Gen teardown](https://www.techinsights.com/blog/deep-dive-teardown-amazon-echo-show-8-3rd-gen-r855d6-smart-display)
- Storage 8 GB; 8" 1280×800, 13 MP camera, micro-USB port, 18 V=1.67 A power — [B&H spec sheet](https://www.bhphotovideo.com/c/product/1642766-REG/amazon_b084dcjksl_echo_show_8_2g.html); [Amazon compliance page (model A8H3N2, FCC ID 2AWTZ-8462)](https://www.amazon.com/gp/help/customer/display.html?nodeId=G3MTWF65FDGW8E6H)
- FCC ID 2AWTZ-8462 (from Amazon's own compliance page, above). WikiDevi lists the 2nd-gen entry without an FCC link — [WikiDevi](https://wikidevi.wi-cat.ru/Amazon_Echo_Devices)
- Four microphones (same as Gen 3) — [TechInsights](https://www.techinsights.com/blog/deep-dive-teardown-amazon-echo-show-8-3rd-gen-r855d6-smart-display)

**Gen 3 (2023) — model R85SD6, ASIN B0BLS3Y632/B0BLS3QJTX, codenames `rhodes` / `pop1`**
- Application processor from Amlogic: "P0P1-R octa-core application processor" with the Amazon AZ2 neural network engine; Wi-Fi front-end from MediaTek — [TechInsights deep-dive teardown](https://www.techinsights.com/blog/deep-dive-teardown-amazon-echo-show-8-3rd-gen-r855d6-smart-display); [PCMag review](https://au.pcmag.com/smart-displays/102267/amazon-echo-show-8-3rd-gen-2023) ("octa-core processor with Amazon's AZ2 neural engine, a nearly two-year-old chip that also powers the Echo Show 15 and the Echo Pop")
- Memory: 2 GB LPDDR4 SDRAM (SK Hynix); storage: 8 GB MLC NAND (Samsung); 13 MP BSI CMOS sensor; no battery — [TechInsights](https://www.techinsights.com/blog/deep-dive-teardown-amazon-echo-show-8-3rd-gen-r855d6-smart-display); total BOM cost estimated $138.79
- Amazon's developer docs name the same SoC family "Amlogic PopcornA (Pop1)": Cortex-A55 up to 1.5 GHz, 32-bit ABI, ARM G52 MP1 GPU, MT7921A Wi-Fi 6E ( specs given for Echo Show 15/21, same Pop1 processor) — [Amazon dev docs: Device Specifications Echo Show](https://developer.amazon.com/docs/device-specs/device-specifications-echo-show.html)
- Conflict: the Gen 3 device tree (dumped by XDA researchers) shows A73 + A53 big.LITTLE clusters with separate regulators ("A73: PWM-regulator pwmao_d… A53: BD71837 BUCK3"), contradicting Amazon's "Cortex-A55" description of Pop1 — [XDA rhodes thread](https://xdaforums.com/t/root-system-hardware-amazon-echo-show-8-3rd-gen-rhodes-pop1-via-uart-and-emmc.4793346) vs [Amazon dev docs](https://developer.amazon.com/docs/device-specs/device-specifications-echo-show.html)
- The SoC is an Amlogic "T7"-generation meson platform (device tree references `t7-power-domain` with 52 power domains, `meson-t7,vpu_security`, `meson-rng`) — [XDA rhodes thread](https://xdaforums.com/t/root-system-hardware-amazon-echo-show-8-3rd-gen-rhodes-pop1-via-uart-and-emmc.4793346)
- Wi-Fi/BT: 802.11a/b/g/n/ac + BT 5.1 module, Thread/Zigbee/Matter hub radios — [Amazon product listing via device.report](https://device.report/amazon); [Tom's Guide review](https://www.tomsguide.com/reviews/amazon-echo-show-8-3rd-gen)
- iFixit has a Gen 3 disassembly guide (not a full chip teardown): [iFixit Show 8 3rd Gen Display Assembly Replacement](https://www.ifixit.com/Guide/Amazon+Echo+Show+8+3rd+Gen+Display+Assembly+Replacement/179345)

**Older/lower-cost siblings (useful silicon reference):**
- Echo Show 5 Gen 1 (2019) and Gen 2 (2021) also use MT8163 with 1 GB DDR3 — hence they share the amonet exploit with Show 8 Gen 1 — [WikiDevi](https://wikidevi.wi-cat.ru/Amazon_Echo_Devices); [Wikipedia](https://en.wikipedia.org/wiki/Amazon_Echo_Show)
- Original Echo Show 10 (3rd Gen, 2021) = MT8183 + Amazon AZ1 — [Wikipedia comparison table](https://en.wikipedia.org/wiki/Amazon_Echo_Show)

### Inferences
- Gen 1 is the only Echo Show 8 whose platform (MT8163) has public bootloader exploits, published GPL kernel sources (via Amazon's Fire-tablet source drops for the same SoC), and a mature custom-ROM scene — it is the only generation where "custom Linux" is realistic today without soldering.
- Gen 2 (MT8183) has no public unlock; its SoC appears in third-party analyses of MediaTek preloader CVEs (CVE-2023-20694/20695/20725 list MT8183 among affected SoCs), so a future software path is conceivable but none exists — [mouseos MT8365 preloader exploit analysis](https://github.com/mouseos/mediatek-mt8365-preloader-exploit/blob/main/docs/CVE-2023-20694/exploit_analysis.md)
- Gen 3's Amlogic platform is fully hardware-locked (see section 3); its realistic path is eMMC repartitioning + pivot_root into a Debian/Alpine userspace on the locked kernel, which has already been demonstrated.

### Gaps
- Gen 3 FCC ID not confirmed. Amazon's 2A4DH grantee has September 2023 filings ("Wireless Tablet" 2A4DH-4258, "Digital Media Receiver" 2A4DH-3907/-6387) that coincide with the Show 8 Gen 3 retail date ([device.report](https://device.report/amazon), [FCCID.io 2A4DH-0808 grantee page](https://fccid.io/2A4DH-0808)), but no source explicitly ties a specific FCC ID to model R85SD6.
- Exact DRAM part numbers / eMMC package for Gen 1 and Gen 2 (no iFixit or TechInsights board-level teardown of those two boards was found; iFixit's full teardowns cover the 2017 Echo Show and earlier Echos, not Show 8 Gen 1/2).
- Gen 1's stock RAM configuration vs. physical: sources agree on 2 GB physical but the stock-vs-unlocked visible RAM story is tangled in the thread history; no authoritative Amazon spec exists.

---

## 2. Operating system: Fire OS (Android) vs. embedded Linux

### Takeaway
All three Echo Show 8 generations run Fire OS — Amazon's Android-derived platform — not the buildroot-style embedded Linux used by the non-display Echo speakers. Gen 1 is Fire OS 6 (Android 7 base), Gen 2 is Fire OS 7 (Android 9 base), Gen 3 runs an Android-9-generation Fire OS 7 stack (SELinux, dm-verity, Treble-style partitions). "Vega OS" (Amazon's reported Fire OS replacement) has started shipping on some 2023+ devices but the Show 8 Gen 3 demonstrably still runs the Android-derived stack as of 2026.

### Cited Findings
- "Even with Amazon's Echo Show devices running Linux in the form of the Android-derived FireOS…" — [Hackaday, 2026-07-08](https://hackaday.com/2026/07/08/hacking-amazon-echo-show-8-3rd-gen-via-uart-and-emmc/)
- Firmware package names prove the Android base per generation: `com.amazon.crown.android.os` (Gen 1, model AEOCW) — [FTVDB](https://ftvdb.com/echo/firmware/com.amazon.crown.android.os); `com.amazon.athena.android.os` (Gen 2, model AEOAT) — [FTVDB](https://ftvdb.com/echo/firmware/com.amazon.athena.android.os)
- Gen 1: "This device does indeed have Android 7 as the stock ROM" (stock Fire OS 6.5.7.x, e.g. Fire OS 6.5.7.4 NS6574/7636, still updated Sept 2026) — [XDA crown LineageOS thread](https://xdaforums.com/t/rom-unofficial-11-crown-lineageos-18-1-for-the-amazon-echo-show-8-2019.4766709/); [FTVDB crown history](https://ftvdb.com/echo/firmware/com.amazon.crown.android.os)
- Gen 2: latest firmware is Fire OS 7.5.7.3 (PS7573/6536) — Fire OS 7 = Android 9 — [FTVDB athena history](https://ftvdb.com/echo/firmware/com.amazon.athena.android.os)
- Gen 3: running system shows a full Android stack — ADB (TCP 5555), SELinux `u:r:shell:s0` enforcing, `ro.debuggable=0`, `pm install` sideloading, dm-verity via dm-0/dm-1, /system /vendor /odm /product /data partitions — measured on-device in 2026 — [XDA rhodes thread](https://xdaforums.com/t/root-system-hardware-amazon-echo-show-8-3rd-gen-rhodes-pop1-via-uart-and-emmc.4793346)
- Amazon's developer documentation classifies Pop1-based Echo Shows (Show 15/21) as "Fire OS 7 / Android Level 28 (Android 9)" app targets, i.e. the same platform as Fire tablets/Fire TV — [Amazon dev docs](https://developer.amazon.com/docs/device-specs/device-specifications-echo-show.html); [Identify Fire TV Devices](https://developer.amazon.com/docs/device-specs/identify-fire-tv-devices.html)
- Wikipedia's infobox claims a transition: "Fire OS (Used 2017-2023) (Android-based); Echo Ship/Vega OS (Used 2023+) (Linux-based)" and says the Echo Show 5 3rd Gen (May 2023) "was also the first device to ever run Vega OS, Amazon's new OS as a replacement to FireOS on smart home devices" — [Wikipedia](https://en.wikipedia.org/wiki/Amazon_Echo_Show). This conflicts with Amazon's own 2024-device dev docs (Show 21/15 Gen 2 = Fire OS 7) and with the 2026 XDA evidence of an Android stack on Show 8 Gen 3 — treat the "Vega OS everywhere from 2023" claim as unverified for the Show 8 line.
- Supporting signal: Amazon's software-version page shows Show 5 Gen 3 on a differently-formatted version line (4201010001520) from the Fire OS devices (33857737604) — [Alexa Device Software Versions](https://www.amazon.com/gp/help/customer/display.html?nodeId=GMB5FVUB6REAVTXY)
- Difference from the *speaker* Echo line: the non-display Echo (1st gen, 2014) runs a genuinely minimal embedded Linux with an ancient kernel ("running on a 2.6.x series Linux kernel… With its similarly archaic version of glibc, newer Linux software would refuse to run") — [Hackaday, 2021-03-22, "Amazon Echo Gets Open Source Brain Transplant"](https://hackaday.com/2021/03/22/amazon-echo-gets-open-source-brain-transplant)
- Difference from Fire tablets/Fire TV: same Fire OS core, but the Show's UI/launcher is the Alexa smart-display shell; ADB and bootloader unlock are not exposed to users ("the Echo Show 8 (3rd Gen) does not expose an officially usable bootloader unlock or ADB interface. There is basically nothing that we can do officially with this device") — [XDA rhodes thread](https://xdaforums.com/t/root-system-hardware-amazon-echo-show-8-3rd-gen-rhodes-pop1-via-uart-and-emmc.4793346). Fire OS 6 on Gen 1 "blocks the installation of third-party apps via ADB" on some versions — [XDA crown unlock thread](https://xdaforums.com/t/unlock-root-twrp-unbrick-amazon-echo-show-8-1st-gen-2019-crown.4766687/)

### Inferences
- For custom-Linux purposes the Show 8 is an Android tablet with a screen-first launcher, not an embedded-Linux appliance: mainline-style Linux requires either (a) an unlocked bootloader + Android kernel (Gen 1, achieved via LineageOS), or (b) a container/chroot on the stock locked kernel (Gen 3, achieved via pivot_root).
- The "Azulia" / buildroot "smart display software" naming in the research brief: no source found using "Azulia" for the Echo Show 8; the buildroot-style Linux applies to the Echo speaker line (2014-era Echo, kernel 2.6.x per Hackaday above). Treat that as a misattribution unless another researcher has primary evidence.

### Gaps
- Which Fire OS / Android version string Gen 3 reports in `ro.build.version` — not directly quoted in sources; inferred from the identical version line (33857737604) shared with Fire OS 7 devices and its Android-9-era security architecture. Amazon's dev-specs page does not list the Show 8 3rd Gen itself.
- Whether any Echo Show 8 SKU has shipped Vega OS via OTA — unverified; Wikipedia's claim vs. Amazon dev docs conflict noted above.

---

## 3. Boot chain: bootloader, secure boot, dm-verity, signed partitions, TrustZone

### Takeaway
Boot security escalates sharply per generation. Gen 1 (MediaTek): BROM → Preloader → LK (Little Kernel) → Linux, with Amazon per-device signing keys but a boot-ROM/preloader exploit chain (amonet) that defeats it entirely — fully unlocked with TWRP and custom kernels since Nov 2025. Gen 2 (MT8183): same MediaTek chain, no known exploit, unlock_status false. Gen 3 (Amlogic): U-Boot with secure boot burned into the chip (eFuses), X.509 certificate check on boot images, dm-verity on /system and /vendor, fastboot and recovery locked at bootloader level — no custom kernels possible; the 2026 hardware hack works around it by reprogramming eMMC off-chip.

### Cited Findings

**Gen 1 (crown, MT8163)**
- Stock fastboot variables (captured by XDA user kitor): `secure: yes`, `prod: 1`, `unlock_status: false`, `kernel: lk`, `product: CROWN`, `antirback_tee_version: 0x0102`, `antirback_lk_version: 0x0001`, `antirback_pl_version: 0x0001`, `rpmb_state: 1`, `warranty: no`, plus `unlock_code` and `otu_code` fields — [XDA crown unlock thread](https://xdaforums.com/t/unlock-root-twrp-unbrick-amazon-echo-show-8-1st-gen-2019-crown.4766687/)
- Boot chain components named in the thread's warnings: Preloader ("the primary bootloader that initializes the DRAM and other basic hardware"), LK, TZ/TEE partitions — "NEVER flash the TEE1, LK, or Preloader partitions… Doing so can PERMANENTLY BRICK your device" — [XDA crown unlock thread](https://xdaforums.com/t/unlock-root-twrp-unbrick-amazon-echo-show-8-1st-gen-2019-crown.4766687/)
- "Amazon uses different signing keys for each device (the only shared piece is the root public key)" — why the unlock must cross-flash a vulnerable Echo Show 5 Gen 1 demo-unit bootloader — [XDA crown unlock thread](https://xdaforums.com/t/unlock-root-twrp-unbrick-amazon-echo-show-8-1st-gen-2019-crown.4766687/)
- "Most units lack BROM USBDL access" (the MediaTek boot ROM's USB download mode), so a failed unlock can be an unrecoverable brick — [XDA crown unlock thread](https://xdaforums.com/t/unlock-root-twrp-unbrick-amazon-echo-show-8-1st-gen-2019-crown.4766687/)
- The exploit chain: amonet = "BootROM + Preloader exploit" (branches `mt8163-echo-show`, `mt8163-checkers`), originally "a MediaTek bootrom exploit and a LittleKernel bootloader exploit" by xyzz for the Fire HD 8 2018 (karnak), released Jan 2019 — [R0rt1z2/amonet on GitHub](https://github.com/R0rt1z2/amonet); [amonet mt8163-checkers README](https://github.com/R0rt1z2/amonet/blob/mt8163-checkers/README.md); [Firmware Security, 2019-01-31](https://firmwaresecurity.com/2019/01/31/amonet-a-bootrom-exploit-for-mediatek-devices)
- Two-stage design and the fastboot component: "They're usually divided into two parts… the bootrom / preloader part (used to flash things to the device) and the bootloader part (which makes the unlock persistent)." The exploits are called kamakiri and amonet; the persistent part is k4y0z's fastboot-mode buffer overflow ("arbitrary code execution after issuing a single fastboot command") turned into "amonet fastbrick, which is basically a one click bootloader unlock" — [Aftermath interview with R0rt1z2, 2026-03-12](https://aftermath.site/amazon-echo-show-jailbreak-lineageos)
- Result: bootloader unlock → TWRP 3.7.0_9-0, custom boot-mode switching remapped onto Mute/Vol keys, and an intentionally insecure "Preloader USBDL" recovery port (USB vendor 0e8d product 2000, "Product: MT8163 Preloader / Manufacturer: PWNED") usable with MTKClient to unbrick — [XDA crown unlock thread](https://xdaforums.com/t/unlock-root-twrp-unbrick-amazon-echo-show-8-1st-gen-2019-crown.4766687/)
- Which devices are covered vs. patched, per the community tracker: Echo Show 5 Gen 1 (checkers), Echo Show 8 Gen 1 (crown), Echo Show 5 Gen 2 (cronos) are unlockable; "Amazon has actively patched this exploit, so even if a port of amonet exists for your device, it may still not be unlockable" — [bootloader-unlock-wall-of-shame, Amazon section](https://github.com/zenfyrdev/bootloader-unlock-wall-of-shame/blob/main/brands/amazon/README.md)
- Gen 2 explicitly excluded: "the Echo Show 8 2nd Generation is not supported, as it uses a different SoC that is not vulnerable to this exploit" — R0rt1z2 in [Hackaday comments, 2026-01-02](https://hackaday.com/2026/01/02/jailbreaking-the-amazon-echo-show/); no unlock exists — [XDA thread asking about Gen 2](https://xdaforums.com/t/amazon-echo-show-8-2nd.4777173)

**Gen 3 (rhodes, Amlogic POP1)**
- Boot chain and partition map read off the eMMC: `boot0/boot1` = unused/stale Amlogic hardware boot areas; `p1` = active Amlogic bootloader package; `p2` = Amlogic MPT partition table; then nvcfg, tee, boot, recovery, logo, misc, cri_data, vendor, odm, system, product, cache, data/userdata — [XDA rhodes thread](https://xdaforums.com/t/root-system-hardware-amazon-echo-show-8-3rd-gen-rhodes-pop1-via-uart-and-emmc.4793346)
- Secure boot is burned into the chip: "Fully protected chain, secure boot burned into the chip"; XDA user RoganDawes observed that with the eMMC removed, the board traps at EL3 ("ELR_EL3:0xFFFE6B14 / ESR_EL3:0x96000004 … Secure Boot (EL3) failing") — [XDA rhodes thread](https://xdaforums.com/t/root-system-hardware-amazon-echo-show-8-3rd-gen-rhodes-pop1-via-uart-and-emmc.4793346)
- U-Boot is present and interruptible over UART but neutered: "You can interrupt it with enter, but it will not bring you anything - literally everything (recovery, fastboot, u-boot, update) is locked" — [XDA rhodes thread](https://xdaforums.com/t/root-system-hardware-amazon-echo-show-8-3rd-gen-rhodes-pop1-via-uart-and-emmc.4793346)
- No custom kernels: "Cannot boot custom kernels (Amazon X.509 cert check on boot images)"; "No fastboot access (blocked by Amazon policy)"; "No kexec support (CONFIG_KEXEC=n in kernel)" — [XDA rhodes thread](https://xdaforums.com/t/root-system-hardware-amazon-echo-show-8-3rd-gen-rhodes-pop1-via-uart-and-emmc.4793346)
- dm-verity: /system (~3.1 GiB) and /vendor (300 MiB) mounted `wait,verify` (dm-verity EIO via dm-0/dm-1); /odm (8 MiB, ext4), /product, /data have NO verity — and /odm also lacks the `nosuid` mount flag, making it the sole persistent-root vector — [XDA rhodes thread](https://xdaforums.com/t/root-system-hardware-amazon-echo-show-8-3rd-gen-rhodes-pop1-via-uart-and-emmc.4793346)
- TrustZone: 52 MB secmon/TEE region reserved at 0x5100000 ("blocks SMC from EL1"); SMC call interface at 0x82000020/21; eFuse read/write SMCs at 0x82000030/31; Amlogic crypto engine (AES/SHA/TDES DMA) at 0xfe440400; `meson-rng` — [XDA rhodes thread](https://xdaforums.com/t/root-system-hardware-amazon-echo-show-8-3rd-gen-rhodes-pop1-via-uart-and-emmc.4793346)
- Root chain on Gen 3 (2026, by XDA user Vowed): off-chip eMMC write of an image to /odm containing a setuid-root binary + SELinux CIL policy + a tee-supplicant library hook (inherits init's capability bounding set) → `adb shell /odm/bin/su` gives uid=0; SELinux set permissive by the daemon; enables insmod, mounts, raw block writes, overlayfs shadowing of dm-verity files, and pivot_root/chroot into Debian or Alpine ("Debian-Debilexa", with a ported Wayland DRM driver running GNOME) — [XDA rhodes thread](https://xdaforums.com/t/root-system-hardware-amazon-echo-show-8-3rd-gen-rhodes-pop1-via-uart-and-emmc.4793346); [Hackaday, 2026-07-08](https://hackaday.com/2026/07/08/hacking-amazon-echo-show-8-3rd-gen-via-uart-and-emmc/)
- Same processor, same lockout on Echo Show 15: "The same can be applied to a newer Echo 15 with the same processor" — [XDA rhodes thread](https://xdaforums.com/t/root-system-hardware-amazon-echo-show-8-3rd-gen-rhodes-pop1-via-uart-and-emmc.4793346)

### Inferences
- Gen 1's anti-rollback is enforced in the MediaTek TEE with RPMB-backed version counters (`antirback_*`, `rpmb_state: 1`) — i.e., downgrade-blocking exists but the amonet chain sidesteps it by taking over before the counters matter.
- Gen 3's security model is "Amazon-controlled root of trust, fuse-burned": since the boot image cert check cannot be bypassed and kexec is compiled out, true custom Linux (own kernel) is impossible without an Amlogic boot-ROM-level exploit; the realistic ceiling is a full GNU userspace under the stock kernel via pivot_root, which is already demonstrated.
- Gen 2 sits in the worst position: MediaTek chain (so theoretically amonet-style) but MT8183-specific preloader/LK were never exploited publicly, and its bootloader ships locked with no unlock path.

### Gaps
- Gen 1 dm-verity: no source directly confirms whether stock Fire OS 6 on crown enforces dm-verity on /system (Fire OS 6 era devices generally do, but this was not verified for crown; TWRP's ability to flash stock update.bin suggests recovery-level access, not verity status).
- Gen 2 boot chain internals (preloader version, fastboot variables, anti-rollback counters) — no public dump found; the device has never been unlocked so nobody has published its `fastboot getvar` output.
- Whether Gen 3's fuse-burned secure boot uses Amlogic's standard eFuse key scheme (AES key in efuse + signed bootloader stages) or an Amazon-custom flow — the XDA thread confirms "burned into the chip" and X.509 checks but not the fuse/key layout.
- TrustZone payload identity on any generation (Amazon's own TEE OS vs. MediaTek/Amlogic stock) — not documented.

---

## 4. Hardware debug access: UART, eMMC, JTAG

### Takeaway
Yes — all generations have documented hardware debug access, and it is the only route on Gen 3. Gen 1 exposes a UART test point (TP55, 921600 baud) and a micro-USB port (for the amonet software exploit; no soldering needed). Gen 3 requires opening the case, cutting the eMMC shield, and soldering/pogo-ing to eMMC pads plus UART pads; JTAG pins exist and the JTAG_ON SMC is honored, but debug auth is locked by the bootloader so JTAG "does not work".

### Cited Findings
- Gen 1 UART: "Pin: TP55 (TX pin), Baud: 921600, Use: Kernel logs, LK dump for unsupported firmware"; a USB-TTL adapter is listed as optional debug equipment — [community LineageOS install guide](https://github.com/winmutt/self-2026/blob/master/LINEAGEOS_INSTALLATION_GUIDE.md)
- Gen 1 boot modes after unlock (button-remapped): HACKED FASTBOOT = hold Mute; TWRP/recovery = hold Vol Up; regular fastboot = all three buttons — [community LineageOS install guide](https://github.com/winmutt/self-2026/blob/master/LINEAGEOS_INSTALLATION_GUIDE.md); [XDA crown unlock thread](https://xdaforums.com/t/unlock-root-twrp-unbrick-amazon-echo-show-8-1st-gen-2019-crown.4766687/)
- Gen 3 UART: solder wires to UART pads, then `sudo minicom -b 921600 -D /dev/ttyUSB0` shows U-Boot logs; interruptible with Enter (but locked, see section 3) — [XDA rhodes thread](https://xdaforums.com/t/root-system-hardware-amazon-echo-show-8-3rd-gen-rhodes-pop1-via-uart-and-emmc.4793346)
- Gen 3 eMMC: 8 GB BGA-153 eMMC under a metal can that must be removed or cut open; pads for the eMMC bus are exposed for fine-wire soldering or a custom pogo-pin adapter; the pogo pads "are located in pretty fine and unified locations across Amazon POP1 Processors" (raising hopes of a reusable 3D-printed stencil); wires must be under ~5 cm — [XDA rhodes thread](https://xdaforums.com/t/root-system-hardware-amazon-echo-show-8-3rd-gen-rhodes-pop1-via-uart-and-emmc.4793346); [Hackaday, 2026-07-08](https://hackaday.com/2026/07/08/hacking-amazon-echo-show-8-3rd-gen-via-uart-and-emmc/)
- Gen 3 eMMC read/write is "the ONLY way to write the ODM partition. Fastboot is fully blocked on this device"; required gear: eMMC reader/programmer (chip clip or pogo adapter), Linux host with ARM32 cross-compiler, e2fsprogs, ADB tools — [XDA rhodes thread](https://xdaforums.com/t/root-system-hardware-amazon-echo-show-8-3rd-gen-rhodes-pop1-via-uart-and-emmc.4793346)
- Gen 3 JTAG: JTAG_A pins on the GPIOD bank (TCK=GPIOD_6, TMS=GPIOD_7, TDI=GPIOD_8, TDO=GPIOD_9, 3.3 V always-on domain); a kernel module can call the JTAG_ON SMC and all 9 core types accept it, "BL31 does not restrict it" — but "Step 5: Connect JTAG adapter (does not work, bootloader locks)" — [XDA rhodes thread](https://xdaforums.com/t/root-system-hardware-amazon-echo-show-8-3rd-gen-rhodes-pop1-via-uart-and-emmc.4793346)
- Same-family documentation exists for Echo Show 15 (identical Pop1 processor): a forensic repo with per-interface docs — hardware, micro-USB, UART, "eMMC Interface and Partitions", micro-HDMI — [jcrasselt/amazon-echo-show-15](https://github.com/jcrasselt/amazon-echo-show-15) (referenced by the XDA rhodes thread as the pogo-pad procedure source)
- Gen 3 disassembly is hard: "even the iFixit teardown guide for this device being rather daunting" — [Hackaday, 2026-07-08](https://hackaday.com/2026/07/08/hacking-amazon-echo-show-8-3rd-gen-via-uart-and-emmc/); iFixit guide: [Show 8 3rd Gen Display Assembly Replacement](https://www.ifixit.com/Guide/Amazon+Echo+Show+8+3rd+Gen+Display+Assembly+Replacement/179345)
- Earlier Echo precedent for hardware debug: the Linux-powered 1st-gen Echo speaker "had been rooted years before thanks to the diagnostic port on the bottom of the device" — [Hackaday, 2021-03-22](https://hackaday.com/2021/03/22/amazon-echo-gets-open-source-brain-transplant); a comment there also notes "a trick to force the Mediatek chips into bootloader mode by shorting a pin to ground" for MediaTek Echos.

### Inferences
- For Gen 3, hardware access is not just for recovery — it IS the exploit: the entire 2026 root method is "opening the device, accessing the eMMC through the motherboard pogo pads, modifying partitions. This completely bypasses the need for an unlocked Android system" ([XDA rhodes thread](https://xdaforums.com/t/root-system-hardware-amazon-echo-show-8-3rd-gen-rhodes-pop1-via-uart-and-emmc.4793346)).
- The Show 15 forensic repo (UART + eMMC + partition docs for the same SoC) is the best existing template for a Gen 3 hardware-hacking rig.

### Gaps
- No teardown photos located of Gen 1/Gen 2 boards showing debug pads (the crown UART pin TP55 is documented only in the community guide, not photographed in a source I could cite).
- Gen 2 (athena) hardware debug access: nothing published at all — no UART pinout, no eMMC points, no JTAG notes.
- Whether Gen 3's UART pads are also usable to enable ADB without eMMC (the rhodes thread's ADB-enabling step references a prior pogo-pin thread — "[Echo Show 8 3rd gen] unlock ADB via pogo pin", xdaforums.com/t/echo-show-8-3rd-gen-unlock-adb-via-pogo-pin.4685681 — cited inside the thread; I did not fetch that thread's contents).

---

## 5. Kernel version and published kernel sources

### Takeaway
Gen 1 runs an old MT8163 Android kernel (older than 4.9; Fire OS 6-era) and is the only generation with genuinely usable published GPL sources: Amazon publishes kernel source via www.amazon.com/echonotices / the "Source Code Notice for Echo and Alexa Devices" page, and the community (github.com/amazon-oss org) hosts the MT8163 kernel + Wi-Fi/BT module sources that LineageOS 18.1 for crown is built from (kernel 4.9.337, repurposed from Amazon's Fire OS 7 MT8163 tablet drop). Gen 3's kernel version is not publicly pinned down in the sources found, and no usable source drop for the Amlogic POP1/T7 Show 8 Gen 3 kernel was found.

### Cited Findings
- Amazon's GPL compliance route (applies to all Echo devices): "You may obtain a copy of the source code for GPL-Licensed Software at www.amazon.com/echonotices… or, for a period of three years after our last shipment… by sending a check or money order for $5 to: Open Source Group, Attn: Source Code Requests, Amazon Legal Department…" — [Amazon Legal Notices (example, Echo Dot 2nd Gen)](https://www.amazon.com/gp/help/customer/display.html?nodeId=202096090); a dedicated "Source Code Notice for Echo and Alexa Devices" page exists at [nodeId=201626480](https://www.amazon.com/gp/help/customer/display.html?nodeId=201626480) (page content could not be extracted to enumerate current tarballs — see Gaps)
- Gen 1 kernel (stock): Fire OS 6 / Android 7 stock ROM; community says their kernel is "a lot more recent than the one used on stock" — exact stock version not stated — [XDA crown LineageOS thread](https://xdaforums.com/t/rom-unofficial-11-crown-lineageos-18-1-for-the-amazon-echo-show-8-2019.4766709/)
- Gen 1 kernel (LineageOS 18.1 port, Nov 2025): "Switched to upstreamed kernel v4.9.337 (android-4.9-q)" — enabled by "Amazon giving some of their MT8163 tablets an upgrade to Android 9, we were able to 're-purpose' this kernel to run on the Echo Show" — [XDA crown LineageOS thread](https://xdaforums.com/t/rom-unofficial-11-crown-lineageos-18-1-for-the-amazon-echo-show-8-2019.4766709/)
- Source repos used for the crown port (all in the `amazon-oss` GitHub org): `android_kernel_amazon_mt8163` (GPL-2.0), `android_kernel_amazon_mt76x8-wifi`, `android_kernel_amazon_mt76x8-bt`, device trees `android_device_amazon_mt8163-common` and `android_device_amazon_crown` (Apache-2.0) — [XDA crown LineageOS thread](https://xdaforums.com/t/rom-unofficial-11-crown-lineageos-18-1-for-the-amazon-echo-show-8-2019.4766709/); build artifacts at [amazon-oss/releases](https://github.com/amazon-oss/releases/releases); kernel tree also mirrored at [bengris32/android_kernel_amazon_mt8163-4.9](https://github.com/bengris32/android_kernel_amazon_mt8163-4.9/blob/lineage-18.1/Kbuild)
- LineageOS 18.1 measured behavior on the MT8163 family: kernel 4.9.337, MediaTek legacy camera1 HAL (not mtkcam3) on checkers/crown/cronos — [jxlarrea/lineageos-echo-show-camera findings](https://github.com/jxlarrea/lineageos-echo-show-camera/blob/main/docs/findings.md)
- postmarketOS effort on Echo devices exists with stock-derived kernels: GitLab group `echo-pmos` hosts "Original Kernel for Echo Show 5" (checkers) and an Echo Dot 2nd-gen kernel "Import kernel from FireOS 6.5.5.9" — [GitLab echo-pmos/amazon-checkers-kernel](https://gitlab.com/echo-pmos/amazon-checkers-kernel); [echo-pmos/amazon-biscuit-kernel](https://gitlab.com/echo-pmos/amazon-biscuit-kernel)
- What Amazon source drops look like (Fire TV examples, mirrors created 2026-03): full kernel trees per SoC with bootloader and build scripts, e.g. `Amazon-OSS-Kernels/android_kernel_mstar_t31` ("SoC: T31. Kernel: 4.9") and `android_kernel_mstar_t1` ("SoC: T1. Kernel: 3.10") — [Amazon-OSS-Kernels org](https://github.com/Amazon-OSS-Kernels/android_kernel_mstar_t31)
- Gen 3 kernel: the XDA DTS/fstab analysis references a device tree with thousands of lines (eMMC at `mmc@fe08c000`, fstab entries, `CONFIG_KEXEC=n` noted) but the kernel version string is not quoted — [XDA rhodes thread](https://xdaforums.com/t/root-system-hardware-amazon-echo-show-8-3rd-gen-rhodes-pop1-via-uart-and-emmc.4793346)
- The related Echo Show 15 forensic project (same SoC) documents the Android build and kernel internals of the Pop1 platform — [jcrasselt/amazon-echo-show-15](https://github.com/jcrasselt/amazon-echo-show-15)

### Inferences
- Gen 1 has an end-to-end open stack today: GPL kernel source (Amazon drop, community-mirrored) + unlocked bootloader + LineageOS 18.1 + postmarketOS-style stock kernel experiments. Any custom-Linux effort should start here.
- The existence of `Amazon-OSS-Kernels` mirrors with per-SoC branches implies Amazon's echonotices drops are ongoing and reasonably complete (kernel + bootloader); a Gen 3/Amlogic drop likely exists on the notice page but was not verifiable this session.
- Kernel 4.9 is a ceiling for Gen 1 custom Android; newer Android struggles and 4.9 "lacks a lot of features required to run modern Android" ([XDA](https://xdaforums.com/t/rom-unofficial-11-crown-lineageos-18-1-for-the-amazon-echo-show-8-2019.4766709/)) — for a non-Android Linux the 4.9 base with MT76x8 drivers is quite usable.

### Gaps
- Exact stock kernel version strings for all three generations (Gen 1 stock, Gen 2, Gen 3) — not quoted in any source found; would require reading `/proc/version` on-device or the kernel source drop headers.
- The current contents of Amazon's "Source Code Notice for Echo and Alexa Devices" page (nodeId=201626480): the page exists but its tarball list could not be extracted (Amazon help pages render via JS); so "Amazon publishes the Echo Show 8 Gen 2/Gen 3 kernel source" is plausible-but-unverified.
- No gpl-violations.org actions against Amazon regarding Echo kernel sources were found (neither confirming nor ruling out historical enforcement).

---

## 6. Firmware updates: OTA, rollback, anti-rollback

### Takeaway
All generations auto-update over the internet ("Alexa-enabled devices receive software updates automatically"), with stock OTA images (`.bin` ZIP packages) publicly catalogued per device by FTVDB. Amazon actively uses OTA to patch bootloader exploits. Gen 1 (MT8163) has TEE/RPMB-enforced anti-rollback counters for preloader/LK/TEE (visible in fastboot as `antirback_*`), and Amazon also disabled boot-ROM download mode on later hardware revisions of other devices — but on crown, post-unlock you can flash any stock update.bin via TWRP, including older ones. Gen 3's update/recovery path is locked at U-Boot level; nothing about its rollback mechanism is publicly documented beyond that.

### Cited Findings
- OTA policy: "Alexa-enabled devices receive software updates automatically when connected to the Internet. These updates usually improve performance and add new features" — [Amazon: Alexa Device Software Versions](https://www.amazon.com/gp/help/customer/display.html?nodeId=GMB5FVUB6REAVTXY)
- Latest catalogued software versions (Amazon's page, as of 2026): Echo Show 8 1st Gen = 12584496772; 2nd Gen = 33857737604; 3rd Gen = 33857737604; note FTVDB records newer Gen 1 builds (e.g. Fire OS 6574.1 / NS65741 build 13222532484, uploaded 2026-09-28), so Amazon's own page lags the real OTA channel — [Amazon](https://www.amazon.com/gp/help/customer/display.html?nodeId=GMB5FVUB6REAVTXY); [FTVDB crown](https://ftvdb.com/echo/firmware/com.amazon.crown.android.os)
- Gen 1 firmware line: Fire OS 6.5.7.0 → 6.5.7.1 → 6.5.7.3 → 6.5.7.4 → "Fire OS 6574.1" (Aug 2025–Sep 2026), 18 public records — [FTVDB crown](https://ftvdb.com/echo/firmware/com.amazon.crown.android.os)
- Gen 2 firmware line: 17 public records Dec 2023–Jul 2026, latest Fire OS 7.5.7.3 (PS7573/6536) — [FTVDB athena](https://ftvdb.com/echo/firmware/com.amazon.athena.android.os)
- Stock recovery path on unlocked Gen 1: "Starting with version 2.0.0 of the exploit, going back to FireOS is as simple as installing a stock update.bin through TWRP… Firmware packages are ZIP files, so simply renaming them from .bin to .zip should work"; flashable stock versions include Fire OS 6.5.7.0 and 6.5.7.1 (i.e., downgrades are possible once unlocked) — [XDA crown unlock thread](https://xdaforums.com/t/unlock-root-twrp-unbrick-amazon-echo-show-8-1st-gen-2019-crown.4766687/); stock packages sourced from [FTVDB](https://ftvdb.com/)
- Anti-rollback on Gen 1: separate version counters for TEE, LK, and preloader (`antirback_tee_version: 0x0102`, `antirback_lk_version: 0x0001`, `antirback_pl_version: 0x0001`) with `rpmb_state: 1`, implying replay-protected storage enforces boot-component downgrade blocking — [XDA crown unlock thread](https://xdaforums.com/t/unlock-root-twrp-unbrick-amazon-echo-show-8-1st-gen-2019-crown.4766687/)
- Amazon's patching behavior: "Amazon has actively patched this exploit, so even if a port of amonet exists for your device, it may still not be unlockable"; on Fire TV Stick 4K 1st Gen, "devices with serial numbers containing VM190 or higher are shipped with DL-Mode disabled in bootROM" (Amazon disabling boot-ROM download mode in later hardware revisions); a FireOS 6.2.8.1 flaw later patched allowed unlocking despite that — [bootloader-unlock-wall-of-shame](https://github.com/zenfyrdev/bootloader-unlock-wall-of-shame/blob/main/brands/amazon/README.md)
- The crown unlock's bricking warning is itself anti-rollback-adjacent: "most units lack BROM USBDL access, so a brick can therefore be permanent and unrecoverable" — [XDA crown unlock thread](https://xdaforums.com/t/unlock-root-twrp-unbrick-amazon-echo-show-8-1st-gen-2019-crown.4766687/)
- Gen 3: OTA/update machinery is locked at the bootloader ("literally everything (recovery, fastboot, u-boot, update) is locked"); flashing modified images bricks the device — "Bootloader rejects modified images… Recovery no longer starts" — [XDA rhodes thread](https://xdaforums.com/t/root-system-hardware-amazon-echo-show-8-3rd-gen-rhodes-pop1-via-uart-and-emmc.4793346)
- Practical consequence of OTA on a rooted Gen 3: the ODM-partition root survives reboots but an OTA that reimages partitions would need the eMMC rig again (the thread's "Device becomes temporarily unbootable / bootloader rejects modified images" warnings) — [XDA rhodes thread](https://xdaforums.com/t/root-system-hardware-amazon-echo-show-8-3rd-gen-rhodes-pop1-via-uart-and-emmc.4793346)
- Community interview confirming OTA-vs-exploit dynamics: "Amazon patches these exploits over time depending on the device" — [Aftermath, 2026-03-12](https://aftermath.site/amazon-echo-show-jailbreak-lineageos)

### Inferences
- On Gen 1, once amonet is installed, you control the update path: stock OTAs can be taken (or refused) deliberately, and downgrade images remain flashable — OTA is not a threat to an unlocked device.
- On Gen 3, stock OTAs are the main way to lose a hardware-hack root: Amazon's U-Boot-level locks mean there is no user-facing flash path at all, and an OTA that touches /odm or tightens SELinux policy would require re-soldering to the eMMC to re-root.
- Amazon's demonstrated countermeasure repertoire: OTA code patches (fastboot/preloader fixes), per-device signing keys, and manufacturing-time boot-ROM DL-mode disablement — expect all three on any future device.

### Gaps
- No documentation of Gen 3's OTA partition layout (whether an A/B slot scheme or recovery-based) — its partition list shows a single `boot`/`recovery` set, suggesting non-A/B, but this is unconfirmed.
- No source on whether Amazon's Gen 3 eFuses encode an anti-rollback version (typical for Amlogic secure boot) — only "secure boot burned into the chip" is documented.
- Whether Amazon's `otu_code` / `unlock_code` fastboot fields on Gen 1 correspond to a latent official unlock mechanism (the fields exist but no one has activated one).

---

## Cross-cutting summary table (compiled from the cited sources above)

| | Gen 1 (2019) | Gen 2 (2021) | Gen 3 (2023) |
|---|---|---|---|
| Model / codename | C7H6N3 / `crown` (AEOCW) | A8H3N2 / `athena` (AEOAT) | R85SD6 / `rhodes` (`pop1`) |
| FCC ID | 2ARO5-7879 | 2AWTZ-8462 | unconfirmed (2A4DH Sept-2023 filings suspected) |
| SoC | MediaTek MT8163 (4× A53, Mali-450) | MediaTek MT8183 (octa) | Amlogic P0P1-R "Pop1/PopcornA" + AZ2 NPU (Amlogic T7-gen; A73+A53 per DTS vs "A55" per Amazon docs — conflict) |
| RAM / storage | 2 GB DDR3 (community-documented) / 8 GB eMMC | 2 GB DDR3 (TechInsights) / 8 GB | 2 GB LPDDR4 (SK Hynix) / 8 GB Samsung MLC NAND (TechInsights) |
| OS | Fire OS 6 (Android 7) | Fire OS 7 (Android 9) | Android-9-generation Fire OS 7 stack (SELinux/dm-verity, per 2026 on-device evidence) |
| Boot chain | MTK BROM → Preloader → LK → kernel; per-device keys, shared root key; TEE w/ RPMB anti-rollback counters | Same MTK chain, never exploited | Amlogic: eMMC boot0/boot1 unused, signed bootloader package (p1) → U-Boot; secure boot burned in chip; X.509 check on boot images; 52 MB TEE |
| Unlock status | **Fully unlocked** (amonet, Nov 2025): TWRP 3.7.0_9-0, LineageOS 18.1 (Android 11, kernel 4.9.337), fastboot, custom kernels | **No unlock**; SoC not vulnerable to amonet | **No software unlock possible**; hardware root via eMMC (2026): ADB + setuid root on /odm + Debian via pivot_root; no custom kernels, no kexec |
| Hardware debug | UART TP55 @921600; micro-USB; remapped boot-mode buttons; post-exploit insecure preloader USBDL (MTKClient) | Nothing published | UART pads @921600 (U-Boot visible but locked); eMMC BGA-153 pads under shield can (pogo/solder); JTAG pins GPIOD_6–9 exist but debug-auth locked by bootloader |
| Kernel sources | Yes — Amazon echonotices drop, mirrored at github.com/amazon-oss (mt8163 + mt76x8 wifi/bt); postmarketOS stock kernel on GitLab | Presumably in echonotices drop (unverified) | Unverified (echonotices page not enumerable; Show 15 forensic repo documents platform) |

Sources are hyperlinked inline throughout; key primary sources: [TechInsights Gen 3 deep-dive](https://www.techinsights.com/blog/deep-dive-teardown-amazon-echo-show-8-3rd-gen-r855d6-smart-display), [XDA crown unlock thread](https://xdaforums.com/t/unlock-root-twrp-unbrick-amazon-echo-show-8-1st-gen-2019-crown.4766687/), [XDA crown LineageOS thread](https://xdaforums.com/t/rom-unofficial-11-crown-lineageos-18-1-for-the-amazon-echo-show-8-2019.4766709/), [XDA rhodes (Gen 3) thread](https://xdaforums.com/t/root-system-hardware-amazon-echo-show-8-3rd-gen-rhodes-pop1-via-uart-and-emmc.4793346), [R0rt1z2/amonet](https://github.com/R0rt1z2/amonet), [WikiDevi](https://wikidevi.wi-cat.ru/Amazon_Echo_Devices), [FTVDB](https://ftvdb.com/echo/firmware/com.amazon.crown.android.os), [Amazon compliance/dev/software-version pages](https://www.amazon.com/gp/help/customer/display.html?nodeId=GMB5FVUB6REAVTXY), [Hackaday](https://hackaday.com/2026/07/08/hacking-amazon-echo-show-8-3rd-gen-via-uart-and-emmc/), [Aftermath interview](https://aftermath.site/amazon-echo-show-jailbreak-lineageos).
