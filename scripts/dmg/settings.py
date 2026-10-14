# dmgbuild layout for the installer DMG (shared by release.sh and
# release-finalize.py through make-dmg.sh). `defines` supplies the paths.
# The window is exactly the 640x400 artwork; dmgbuild finds background@2x.png
# next to background.png and combines both into one HiDPI .background.tiff.
app = defines["app"]

format = "UDZO"
filesystem = "HFS+"
files = [(app, "Isocast.app")]
symlinks = {"Applications": "/Applications"}
icon = defines.get("icon") or None
background = defines["background"]

# Finder reads WindowBounds as the whole frame: 400 content + the 32 pt title
# bar of a toolbar-less window (measured on macOS 26.7 and 27.0).
window_rect = ((200, 120), (640, 432))
default_view = "icon-view"
show_status_bar = False
show_tab_view = False
show_toolbar = False
show_pathbar = False
show_sidebar = False
show_icon_preview = False
show_item_info = False
include_icon_view_settings = True
include_list_view_settings = False
arrange_by = None
label_pos = "bottom"
icon_size = 128
text_size = 13
# Item centres; the artwork's arrow runs x 264-372 at y 200.
icon_locations = {"Isocast.app": (180, 200), "Applications": (460, 200)}
