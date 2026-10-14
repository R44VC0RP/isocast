# Security research, exploits, and root/unlock attempts against the 2025 Echo Show 8 (4th Gen), Echo Show 11, Vega OS, and AZ-series silicon

Research date: 2026-10-11. Scope: the Echo Show 8 4th Gen (Nov 2025, AZ3 Pro, Vega OS,
Alexa+) and its sibling Echo Show 11, plus Vega OS and AZ1/AZ2/AZ3 silicon generally,
since the 2025 models are too new for much direct coverage. This file builds on — and
deliberately does not repeat — the two prior research notes in
`research_notes/Echo Show 8 custom Linux/` (`exploits_root.md`, `adjacent_devices.md`),
which cover the Gen 1 amonet jailbreak, the Gen 2 (MT8183) dead end, and the Gen 3
"rhodes" UART/eMMC hardware root in detail.

**Bottom-line answer to the objective:** As of October 2026, **no public path of any
kind — software exploit, hardware glitch, bootloader unlock, sideload, or forensic
method — exists for the 2025 Echo Show 8 (4th Gen, "madeleine", MD6NE4) or Echo Show
11.** Every unlock tool, ROM, and guide in the Echo scene explicitly excludes them.
The 2025 models sit on a triple barrier: (1) new silicon (Amazon AZ3 Pro, internal
part reportedly "Azma15", MediaTek co-design lineage — *not* the Amlogic Pop1 of the
rooted Gen 3, *not* the MT8163 of the amonet devices), (2) Vega OS instead of
Android/Fire OS, which removes the entire ADB/fastboot/TWRP/LK tooling surface the
old exploits relied on, and (3) zero published security research on Vega OS after
~3 years in the field (first shipped on the 3rd-gen Echo Show 5 "Hypnos" in May
2023). The only theoretical route is a ground-up hardware effort (eMMC chip-off +
UART) of the kind that worked on Gen 3 — nobody has published attempting it, and on
Vega OS even a successful eMMC write runs into firmware signing and the absence of
any Android userland to hijack. Absence of research was verified from multiple
independent search angles and is itself a key finding.

Device-model cheat sheet for this file:

| Device | Silicon | OS | Exploit status (Oct 2026) |
| --- | --- | --- | --- |
| **Echo Show 8 4th Gen (2025, "madeleine", MD6NE4)** | Amazon AZ3 Pro ("Azma15?" per device DB) | Vega OS | **Nothing public; no research; no teardown/pinout** |
| **Echo Show 11 (2025)** | Amazon AZ3 Pro | Vega OS | **Nothing public** |
| Echo Dot Max (2025, "macaroon") | Amazon AZ3 ("Azma11?") | Vega OS | Nothing public |
| Echo Show 5 3rd Gen (2023, "Hypnos") | (first Vega OS Echo, May 2023) | Vega OS | Nothing public; never in amonet support list |
| Echo Show 8 3rd Gen (2023, "rhodes", R85SD6) | Amlogic Pop1-R AP + AZ2 NPU | Fire OS 7 | Hardware root via UART + eMMC (XDA "Vowed", Jun 2026) |
| Echo Show 15 (2021, "hoya") / 15 2nd Gen | Amlogic AP + AZ2 | Fire OS 7 | eMMC/UART forensic route (Crasselt & Pugliese, DFRWS 2024) |
| Echo Show 5 G1/G2, Show 8 G1 (2019–21) | MediaTek MT8163 | Fire OS 6 | amonet BROM/preloader unlock + LineageOS (Nov 2025–) |

---

## 1. Any public exploits, root methods, bootloader unlocks, or jailbreak claims for the 2025 Show 8 / Show 11?

### Takeaway
None exist. Since the Nov 12, 2025 launch, no exploit, root, unlock, TWRP port,
custom ROM, sideload method, or credible jailbreak claim has surfaced for either
2025 smart display — verified across XDA threads, GitHub, Reddit, Hackaday,
AFTVnews, and the jailbreak-guide ecosystem. The community consensus is uniform:
only the 2019–2021 MT8163 devices (Show 5 G1/G2, Show 8 G1) are exploitable, and
the 2025 hardware/OS is out of reach of every known technique.

