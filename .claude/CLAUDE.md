# cosmic-ext-app-switcher

Resident COSMIC window switcher with live thumbnail previews.

## Build & install

```bash
sudo apt install pkg-config libxkbcommon-dev libgbm-dev
source ~/.cargo/env          # Rust not on PATH by default in this environment
cargo build --release
make install                 # builds + installs to ~/.local/bin/ + enables shortcut
```

## Makefile targets

| Target | Effect |
|---|---|
| `make install` | build, install binary, enable shortcut |
| `make uninstall` | disable shortcut, remove binary |
| `make enable` | register in COSMIC shortcuts (live reload) |
| `make disable` | remove from COSMIC shortcuts, restore default |
| `make status` | show binary + shortcut state |

## Architecture

**Resident daemon**: cosmic-comp launches a short-lived launcher on every Super+Tab keypress.

- First invocation (or autostart `--daemon`) binds the Unix socket and becomes the resident daemon
- Later invocations connect, send `"next"`/`"prev"`/`"quit"`, and exit
- The daemon keeps a calloop Wayland backend, window state, and preview cache alive
- Map one panel-sized layer surface at daemon startup with `Anchor::empty()` (the compositor centers it), `size: Some((Some(w), Some(h)))`, `keyboard_interactivity: None`, `input_zone: Some(Vec::new())` (click-through while hidden), and `exclusive_zone: -1`. Keep it mapped; show sets the view and `set_input_zone(None)`, while hide restores the empty region. `ui::panel_size` computes the panel plus padding and shadow margin. Creating it at startup lets the compositor's fractional-scale handshake finish while it is transparent, removing the initial configure/resize flash. Do not use a full-output surface.
- Super release → `ModifiersChanged(logo=false)` → activate selected window

**Connections**: libcosmic manages its own `wl_display` for rendering. The resident backend owns a persistent connection for enumeration/activation. A separate resident capture worker owns a persistent connection, one capture session and one buffer per window, and refreshes previews on a timer.

## Critical gotchas

