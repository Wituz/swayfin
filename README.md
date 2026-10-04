"My" file browser :))) (100% certified clankermade in Rust for the pretentiousness)

Key features:
- Ultra fast startup (ideal for Sway... I hate waiting)
- Just what you need, no more
- Thumbnail support, image, video and audio preview
- Full mouse support, drag and drop to other apps, copy paste, etc. etc.
- Set any folder temporarily as downloads folder. So anything you download will auto-move into that folder. So handy tbh
- It has zoxide support ! So you never have to navigate folders anymore. Amazing convenience

In my sway setup, I've bound it to open with $mod + e. I recommend you do a similar bind (but probably most of you would think that $mod + e is a bit of an offensive bind, so just bind to whatever fits you :D)
  
It has quite a few dependencies, but nothing that takes more than 10 minutes to install

#### How it looks
<img width="503" height="268" alt="image" src="https://github.com/user-attachments/assets/61baaa29-0226-40e4-b7f6-f65dd5d513f5" />
<img width="616" height="383" alt="image" src="https://github.com/user-attachments/assets/ed20cdec-bf0a-4fbf-a7d8-e8e3b1b8ad73" />

#### You can preview video and audio by hovering the thumbnails of it (incredible)
And WHILE you preview those things, you can scroll to zoom - of course no limit, zoom is infinite in both directions. 
<img width="526" height="365" alt="image" src="https://github.com/user-attachments/assets/1fcb6fb0-d31f-411f-962c-9694ed315a07" />
<img width="525" height="364" alt="image" src="https://github.com/user-attachments/assets/d92f5343-6af9-4a7f-89d8-1df9b3a2c3c9" />

#### Of course it has whatever dialogs you might need
And you can use it as handler for opening/saving files/folders/bits/bytes/whatever in all applications
<img width="528" height="360" alt="image" src="https://github.com/user-attachments/assets/b21c5bc8-ae7f-45ea-a7cb-00c023b1def2" />
<img width="617" height="382" alt="image" src="https://github.com/user-attachments/assets/b7646feb-b0bb-4070-a06d-f68a0756020a" />

Probably a lot of other things, I have forgot about it can also do.

I'll have Claude make a full list of features here:

## Full feature list

### Speed and looks
- First frame in under 10 ms. No GTK, no Qt, no GPU: plain Wayland, drawn on the CPU.
- Nothing runs before the first frame unless the frame needs it. The folder is read while connecting to the compositor; thumbnails, the keyboard layout, mpv and the rest load only when first needed.
- Redraws only when something changes. All file system work happens off the UI thread.
- OLED black, the Tamzen bitmap font built in, 1px dividers. No anti-aliasing, gradients, shadows, rounding or animations.
- Colors are taken from `~/.config/sway/config`.