### Cited Findings
- Amazon launched the Echo Show 8 (4th Gen, $179.99) and Echo Show 11 ($219.99) on
  **November 12, 2025**, both with the AZ3 Pro chip and Alexa+ Early Access out of
  the box — [About Amazon (Sept 30, 2025)](https://www.aboutamazon.com/news/devices/amazon-new-echo-devices-alexa-plus);
  [Android Authority](https://www.androidauthority.com/amazon-echo-next-gen-lineup-3602556/)
- The 4th-gen Show 8 runs **Vega OS**: TechRadar's review states "Alexa and VegaOS
  generally run well, and the Echo Show 8 is fantastically responsive thanks to the
  AZ3 Pro chip" — [TechRadar review](https://www.techradar.com/home/smart-home-hubs/echo-show-8-4th-gen-2025-review);
  Tom's Guide's spec table lists "AZ3 Pro with AI Accelerator" (vs Gen 3's "AZ2
  Neural Network Engine") and **8GB eMMC** storage — [Tom's Guide review](https://www.tomsguide.com/home/smart-home/echo-show-8-4th-gen-review);
  [Tom's Guide 4th vs 3rd Gen comparison](https://www.tomsguide.com/home/smart-home/echo-show-8-4th-gen-vs-echo-show-8-3rd-gen)
- The model number of the 2025 Show 8 is **MD6NE4** — [Amazon safety & compliance
  page](https://www.amazon.sg/gp/help/customer/display.html?nodeId=Tz5QdAt8wypFUL0RbO);
  a community device database gives its internal codename as **"madeleine"** and its
  core chip as "Amazon Azma15? (AZ3 Pro NPU)" (the "?" marks the DB's own
  uncertainty), alongside the Echo Dot Max (2025, "macaroon", "Amazon Azma11? /
  AZ3 NPU") and Gen 3 Show 8 "Amlogic POP1-R (AZ2 NPU)" — [gilderchuck
  connect-the-dots device wiki](https://github-wiki-see.page/m/gilderchuck/connect-the-dots/wiki)
- The most current community reflash guide (Aug 31, 2026) explicitly warns that it
  covers only the 2019 first gen: "Not the 3rd-gen or 2025 Echo Show 8 sold new
  today, which are different hardware and aren't covered here… Everything Amazon
  sells new today is different hardware — both the 2023 3rd generation and the 2025
  4th generation — and none of this works on either" — [localsmarthomeguide.com
  reflash guide](https://localsmarthomeguide.com/articles/echo-show-8-to-home-assistant-smart-display/)
- Derek Seaman's jailbreak guide (Nov 2025, updated through 2026): "At the time of
  the publication of this post, NO OTHER Echo Show models OR generations are
  supported. You MUST use an Echo Show 5 Gen 1 or 2 or Echo Show 8 Gen 1 (only)" —
  [Derek Seaman guide](https://www.derekseaman.com/2025/11/home-assistant-hacking-your-echo-show-5-and-8.html)
- On sideloading the 2025 Echo Show 11 (Reddit, ~Apr 2026): "is there any guide how
  to sideload apps to amazon echo show 11? or its not possible right now" — answer:
  "To my knowledge, you can only side load apps to the first and second generation
  echo show five along with the echo show eight first generation" — [Reddit thread
  snapshot (r/amazonecho)](https://reddit.sentinel-team.org/posts/1sds6mo/snapshots/2026-04-07T00%3A00%3A00.5316Z)
- The amonet exploit chain cannot extend to new devices: the Echo Show 5 G2
  ("cronos") XDA FAQ states "the exploit targets MediaTek chipsets, while many Echo
  Show models use different SoCs. Second, the vulnerability was patched in March
  2019, so only devices released around 2019-2021 are technically exploitable" —
  [XDA cronos thread](https://xdaforums.com/t/unlock-root-twrp-unbrick-amazon-echo-show-5-2nd-gen-2021-cronos.4772596/);
  R0rt1z2 (exploit developer) on the Show 8 Gen 2: "The Echo Show 8 (2nd gen) is a
  lost cause, at least for now, since it uses a different processor… and Amazon
  also used different signing keys" — [XDA Show 2nd Gen thread](https://xdaforums.com/t/echo-show-2nd-generation-possible-to-unlock-similar-to-echo-show-8-1st-gen.4771627/)
- AFTVnews' Oct 2025 coverage of the unlock wave frames it as covering Amazon's
  **older** devices ("Amazon's older Echo Show 5, Echo Spot, and Echo Dot have been
  rooted…"); the 2025 Vega OS devices are absent from every tool and ROM list —
  [AFTVnews (Oct 14, 2025)](https://www.aftvnews.com/amazons-older-echo-show-5-echo-spot-and-echo-dot-have-been-rooted-with-unlocked-bootloaders-and-custom-lineageos-roms/)
- Targeted searches (XDA, GitHub, Reddit r/amazonecho + r/hacking, Hackaday,
  AFTVnews, Liliputing, general web) for "Echo Show 8 2025/4th gen root/jailbreak/
  unlock/hack", "Echo Show 11 sideload", "madeleine MD6NE4 hack", and "AZ3 Pro
  exploit" returned **zero** exploit claims, projects, or credible jailbreak
  promises for the 2025 models — only guides steering people to the 2019 hardware
  (this absence was cross-checked from ≥6 independent query angles during this
  research, Oct 2026)

### Inferences
- Classification per the assignment's taxonomy: **(d) nothing exists** for the 2025
  Show 8 and Show 11 — not even a claimed-but-unverified method. The only "new
 device" chatter found is users asking and being told "no."
- The 2025 models are protected by platform rotation, not by patching: they use a
  different SoC vendor lineage (AZ3 Pro, MediaTek co-design DNA — see §3), a
  different OS family (Vega OS, not Android/Fire OS), and different signing — so
  none of the three demonstrated Echo attack classes (MT8163 BROM chain, Amlogic
  eMMC/UART hardware root, Show 15 ADB-key injection) transfers without entirely
  new research.
- Demand exists (Show 11 sideload question, Show 8 Gen 2 questions, Home Assistant
  community interest), so the absence of results reflects genuine difficulty plus
  the newness of the platform, not lack of interest.

### Gaps
- The XDA "rhodes" Gen 3 thread (xdaforums 4793346) returns HTTP 403 to automated
  fetch; it could not be searched page-by-page for "madeleine"/2025-model
  questions. Absence of 2025-model attempts there is inferred from secondary
  coverage and search snippets, not from reading every post.
- No internal codename/model confirmation was found for the 2025 **Echo Show 11**
  (the device database snippet showed Show 8 and Dot Max but not Show 11's row).

---

## 2. Security research on Vega OS itself: CVEs, disclosures, conference talks, attack surface

### Takeaway
No CVE, vulnerability disclosure, ZDI advisory, DEF CON/Black Hat/REcon talk, or
academic paper targets Vega OS as of October 2026 — roughly 3.5 years after it first
shipped (Echo Show 5 3rd Gen, May 2023) and one year after it became the OS of
Amazon's flagship devices. What is publicly known about Vega's attack surface comes
from developer documentation and two independent discoveries: (1) Vega is a
closed, Linux-based OS (Linux kernel, glibc 2.35, React Native "KeplerScript"
runtime, VPKG packages, linker-namespace isolation, **no consumer sideloading**),
and (2) Vega ships **`vda`, a renamed but protocol-identical `adb`**, against which
stock Google adb works — the single most interesting published attack-surface fact.

### Cited Findings
- Vega OS first reached consumers quietly on the **3rd-gen Echo Show 5 (codename
  "Hypnos")** in May 2023: settings showed "OS 1.1" instead of a Fire OS 6.x
  number, open-source disclosures revealed a **modified Linux (5.16 kernel)**
  system with "Vega Build Hypnos Release" file names, and Netflix was absent —
  [AFTVnews (Nov 14, 2023)](https://www.aftvnews.com/amazons-in-house-vega-os-is-already-on-the-new-echo-show-5/);
  [Lowpass (Nov 16, 2023)](https://www.lowpass.cc/p/amazon-vega-os-echo-show-5);
  [9to5Google](https://9to5google.com/2023/11/14/amazon-android-echo-show-replaced/)
- As of Sept 30, 2025, Vega runs the new Fire TV Stick 4K Select and the new Echo
  devices: Amazon's own Vega OS team member — "we're launching both a FireTV stick
  on VegaOS and our new Echo devices" — [Scott Battaglia on LinkedIn (Sept 30,
  2025)](https://www.linkedin.com/posts/scottbattaglia_vega-amazon-appstore-developer-portal-activity-7378885763620315136-4Kar);
  TechRadar confirms VegaOS on the 2025 Show 8 — [TechRadar](https://www.techradar.com/home/smart-home-hubs/echo-show-8-4th-gen-2025-review)
- Vega OS internals (from Amazon's own developer docs): Linux userland with
  **glibc 2.35** (`libc.so.6`, `ld-linux-.so`), a public-ABI list, per-app
  **linker namespacing (LNS)** enforced at runtime by the dynamic linker, app
  packages in **VPKG** format (not APK), and KeplerScript (React Native-based)
  as the app runtime — [Amazon Vega docs: linker namespacing](https://developer.amazon.com/docs/vega/0.23/linker-namespacing.html);
  [Amazon Vega docs: release notes](https://developer.amazon.com/docs/vega/0.22/vega-release-notes.html)
- **`vda` is `adb` in disguise**: an independent test-lab analysis (Aug 2026) of a
  Vega Fire TV Stick found the Vega Device Adapter speaks the exact adb protocol
  ("Server Version 41… the exact protocol number Google's adb speaks"), uses the
  same `~/.android/adbkey` RSA keys, and **stock Google platform-tools drives a
  Vega device unmodified** — [RobusTest: "The one-line fork" (Aug 31,
  2026)](https://robustest.com/blog/the-one-line-fork)
- On Fire TV Vega devices, consumer sideloading is removed by design ("the familiar
  'allow apps from unknown sources' toggle… simply doesn't appear"), and the only
  sideload path requires an Amazon developer account, hidden developer options, a
  USB cable, and Amazon's proprietary tooling — and "every Vega OS app it
  sideloaded eventually stopped launching altogether" (intentional or a bug
  undetermined) — [Gadget Hacks / Cord Cutters (Apr 30, 2026, citing AFTVnews Oct
  2025)](https://cord-cutters.gadgethacks.com/news/amazon-fire-tv-vega-os-explained-app-gaps-and-lost-sideloading/)
- The only "security" items in Vega's public history are **npm dependency
  vulnerabilities in the SDK itself** (the `logkitty@0.6.1` chain removed from
  KeplerMediaDescriptor in SDK v0.22.3, Mar 2026) — developer-tooling hygiene, not
  OS vulnerabilities — [Amazon Vega release notes](https://developer.amazon.com/docs/vega/0.22/vega-release-notes.html);
  [Amazon Developer Community (Mar 5, 2026)](https://community.amazondeveloper.com/t/vega-sdk-v0-22-1-v0-22-3-patch-release-notes/27818)
- CVE/database searches (NVD/MITRE via multiple query phrasings: "Vega OS", "AZ2",
  "AZ3", "Echo Show", "Amazon Echo") surface **no CVE mentioning Vega OS or AZ
  silicon**; the only Echo-family CVEs remain legacy items — CVE-2018-11567
  (skill reprompt eavesdropping, DISPUTED, pre-Apr-2018), CVE-2022-25809 ("Alexa
  versus Alexa" self-issued voice commands on Echo Dot 3rd/4th Gen), and ZDI-20-537
  (Pwn2Own 2019 Echo Show 5 integer overflow, fixed) — [NVD CVE-2018-11567](https://nvd.nist.gov/vuln/detail/cve-2018-11567);
  [NVD CVE-2022-25809](https://nvd.nist.gov/vuln/detail/CVE-2022-25809);
  [ZDI-20-537](https://www.zerodayinitiative.com/advisories/ZDI-20-537/)
- Adjacent but not applicable: MediaTek's "vow" (voice-on-wake) driver has multiple
  published LPE CVEs (e.g. CVE-2022-26458, CVE-2023-20772/20773), but their
  affected-CPE lists cover phone/tablet SoCs (MT68xx/MT87xx), not AZ-series or Echo
  devices — [NVD CVE-2022-26458](https://nvd.nist.gov/vuln/detail/CVE-2022-26458)
- No DEF CON, Black Hat, REcon, or comparable talk on Vega OS or AZ silicon was
  found in any search; the only conference-adjacent Vega content is Amazon's own
  developer-relations appearances (droidcon, React Conf, Vega Developer Days) —
  [Amazon Developer Community roundup (Jul 14, 2026)](https://community.amazondeveloper.com/t/july-14-2026-vega-os-getting-started-ring-security-tower/28753)
- Generic Echo local network surface (all generations, not 2025-specific): TCP 443
  to cloud, 8443 device-to-device, UDP 5353 mDNS — [Port Lookup overview (Mar
  2026)](https://portlookup.com/the-ports-your-smart-home-devices-are-opening-and-what-could-go-wrong/)
  (secondary source; the 2023-era Gen 3 Show 8 was also reported to have "a few
  TCP ports open and developer options" — [XDA pogo-pin thread (Aug
  2024)](https://xdaforums.com/t/echo-show-8-3rd-gen-unlock-adb-via-pogo-pin.4685681/))

### Inferences
- Vega OS has had essentially **zero independent security research** despite being
  in consumers' homes since May 2023 and powering Amazon's flagship hardware since
  Nov 2025. This is a genuine absence, verified from CVE databases, conference
  archives, security-blog searches, and academic sources. Plausible causes: no
  AOSP source to diff against, no sideloading (so no researcher foothold), no
  public firmware images documented, and the classic smart-display research
  targets (Fire OS Echo/Fire TV) still being cheaper and more popular.
- The `vda` = adb discovery is the most concrete published crack in Vega's armor:
  the entire mature adb attack surface (and its history of adb daemon
  vulnerabilities) conceptually applies to Vega devices — but it is gated behind
  developer-mode enablement + USB + (on Fire TV at least) an Amazon developer
  account, and AFTVnews' experience (sideloaded apps stop launching) suggests
  Amazon monitors or breaks unauthorized installs. Nothing published says whether
  the 2025 Echo Shows expose a `vda`/adb listener at all.
- Because Vega is a small, purpose-built Linux OS with signed packages and no
  third-party code execution on device, the realistic near-term vulnerability
  classes are: the KeplerScript/React Native app runtime, the VPKG/install
  pipeline, the vda bridge, network-facing services, and the OTA pipeline — none
  of which have public research.

### Gaps
- No port scan or network-surface analysis of the 2025 Show 8/11 has been
  published; whether they listen on anything beyond cloud (443) and mDNS is
  unknown.
- Whether Echo-family Vega devices (as opposed to Fire TV Vega sticks) expose the
  `vda`/adb bridge or hidden developer options is undocumented.
- Two aggregator articles on Vega's "security implications" (news.csraid.com, Oct
  2025 / Apr 2026) exist but appear to be largely automated commentary with no
  original research; they were deliberately not used as sources for facts here.
- Whether Vega OS encrypts its filesystem (the Fire OS Show 15 filesystem was
  unencrypted per DFRWS 2024) is unknown and is decisive for any future forensic
  route — [arXiv:2408.15768](https://arxiv.org/html/2408.15768) (the paper raises
  but cannot answer this question for Vega).

---

## 3. Does the 2025 hardware keep Gen 3's weaknesses (UART pads + off-chip eMMC + unverified /odm)? Does AZ3 Pro share Amlogic DNA with Gen 3's Pop1?

### Takeaway
Unknown for the physical layout; **no** for the silicon lineage. The Gen 3 (rhodes)
weakness was a stack of generic physical-access conditions — off-chip eMMC (Samsung
NAND), exposed UART pads, a metal shield that can be cut, and a Fire OS/Android
system where the `/odm` partition was not covered by dm-verity verification. The
2025 Show 8 still uses **eMMC storage (8 GB)**, so a chip-off/off-chip-write attack
is physically plausible in principle — but nobody has published a teardown,
pinout, or attempt, and the parts of the Gen 3 technique that made it *useful*
(enabling ADB over serial, injecting ADB keys into `/data/misc/adb`, booting a
Debian userspace) are Android/Fire OS artifacts that do not exist on Vega OS. On
silicon: the AZ-series has **MediaTek co-design DNA (AZ1 was built with MediaTek
around the MT8512), not Amlogic DNA** — the Amlogic Pop1 in Gen 3 was the *separate
application processor* beside the AZ2 NPU, and for 2025 Amazon's AZ3/AZ3 Pro
appears to be the core SoC itself (community DB: "Azma15"), so Gen 3's
Amlogic-specific assumptions do not transfer.

### Cited Findings
- Gen 3 (rhodes) hardware: TechInsights teardown — "The application processor comes
  from Amlogic… the P0P1-R octa-core application processor from Amlogic," 2 GB SK
  Hynix LPDDR4, **8 GB Samsung MLC NAND** (off-chip storage), MediaTek WiFi
  front-end — [TechInsights Gen 3 Show 8 teardown](https://www.techinsights.com/blog/deep-dive-teardown-amazon-echo-show-8-3rd-gen-r855d6-smart-display);
  the community device DB lists Gen 3 Show 8 as "Amlogic POP1-R (AZ2 NPU)" —
  [connect-the-dots wiki](https://github-wiki-see.page/m/gilderchuck/connect-the-dots/wiki)
- AZ-series silicon lineage is MediaTek, not Amlogic: the **AZ1 Neural Edge was
  "built alongside MediaTek"** (announced Sept 2020), and MediaTek's **MT8512**
  "forms the basis for the processor," fabricated at 12 nm — [The Verge (Sept 24,
  2020)](https://www.theverge.com/2020/9/24/21453205/amazon-az1-neural-edge-processor-echo-show-10-alexa-voice-on-device);
  [Biometric Update (Oct 2020)](https://www.biometricupdate.com/202010/amazon-aims-to-improve-biometric-features-and-privacy-with-new-edge-ai-chip-in-echo-devices);
  the original Echo Studio (2019) similarly used a MediaTek MT8516 AP —
  [TechInsights Echo Studio teardown](https://www.techinsights.com/products/ddt-1911-808)
- The AZ2 in Show 15 / Show 8 Gen 3 was a co-processor next to a separate Amlogic
  application processor ("Amlogic octa-core Cortex multimedia SoC + Amazon AZ2") —
  [TechInsights Show 15 teardown via Electronics360](https://electronics360.globalspec.com/article/18358/techinsights-teardown-amazon-echo-show-15)
  (cited in prior notes); for 2025, Amazon describes AZ3/AZ3 Pro as "at the core of
  our new Echo devices… two custom-designed silicon chips," with the AZ3-only Echo
  Dot Max needing no other announced processor — [About Amazon (Sept 30,
  2025)](https://www.aboutamazon.com/news/devices/amazon-new-echo-devices-alexa-plus)
- 2025 Show 8 (madeleine, MD6NE4) core chip per the community device DB: "Amazon
  Azma15? (AZ3 Pro NPU)"; Echo Dot Max: "Amazon Azma11? (AZ3 NPU)" — attribution
  marked uncertain by the DB itself — [connect-the-dots
  wiki](https://github-wiki-see.page/m/gilderchuck/connect-the-dots/wiki)
- The 2025 Show 8 still uses **8 GB eMMC** storage (Tom's Guide spec) — no move to
  soldered/encrypted eUFS or in-package storage is documented — [Tom's Guide
  review](https://www.tomsguide.com/home/smart-home/echo-show-8-4th-gen-review)
- What the Gen 3 technique actually required (per Hackaday's writeup of Vowed's
  XDA thread): open the case, remove/cut the metal can over the eMMC, solder to
  eMMC pads and UART pads (or build a pogo-pin adapter), attach an eMMC programmer,
  use the serial console to enable ADB, and alter system files directly; Debian was
  installed by wholesale eMMC content replacement — [Hackaday (Jul 8,
  2026)](https://hackaday.com/2026/07/08/hacking-amazon-echo-show-8-3rd-gen-via-uart-and-emmc/);
  [Dev Gadgets summary](https://devgadgets.io/breaking-the-chains-hardware-hack-unlocks-amazon-echo-show-8-3rd-gen-for-custom-os-and-enhanced-control/)
- Prior isocast research notes (from the rhodes thread) record the software-side
  specifics that made the Gen 3 method work: Fire OS 7.5.7.2 / Android 9, kernel
  5.4.86, persistent root surviving reboots, SELinux permissive after root,
  Debian/Alpine via `pivot_root`, ADB-key injection into `/data/misc/adb`, dm-verity
  EIO on `/system` writes while the **`odm` partition was not verified** (the
  write-target for persistence) — [XDA rhodes thread (Jun 29,
  2026)](https://xdaforums.com/t/root-system-hardware-amazon-echo-show-8-3rd-gen-rhodes-pop1-via-uart-and-emmc.4793346/)
  (thread content as captured in `research_notes/Echo Show 8 custom
  Linux/adjacent_devices.md`, which cites it directly)
- The Show 15 forensic paper documents the same class of access on the Amlogic+AZ2
  platform: locked bootloader exposed via a button-combo fastboot, UART at 921600
  baud with a U-Boot v2019.01 shell (all commands blocked), and an undocumented
  eMMC pinout on test points enabling a full non-invasive dump of the **unencrypted**
  filesystem — [Crasselt & Pugliese, DFRWS USA 2024 / arXiv:2408.15768](https://arxiv.org/html/2408.15768)
- **No teardown, mainboard photo analysis, UART/eMMC pinout, or chip-off attempt
  for the 2025 Show 8 / Show 11 appears anywhere public**: iFixit's Show 8
  teardowns cover the Gen 3 only — [iFixit Echo Show 8 3rd Gen device
  page](https://www.ifixit.com/Device/Amazon_echo_show_8_3rd_Gen) — and searches
  for a 2025-model teardown (TechInsights, iFixit, Reddit, YouTube summaries)
  found none as of Oct 2026

### Inferences
- Silicon verdict: the AZ3 Pro does **not** share Amlogic DNA with Gen 3's Pop1.
  The AZ line descends from the Amazon–MediaTek AZ1 (MT8512) collaboration, and the
  2025 devices appear to consolidate the AP + NPU into the AZ3/AZ3 Pro itself
  ("Azma11/Azma15"). This matters: Gen 3's route leaned on Amlogic-platform
  conventions (U-Boot serial autoboot prompt, Amlogic boot-ROM behavior, Android
  fastboot/recovery partitions) that have no documented equivalent on an
  Amazon/MediaTek Vega-OS part.
- Physical-layer verdict: the eMMC/UART *technique class* is platform-generic and
  the 2025 Show 8 still has eMMC storage, so a sufficiently motivated hardware
  hacker could presumably dump (and possibly write) its flash the way Vowed did on
  Gen 3 — the showstopper is what happens *after*: on Vega OS there is no ADB to
  enable, no `/data/misc/adb` key file, no Android `odm` partition (VPKG packages
  and a different partition scheme), and per the DFRWS paper's own caveat, firmware
  is signed and it is unknown whether Vega's filesystem is encrypted or whether
  writes to any unverified partition would be honored at boot.
- Therefore the honest classification for the Gen 3 technique on the 2025 Show 8
  is **(c) theoretical transferability only** — the physical access skill transfers,
  the exploitation chain does not, and no one has publicly attempted even step one
  (locating the pads on the new mainboard).
- Amazon's design trajectory (removing the physical camera shutter on the 2025
  Show 8, tighter integration) suggests continued physical-surface reduction, but
  there is no evidence of specific anti-tamper hardening (potting, encrypted
  eMMC, secure-boot-enforced rollback) on the 2025 model either way — nothing has
  been torn down publicly.

### Gaps
- No 2025 Show 8/11 mainboard teardown exists publicly, so: presence/absence of
  UART pads, eMMC test points, pogo connectors, and shield design are all unknown.
- Whether the AZ3 Pro ("Azma15") boot chain verifies every boot partition (i.e.,
  whether a Vega equivalent of the unverified-`odm` trick could exist) is unknown.
- Whether Vega OS encrypts at-rest data is unknown (decisive for forensics and for
  any eMMC-write persistence trick).
- The exact relationship of "Azma11/15" to MediaTek silicon (is AZ3 a derived
  MT85xx-family part like AZ1 was MT8512-based?) is unconfirmed; only AZ1's
  MediaTek basis is documented.

---

## 4. What did the Gen 3 researchers (XDA "Vowed", jcrasselt for Show 15) say about newer devices?

### Takeaway
Neither researcher has published a statement about the 2025 (AZ3 Pro / Vega OS)
models. The only forward-looking statement from that lineage is in the Crasselt &
Pugliese DFRWS 2024 paper, which explicitly flags Vega as an open question — the
Show 15 eMMC pinout "could potentially become handy if Vega is rolled out on older
devices and its file system remains unencrypted," and "it remains to be seen
whether manipulations of the firmware become feasible, despite it being signed."
Vowed's rhodes thread reportedly extended the technique only to the Show 15 2nd Gen
(same Amlogic processor as Gen 3), which is a Fire OS device — not to any AZ3/Vega
hardware.

### Cited Findings
- Crasselt & Pugliese (DFRWS USA 2024) on Vega, verbatim: "it is yet unknown which
  devices will run the new OS with the internal name Vega… the unveiled pinout for
  the eMMC interface on the Echo Show 15 could potentially become handy if Vega is
  rolled out on older devices and its file system remains unencrypted. Moreover,
  as the pinout for the eMMC interface can be used for write access, it remains to
  be seen whether manipulations of the firmware become feasible, despite it being
  signed" — [arXiv:2408.15768](https://arxiv.org/html/2408.15768)
- jcrasselt's public artifacts stop at the Show 15: his GitHub repo
  (jcrasselt/amazon-echo-show-15, 5 commits, 12 stars) documents Micro-USB,
  UART, eMMC, Micro-HDMI access and token decryption for the Show 15 only, with
  no mention of newer devices or any follow-up work — [jcrasselt/amazon-echo-show-15](https://github.com/jcrasselt/amazon-echo-show-15)
- The XDA rhodes thread (Jun 29, 2026, user "Vowed") states the technique extends
  to "a newer Echo 15 with the same processor" (Show 15 2nd Gen sharing the Gen 3
  Amlogic SoC), explicitly noted as untested by the author — captured in the prior
  isocast notes citing [XDA rhodes thread](https://xdaforums.com/t/root-system-hardware-amazon-echo-show-8-3rd-gen-rhodes-pop1-via-uart-and-emmc.4793346/)
- No post by Vowed (or R0rt1z2, bengris32, k4y0z) discussing the 2025 Show 8/11,
  AZ3 silicon, or Vega OS was found in search coverage of the XDA Echo forums
  through Oct 2026; the scene's public statements about non-MT8163 devices remain
  "different SoC / not supported / lost cause for now" — [XDA Show 2nd Gen
  thread](https://xdaforums.com/t/echo-show-2nd-generation-possible-to-unlock-similar-to-echo-show-8-1st-gen.4771627/);
  [Hackaday comment by R0rt1z2](https://hackaday.com/2026/01/02/jailbreaking-the-amazon-echo-show/#comment-8233198)
- Unresolved conflict worth noting for the writer: Wikipedia lists the Echo Show
  15 2nd Gen as using "AZ3 Pro with AI Accelerator," while the rhodes thread
  implies it shares Gen 3's Amlogic application processor — both cannot be fully
  true unless AZ3 Pro is a co-processor there as it was for AZ2; flagged as
  unresolved in the prior notes (adjacent_devices.md) and not clarified by any new
  source in this pass

### Inferences
- The silence of the two hardware-root researchers on the 2025 models is
  consistent with (and mildly corroborates) the "nothing exists" finding: the
  people with the demonstrated skills to attack Echo hardware have publicly
  scoped their work to the Amlogic/Fire OS generation and pointedly not claimed
  anything for AZ3/Vega.
- jcrasselt's 2024 caveat reads, two years later, as still accurate but now
  inverted: Vega is not being "rolled out to older devices" (Amazon confirmed
  existing Fire OS devices keep Fire OS); instead it ships only on new silicon —
  so his eMMC-pinout hope would require redoing the hardware work on a brand-new
  mainboard with no published pinout.
- If anyone is likely to break the 2025 Show 8 eventually, it is this lineage of
  researchers (or their successors) applying chip-off forensics first (read-only
  dump of Vega's filesystem would itself be the first public security research on
  Vega OS), then investigating write paths. As of Oct 2026 that work has not
  started publicly.

### Gaps
- The rhodes thread is 403 to automated fetch; Vowed's exact words on any
  post-Gen-3 device (beyond the Show 15 2nd Gen remark carried in prior notes)
  could not be re-verified first-hand.
- Whether jcrasselt/Pugliese have unpublished follow-up work (paper is v1, Aug
  2024, no updates) is unknown.

---

## 5. Alexa+ / cloud-side: any research on abusing the Alexa+ agent to run arbitrary actions on-device?

### Takeaway
No published research demonstrates abusing Alexa+ to execute arbitrary actions on
an Echo device. Alexa+ (launched with Early Access in 2025, powered partly by
Anthropic's Claude via Amazon Bedrock) is a cloud-first agentic assistant whose
on-device footprint on 2025 hardware is AZ3 Pro inference for wake/voice/vision —
and no Alexa+-specific attack, red-team report, or responsible disclosure has been
made public. The generic agent-security literature (indirect prompt injection)
establishes that tool-using LLM agents of Alexa+'s shape are a real attack class,
and Echo history includes cloud/skill-layer bugs (Check Point 2020, "Alexa versus
Alexa" CVE-2022-25809) — but the on-device effect of any such compromise is
confined to Alexa's sanctioned action space (smart-home commands, purchases,
skills), not privileged code execution or sideloading.

### Cited Findings
- Alexa+ architecture: Claude models "help power Alexa+" via Amazon Bedrock, with
  Anthropic citing its "jailbreaking resistance" safety work — [Anthropic (Feb
  2026-dated post)](https://subvolts.com/anthropic/official/blog/claude-and-alexa-8f6149/);
  Amazon says Early Access users talk to Alexa+ 2x more and complete more tasks
  (reservations, calendars, smart home) — [About Amazon (Sept 30,
  2025)](https://www.aboutamazon.com/news/devices/amazon-new-echo-devices-alexa-plus)
- On-device vs cloud split for 2025 hardware: AZ3 Pro "adds support for
  state-of-the-art language models and vision transformers" and runs sensor fusion
  (Omnisense) locally — [About Amazon](https://www.aboutamazon.com/news/devices/amazon-new-echo-devices-alexa-plus);
  [Futurum analysis](https://futurumgroup.com/insights/amazons-az3-chips-help-advance-voice-first-ai-agentic-ux/)
- Generic agent-injection research establishing the threat class: InjecAgent —
  30 LLM agents benchmarked; ReAct-prompted GPT-4 fell to indirect prompt
  injection 24% of the time (47% with reinforced hacking prompts), some agents
  >80%; attack categories explicitly include "unauthorized… smart home device
  manipulation" and data exfiltration — [InjecAgent (arXiv:2403.02691)](https://www.alphaxiv.org/abs/2403.02691);
  the foundational taxonomy of Indirect Prompt Injection in real-world
  LLM-integrated applications (Bing Chat etc.), demonstrating remote control of
  application functionality — [Greshko et al., ACM CCS 2023](https://dlnext.acm.org/doi/pdf/10.1145/3605764.3623985)
- Third-party risk rating of Alexa-as-tool surfaces: an MCP-gateway vendor rates 4
  of 18 Alexa tools "high risk" with documented attack patterns (destructive
  action autonomy, runaway tool loops, prompt injection via tool results) —
  [PolicyLayer Alexa MCP risk profile](https://policylayer.com/risk/high/serversmx-alexa-mcp)
  (vendor marketing page; indicative, not primary research)
- Echo cloud/skill-layer precedent (all pre-Alexa+, all patched/known): Check
  Point's 2020 CORS/XSS chain allowing silent skill installation and voice-history
  theft (account-layer, fixed June 2020) — [Check Point
  Research](https://research.checkpoint.com/2020/amazons-alexa-hacked/); CVE-2022-25809
  "Alexa versus Alexa": Echo Dot 3rd/4th Gen executed arbitrary voice commands via
  malicious skill (remote) or malicious Bluetooth pairing (proximate) — [NVD
  CVE-2022-25809](https://nvd.nist.gov/vuln/detail/CVE-2022-25809); CVE-2018-11567
  skill-reprompt eavesdropping (disputed by Amazon) — [NVD](https://nvd.nist.gov/vuln/detail/cve-2018-11567)
- No Alexa+-specific security paper, blog, disclosed incident, or exploit was
  found in any search (arXiv, security blogs, news) through Oct 2026 — searches
  surfaced only generic prompt-injection research and vendor commentary

### Inferences
- Alexa+ is the least-researched major consumer AI agent: launched broadly in 2025,
  present on millions of devices, yet with no published attack research. That is a
  striking absence and a likely future research target.
- Even a fully successful Alexa+ prompt-injection compromise yields **action-level**
  control (what Alexa is allowed to do: routines, purchases, calls, smart-home
  commands, and the "AvA"-style self-issued voice channel), not **code-level**
  control. It cannot sideload software on a Vega OS device (no sideload surface
  exists) or escape to the OS. For the objective "privileged code execution or
  arbitrary software on-device," Alexa+ abuse is the wrong lever; it is, however,
  the right lever for privacy and smart-home integrity attacks.
- The Omnisense/AZ3 Pro local-inference stack adds a genuinely novel surface with
  no precedent in the literature: on-device vision transformers processing camera
  input and Wi-Fi-radar sensing. No research exists on manipulating these local
  models (e.g., adversarial inputs to the presence-detection pipeline).

### Gaps
- No public security-architecture description of Alexa+ (which actions require
  confirmation, how smart-home scopes are bounded, whether on-device models can
  trigger device actions offline) was found.
- Whether any Alexa+ findings exist under Amazon VRP nondisclosure is unknowable
  from public sources.
- No research on attacking the AZ3 Pro's on-device models (adversarial examples
  against wake-word/vision/presence pipelines) exists publicly.

---

## 6. Bug bounty results for Echo / Vega OS devices (Amazon VRP)

### Takeaway
Amazon runs an active, public, paid Vulnerability Research Program covering Echo
devices (HackerOne: "amazonvrp-devices"), with rewards up to $30,000 and ~$800K
total paid — so a paid route for 2025-Echo/Vega OS research exists and is being
used — but **no published disclosure, advisory, or researcher writeup names the
2025 Show 8/11, Vega OS, or AZ silicon**, and the only famous Echo payout remains
Pwn2Own 2019's $60,000 for the Echo Show 5.

### Cited Findings
- Amazon VRP – Devices (HackerOne): scope "all Amazon-branded or manufactured
  devices sold by Amazon or an authorized retailer, including all Fire devices and
  tablets, **Echo**, Kindle, Blink, Alexa, and Luna Gaming devices… running the
  latest available software"; reward bands $200–$30,000; program stats: **total
  bounties paid $803,702**, average $500–$2,000, top bounty range $6,000–$25,000,
  **310 reports received in the last 90 days, 108 resolved** (page live as of
  Sept–Oct 2026) — [HackerOne: amazonvrp-devices](https://hackerone.com/amazonvrp-devices);
  program mirror with asset list ("Echo Family Devices — Eligible") —
  [hack-hardware.com program page](https://hack-hardware.com/programs/amazonvrp-devices)
- Amazon states Alexa/Echo devices are protected through the VRP with financial
  compensation and a private program for regular researchers — [Amazon devices
  security page](https://www.amazon.com/b?ie=UTF8&node=23638098011)
- The only documented Echo-device bounty payout in history remains Pwn2Own Tokyo
  2019: $60,000 to Team Fluoroacetate for the Echo Show 5 integer overflow (ZDI
  contest, not Amazon's own program) — [TechCrunch (Nov 9,
  2019)](https://techcrunch.com/2019/11/09/browser-bug-amazon-echo-hack/);
  [ZDI-20-537](https://www.zerodayinitiative.com/advisories/ZDI-20-537/)
- The 2025-era Echo exploits that do exist (amonet unlock wave, Gen 3 hardware
  root) were released as free community tooling on XDA/GitHub with no bounty or
  coordinated-disclosure association — [GitHub: R0rt1z2/amonet](https://github.com/R0rt1z2/amonet);
  [XDA rhodes thread](https://xdaforums.com/t/root-system-hardware-amazon-echo-show-8-3rd-gen-rhodes-pop1-via-uart-and-emmc.4793346/)

### Inferences
- The VRP's "latest available software" scope explicitly includes current Echo
  hardware, i.e., the 2025 Vega OS devices; the volume of reports (310/90 days)
  implies findings are being submitted privately. Nothing has surfaced publicly,
  which fits both a young platform and Amazon's policy of not publishing per-device
  disclosure details.
- Economics favor private VRP submission for any Vega OS software flaw (up to
  $30K, fast payment) over free jailbreak release — one structural reason the
  community scene may stay quiet on 2025 devices even if bugs are found. The
  jailbreak community's historical counter-pattern (bootrom bugs on end-of-life
  hardware are worth little as bounties) does not apply to a brand-new platform.
- For the report writer: "no bug bounty results for Echo/Vega OS" is only provable
  as "no *published* results"; private VRP activity cannot be ruled out and is
  in fact likely given program volume.

### Gaps
- HackerOne does not disclose per-asset or per-reporter bounty data; any Vega OS /
  2025 Show 8 reports inside the program are invisible publicly.
- Whether Amazon's device VRP has ever paid out specifically for an Echo Show
  (any generation) is not itemized anywhere public.

---

## 7. Definitive verdict: does ANY path exist to privileged code execution or sideloading on the 2025 Echo Show 8 / Show 11, and what are the realistic future prospects?

### Takeaway
**(a) Works on the 2025 Show 8 / Show 11: nothing.** No software exploit, no
hardware method, no sideload, no forensic acquisition technique has been
demonstrated or even attempted publicly. **(b) Works on other devices but not the
2025 models:** the amonet MT8163 chain (Show 5 G1/G2, Show 8 G1 — bootloader
unlock, root, TWRP, LineageOS), the Show 15 eMMC/UART + ADB-key-injection route
(forensics/shell, no root), and the Gen 3 rhodes UART+eMMC root with Debian via
`pivot_root` — all rely on Android/Fire OS and MT8163-or-Amlogic silicon absent
from the 2025 models. **(c) Theoretical transferability only:** a ground-up
hardware attack (case-open, eMMC dump/write, UART hunt) on the 2025 Show 8's 8 GB
eMMC is physically plausible since the storage medium is unchanged, but no pinout
exists publicly, the boot chain is presumably signed throughout, there is no
Android userland to hijack, and Vega filesystem encryption status is unknown; the
`vda`/adb bridge (proven on Vega Fire TV) is the one documented software surface
worth probing if the Echo build exposes it. **(d) Realistic future prospects:
moderate for hardware forensics in 1–3 years, low for a durable software
jailbreak.** The structural precedents — Google patched the Nest Hub 2nd Gen's
bootloader flaw by OTA within weeks (any Vega bootloader bug is equally
OTA-patchable, and Amazon demonstrably patches software holes within weeks), while
hardware eMMC access is forever — mean the first real "root" of a 2025 Show, if it
comes, will look like the rhodes Gen 3 method: soldering, not an exploit zip. As
of October 2026, not one researcher has publicly started that work, and the
absence of any Vega OS security research after 3.5 years in the field is itself
the headline finding.

### Cited Findings (supporting the verdict's prospect assessment)
- Precedent that bootloader-stage flaws on smart displays get OTA-patched fast:
  the Nest Hub 2nd Gen U-Boot unlock was patched by Google within weeks of
  disclosure, vs. the mask-ROM amonet flaw which is unpatchable for the hardware's
  life — documented in prior notes
  (`adjacent_devices.md`) citing [Hackaday (Jun 2022)](https://hackaday.com/2022/06/20/breaking-google-nest-hubs-secure-boot/)
  and [GitHub: R0rt1z2/amonet](https://github.com/R0rt1z2/amonet); the same
  one-sided dynamics would govern any Vega OS bug Amazon can reach by OTA
- Amazon's demonstrated patch speed on device software holes: the Pro-me3us
  Fire TV system-user exploit was patched in Fire OS 8.1.5.3 within ~1 month of
  release (Sept→Oct 2025) — [Liliputing (Oct 6,
  2025)](https://liliputing.com/amazon-patches-the-exploit-that-supercharged-fire-tv-and-fire-tablet-hacking)
  (from prior notes)
- Amazon's platform-rotation strategy (the actual reason 2025 devices are clean):
  new devices moved to AZ3 silicon + Vega OS while existing Fire OS devices keep
  Fire OS and their sideloading — [Gadget Hacks/Cord Cutters (Apr
  2026)](https://cord-cutters.gadgethacks.com/news/amazon-fire-tv-vega-os-explained-app-gaps-and-lost-sideloading/);
  [About Amazon (Sept 2025)](https://www.aboutamazon.com/news/devices/amazon-new-echo-devices-alexa-plus)
- Hardware access is the one path vendors cannot revoke: the Gen 3 rhodes root and
  the Show 15 forensic dump both survived *because* they operate below the OS on
  off-chip eMMC — [Hackaday (Jul 8,
  2026)](https://hackaday.com/2026/07/08/hacking-amazon-echo-show-8-3rd-gen-via-uart-and-emmc/);
  [arXiv:2408.15768](https://arxiv.org/html/2408.15768)
- The 2025 Show 8 retains eMMC storage (8 GB), keeping that door physically open —
  [Tom's Guide](https://www.tomsguide.com/home/smart-home/echo-show-8-4th-gen-review)
- The `vda`-is-adb discovery (Vega Fire TV, Aug 2026) proves Amazon kept an
  adb-protocol bridge in its "not Android" OS — the most promising published
  software surface, untested on Echo hardware — [RobusTest](https://robustest.com/blog/the-one-line-fork)
- Amazon VRP pays up to $30,000 for Echo-device findings, giving professional
  researchers incentive to find (and privately disclose) Vega flaws before any
  community jailbreak scene forms — [HackerOne:
  amazonvrp-devices](https://hackerone.com/amazonvrp-devices)

### Inferences
- For the isocast project's purposes: if the goal is custom Linux on an Echo Show
  8, the 2019 Gen 1 (amonet + LineageOS 18.1, permanent BROM unlock) and the 2023
  Gen 3 (UART/eMMC + Debian, invasive but demonstrated) remain the only viable
  hardware. The 2025 model should be treated as a closed box for at least the next
  1–2 years.
- Most probable sequence for the first 2025-Show breakthrough, if it ever happens:
  (1) an iFixit/TechInsights-style teardown publishes the mainboard; (2) a
  forensic researcher (jcrasselt lineage) documents UART/eMMC pads and dumps the
  Vega filesystem — this alone would be the first public Vega OS security
  research; (3) only then can write-path/persistence questions (signed boot,
  unverified partitions, encryption) be answered. No public sign of step (1) as
  of Oct 2026.
- Watch items for the future: a TechInsights/iFixit 2025 Show 8 teardown; any
  "Azma"/AZ3 boot-ROM research in the MediaTek tooling scene (mtkclient/MTK
  auth-bypass community); whether Amazon exposes `vda`/developer options on Echo
  Vega builds; the first Alexa+ prompt-injection paper; and any ZDI advisory
  naming Echo after a future Pwn2Own (smart-speaker categories persist in Pwn2Own
  rules — [ZDI Pwn2Own Ireland 2025 rules](https://www.zerodayinitiative.com/Pwn2OwnIreland2025Rules.html)).

### Gaps
- Everything decisive about the 2025 platform's hackability remains unpublished:
  mainboard layout, boot-chain verification policy, filesystem encryption, network
  listeners, and debug-mode behavior. The honest summary for the report writer is:
  **"no path is known to exist; the enabling research has not been done; the
  physical storage medium (eMMC) leaves a theoretical hardware door open; and no
  one in the public scene has started walking through it."**
