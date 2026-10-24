Isocast for Linux (preview)
===========================

Isocast records your scenes, and every source as its own file (ISO), with
a desktop app laid out like OBS Studio. This is an early Linux build.

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
- Fonts and keyboard: fontconfig, freetype, libxkbcommon (installed on any
  desktop).
- Screen, microphone, desktop audio and camera capture (as it arrives):
  PipeWire, and xdg-desktop-portal with your desktop's backend
  (xdg-desktop-portal-gnome, -kde or -wlr) for choosing a screen.

Built for glibc 2.39 (Ubuntu 24.04) or newer, x86_64 and arm64.

Where things go
---------------
- Recordings: ~/Videos/Isocast (Settings > Output changes it).
- Setup, scene collections, logs: $XDG_DATA_HOME/isocast
  (~/.local/share/isocast).
- The engine's socket: $XDG_RUNTIME_DIR/isocast/isocastd.sock, or in the
  folder above when XDG_RUNTIME_DIR is unset or not a folder. The app, the
  CLI and the engine must see the same XDG_RUNTIME_DIR to find each other
  (over SSH or sudo it can differ from your desktop session's).
- Stream keys and pairing keys: $XDG_CONFIG_HOME/isocast/credentials, files
  only you can read (not an encrypted keyring yet).

Known gaps in this preview
--------------------------
- Recording and streaming use software encoding (OpenH264, H.264 only) and
  AAC; previews are decoded in software. Hardware encoding (VA-API, NVENC)
  comes later.
- Not on Linux yet: the virtual camera and browser sources. Media files and
  images may be missing too.
- Open and save dialogs use Isocast's own file browser.

Licences: MIT or Apache-2.0 (LICENSE-MIT, LICENSE-APACHE); third-party
notices in THIRD_PARTY_NOTICES. OpenH264 is downloaded from Cisco, under
Cisco's binary licence (see above).
