# dmgbuild settings — the installer window's look.
#
# dmgbuild writes the .DS_Store programmatically (alias records included),
# which is what a committed Finder-made .DS_Store cannot do: Finder stores
# the background as a volume-specific alias that breaks on every freshly
# created volume. No Finder scripting, so it runs headless on CI.
#
#   dmgbuild -s settings.py -D app=dist/aibo.app aibo out.dmg

import os.path

app = defines.get("app", "dist/aibo.app")  # noqa: F821
files = [app]
symlinks = {"Applications": "/Applications"}

# Relative to the invocation directory (the repo root), because
# dmgbuild exec()s this file without __file__.
background = defines.get("background", "packaging/macos/dmg/background.tiff")  # noqa: F821
default_view = "icon-view"
window_rect = ((200, 160), (660, 440))
icon_size = 104
icon_locations = {
    os.path.basename(app): (165, 205),
    "Applications": (495, 205),
}
show_status_bar = False
show_tab_view = False
show_toolbar = False
show_pathbar = False
show_sidebar = False
format = "UDZO"
