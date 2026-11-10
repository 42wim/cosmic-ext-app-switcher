# Architecture

The switcher is a resident daemon with a panel-sized libcosmic UI, a persistent
Wayland backend, and a background preview-capture worker.  It is started by
COSMIC's `WindowSwitcher` shortcut; the first invocation becomes the daemon and
later invocations only forward commands over a Unix socket.

## Processes and connections

```text
COSMIC shortcut
  └─ switcher launcher
       ├─ connect to $XDG_RUNTIME_DIR/cosmic-ext-app-switcher.sock
       ├─ send "next" / "prev" / "quit"
       └─ exit

resident daemon
  ├─ UI: libcosmic/wgpu, layer surfaces, keyboard input, socket listener
  ├─ backend (calloop): persistent Wayland connection, window state, activation
  └─ capture worker (calloop): persistent Wayland connection, sessions, buffers
```

The launcher is deliberately short-lived.  The daemon keeps the UI, backend,
and preview cache warm, so a later shortcut press does not wait for window
enumeration or preview capture.

`--daemon` starts the daemon hidden.  The installed wrapper supervises that
mode: after a fatal Wayland/backend/capture error the process exits and the
wrapper starts a fresh daemon after two seconds.  `--quit` and termination
signals stop the supervisor loop.

## Startup

1. The first invocation binds the Unix socket before starting capture.
2. `wayland::spawn_backend` starts the resident backend and waits for its
   initial output/window state.
3. The libcosmic application creates a panel-sized layer surface once.  It is
   transparent and click-through while hidden, which lets the compositor finish
   the fractional-scale handshake before the first show.
4. The capture worker warms the preview cache and keeps refreshing it while the
   daemon is hidden.

The socket path is `$XDG_RUNTIME_DIR/cosmic-ext-app-switcher.sock` (or a
temporary path when `XDG_RUNTIME_DIR` is unset).

## Backend (`src/wayland.rs`)

The backend runs a `calloop` event loop with a persistent Wayland connection.
It binds:

- `ext_foreign_toplevel_list_v1` and `zcosmic_toplevel_info_v1` v2+ for the
  window list, metadata, state, outputs, and activation handles;
- `zcosmic_toplevel_manager_v1` for activation;
- `wl_output` for scope filtering;
- `wl_seat` for activation.

Window metadata is keyed by the ext-toplevel `identifier`, not by list index.
The backend tracks MRU order, sorts the current window first, and updates MRU on
activation and on the compositor's `Activated` state transitions.  This makes a
repeated Alt+Tab toggle between the last two windows.

Activation calls `zcosmic_toplevel_manager_v1.activate(handle, seat)` after the
overlay has been hidden.  The UI waits 50 ms between hiding its keyboard-focus
surface and sending the activation request, because activating while an
exclusive layer surface still owns the keyboard can be ignored.  An activation
generation counter cancels stale requests if the user cancels or opens the
switcher again.

The backend reports the active output's id, name, and logical size with each
show.  The UI recreates the panel surface when that identity changes, so the
switcher follows the active monitor.

## Preview capture (`src/capture.rs`)

The capture worker owns its own persistent Wayland connection and uses
`ext_foreign_toplevel_image_capture_source_manager_v1` plus
`ext_image_copy_capture_v1`.

- One capture session and one buffer are kept alive per window.
- Each frame is captured into a linear dmabuf when the compositor offers one,
  with a shared-memory fallback.
- Captured frames are converted to RGBA and downscaled to a 512 px longest side.
- The compositor's per-session damage tracking is used to skip readback and
  downscale when a frame has no damage.
- Previews are refreshed about every 10 seconds while the switcher is hidden.
- While the switcher is visible, periodic refresh is paused and only windows
  with no cached preview are captured, so capture does not delay the first
  frame or cycling.
- A generation counter ignores previews that arrive for an older show.
- Cached previews are keyed by the ext-toplevel identifier and pruned when a
  window closes.

If the preview protocols are unavailable, or `--no-previews` is used, cards
fall back to the application icon.

## UI and surfaces (`src/app.rs`, `src/ui.rs`)

