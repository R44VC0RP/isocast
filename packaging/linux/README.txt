Isocast for Linux (preview)
===========================

Isocast records your scenes, and every source as its own file (ISO), with
a desktop app laid out like OBS Studio. This is an early Linux build.

Choose a download
-----------------
- Ubuntu 24.04: use its .deb for browser sources (WebKitGTK helper included).
- Debian 12: use the Debian 12 .deb for a system install; for screen and
  audio capture install PipeWire >= 1.0.5 from bookworm-backports.
- Arch and other glibc distributions, or a per-user install: use this
  Debian 12-built tarball for glibc 2.36 or newer. Match `uname -m`
  (x86_64 or aarch64) and install the runtime libraries listed below.
  Don't install the tarball over a .deb.
- Browser sources in the tarball are optional: the included helper needs
  glibc 2.39 or newer, WebKitGTK 6 and GTK 4's gtk4-broadwayd. Install
  webkitgtk-6.0 and gtk4 on Arch, webkitgtk6.0 and gtk4 on Fedora, or
  libwebkitgtk-6.0-4 and libgtk-4-bin on Ubuntu 24.04. On older glibc or
  without WebKitGTK, only browser sources are unavailable; `isocast doctor`
  reports the reason. The Debian 12 .deb does not include the helper.

Run it
------
    tar xzf isocast-linux-<arch>.tar.gz
    cd isocast-linux-<arch>
    ./install.sh            # install for this user (app menu entry,
                            # ~/.local/bin/isocast and isocast-ui) and
                            # download Cisco's OpenH264 (see below)
    isocast-ui              # the app; it starts the engine (isocastd) beside it

To run it from this folder without installing, get OpenH264 first:
`./install.sh --openh264`, then `./isocast-ui`.

Server without a desktop: run `./install.sh --engine-only` instead. It
installs isocast, isocastd and the media/browser helpers, but no UI, menu
entry or icons. Add ~/.local/bin to PATH. See "Run an engine on a server"
in the project README for runtime packages, pairing and remote access.

H.264 (OpenH264, from Cisco)
----------------------------
Isocast records, streams and previews H.264 with OpenH264, which this
download does not contain. install.sh downloads Cisco's own binary,
libopenh264-2.6.0 for your architecture, from
https://ciscobinary.openh264.org over HTTPS, checks its SHA-256 and puts
it in ~/.local/lib/isocast. It is Cisco's binary, under Cisco's licence,
reproduced in full in CISCO_H264_LICENSE.txt (and THIRD_PARTY_NOTICES).
OpenH264 Video Codec provided by Cisco Systems, Inc.
- You control it: `~/.local/lib/isocast/install.sh --remove-openh264`
  turns it off (removes only Cisco's library) and `--openh264` turns it
  on again (downloads it again); `install.sh --no-openh264` installs
  Isocast without it.
- Without it, Isocast still starts, but H.264 recording, streaming and
  live previews are off; the error and `isocast doctor` say so and name
  the command that fixes it (`~/.local/lib/isocast/install.sh --openh264`).
- `install.sh --uninstall` removes it with Isocast.

The CLI (`./isocast`, `isocast --help`) and `isocast mcp` control the same
engine.

What it needs (install with your package manager)
--------------------------------------------------
- A Wayland session (GNOME, KDE Plasma, sway...), or X11 as a fallback.
- Vulkan is required: your GPU's Vulkan driver (Mesa's radv/anv/nvk, or
  NVIDIA's), or without one Mesa's software Vulkan (lavapipe, in
  mesa-vulkan-drivers), with which the app draws on the CPU. With no Vulkan
  driver at all, neither the app nor the engine starts.
    Debian/Ubuntu: libvulkan1 mesa-vulkan-drivers
    Fedora:        vulkan-loader mesa-vulkan-drivers
    Arch:          vulkan-icd-loader vulkan-radeon / vulkan-intel / nvidia-utils
- Fonts and keyboard: fontconfig, freetype, libxkbcommon and its X11 library
  (libxkbcommon-x11-0 on Debian/Ubuntu; libxkbcommon-x11 on Arch/Fedora).
- Only the isocast-media helper needs GStreamer 1.x (libgstreamer1.0-0,
  gstreamer1.0-plugins-base). Without it the engine still starts and records
  other sources; doctor reports media files unavailable. Media sources play what
  your distribution's GStreamer plugins decode (VA-API hardware decoders
  when present); for H.264/AAC MP4 add the libav plugin, for VP8/VP9 the
  good plugins. These plugins (gstreamer1.0-libav among them) are loaded by
  your system's GStreamer and aren't part of Isocast. `isocast doctor`
  lists the decoders found.
    Debian/Ubuntu: gstreamer1.0-libav gstreamer1.0-plugins-good
    Fedora:        gstreamer1-plugin-libav gstreamer1-plugins-good
    Arch:          gst-libav gst-plugins-good
- PipeWire is optional. Portal screen sharing, microphone and desktop/app
  audio capture, and audio playback use its client library. For screen
  selection also install xdg-desktop-portal with your desktop's backend.
  Synthetic sources, native V4L2 cameras and native X11 capture don't use it.

Where things go
---------------
- Recordings: ~/Videos/Isocast (Settings > Output changes it).
- Setup, scene collections, logs: $XDG_DATA_HOME/isocast
  (~/.local/share/isocast).
- The engine's socket: $XDG_RUNTIME_DIR/isocast/isocastd.sock, or in the
  folder above when XDG_RUNTIME_DIR is unset or not a folder. The app, the
  CLI and the engine must see the same XDG_RUNTIME_DIR to find each other
  (over SSH or sudo it can differ from your desktop session's).
- Stream keys and pairing keys: the desktop keyring (GNOME Keyring, KWallet
  or KeePassXC, through the Secret Service). Isocast never shows the
  keyring's unlock prompt, so unlock it first (logging in to the desktop
  usually does). Without a keyring, or with ISOCAST_SECRET_STORE=file, they
  are files only you can read in $XDG_CONFIG_HOME/isocast/credentials.

Known gaps in this preview
--------------------------
- Recording and streaming use software encoding (OpenH264, H.264 only) and
  AAC; previews are decoded in software. Hardware encoding (VA-API, NVENC)
  comes later.
- Not on Linux yet: the virtual camera. Media files and images may be
  missing too.
- Open and save dialogs use Isocast's own file browser.

Licences: MIT or Apache-2.0 (LICENSE-MIT, LICENSE-APACHE); third-party
notices in THIRD_PARTY_NOTICES. OpenH264 is downloaded from Cisco, under
Cisco's binary licence (see above).