- Bind `zcosmic_toplevel_info_v1` at **v2+** and enumerate through `ext_foreign_toplevel_list_v1` + `get_cosmic_toplevel`; cctk's `ToplevelInfoState` was unreliable here, so `wayland.rs` implements the dispatch directly
- Bind outputs before creating the toplevel-info objects, or `output_enter`/scope filtering will be empty
- A persistent layer surface created at 1x1 gets stuck at that requested size: `set_anchor`/`set_size` do not reliably commit, and libcosmic never drains `to_commit`. Create the persistent surface at its final panel size up front instead.
- The persistent surface stays on the output where it was created. `ShowData.output` carries the active output id/name/logical size; recreate the surface when it changes.
- Handle `LayerEvent::Done` from the iced wayland event stream: the compositor can close the layer surface on output changes or session lock. Clear `surface_id` so the next show recreates it instead of drawing into a dead surface.
- `calloop-wayland-source` logs "Error trying to flush the wayland display" and treats the error as fatal, but iced's SCTK thread drops the error. The custom logger in `main.rs` exits the process on that exact message so the wrapper supervisor (or the next shortcut invocation) starts a fresh daemon instead of leaving a half-dead process listening on the socket.
- Keyboard focus uses a separate 1x1 transparent layer surface created per show with `KeyboardInteractivity::Exclusive`; the visible panel surface stays `KeyboardInteractivity::None`. The focus surface is destroyed on hide. `view_window` must special-case its id and return a transparent view, otherwise the whole grid is rendered into the 1x1 surface. This avoids missed modifier-release events when the persistent panel surface toggles keyboard interactivity.
- Session-triggered modifiers are intercepted by cosmic-comp; the switcher learns the release from the iced keyboard subscription. If the focus surface does not have keyboard focus, the release is lost and the switcher appears stuck. The per-show focus surface is the reliable handshake.
- Activation hides the overlay and destroys the focus surface first, then calls `zcosmic_toplevel_manager_v1.activate` after 50 ms. Activating while an exclusive layer surface still owns the keyboard can be ignored. `activation_seq` cancels a pending activation on Cancel or a new show.
- The installed wrapper supervises `--daemon`: clean quit (status 0) and termination signals (status >= 128) end the loop; other exits are restarted after two seconds. Keep that behavior when editing `scripts/wrapper.sh`.
- Backend and capture worker errors send `BackendEvent::Fatal`, which sets the restart flag and exits the UI; `app::run` returns an error so the `--daemon` loop restarts. An ordinary `--quit` does not set the flag.
- `super_held` must initialize to `false` — `true` causes immediate activation on the first modifier event
- cosmic-comp intercepts all Super+key combos before our exclusive surface sees them — Tab keypresses never arrive via keyboard events; IPC socket is the only cycling mechanism
- The required protocols are withheld from sandboxed clients; Flatpak is unsupported. Build and install natively.
- Keep one dmabuf or shm capture buffer per window; with many 4K windows the capture cache can use hundreds of MB.
- The daemon stays alive with `exit_on_close(false)`; Activate/Cancel must hide the layer surface, not call `iced::exit`
- Cache previews by ext-toplevel `identifier`, not list index; window order changes between shows
- MRU order is updated on `Activated` state transitions and immediately on activation; current window is sorted first, then MRU order
- The UI uses a responsive grid of up to 3 rows; more windows slide/paginate. The page counter is in the top-right counter bar. The selected window's icon/title are drawn as an overlay pill under its preview, limited to `preview_w`, with up to three lines. `Layout::card_origin` supplies the selected card's grid coordinates; `Layout` reserves `COUNTER_BAR` above and `LABEL_BAND` below the grid. The label for a last-row card fits in the bottom band; upper-row labels intentionally overlap the next row
- The `responsive` closure must return a transparent frame while hidden or while the surface size is <= 1, before the first configure
- `Layout` uses the output's logical size from `ShowData.output`, not the panel-sized surface size. The surface is only the panel plus `SURFACE_MARGIN`; keeping the layout target independent of it preserves the 6x3 footprint and prevents a small-surface grid flash
- `MIN_CARD_W` is 170 so a 1228.8x768 output still fits six columns; raising it brings back the 4x3-then-6x3 flash
- The strip panel always keeps the full 6x3 footprint (`panel_w`/`panel_h`). For fewer cards, `Layout::compute` searches column counts, maximizes card size, avoids a single orphan in the last row, and sizes the grid inside the same panel
- `--preview-count N` is a page size, not a total cap: `ShowData` carries it to the UI, the layout shows at most N cards, and the existing sliding range paginates through all windows. It is read at daemon startup, so restart the daemon to change it
- Cache preview frames even when the switcher hides mid-capture; capture windows with no cached preview first so the tail of a long window list still builds a cache
- `libcosmic` is built with `default-features = false`; without the `wgpu` feature iced uses the **tiny-skia CPU renderer**. That is fine for a small icon panel but a preview panel takes ~0.5 s per frame to composite. Keep `"wgpu"` in the libcosmic features so iced uses the GPU renderer and falls back to tiny-skia only if wgpu cannot initialize.
- `AppVisual` stores a resolved `icon::Handle` and only the desktop-entry fallback label. Keep the handle: rebuilding `icon::from_name()` in `view` creates a new handle id each frame, so iced's SVG raster cache misses and re-rasterizes every icon on every frame (a major source of the ~1 s draw). Window titles are resolved per window with `window_label`; never cache a title-dependent label by `app_id`.
- Cached preview image handles and the app-icon visual cache keep the first show fast; do not recreate handles or resolve every icon on show
- The capture worker runs continuously: warm the cache at startup, refresh every 10s while hidden, pause the periodic refresh on show, and capture only missing previews while visible. Capturing all windows on show competes with the compositor and makes the first frame and cycling slow
- Keep capture sessions and buffers alive per window so the compositor's damage tracker reports only changed windows; use the capture frame's damage list to skip readback/downscale when empty
- Reuse cached preview image handles across shows; recreating them forces a full texture re-upload on the first frame and delays input processing
- Use a generation counter to cancel stale capture threads and ignore stale preview events
- The backend uses `calloop` + `calloop-wayland-source`; the UI receives `BackendEvent`s through a tokio unbounded channel
- `make install` installs an XDG autostart entry with `--daemon`; `make quit` stops the daemon
- A Nix-built binary needs the `LD_LIBRARY_PATH` wrapper; `make install` overwrites it, so recreate it afterward or build with rustup
- Use `cosmic::iced::Subscription`, `cosmic::iced::futures`, `cosmic::iced::stream` — not `iced_futures` directly
- Socket subscription sender type: `cosmic::iced::futures::channel::mpsc::Sender<Message>`; the listener is pre-bound in `main`, so the subscription uses `SocketSub` as the `run_with` key and converts the std listener to tokio

