# Echo Show 8 (2025, 4th Gen) — Hardware & Firmware Architecture Reference

Device under study: Amazon Echo Show 8 4th generation, announced at Amazon's Devices & Services event Sept 30, 2025, released November 2025. ASIN B0DC93K4NW, $199.99, 8.7" 1340×800 display, model number **MD6NE4** (responsible FCC party: Amazon.com Services LLC). Researched as of October 2026 (~11 months after launch).

---

## 1. What is the AZ3 Pro chip?

### Takeaway
Amazon presents AZ3 Pro as its own "custom-designed silicon" — a full system-on-chip with an integrated AI accelerator, not just the neural-engine coprocessor that AZ1/AZ2 were in earlier devices. But there is **no die-level teardown (TechInsights or otherwise) of AZ3/AZ3 Pro published anywhere as of Oct 2026**, so the CPU complex, fab/process, and any silicon-partner involvement (MediaTek/Amlogic) are publicly unknown.

### Cited Findings
- Amazon's launch announcement: "At the core of our new Echo devices are two custom-designed silicon chips—AZ3 and AZ3 Pro—with an all-new AI Accelerator designed to run AI edge models of the future. Echo Dot Max uses AZ3… Echo Studio, Echo Show 8, and Echo Show 11 include the more advanced AZ3 Pro, which builds on the power of AZ3's audio capabilities, adding support for state-of-the-art language models and vision transformers." AZ3 claims >50% better wake-word detection and "our best microphone array yet." — [Amazon (aboutamazon.com)](https://www.aboutamazon.com/news/devices/amazon-new-echo-devices-alexa-plus)
- AZ3/AZ3 Pro are the foundation of "Omnisense," Amazon's sensor-fusion platform combining camera, microphones, radar, ultrasound and other sensors for on-device, privacy-claimed multimodal AI. — [Futurum Group](https://futurumgroup.com/insights/amazons-az3-chips-help-advance-voice-first-ai-agentic-ux); [How-To Geek](https://www.howtogeek.com/amazon-new-smart-speakers-are-even-smarter)
- PCMag positions AZ3 Pro as "a distinct step up from the third-gen Echo Show 8's AZ2 neural engine, which was already several years old" — i.e., in the Gen 3, AZ2 was a neural *engine* next to a main SoC, while AZ3 Pro is marketed as the Show 8 2025's *processor*. — [PCMag review](https://me.pcmag.com/en/smart-displays/33403/amazon-echo-show-8-4th-gen-2025)
- Wikipedia's Echo Show comparison table shows the evolution explicitly: Gen 3 Show 8 (2023) = "Octa-Core SoC + AZ2 Engine"; 4th Gen (2025) = "AZ3 Pro with AI Accelerator" in the Processor row. — [Wikipedia: Amazon Echo Show](https://en.wikipedia.org/wiki/Amazon_Echo_Show)
- A July 2026 secondary source (TechTimes) asserts Amazon "built its own chip rather than continuing to rely on Qualcomm processors," listing AZ3 (Dot Max) and AZ3 Pro (Studio, Show 8, Show 11), and that pre-Sept-2025 Echos used third-party Qualcomm/MediaTech processors. This is an aggregator restating Amazon's "custom-designed" claim, not independent silicon analysis. — [TechTimes](https://www.techtimes.com/articles/319705/20260704/amazon-alexa-device-ai-confirmed-what-az3-chip-handles-what-still-goes-cloud.htm)
- TechInsights has published deep-dive teardowns of the **Echo Hub (G6A87E)**, **Echo Show 8 3rd Gen (R855D6)**, Echo Show 5, and Echo Show 21 — but **no teardown of the 2025 Show 8/11 or an AZ3/AZ3 Pro die analysis** appears in their catalog. — [TechInsights Echo Hub teardown](https://www.techinsights.com/blog/deep-dive-teardown-amazon-alexa-echo-hub-g6a87e-smart-display); [TechInsights Show 8 3rd Gen teardown](https://www.techinsights.com/blog/deep-dive-teardown-amazon-echo-show-8-3rd-gen-r855d6-smart-display)

### Inferences
- **Labeled inference from precedent:** Amazon's earlier "AZ" parts sat *alongside* a merchant SoC (Gen 3 Show 8: Amlogic Pop1/A311D2 main SoC + AZ2 engine). The 2025 marketing language ("processor… AZ3 Pro") and its use in three different device categories suggests AZ3 Pro is now the main SoC with an integrated NPU — but Amazon has published no CPU core complex, GPU, process node, or fab data, and Amazon has historically co-designed/re-badged merchant silicon (e.g., "Amlogic PopcornA (Pop1)" branding on Amlogic parts). Whether AZ3 Pro is truly Amazon-internal RTL or a deeper partnership with MediaTek/Amlogic (who fabbed it, which cores, which node) **cannot be determined from public sources**.
- Since no FCC internal-photo set or teardown has been indexed/reported for MD6NE4, even the board-level silicon complement (Wi-Fi/BT combo chip, PMIC, eMMC vendor) is unknown.

### Gaps
- No TechInsights/iFixit/FCC-teardown of the 2025 Show 8 (MD6NE4) or Show 11 found; the FCC ID for MD6NE4 is not on the device's public Amazon help page text I could retrieve (Amazon says "FCC ID can be found on the device") and I could not locate the grant in the fccid.io index from model number alone. **Open follow-up: read the FCC ID off a physical unit and pull the internal-photo exhibit from fccid.io** — that would reveal the SoC package marking and board layout without buying a TechInsights subscription.
- Fab partner, process node, CPU/GPU complex of AZ3 Pro: unknown publicly.
- Codename for the 2025 Show 8 (Gen 3 was "rhodes"/AEORH; Gen 1 was "crown"): not found in public sources.

---

## 2. Operating system — Vega OS and how "apps" work

### Takeaway
The Echo Show 8 (2025) is effectively confirmed to run Amazon's **Vega OS**, Amazon's in-house **Linux-based** (non-Android) operating system: Amazon's own developer pages omit the 2025 Show 8/11 from all Fire OS/Android device-spec tables, Wikipedia records the Echo Show line as "Echo Ship/Vega OS (2023+, Linux-based)", and Amazon publishes a differently-named source drop for it ("Echo_Show_8_Source_Release_1571.tar.gz") matching the Vega-generation devices. On Vega OS there is **no Android runtime, no APK sideloading, no ADB, and no app mechanism that grants third parties hardware access** — "apps" are curated web apps (React TV Web Apps running in a Silk-derived web runtime) plus Alexa skills.

### Cited Findings
- Vega OS background: Linux-based in-house OS, plan first revealed by Janko Roettgers (Lowpass) in late 2023 as an Android/Fire OS replacement intended eventually for Amazon's whole hardware line. — [Lowpass](https://www.lowpass.cc/p/amazon-vega-os-echo-show-5); [Android Police](https://www.androidpolice.com/amazon-replacing-android-on-some-of-its-devices)
- First Vega device was the **Echo Show 5 3rd Gen (2023)** — spotted running "OS 1.1" instead of a Fire OS 6/7 version number; Netflix video didn't work because Android app support was gone. — [AFTVnews](https://www.aftvnews.com/amazons-in-house-vega-os-is-already-on-the-new-echo-show-5); [Liliputing](https://liliputing.com/amazons-vega-os-is-already-running-on-some-echo-show-devices-goodbye-android)
- Echo Hub was the second Vega device, Echo Spot the third (identified via Amazon's published open-source notices, which show Linux instead of Android components). — [AFTVnews](https://www.aftvnews.com/amazon-charges-ahead-with-its-in-house-vega-os-on-yet-another-new-device)
- AFTVnews (Oct 14, 2025, at the launch of the new Shows): "Before the recent switch to Vega OS, all Echo devices ran on a modified version of Amazon's Android-based Fire OS." — [AFTVnews](https://www.aftvnews.com/amazons-older-echo-show-5-echo-spot-and-echo-dot-have-been-rooted-with-unlocked-bootloaders-and-custom-lineageos-roms)
- Wikipedia Echo Show infobox: Operating system = "Fire OS (Android-based, used 2017–2023); **Echo Ship/Vega OS** (Linux-based, used 2023+)". The article's Echo Show 5 (3rd Gen) section states it "was the first device to ever run Vega OS." — [Wikipedia: Amazon Echo Show](https://en.wikipedia.org/wiki/Amazon_Echo_Show)
- Amazon's developer "Device Specifications: Echo Show" page (updated Nov 5, 2025) lists only Fire OS 7 / Android 9 devices (Show 15 gen 1/2, Show 21) — the 2025 Show 8 and Show 11 are **absent from the Android/Fire OS app-target tables**. The parallel Fire TV "Fire OS Overview" page lists Vega devices explicitly (Fire TV Stick 4K Select `AFTCA002`, Fire TV Stick HD 2026 `AFTCL001` — "Vega OS version: OS 1.1") but does not include Echo Show devices at all. — [Amazon developer docs](https://developer.amazon.com/docs/device-specs/device-specifications-echo-show.html); [Fire OS Overview](https://developer.amazon.com/docs/fire-tv/fire-os-overview.html)
- Amazon publishes a source drop for the new device on its "Source Code Notice for Echo and Alexa Devices" page under "Echo Show 8 (2025 release)": **`Echo_Show_8_Source_Release_1571.tar.gz`** (and "Echo Studio (2025 release)": `Echo_Studio_src-1571.tar.gz`). The naming scheme differs from all Fire OS-era drops (e.g., `Echo_Spot_src-5.5.6.9-…tar.bz2`, `Echo_v2_src-6.5.7.1-…tar.bz2`) and matches the new-generation release train that AFTVnews used to identify Vega OS on Echo Hub/Spot. — [Amazon Source Code Notice](https://www.amazon.com/gp/help/customer/display.html?nodeId=201626480)
- Vega OS app model (documented for Fire TV, the same OS family): no sideloading, no Downloader app; apps are built as web apps — Amazon's developer blog "Getting started with Vega OS development" (July 2026) describes Vega Developer Tools including an x86_64 "Vega Virtual Device", and Zattoo/Witbe coverage describes a "hybrid execution model" where apps run as web technologies on Linux. AFTVnews characterizes Vega apps as "Silk Web Apps for TV running Linux" (React TV Web Apps). — [AFTVnews on Vega launch](https://www.aftvnews.com/amazon-launches-vega-os-for-fire-tv-heres-how-it-affects-new-old-fire-tvs-apps-and-sideloading); [Amazon developer blog](https://developer.amazon.com/apps-and-games/blogs/2026/07/guide-to-building-for-fire-tv-on-vega-os); [Witbe](https://www.witbe.net/articles/amazon-fire-tv-stick-vega-os-testing-automation); [AFTVnews on sideloading loss](https://www.aftvnews.com/these-are-the-fire-tvs-that-dont-support-sideloading-or-downloader-due-to-vega-os-replacing-fire-os)
- Existing Fire OS devices will not be converted to Vega — Amazon has never migrated devices across an OS family boundary. — [AFTVnews](https://www.aftvnews.com/these-are-the-fire-tvs-that-dont-support-sideloading-or-downloader-due-to-vega-os-replacing-fire-os)

### Inferences
- **Labeled inference:** On the 2025 Show 8, third-party extensibility is limited to (a) Alexa skills/cloud-side integrations and (b) whatever curated web-app packaging Amazon permits (the "TV & Videos" experience with Prime Video/Netflix/YouTube apps are first-party or partnered web/native-Vega apps). No public SDK exposes the microphone array, camera, or Omnisense sensor stack to third parties; nothing in Amazon's developer documentation for Echo Show/Vega offers hardware-level API access. The mic/camera feed is processed by Amazon's own on-device stack (AZ3 Pro NPU + Omnisense).
- **Labeled inference:** "Echo Ship" in the Wikipedia infobox appears to be an internal/alternate name for the Echo-device variant of the Vega platform (unverified — the linked page is a red link; treat as a lead, not a fact).
- Whether Vega is Yocto/buildroot-based, uses systemd or Android-derived userland, or has any app sandbox/SDK at an OS level: **not publicly documented**. The `Echo_Show_8_Source_Release_1571.tar.gz` tarball is the primary artifact that would answer the build-system question (downloading and inspecting it is the obvious next step — see Gaps).

### Gaps
- No public architecture document for Vega OS exists (build system, init system, sandboxing, update/A-B mechanism). Not answerable without inspecting the source tarball or a live device.
- Amazon has not published which Fire TV/Echo devices run Vega on product pages (AFTVnews has criticized this), and I found no first-party Amazon statement that *explicitly* names the 2025 Show 8 as a Vega device — the conclusion rests on the converging evidence above. A device Settings → "software version" screen shot (expected to show an "OS 1.x" number rather than Fire OS 7.x/8.x) would close this conclusively.

---

## 3. Boot chain: secure boot, verified boot, signed partitions, dm-verity

### Takeaway
**Nothing about the 2025 Show 8's boot chain has been published or dumped** — no bootrom analyses, no partition-table dumps, no documentation. By contrast, the platform family's history shows Amazon ships locked bootloaders on all Echo/Fire devices, actively **disabled the bootrom USB-download (BROM/USBDL) exploit path in later hardware revisions** of the 2019 Echo Show line, and the 2023 Gen 3 could only be rooted by *physically* rewriting eMMC. The reasonable expectation for the 2025 model is a locked, signed boot chain with no exposed fastboot/adb, but that is inference, not measurement.

### Cited Findings
- Gen 3 (rhodes) root required hardware: the XDA guide "achieves persistent root access on the Amazon Echo Show 8 (3rd Generation, codename 'Rhodes', model AEORH) running Fire OS 7.5.7.2 (Android 9 Pie, kernel 5.4.86-ab72) on the Amlogic T7 (A311D2) SoC… This requires physical hardware access to the eMMC chip. This is NOT a software-only exploit. You need an eMMC reader/clip." Result: persistent root surviving reboots, full capability set (CapBnd=0x3fffffffff), SELinux permissive, loadable kernel modules — i.e., verified boot was ultimately bypassed by direct eMMC modification, not defeated in software. — [XDA Forums thread](https://xdaforums.com/t/root-system-hardware-amazon-echo-show-8-3rd-gen-rhodes-pop1-via-uart-and-emmc.4793346)
- Gen 1 (2019, "crown"/"checkers") software unlocks by XDA user Rortiz2 used (a) a fastboot-based exploit and (b) a bootrom (BROM/USBDL) path — and Wikipedia (citing the XDA threads) records that "the latter method only works on units manufactured in 2019, as **Amazon disabled BROM USBDL access on later hardware revisions**." This is direct evidence Amazon patches/hardens bootrom access in production when exploits surface. — [Wikipedia: Amazon Echo Show § Security](https://en.wikipedia.org/wiki/Amazon_Echo_Show); [XDA Show 5 1st gen thread](https://forum.xda-developers.com/t/unlock-root-twrp-unbrick-amazon-echo-show-5-1st-gen-2019-checkers.4762900/); [Amonet source](https://github.com/R0rt1z2/amonet/tree/mt8163-checkers)
- AFTVnews: Echo devices "have remained locked down" throughout the Fire OS era despite having the same theoretical potential as Fire TVs/tablets. — [AFTVnews](https://www.aftvnews.com/amazons-older-echo-show-5-echo-spot-and-echo-dot-have-been-rooted-with-unlocked-bootloaders-and-custom-lineageos-roms)
- The 2019 Pwn2Own Echo Show 5 hack was a Chromium/patch-gap bug yielding code execution — not a bootchain unlock. — [Wikipedia § Security](https://en.wikipedia.org/wiki/Amazon_Echo_Show)

### Inferences
- **Labeled inference from Gen 3 + Gen 1 history:** the 2025 Show 8 almost certainly has (a) a locked bootloader with no user-facing unlock, (b) signed firmware/update images, and (c) some verified-boot/dm-verity-equivalent on the Vega root filesystem. The Gen 3's eMMC-clip workaround shows Amazon's chain of trust stops at the bootROM↔eMMC boundary (the SoC did not enforce eMMC contents authenticity against tampering on the 2023 Amlogic platform). Whether AZ3 Pro's bootROM does better (e.g., signed boot images verified from mask ROM, eMMC secure partitioning, or fusing) is unknown.
- **Labeled inference:** no UART console/fastboot is likely exposed in normal firmware; on rhodes the UART path was available only as an on-board debug interface, not a user feature.

### Gaps
- No boot-chain documentation, partition dump, or hardware revision analysis exists for MD6NE4 as of Oct 2026. Nobody has publicly opened one. Answers require: teardown + UART pad identification + eMMC read (replicating the rhodes methodology), and/or analysis of the update image signing inside `Echo_Show_8_Source_Release_1571.tar.gz` (the tarball contains GPL sources, not Amazon's proprietary verified-boot config, but kernel config/cmdline would reveal dm-verity usage).

---

## 4. Kernel and kernel-source publication

### Takeaway
Amazon **does publish a GPL source drop for the 2025 Show 8**: `Echo_Show_8_Source_Release_1571.tar.gz` is listed for download on its "Source Code Notice for Echo and Alexa Devices" page. The kernel version and configuration inside that archive have not been publicly analyzed; the preceding Gen 3 ran a 5.4 Android-common kernel (5.4.86-ab72), while the Vega family runs a non-Android Linux userland whose kernel version is not documented outside the tarballs.

### Cited Findings
- Amazon's Source Code Notice page lists, under "Echo Show 8 (2025 release)": `Echo_Show_8_Source_Release_1571.tar.gz`, and under "Echo Studio (2025 release)": `Echo_Studio_src-1571.tar.gz`; older devices use Fire-OS-versioned archives (e.g., `Echo_Spot_src-5.5.6.9-20251120.tar.bz2`, `Echo_v2_src-6.5.7.1-20251024.tar.bz2`). The page states: "In accordance with certain free and open source software licenses, Amazon is pleased to make available to you for download an archive file of machine readable source code." — [Amazon Source Code Notice for Echo and Alexa Devices](https://www.amazon.com/gp/help/customer/display.html?nodeId=201626480)
- Gen 3 rhodes ran Fire OS 7.5.7.2 = Android 9 with Linux kernel **5.4.86-ab72** (ab72 = Amlogic-based kernel branch). — [XDA Forums thread](https://xdaforums.com/t/root-system-hardware-amazon-echo-show-8-3rd-gen-rhodes-pop1-via-uart-and-emmc.4793346)
- AFTVnews identified Vega OS on Echo Hub/Echo Spot precisely by inspecting these published source notices (Linux components, no Android). — [AFTVnews](https://www.aftvnews.com/amazon-charges-ahead-with-its-in-house-vega-os-on-yet-another-new-device)
- The Fire TV Vega devices' OS is versioned "OS 1.1" as of 2025–2026 devices, suggesting a young, separately-versioned release train from the 1571 source release. — [Amazon Fire OS Overview table](https://developer.amazon.com/docs/fire-tv/fire-os-overview.html)

### Inferences
- **Labeled inference:** `Echo_Show_8_Source_Release_1571.tar.gz` almost certainly contains the Linux kernel source for the AZ3 Pro platform (GPL obligation), which is the single most valuable public artifact for a port effort — it defines the device-tree, drivers (mic array codecs, camera sensor, display panel), and kernel config for the SoC. "1571" appears to be a shared release/build number for the 2025 Vega generation (Studio and Show 8 use the same number).
- **Labeled inference:** since AZ3 Pro silicon is new, the kernel will contain Amazon/Partner-specific drivers not in mainline — a mainline-Linux port would require substantial reverse engineering of the device tree and vendor drivers, analogous to the Amlogic A311D2 situation on rhodes.

### Gaps
- I did not download or open the 1571 tarball (Amazon page was intermittently inaccessible to automated fetch; the download link itself may require no auth). **Kernel version, config, dm-verity/AVB flags, and device-tree contents for the 2025 Show 8 remain unverified. This is the highest-value, cheapest next step for the overall hackability assessment.**

---

## 5. Hardware debug access (UART, eMMC, JTAG) and teardown status vs. Gen 3

### Takeaway
As of October 2026 **no one has published a teardown, board photo, UART/eMMC test-point map, or any hardware debug work on the 2025 Show 8** — coverage is entirely reviews; iFixit and TechInsights have not touched it. The realistic template is the Gen 3 (rhodes) XDA work: open the case, find the UART console pads, dump/rewrite eMMC with a clip, and rebuild boot images.

### Cited Findings
- Gen 3 (rhodes, AEORH, 2023) was rooted via **UART + eMMC**: persistent root achieved by dumping the eMMC, modifying boot/system images, and reflashing; requires an eMMC reader/clip and opening the device. (Thread includes the full hardware method.) — [XDA Forums](https://xdaforums.com/t/root-system-hardware-amazon-echo-show-8-3rd-gen-rhodes-pop1-via-uart-and-emmc.4793346)
- TechInsights deep-dive teardown of the Show 8 3rd Gen (R855D6): main PCB carries the Amlogic P0P1-R octa-core application processor; design wins split across vendors — Amlogic (SoC), MediaTek (Wi-Fi front-end), SK Hynix (2 GB LPDDR4), Samsung (NAND flash); 13 MP BSI CMOS image sensor; four microphones; 8" display. This is the level of board documentation that does *not yet exist* for the 2025 model. — [TechInsights blog](https://www.techinsights.com/blog/deep-dive-teardown-amazon-echo-show-8-3rd-gen-r855d6-smart-display)
- iFixit has repair guides/teardowns for Echo Show 8 Gen 2 and Gen 3 (display assembly, motherboard replacement) but **no device page or guide for the 2025 4th Gen**. — [iFixit Show 8 3rd Gen guide](https://www.ifixit.com/Guide/Amazon+Echo+Show+8+3rd+Gen+Display+Assembly+Replacement/179345); [iFixit Show 8 2nd Gen guide](https://www.ifixit.com/Guide/Amazon+Echo+Show+8+2nd+Generation+Motherboard+Replacement/171522)
- TechInsights has teardowns of adjacent 2023–2024 Amazon displays (Echo Hub G6A87E, Echo Show 21, Echo Show 5) — nothing for the Sept 2025 launches. — [TechInsights Echo Hub](https://www.techinsights.com/blog/deep-dive-teardown-amazon-alexa-echo-hub-g6a87e-smart-display)
- Community hardware hacking of Echo Shows so far covers only the 2019 devices (Show 5 gen 1/2 "checkers", Show 8 gen 1 "crown", Dot gen 2 "biscuit", Spot "rook"): bootloader unlock via amonet-style BROM exploit + TWRP + LineageOS 18.1 (Android 11); the BROM path was closed by Amazon on post-2019 hardware revisions. — [Derek Seaman jailbreak guide (Nov 2025)](https://www.derekseaman.com/2025/11/home-assistant-hacking-your-echo-show-5-and-8.html); [AFTVnews](https://www.aftvnews.com/amazons-older-echo-show-5-echo-spot-and-echo-dot-have-been-rooted-with-unlocked-bootloaders-and-custom-lineageos-roms); [Wikipedia § Security](https://en.wikipedia.org/wiki/Amazon_Echo_Show)
- FCC: the 2025 Show 8's compliance record exists (responsible party Amazon.com Services LLC; FCC ID printed on the device, per Amazon's safety page), but I found no public report extracting the internal-photo exhibits; Amazon routinely keeps internal photos under FCC confidentiality for recent filings (e.g., a Sept 2026 "Electronic Display Device" grant had photos held confidential). — [Amazon safety page](https://www.amazon.com/gp/help/customer/display.html?nodeId=Tz5QdAt8wypFUL0RbO); [Android Police on a separate confidential Amazon FCC grant](https://www.androidpolice.com/amazon-new-echo-display-on-the-way)

### Inferences
- **Labeled inference from rhodes:** the 2025 Show 8 board almost certainly exposes a UART console and bootable eMMC (industry-standard for this class of device), and the rhodes methodology (identify UART pads → get shell/U-Boot → dump eMMC → offline image modification → reflash) is the most plausible attack template. The new variables are (a) whether AZ3 Pro's bootROM validates signed boot images from eMMC (blocking offline modification) and (b) whether the eMMC is a removable BGA package a clip can reach.
- **Labeled inference:** the absence of any community hardware work is explained mostly by the device's age (11 months) and by the fact that the hackable-cheap used market (gen 1/gen 3 units) satisfies most Home Assistant demand — not by proven impossibility.

### Gaps
- No photographed board, no identified UART/eMMC/JTAG test points, no FCC internal-photo extraction for MD6NE4. Any statement about the 2025 Show 8's debug access beyond the family-level inference above would be fabrication. **Open follow-up: buy a unit, photograph the board, identify the SoC package marking (this also answers the AZ3 Pro fab question), and check for UART pads — exactly what the coordinator's project (Isocast) would need anyway.**

---

## 6. Microphone array and camera: specs and wiring

### Takeaway
Spec-level facts are documented: a **13 MP auto-framing front camera** with Omnisense-based visual identification, and a **5-microphone far-field array** (up from 4 on Gen 3), with a combined mic/camera electronic disable button (side-mounted) — Gen 3's physical camera shutter appears to have been *dropped*. Wiring details are unknown (no teardown), but Amazon states the AZ3 Pro chip itself runs the audio/vision AI (wake-word, presence, visual ID), implying the mic array and camera are processed directly by the AZ3 Pro's AI accelerator rather than a separate far-field DSP.

### Cited Findings
- Wikipedia comparison table (citing Amazon's product pages): Echo Show 8 4th Gen — 13 MP auto-framing camera; mic/camera-off button ✔ (side); **camera cover: –** (no mechanical shutter, unlike Gens 1–3); Zigbee/Matter/Thread hub; 2.8" woofer + 2 full-range drivers; 8.7" 1340×800; AZ3 Pro. — [Wikipedia: Amazon Echo Show](https://en.wikipedia.org/wiki/Amazon_Echo_Show) (citing [Amazon product listing](https://www.amazon.com/dp/B0DC93K4NW))
- Android Central: the 2025 Show 8 "has **five far-field array mics versus four**" (vs Gen 3) and "adds 3.3x zoom to the camera"; privacy controls described as "built-in camera shutter and microphone mute button" — note this *conflicts* with Wikipedia's "no camera cover" row; the weight of evidence (Amazon help text below) favors an electronic disable rather than a mechanical shutter. — [Android Central](https://www.androidcentral.com/best-amazon-echo-show)
- Amazon help ("Set Up Your Echo Show 8 (2025 release)"): "**The camera is enabled by default**… To enable/disable your device's microphones and camera, press the Microphones & Camera Enable/Disable button." A single electronic control for both, described as enable/disable rather than a hardwired disconnect. — [Amazon Customer Service](https://www.amazon.com/gp/help/customer/display.html?nodeId=T57xOIjRG5FLoczPDn)
- Amazon launch materials: "A 13-megapixel camera and the AZ3 Pro chip enable Omnisense and on-device AI to recognize a user as they approach, instantly delivering personalized, visual information"; AZ3 "powers our best microphone array yet… filtering out background noise and improving Alexa's ability to detect the wake-word by over 50%." — [Amazon (aboutamazon.com)](https://www.aboutamazon.com/news/devices/amazon-new-echo-devices-alexa-plus); [Sypnotix summary of launch release](https://www.sypnotix.com/news-ticker/amazon-debuts-alexa-plus-on-new-echo-devices)
- Omnisense sensor fusion spans camera, audio, radar, ultrasound and other sensors, processing on-device. — [Futurum Group](https://futurumgroup.com/insights/amazons-az3-chips-help-advance-voice-first-ai-agentic-ux); [How-To Geek](https://www.howtogeek.com/amazon-new-smart-speakers-are-even-smarter)
- Connectivity: Wi-Fi 6E (802.11ax), Zigbee/Matter hub, Thread border router; 2.8" woofer + 2 full-range drivers with spatial audio. — [Review Central](https://reviewcentralme.com/2025/12/22/review-amazon-echo-show-8-2025); [Wikipedia](https://en.wikipedia.org/wiki/Amazon_Echo_Show)
- Gen 3 baseline: 4-microphone far-field array, 13 MP camera *with physical cover*, per Amazon's own spec tables. — [Amazon developer docs](https://developer.amazon.com/docs/device-specs/device-specifications-echo-show.html)

### Inferences
- **Labeled inference:** mic-array and camera feeds terminate at the AZ3 Pro (integrated NPU + AI accelerator running wake-word, presence detection, and vision transformers). A separate far-field DSP (à la XMOS in early Echos, or the AZ2 coprocessor card in Gen 3) is unlikely given Amazon's own framing that AZ3/AZ3 Pro *are* the audio/vision engines — but the exact codec chain (analog mics → ADC → I2S/TDM → SoC vs digital mems → SoC) is unknown without a teardown.
- **Labeled inference:** the "Microphones & Camera Enable/Disable" button is most likely a GPIO-driven software path with an on-screen indicator (Amazon's text says "enable/disable", and the camera is enabled *by default*), not an analog hard-gate on the mic supply. On older Echo marketing Amazon explicitly claimed mics were "electrically disconnected"; no such wording appears in the 2025 Show 8 help text I found — a meaningful weakening for privacy claims, though confirming the button's electrical behavior requires board analysis.
- **Labeled inference for the parent objective (custom software with deep mic/camera access):** even if the device were rooted, the Omnisense audio/vision stack is Amazon-proprietary closed software on a proprietary NPU; third-party access to raw mic/camera streams would require either the kernel drivers exposed in the 1571 source drop (ALSA/V4L2 nodes, if the drivers are mainline-style) or reverse engineering. The Gen 3 LineageOS-style port experience (bengris32's "Full camera and bluetooth enablement for LineageOS 18.1" for gen-1 Shows) shows camera enablement on Echo hardware is nontrivial even with Android sources. — [GitHub echo-show topic](https://github.com/topics/echo-show)

### Gaps
- Exact mic components (MEMS vendor/model), count confirmation beyond Android Central's "five", ADC/audio codec chip, camera sensor model, and any radar/ultrasonic sensor hardware: unknown (no teardown).
- Whether the mute path is electrical or firmware-interpreted: unknown.
- Whether the 1571 kernel source exposes standard ALSA/V4L2 interfaces for the mic array and camera: unknown until the tarball is inspected.

---

## Bottom-line synthesis for the hackability question (for the report writer)

**Verified:** The 2025 Show 8 is a new-silicon (AZ3 Pro, internals unpublished), Vega OS (Linux, non-Android, no sideloading/ADB/app SDK) device, whose GPL kernel sources Amazon does publish (`Echo_Show_8_Source_Release_1571.tar.gz`), with no public teardown, root, or boot-chain work as of Oct 2026. Third-party "apps" are web apps/skills with no hardware access.

**Strong inference (from Gen 3 rhodes + Gen 1 history):** boot chain locked/signed; any custom-OS path likely requires the rhodes-style physical method (UART + eMMC), whose feasibility on AZ3 Pro depends on unknowns (bootROM signature enforcement, eMMC accessibility). Amazon demonstrably hardens bootrom exploit paths in hardware revisions.

**Cheapest concrete next steps, in order:** (1) download and inspect `Echo_Show_8_Source_Release_1571.tar.gz` (kernel version, config, dm-verity, device tree, ALSA/V4L2 drivers); (2) pull the FCC ID off a physical unit and fetch internal photos from fccid.io; (3) board-level teardown for UART/eMMC test points, replicating the XDA rhodes methodology.

**Source-quality note:** AZ3 Pro "custom silicon" claims all trace back to Amazon's own launch materials; TechTimes/Futurum add no independent silicon evidence. The only independent analyst of Amazon display silicon (TechInsights) has not published on AZ3. Treat "Amazon-designed, Amazon-fabbed" as unverified marketing until a die teardown exists.

## Source index
- Amazon launch/news: https://www.aboutamazon.com/news/devices/amazon-new-echo-devices-alexa-plus ; https://www.aboutamazon.com/news/devices/getting-started-echo-show-8-11-alexa-plus-features
- Amazon help/compliance (MD6NE4): https://www.amazon.com/gp/help/customer/display.html?nodeId=Tz5QdAt8wypFUL0RbO ; setup: https://www.amazon.com/gp/help/customer/display.html?nodeId=T57xOIjRG5FLoczPDn
- Amazon source-code notices: https://www.amazon.com/gp/help/customer/display.html?nodeId=201626480
- Amazon developer specs: https://developer.amazon.com/docs/device-specs/device-specifications-echo-show.html ; https://developer.amazon.com/docs/fire-tv/fire-os-overview.html ; Vega dev blog: https://developer.amazon.com/apps-and-games/blogs/2026/07/guide-to-building-for-fire-tv-on-vega-os
- Wikipedia (spec table, history, security section): https://en.wikipedia.org/wiki/Amazon_Echo_Show
- AFTVnews (Vega OS + rooting coverage): https://www.aftvnews.com/amazons-in-house-vega-os-is-already-on-the-new-echo-show-5 ; https://www.aftvnews.com/amazon-charges-ahead-with-its-in-house-vega-os-on-yet-another-new-device ; https://www.aftvnews.com/amazon-launches-vega-os-for-fire-tv-heres-how-it-affects-new-old-fire-tvs-apps-and-sideloading ; https://www.aftvnews.com/these-are-the-fire-tvs-that-dont-support-sideloading-or-downloader-due-to-vega-os-replacing-fire-os ; https://www.aftvnews.com/amazons-older-echo-show-5-echo-spot-and-echo-dot-have-been-rooted-with-unlocked-bootloaders-and-custom-lineageos-roms
- Lowpass (Vega scoop): https://www.lowpass.cc/p/amazon-vega-os-echo-show-5
- Android Police (Vega on Show 5 2023): https://www.androidpolice.com/amazon-replacing-android-on-some-of-its-devices
- Liliputing: https://liliputing.com/amazons-vega-os-is-already-running-on-some-echo-show-devices-goodbye-android
- XDA (Gen 3 rhodes UART+eMMC root): https://xdaforums.com/t/root-system-hardware-amazon-echo-show-8-3rd-gen-rhodes-pop1-via-uart-and-emmc.4793346 ; Gen 1 threads via Wikipedia refs; Amonet: https://github.com/R0rt1z2/amonet/tree/mt8163-checkers ; LineageOS releases: https://github.com/amazon-oss/releases/releases
- Derek Seaman (gen-1 jailbreak guide, Nov 2025): https://www.derekseaman.com/2025/11/home-assistant-hacking-your-echo-show-5-and-8.html
- TechInsights (Gen 3 + Echo Hub teardowns): https://www.techinsights.com/blog/deep-dive-teardown-amazon-echo-show-8-3rd-gen-r855d6-smart-display ; https://www.techinsights.com/blog/deep-dive-teardown-amazon-alexa-echo-hub-g6a87e-smart-display
- Reviews w/ specs: https://me.pcmag.com/en/smart-displays/33403/amazon-echo-show-8-4th-gen-2025 ; https://www.androidcentral.com/best-amazon-echo-show ; https://reviewcentralme.com/2025/12/22/review-amazon-echo-show-8-2025 ; https://www.the-ambient.com/reviews/amazon-echo-show-8-4th-gen-2025-review
- AZ3 analysis (secondary): https://futurumgroup.com/insights/amazons-az3-chips-help-advance-voice-first-ai-agentic-ux ; https://www.howtogeek.com/amazon-new-smart-speakers-are-even-smarter ; https://www.techtimes.com/articles/319705/20260704/amazon-alexa-device-ai-confirmed-what-az3-chip-handles-what-still-goes-cloud.htm
- Witbe (Vega app execution model): https://www.witbe.net/articles/amazon-fire-tv-stick-vega-os-testing-automation