The panel surface is created once at daemon startup:

- `Layer::Overlay`, no anchor (the compositor centers it), explicit panel size,
  `KeyboardInteractivity::None`, empty input region while hidden, and no
  exclusive zone.
- On show, the input region is restored; on hide, it becomes empty again.
- The surface is recreated only when the active output changes.

Keyboard focus is handled by a separate 1x1 transparent layer surface created
fresh for each show with `KeyboardInteractivity::Exclusive`.  It is destroyed on
hide.  This gives the modifier-release event a reliable keyboard focus owner
without keeping the visible panel surface keyboard-interactive.  The
`view_window` path special-cases this surface and renders a transparent frame
instead of the preview grid.

The panel layout uses the full output's logical size, not the surface size:

- The panel targets a 6-column by 3-row footprint, sized from the output minus
  margins.
- Up to 18 cards are visible at once.
- With fewer windows or `--preview-count N`, the cards use fewer rows/columns
  and grow inside the same 6x3 panel footprint.
- Rows with fewer cards are centered.
- More windows paginate: the visible range slides with the selection and the
  page counter appears in the top-right counter bar.
- The selected window's icon and title are drawn as a pill directly below its
  preview, limited to the preview width.  Long titles wrap to up to three lines
  and then ellipsize.
- The label for a last-row card fits in the reserved band inside the panel;
  upper-row labels intentionally overlap the next row.
- Unselected previews are dimmed, and the selected card gets a brighter border
  and a soft glow.

The UI uses libcosmic's wgpu renderer with the tiny-skia software fallback.
Resolved icon handles, preview handles, and the visual cache are reused across
views and shows; rebuilding icon handles per frame would defeat iced's SVG
raster cache and make the first frame much slower.

## Show/hide flow

1. A launcher sends `next` or `prev` over the socket.
2. If hidden, the UI asks the backend for a fresh `ShowData`; otherwise it
   changes the selected index.
3. The backend sends the window list, cached previews, active output, and MRU
   order.
4. The UI restores the panel input region, creates the keyboard-focus surface,
   and draws the grid.
5. Activate/Cancel hides the overlay, destroys the focus surface, and (for
   Activate) asks the compositor to focus the selected window after 50 ms.

`--preview-count`, `--scope`, and `--no-previews` are read when the daemon
starts; restart it to change them.  Theme changes are loaded on the next show.

## Failure handling

- Backend and capture-worker errors send `BackendEvent::Fatal`; the UI exits
  with an error and the `--daemon` supervisor restarts it.
- `calloop-wayland-source` treats a failed socket flush as fatal.  A small
  logger guard exits the process on that exact error so a broken Wayland
  connection cannot leave a half-dead daemon listening on the socket.
- `LayerEvent::Done` is handled by clearing the surface id, so a compositor
  close (output change, session lock) is recovered on the next show.
- Output identity changes recreate the panel surface.
- The installed wrapper supervises `--daemon` and restarts it after crashes.

## Debugging

Run the daemon in the foreground with debug logging:

```bash
make quit
RUST_LOG=cosmic_ext_app_switcher=debug \
  ~/.local/bin/cosmic-ext-app-switcher --daemon
```

Useful log lines include:

```text
app show start: N windows
backend show done after ...
app show ready after ... surface WxH
view grid: surface WxH, screen WxH after ...
view built: layout ..., strip ..., total ...
modifier release: super_held=... alt_held=...
activate selected ...
activated ...
```

Use:

```bash
cosmic-ext-app-switcher --check-compat
journalctl --user -b | grep -i app-switcher
```

## Known limitations

- Previews are snapshots, not live video.
- Capture uses dmabuf when available and shared memory otherwise; frames are
  still downscaled on the CPU.
- The grid shows up to 3 rows and paginates beyond that.
- The daemon keeps a libcosmic/wgpu process and one capture buffer per window
  resident.  Memory use grows with the number and resolution of open windows.
- Flatpak is unsupported because cosmic-comp withholds the required privileged
  protocols from sandboxed clients.
- Server-side window decorations may be omitted from previews.
