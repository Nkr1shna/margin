# dmgbuild settings for Margin's disk image (run from .github/workflows/apps.yml):
#   uvx dmgbuild==1.6.7 -s deploy/dmg/settings.py -D app=path/to/Margin.app "Margin" out.dmg
# The volume name is the app name ("Margin" or "Margin Dev"); "Margin Dev" gets the
# background that says so. Backgrounds are rendered from background.svg by render.sh.
import inspect
import os.path

app = defines["app"]  # noqa: F821 (dmgbuild provides `defines`)
app_name = os.path.basename(app.rstrip("/"))
# dmgbuild execs this file without __file__; the code object still knows its path.
here = os.path.dirname(os.path.abspath(inspect.currentframe().f_code.co_filename))

dev = app_name.endswith(" Dev.app")
# dmgbuild picks up background@2x.png next to background.png and makes a HiDPI image.
background = os.path.join(here, "background-dev.png" if dev else "background.png")

format = "UDZO"
filesystem = "HFS+"
files = [app]
symlinks = {"Applications": "/Applications"}
hide_extensions = [app_name]
icon_locations = {
    app_name: (180, 180),
    "Applications": (460, 180),
}

# Window: 640x400 content, no toolbar, sidebar, status or path bar.
window_rect = ((200, 160), (640, 400))
default_view = "icon-view"
show_status_bar = False
show_tab_view = False
show_toolbar = False
show_pathbar = False
show_sidebar = False
sidebar_width = 0
include_icon_view_settings = True
include_list_view_settings = False

arrange_by = None
grid_offset = (0, 0)
grid_spacing = 100
scroll_position = (0, 0)
label_pos = "bottom"
text_size = 13
icon_size = 112