## Shortcut config

`~/.config/cosmic/com.system76.CosmicSettings.Shortcuts/v1/system_actions`

cosmic-comp watches this file and reloads live on change.

Super+Tab and Alt+Tab both route through the single `WindowSwitcher` system action, so they
can only differ via the sibling `custom` file, which holds key bindings rather than action
overrides. `shortcuts()` in cosmic-settings-daemon builds the map as `defaults.extend(custom)`,
and `Binding`'s `Eq`/`Hash` cover only modifiers + key (not `description`/`keycode`), so
`(modifiers: [Alt], key: "Tab"): Spawn("<binary> --scope current-monitor")` replaces the
default `System(WindowSwitcher)` binding for those keys. COSMIC's shipped bindings are in
`/usr/share/cosmic/com.system76.CosmicSettings.Shortcuts/v1/defaults`. This is what the
`--scope` flag is for; the README documents the recipe.

## Key dependencies

- `libcosmic` (git, pop-os/libcosmic, rev `417923f`) — features: `wayland`, `tokio`, `winit`, `multi-window`, `wgpu`; re-exports `cosmic::cctk`
- `parking_lot` — poison-free mutexes shared with the backend and capture worker
- `cosmic::cctk` — `ToplevelInfoState`, `ToplevelManagerState`, `ScreencopyState`, and Wayland helpers
- `image` — downscaling captured previews
- `libc` — DRM device IDs for dmabuf allocation
- `tokio` — async socket listener in subscription
- `freedesktop-desktop-entry` — `.desktop` file parsing for icon names

Keep the revs above in sync with `Cargo.toml`/`Cargo.lock` — check both when bumping.

### Finding the right rev after a COSMIC/cosmic-comp upgrade

`zcosmic_*` protocols are unstable (`z`-prefixed) and their wire format can and does
shift between cosmic-comp releases (e.g. a `_v1` global disappearing in favor of `_v2`,
or a bumped max version like `zcosmic_toplevel_info_v1` going from v1 to v3). A client
built against a mismatched `cosmic-protocols` rev can misinterpret event opcodes —
this tends to surface as a segfault, not a clean error, since a wrong `event_created_child!`
opcode mapping corrupts the object's type at the wayland-client level rather than failing loudly.

There's no published table mapping cosmic-comp releases to compatible
`libcosmic`/`cosmic-protocols` revs. To find one after an upgrade:

1. Check the installed version: `dpkg -l cosmic-comp` (format: `0.1~<unix-ts>~24.04~<short-sha>`
   — the trailing hex is cosmic-comp's own commit).
2. Look up that commit on `github.com/pop-os/cosmic-comp` and open its `Cargo.lock`.
3. Find the `libcosmic` and `cosmic-protocols` entries in that lock file — their pinned
   `rev`s are what cosmic-comp itself was built and tested against for that release.
4. Update this project's `Cargo.toml` to match, run `cargo build`, and re-verify Super+Tab
   end-to-end (a build succeeding is not sufficient — the crash described above happens
   at runtime, not compile time).

Before any of that, `cosmic-ext-app-switcher --check-compat` (also run by `make check-compat`)
tells you whether a required global was removed or advertised below the version we bind.
The list lives in `REQUIRED_GLOBALS` in `src/wayland.rs`, and the switcher checks the same
list at startup. A clean result does not rule out the opcode-mismatch crash above: that needs
the end-to-end Super+Tab test.
