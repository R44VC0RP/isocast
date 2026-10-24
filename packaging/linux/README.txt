Isocast for Linux (preview)
===========================

Isocast records your scenes, and every source as its own file (ISO), with
a desktop app laid out like OBS Studio. This is an early Linux build.

Run it
------
    tar xzf isocast-linux-<arch>.tar.gz
    cd isocast-linux-<arch>
    ./isocast-ui            # the app; it starts the engine (isocastd) beside it
    ./install.sh            # optional: install for this user (app menu entry,
                            # ~/.local/bin/isocast and isocast-ui)

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
- GStreamer 1.x (libgstreamer1.0-0, gstreamer1.0-plugins-base), present on
  any desktop: the engine needs it to start. Media file sources play what
  your distribution's GStreamer plugins decode (VA-API hardware decoders
  when present); for H.264/AAC MP4 add the libav plugin, for VP8/VP9 the
  good plugins. These plugins (gstreamer1.0-libav among them) are loaded by
  your system's GStreamer and aren't part of Isocast. `isocast doctor`
  lists the decoders found.
    Debian/Ubuntu: gstreamer1.0-libav gstreamer1.0-plugins-good
    Fedora:        gstreamer1-plugin-libav gstreamer1-plugins-good
    Arch:          gst-libav gst-plugins-good
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
- Recording uses software encoding (OpenH264, H.264 only) and AAC; previews
  are decoded in software. Hardware encoding (VA-API, NVENC) comes later.
- Not on Linux yet: going live (the engine has no live AAC encoder there
  yet), hearing sources through your speakers (monitoring), the virtual
  camera and browser sources. Media files and images may be missing too.
- Open and save dialogs use Isocast's own file browser.

Licences: MIT or Apache-2.0 (LICENSE-MIT, LICENSE-APACHE); third-party
notices in THIRD_PARTY_NOTICES. OpenH264 is Cisco's BSD-licensed codec.