### Browsing
- Columns: name, size, modified date.
- Sort by name, date or type (extension) with the header buttons. Click the active one to flip its direction. Folders always come first.
- The sort is remembered per folder.
- Dotfiles toggle with `.` (off at every launch). Names listed in a folder's `.hidden` file are always hidden.
- The folder is watched, so changes from other programs show up right away. Selection and scroll stay put.
- Back/forward history on the mouse side buttons, restoring the scroll position and selection of each folder.
- Go to any folder by typing part of its name, via [zoxide](https://github.com/ajeetdsouza/zoxide). Every folder you visit is added to zoxide.

### Thumbnails and previews
- Thumbnails for anything KDE can thumbnail (images, videos, PDFs, fonts, …), shared with Dolphin and other apps through `~/.cache/thumbnails`.
- Hover an image's thumbnail to see the full image over the list. Scroll to zoom. It re-decodes the visible part sharp at every zoom level and shows hard-edged pixels past 100%.
- Hover a video's thumbnail to play it with sound, looping. Scroll to zoom.
- Audio files get a play/pause button instead of a thumbnail. Playback stops when you leave the folder.

### Files
- Open with the default app (freedesktop `mimeapps.list`, the same associations as every other app).
- "Open with" app picker, which can also set the new default.
- New folder, rename, duplicate, permanent delete (asks first).
- Name conflicts on move/copy/paste ask what to do.
- Copy, cut and paste through the system clipboard. It works both ways with Dolphin, Nautilus, other swayfin windows and anything that pastes file paths. Cut files are drawn dimmed until pasted.
- Drag and drop, both ways, with other swayfin windows and other apps. Drop onto a folder row to put it in that folder. Hold Ctrl when starting a drag to copy instead of move.
- Moves across file systems are safe: copy to a temp name, rename into place, then delete the original.
- Open a terminal with `nvim` in the current (or selected) folder with `e`.

### Downloads redirect
- `Shift+D` makes the current folder the downloads target. Everything that lands in your downloads folder is moved there once it finishes downloading. Browser partial files are left alone.
- One target for all windows. The header shows a down arrow and the target folder.
- The mover runs as its own small background process, so it keeps working after you close every window.
- `Shift+D` again in that folder turns it off.

### File dialogs
- Works as the system file picker for every app that uses xdg-desktop-portal (Firefox, Chromium, Electron apps, GTK and Qt apps, Flatpaks, …): open, open multiple, pick folder and save.
- Everything from the normal file manager works inside the dialog too: previews, drag and drop, new folder, etc.
- Saving over an existing file asks first.

## Shortcuts

| Key | Action |
| --- | --- |
| `Enter` / double-click | Open file or enter folder |
| `Alt+Enter` | Open with… |
| `Right` / `Left` | Enter folder / go up |
| `Up` / `Down` | Move selection |
| `Shift+Up` / `Shift+Down` / `Shift+click` | Extend selection |
| `Ctrl+click` | Toggle a row in the selection |
| `Ctrl+A` | Select all |
| `Ctrl+C` / `Ctrl+X` / `Ctrl+V` | Copy / cut / paste |
| `Ctrl+D` | Duplicate |
| `Ctrl+N` | New folder |
| `F2` | Rename |
| `Delete` | Delete (asks first) |
| `z` or `Ctrl+K` | Go to folder (zoxide) |
| `.` | Show/hide dotfiles |
| `p` | Play/pause the selected audio file |
| `e` | Open `nvim` in a terminal in this folder (or the selected one) |
| `Shift+D` | Set/unset this folder as the downloads target |
| Mouse back / forward | History back / forward |
| Scroll while previewing | Zoom |

In file dialogs, `Enter` picks and `Esc` cancels.

## Installation

Wayland only. Made for Sway, but any wlroots compositor should work.

### Dependencies

Arch package names (other distros have the same things under similar names):

| Package | Needed for |
| --- | --- |
| `rust` | Building |
| `cmake`, `qt6-base`, `kio` | Thumbnails (the `swayfin-thumbd` helper). Optional: without them only already-cached thumbnails show. |
| `kdegraphics-thumbnailers`, `ffmpegthumbs`, `kimageformats`, … | More thumbnail types. Optional. |
| `libxkbcommon` | Typing in text fields (loaded when a text field opens) |
| `mpv` | Audio playback and video previews (libmpv) |
| `zoxide` | Go to folder |
| `neovim` and a terminal | The `e` shortcut. Uses `$TERMINAL`, or `kitty` if unset. |
| `xdg-desktop-portal-termfilechooser` (AUR) | Using swayfin as the file dialog in other apps |

### Build

```sh
git clone https://github.com/Wituz/swayfin.git
cd swayfin
cargo build --release
```

The binary is `target/release/swayfin`. Keep the cloned folder where it is: the file dialog wrapper and the thumbnail helper are found relative to it.

### Launch it from Sway

In `~/.config/sway/config`:

```
bindsym $mod+e exec /path/to/swayfin/target/release/swayfin
```

It opens in your home folder. After a `git pull`, `cargo build --release` updates what that key launches.

### Make it the default file manager

So "Show in folder" and similar in other apps open swayfin, create `~/.local/share/applications/swayfin.desktop`:

```ini
[Desktop Entry]
Type=Application
Name=swayfin
Exec=/path/to/swayfin/target/release/swayfin
MimeType=inode/directory;
Terminal=false
```

and run:

```sh
xdg-mime default swayfin.desktop inode/directory
```

## Using it as the file dialog in other apps

Apps ask xdg-desktop-portal for a file dialog. [xdg-desktop-portal-termfilechooser](https://github.com/hunkyburrito/xdg-desktop-portal-termfilechooser) forwards that to a script, and `portal/swayfin-wrapper.sh` starts swayfin in dialog mode.

1. Install `xdg-desktop-portal-termfilechooser`.

2. Create `~/.config/xdg-desktop-portal-termfilechooser/config`:

   ```ini
   [filechooser]
   cmd=/path/to/swayfin/portal/swayfin-wrapper.sh
   default_dir=$HOME
   create_help_file=0
   open_mode=suggested
   save_mode=suggested
   ```

3. Tell xdg-desktop-portal to use it for file dialogs. In `~/.config/xdg-desktop-portal/sway-portals.conf` (or `portals.conf`), under `[preferred]`:

   ```ini
   org.freedesktop.impl.portal.FileChooser=termfilechooser
   ```

4. Float the dialog window. In `~/.config/sway/config`:

   ```
   for_window [app_id="swayfin-chooser"] floating enable, resize set 1100 700, move position center
   ```

5. Restart the portals (or log out and back in):

   ```sh
   systemctl --user restart xdg-desktop-portal xdg-desktop-portal-termfilechooser
   ```

6. Make apps use the portal:
   - **Firefox**: in `about:config`, set `widget.use-xdg-desktop-portal.file-picker` to `1`.
   - **Chromium / Electron apps**: usually automatic on Wayland. If not, start them with `--xdg-portal-required-version=4`, or set `GTK_USE_PORTAL=1`.
   - **GTK apps**: `GTK_USE_PORTAL=1` in your environment (e.g. in the Sway config: `exec systemctl --user set-environment GTK_USE_PORTAL=1`, or in `~/.config/environment.d/`).
   - **Qt/KDE apps**: `QT_QPA_PLATFORMTHEME=xdgdesktopportal`.
   - **Flatpaks**: always use the portal.

You can try the dialog without any app:

```sh
target/release/swayfin --choose open ~ /tmp/picked && cat /tmp/picked
```

Modes are `open`, `multiple`, `directory` and `save`. For `save`, give a file path to prefill the name.
