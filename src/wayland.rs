// SPDX-License-Identifier: GPL-3.0-only

//! Resident Wayland backend for window enumeration, activation, and MRU state.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
};

use anyhow::{Context, Result, anyhow};
use calloop::EventLoop;
use calloop_wayland_source::WaylandSource;
use cosmic::cctk::{
    GlobalData,
    cosmic_protocols::{
        toplevel_info::v1::client::{
            zcosmic_toplevel_handle_v1::{self, State as ToplevelState, ZcosmicToplevelHandleV1},
            zcosmic_toplevel_info_v1::{self, ZcosmicToplevelInfoV1},
        },
        toplevel_management::v1::client::zcosmic_toplevel_manager_v1::ZcosmicToplelevelManagementCapabilitiesV1,
    },
    sctk::{
        output::{OutputHandler, OutputState},
        registry::{ProvidesRegistryState, RegistryState},
        seat::{Capability, SeatHandler, SeatState},
    },
    toplevel_management::{ToplevelManagerHandler, ToplevelManagerState},
    wayland_client::{
        Connection, Dispatch, Proxy, QueueHandle, WEnum,
        globals::{GlobalListContents, registry_queue_init},
        protocol::{wl_output, wl_registry, wl_seat},
    },
    wayland_protocols::ext::foreign_toplevel_list::v1::client::{
        ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
        ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
    },
};
use parking_lot::Mutex;

use crate::{
    capture::{self, CaptureCommand},
    events::{BackendCommand, BackendEvent, OutputTarget, PreviewImage, ShowData, ToplevelEntry},
};
use switcher_config::WorkspaceScope;

/// Globals the switcher can't work without.  `zcosmic_toplevel_info_v1` v2 is
/// required because the v2+ API is the only one that lets us correlate a
/// toplevel with the `ext_foreign_toplevel_handle_v1` used for capture.
const REQUIRED_GLOBALS: &[(&str, u32)] = &[
    ("zcosmic_toplevel_info_v1", 2),
    ("zcosmic_toplevel_manager_v1", 1),
    ("ext_foreign_toplevel_list_v1", 1),
    ("zwlr_layer_shell_v1", 1),
    ("wl_seat", 7),
];

/// Globals used only for window previews.  If any are missing, the switcher
/// still runs and falls back to app icons.
const PREVIEW_GLOBALS: &[(&str, u32)] = &[
    ("ext_foreign_toplevel_image_capture_source_manager_v1", 1),
    ("ext_image_copy_capture_manager_v1", 1),
    ("wl_shm", 1),
];

pub struct GlobalStatus {
    interface: &'static str,
    needed: u32,
    advertised: Option<u32>,
}

impl GlobalStatus {
    pub fn ok(&self) -> bool {
        self.advertised.is_some_and(|v| v >= self.needed)
    }
}

impl std::fmt::Display for GlobalStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (interface, needed) = (self.interface, self.needed);
        match self.advertised {
            Some(v) if v >= needed => write!(f, "{interface}: ok (v{v}, needs v{needed})"),
            Some(v) => write!(f, "{interface}: TOO OLD (v{v}, needs v{needed})"),
            None => write!(f, "{interface}: NOT offered (needs v{needed})"),
        }
    }
}

pub struct CompatReport {
    pub required: Vec<GlobalStatus>,
    pub preview: Vec<GlobalStatus>,
}

fn globals_status(
    expected: &[(&'static str, u32)],
    advertised: &[(String, u32)],
) -> Vec<GlobalStatus> {
    expected
        .iter()
        .map(|&(interface, needed)| GlobalStatus {
            interface,
            needed,
            advertised: advertised
                .iter()
                .filter(|(i, _)| i == interface)
                .map(|&(_, v)| v)
                .max(),
        })
        .collect()
}

/// Connects to the compositor and reports on every global the switcher uses,
/// for `--check-compat`.
pub fn probe_required_globals() -> Result<CompatReport> {
    let conn = Connection::connect_to_env()
        .context("could not connect to the Wayland compositor (is COSMIC running?)")?;
    let (globals, _queue) = registry_queue_init::<RegistryProbe>(&conn)?;
    let advertised: Vec<(String, u32)> = globals
        .contents()
        .clone_list()
        .into_iter()
        .map(|g| (g.interface, g.version))
        .collect();
    Ok(CompatReport {
        required: globals_status(REQUIRED_GLOBALS, &advertised),
        preview: globals_status(PREVIEW_GLOBALS, &advertised),
    })
}

/// One window's metadata, shared between its ext-toplevel handle and its
/// cosmic-toplevel handle.
#[derive(Default)]
struct WindowData {
    ext_handle: Option<ExtForeignToplevelHandleV1>,
    cosmic_handle: Option<ZcosmicToplevelHandleV1>,
    identifier: String,
    title: String,
    app_id: String,
    state: HashSet<ToplevelState>,
    outputs: Vec<wl_output::WlOutput>,
    closed: bool,
}

type WindowRef = Arc<Mutex<WindowData>>;

struct AppData {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    _ext_list: ExtForeignToplevelListV1,
    cosmic_info: ZcosmicToplevelInfoV1,
    windows: Vec<WindowRef>,
    /// Most-recently-used window identifiers, front = most recent.
    mru: Vec<String>,
    toplevel_manager_state: Option<ToplevelManagerState>,
    conn: Connection,
    scope: WorkspaceScope,
    event_tx: tokio::sync::mpsc::UnboundedSender<BackendEvent>,
    preview_cache: Arc<Mutex<HashMap<String, Arc<PreviewImage>>>>,
    generation: Arc<AtomicU64>,
    capture_tx: calloop::channel::Sender<CaptureCommand>,
    preview_count: Option<usize>,
    no_previews: bool,
    quit: bool,
}

fn touch_mru_list(mru: &mut Vec<String>, identifier: &str) {
    if identifier.is_empty() {
        return;
    }
    mru.retain(|known| known != identifier);
    mru.insert(0, identifier.to_string());
}

fn sort_windows_by_mru(windows: &mut [WindowRef], mru: &[String]) {
    let mru_rank: HashMap<&str, usize> = mru
        .iter()
        .enumerate()
        .map(|(rank, identifier)| (identifier.as_str(), rank))
        .collect();

    windows.sort_by_key(|window| {
        let window = window.lock();
        let active = window.state.contains(&ToplevelState::Activated);
        let rank = mru_rank
            .get(window.identifier.as_str())
            .copied()
            .unwrap_or(usize::MAX);
        (if active { 0usize } else { 1 }, rank)
    });
}

impl AppData {
    fn handle_command(&mut self, command: BackendCommand) {
        match command {
            BackendCommand::Show { reverse } => {
                let started = std::time::Instant::now();
                self.show(reverse);
                log::debug!("backend show done after {:?}", started.elapsed());
            }
            BackendCommand::Hide => {
                self.generation.fetch_add(1, Ordering::SeqCst);
                let _ = self.capture_tx.send(CaptureCommand::Hide);
            }
            BackendCommand::Cancel => {}
            BackendCommand::Activate { identifier } => self.activate(&identifier),
            BackendCommand::Quit => {
                let _ = self.capture_tx.send(CaptureCommand::Shutdown);
                self.quit = true;
            }
        }
    }

    fn touch_mru(&mut self, identifier: &str) {
        touch_mru_list(&mut self.mru, identifier);
    }

    fn refresh_preview(&self, identifier: &str) {
        if identifier.is_empty() || self.no_previews {
            return;
        }
        let _ = self.capture_tx.send(CaptureCommand::Refresh {
            identifiers: vec![identifier.to_string()],
        });
    }

    fn show(&mut self, reverse: bool) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.windows.retain(|window| !window.lock().closed);

        // Drop MRU entries for windows that no longer exist.
        let live: HashSet<String> = self
            .windows
            .iter()
            .filter_map(|window| {
                let window = window.lock();
                (!window.identifier.is_empty()).then(|| window.identifier.clone())
            })
            .collect();
        self.mru.retain(|identifier| live.contains(identifier));

        let entries = self.build_entries();

        // Drop cached previews for windows that no longer exist.  Keep the
        // cache for windows outside the preview-count limit too, so changing
        // or removing the limit does not force a recapture.
        {
            let identifiers: HashSet<&str> = entries
                .iter()
                .map(|entry| entry.identifier.as_str())
                .collect();
            self.preview_cache
                .lock()
                .retain(|identifier, _| identifiers.contains(identifier.as_str()));
        }

        let previews: Vec<(String, Arc<PreviewImage>)> = {
            let cache = self.preview_cache.lock();
            entries
                .iter()
                .filter_map(|entry| {
                    cache
                        .get(&entry.identifier)
                        .cloned()
                        .map(|image| (entry.identifier.clone(), image))
                })
                .collect()
        };

        let output = self.active_output();
        let cached_ids: HashSet<&str> = previews
            .iter()
            .map(|(identifier, _)| identifier.as_str())
            .collect();
        let missing: Vec<String> = entries
            .iter()
            .filter(|entry| !cached_ids.contains(entry.identifier.as_str()))
            .map(|entry| entry.identifier.clone())
            .collect();
        log::debug!(
            "show: {} windows, {} cached previews, {} missing",
            entries.len(),
            previews.len(),
            missing.len()
        );
        let _ = self.event_tx.send(BackendEvent::Show(ShowData {
            generation,
            entries,
            previews,
            reverse,
            preview_count: self.preview_count,
            output,
        }));

        // The resident capture worker fills any missing previews while the
        // switcher is visible and pauses its periodic refresh until hide.
        let _ = self.capture_tx.send(CaptureCommand::Show { missing });
    }

    fn output_target(&self, output: &wl_output::WlOutput) -> Option<OutputTarget> {
        let info = self.output_state.info(output)?;
        let logical_size = info.logical_size?;
        Some(OutputTarget {
            id: info.id,
            name: info.name.clone(),
            logical_size,
        })
    }

    fn active_output(&self) -> Option<OutputTarget> {
        let active_outputs = self
            .windows
            .iter()
            .find(|window| window.lock().state.contains(&ToplevelState::Activated))
            .map(|window| window.lock().outputs.clone());

        if let Some(outputs) = active_outputs {
            for output in outputs {
                if let Some(target) = self.output_target(&output) {
                    return Some(target);
                }
            }
        }

        self.output_state
            .outputs()
            .find_map(|output| self.output_target(&output))
    }

    fn build_entries(&self) -> Vec<ToplevelEntry> {
        let mut windows: Vec<WindowRef> = self
            .windows
            .iter()
            .filter(|window| !window.lock().closed)
            .cloned()
            .collect();

        let active_outputs = windows
            .iter()
            .find(|window| window.lock().state.contains(&ToplevelState::Activated))
            .map(|window| window.lock().outputs.clone())
            .unwrap_or_default();

        sort_windows_by_mru(&mut windows, &self.mru);

        let mut entries = Vec::new();
        for window in windows {
            let window = window.lock();
            let is_active = window.state.contains(&ToplevelState::Activated);
            let in_scope = match self.scope {
                WorkspaceScope::AllWorkspaces | WorkspaceScope::CurrentWorkspace => true,
                WorkspaceScope::CurrentOutput => {
                    active_outputs.is_empty()
                        || window
                            .outputs
                            .iter()
                            .any(|output| active_outputs.contains(output))
                }
            };

            if !is_active && !in_scope {
                continue;
            }
            if window.identifier.is_empty() || (window.app_id.is_empty() && window.title.is_empty())
            {
                continue;
            }

            entries.push(ToplevelEntry {
                app_id: window.app_id.clone(),
                title: window.title.clone(),
                identifier: window.identifier.clone(),
            });
        }

        entries
    }

    fn activate(&mut self, identifier: &str) {
        let Some(window) = self
            .windows
            .iter()
            .find(|window| window.lock().identifier == identifier)
            .cloned()
        else {
            log::warn!("cannot activate window {identifier}: no longer present");
            return;
        };

        let cosmic_handle = window.lock().cosmic_handle.clone();
        let activated = if let (Some(cosmic_toplevel), Some(manager)) =
            (cosmic_handle.as_ref(), self.toplevel_manager_state.as_ref())
        {
            for seat in self.seat_state.seats() {
                manager.manager.activate(cosmic_toplevel, &seat);
            }
            self.conn.flush().ok();
            true
        } else {
            log::warn!("cannot activate window {identifier}: no cosmic toplevel handle");
            false
        };

        if activated {
            log::debug!("activated {identifier}");
            // Mark the target active immediately so a very fast second
            // Alt+Tab toggles back correctly even before the compositor's
            // state event is dispatched.
            window.lock().state.insert(ToplevelState::Activated);
            self.touch_mru(identifier);
        }
    }
}

impl Dispatch<ExtForeignToplevelListV1, GlobalData> for AppData {
    fn event(
        state: &mut Self,
        _proxy: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _data: &GlobalData,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            let Some(window) = toplevel.data::<WindowRef>().cloned() else {
                return;
            };
            window.lock().ext_handle = Some(toplevel.clone());
            state.windows.push(window.clone());

            let cosmic_handle =
                state
                    .cosmic_info
                    .get_cosmic_toplevel(&toplevel, qh, window.clone());
            window.lock().cosmic_handle = Some(cosmic_handle);
        }
    }

    cosmic::cctk::wayland_client::event_created_child!(AppData, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE =>
            (ExtForeignToplevelHandleV1, WindowRef::default()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, WindowRef> for AppData {
    fn event(
        state: &mut Self,
        _handle: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        data: &WindowRef,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let mut window = data.lock();
        match event {
            ext_foreign_toplevel_handle_v1::Event::Title { title } => {
                window.title = title;
                let identifier = window.identifier.clone();
                drop(window);
                state.refresh_preview(&identifier);
            }
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                window.app_id = app_id;
                let identifier = window.identifier.clone();
                drop(window);
                state.refresh_preview(&identifier);
            }
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                let active = window.state.contains(&ToplevelState::Activated);
                window.identifier = identifier.clone();
                drop(window);
                if active && !identifier.is_empty() {
                    state.touch_mru(&identifier);
                }
            }
            ext_foreign_toplevel_handle_v1::Event::Closed => window.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<ZcosmicToplevelInfoV1, ()> for AppData {
    fn event(
        _state: &mut Self,
        _proxy: &ZcosmicToplevelInfoV1,
        event: zcosmic_toplevel_info_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zcosmic_toplevel_info_v1::Event::Toplevel { .. }
            | zcosmic_toplevel_info_v1::Event::Finished
            | zcosmic_toplevel_info_v1::Event::Done => {}
            _ => {}
        }
    }
}

impl Dispatch<ZcosmicToplevelHandleV1, WindowRef> for AppData {
    fn event(
        app: &mut Self,
        _handle: &ZcosmicToplevelHandleV1,
        event: zcosmic_toplevel_handle_v1::Event,
        data: &WindowRef,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let mut window = data.lock();
        match event {
            zcosmic_toplevel_handle_v1::Event::State { state } => {
                let was_active = window.state.contains(&ToplevelState::Activated);
                window.state.clear();
                for chunk in state.as_chunks::<4>().0 {
                    let value = u32::from_ne_bytes(*chunk);
                    if let Ok(value) = ToplevelState::try_from(value) {
                        window.state.insert(value);
                    }
                }
                let is_active = window.state.contains(&ToplevelState::Activated);
                let identifier = window.identifier.clone();
                drop(window);
                if is_active && !was_active {
                    app.touch_mru(&identifier);
                }
                app.refresh_preview(&identifier);
            }
            zcosmic_toplevel_handle_v1::Event::Geometry { .. } => {
                let identifier = window.identifier.clone();
                drop(window);
                app.refresh_preview(&identifier);
            }
            zcosmic_toplevel_handle_v1::Event::OutputEnter { output } => {
                if !window.outputs.contains(&output) {
                    window.outputs.push(output);
                }
            }
            zcosmic_toplevel_handle_v1::Event::OutputLeave { output } => {
                window.outputs.retain(|known| known != &output);
            }
            zcosmic_toplevel_handle_v1::Event::Closed => window.closed = true,
            _ => {}
        }
    }
}

impl ProvidesRegistryState for AppData {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    cosmic::cctk::sctk::registry_handlers!(OutputState, SeatState);
}

impl OutputHandler for AppData {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl SeatHandler for AppData {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        _: Capability,
    ) {
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        _: Capability,
    ) {
    }
}

impl ToplevelManagerHandler for AppData {
    fn toplevel_manager_state(&mut self) -> &mut ToplevelManagerState {
        self.toplevel_manager_state.as_mut().unwrap()
    }

    fn capabilities(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _capabilities: Vec<WEnum<ZcosmicToplelevelManagementCapabilitiesV1>>,
    ) {
    }
}

cosmic::cctk::sctk::delegate_registry!(AppData);
cosmic::cctk::sctk::delegate_output!(AppData);
cosmic::cctk::sctk::delegate_seat!(AppData);
cosmic::cctk::delegate_toplevel_manager!(AppData);

/// Starts the resident backend.  Returns after the Wayland connection and
/// initial window state are ready.
pub fn spawn_backend(
    scope: WorkspaceScope,
    no_previews: bool,
    preview_count: Option<usize>,
) -> Result<(
    calloop::channel::Sender<BackendCommand>,
    tokio::sync::mpsc::UnboundedReceiver<BackendEvent>,
    Option<OutputTarget>,
)> {
    let (cmd_tx, cmd_rx) = calloop::channel::channel();
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    let (start_tx, start_rx) = mpsc::sync_channel(1);
    let preview_cache = Arc::new(Mutex::new(HashMap::new()));
    let generation = Arc::new(AtomicU64::new(0));
    let capture_tx = if no_previews {
        let (tx, _rx) = calloop::channel::channel();
        tx
    } else {
        capture::spawn_worker(preview_cache.clone(), generation.clone(), event_tx.clone())?
    };

    let thread_start = start_tx.clone();
    let fatal_tx = event_tx.clone();
    let startup = BackendStartup {
        scope,
        no_previews,
        preview_count,
        cmd_rx,
        event_tx,
        preview_cache,
        generation,
        capture_tx,
        start_tx,
    };
    std::thread::spawn(move || {
        if let Err(err) = backend_main(startup) {
            log::error!("backend: {err:#}");
            let _ = fatal_tx.send(BackendEvent::Fatal(format!(
                "backend lost its Wayland connection: {err:#}"
            )));
            let _ = thread_start.send(Err(err));
        }
    });

    let output = start_rx
        .recv()
        .map_err(|_| anyhow!("backend thread exited during startup"))??;
    Ok((cmd_tx, event_rx, output))
}

struct BackendStartup {
    scope: WorkspaceScope,
    no_previews: bool,
    preview_count: Option<usize>,
    cmd_rx: calloop::channel::Channel<BackendCommand>,
    event_tx: tokio::sync::mpsc::UnboundedSender<BackendEvent>,
    preview_cache: Arc<Mutex<HashMap<String, Arc<PreviewImage>>>>,
    generation: Arc<AtomicU64>,
    capture_tx: calloop::channel::Sender<CaptureCommand>,
    start_tx: mpsc::SyncSender<Result<Option<OutputTarget>>>,
}

fn backend_main(startup: BackendStartup) -> Result<()> {
    let BackendStartup {
        scope,
        no_previews,
        preview_count,
        cmd_rx,
        event_tx,
        preview_cache,
        generation,
        capture_tx,
        start_tx,
    } = startup;

    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<AppData>(&conn)?;
    let qh = queue.handle();

    let advertised: Vec<(String, u32)> = globals
        .contents()
        .clone_list()
        .into_iter()
        .map(|g| (g.interface, g.version))
        .collect();
    let missing: Vec<String> = globals_status(REQUIRED_GLOBALS, &advertised)
        .into_iter()
        .filter(|s| !s.ok())
        .map(|s| s.to_string())
        .collect();
    if !missing.is_empty() {
        return Err(anyhow!(
            "cosmic-ext-app-switcher can't run: the compositor doesn't offer the Wayland \
             interfaces it needs ({}). A COSMIC update may have removed or changed them, or \
             the switcher is running inside a sandbox such as Flatpak. Run \
             `cosmic-ext-app-switcher --check-compat` for details.",
            missing.join("; "),
        ));
    }

    // Bind outputs before the toplevel-info objects so cosmic-comp can send the
    // initial output_enter events for each toplevel.
    let registry_state = RegistryState::new(&globals);
    let output_state = OutputState::new(&globals, &qh);
    let seat_state = SeatState::new(&globals, &qh);
    let ext_list = registry_state
        .bind_one::<ExtForeignToplevelListV1, _, _>(&qh, 1..=1, GlobalData)
        .context("compositor does not offer ext_foreign_toplevel_list_v1")?;
    let cosmic_info = registry_state
        .bind_one::<ZcosmicToplevelInfoV1, _, _>(&qh, 2..=3, ())
        .context("compositor does not offer zcosmic_toplevel_info_v1 v2+")?;
    let toplevel_manager_state = ToplevelManagerState::try_new(&registry_state, &qh)
        .context("compositor does not offer zcosmic_toplevel_manager_v1")?;

    let mut data = AppData {
        registry_state,
        output_state,
        seat_state,
        _ext_list: ext_list,
        cosmic_info,
        windows: Vec::new(),
        mru: Vec::new(),
        toplevel_manager_state: Some(toplevel_manager_state),
        conn: conn.clone(),
        scope,
        event_tx,
        preview_cache,
        generation,
        capture_tx,
        preview_count,
        no_previews,
        quit: false,
    };

    for _ in 0..5 {
        queue.roundtrip(&mut data)?;
    }

    let mut event_loop = EventLoop::<AppData>::try_new()?;
    WaylandSource::new(conn, queue)
        .insert(event_loop.handle())
        .map_err(|err| anyhow!("could not insert Wayland event source: {err:?}"))?;
    event_loop
        .handle()
        .insert_source(cmd_rx, |event, _, data| match event {
            calloop::channel::Event::Msg(command) => data.handle_command(command),
            calloop::channel::Event::Closed => data.quit = true,
        })
        .map_err(|err| anyhow!("could not insert backend command channel: {err:?}"))?;

    let initial_output = data.active_output();
    start_tx.send(Ok(initial_output)).ok();

    while !data.quit {
        event_loop.dispatch(None, &mut data)?;
    }

    Ok(())
}

// --- Registry for probe_required_globals: registry_queue_init collects the global list
// itself, so there's nothing to handle here ---

struct RegistryProbe;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for RegistryProbe {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, sync::Arc};

    use parking_lot::Mutex;

    use super::{
        PREVIEW_GLOBALS, REQUIRED_GLOBALS, ToplevelState, WindowData, WindowRef, globals_status,
        sort_windows_by_mru, touch_mru_list,
    };

    fn window(identifier: &str, active: bool) -> WindowRef {
        let mut state = HashSet::new();
        if active {
            state.insert(ToplevelState::Activated);
        }
        Arc::new(Mutex::new(WindowData {
            identifier: identifier.to_string(),
            state,
            ..Default::default()
        }))
    }

    fn identifiers(windows: &[WindowRef]) -> Vec<String> {
        windows
            .iter()
            .map(|window| window.lock().identifier.clone())
            .collect()
    }

    #[test]
    fn touching_mru_moves_a_window_to_the_front() {
        let mut mru = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        touch_mru_list(&mut mru, "b");
        assert_eq!(mru, ["b", "a", "c"]);
        touch_mru_list(&mut mru, "d");
        assert_eq!(mru, ["d", "b", "a", "c"]);
        touch_mru_list(&mut mru, "");
        assert_eq!(mru, ["d", "b", "a", "c"]);
    }

    #[test]
    fn mru_sort_keeps_active_first_then_recent_order() {
        let mut windows = vec![window("a", true), window("b", false), window("c", false)];
        sort_windows_by_mru(
            &mut windows,
            &["c".to_string(), "b".to_string(), "a".to_string()],
        );
        assert_eq!(identifiers(&windows), ["a", "c", "b"]);

        sort_windows_by_mru(
            &mut windows,
            &["b".to_string(), "a".to_string(), "c".to_string()],
        );
        assert_eq!(identifiers(&windows), ["a", "b", "c"]);

        sort_windows_by_mru(&mut windows, &[]);
        assert_eq!(identifiers(&windows), ["a", "b", "c"]);
    }

    #[test]
    fn toggle_order_after_activation() {
        // Active A, previous B: the switcher selects B (index 1).
        let mut windows = vec![window("a", true), window("b", false)];
        sort_windows_by_mru(&mut windows, &["a".to_string(), "b".to_string()]);
        assert_eq!(identifiers(&windows)[1], "b");

        // B becomes active; MRU is now B, A; the next switcher selects A.
        let mut windows = vec![window("a", false), window("b", true)];
        sort_windows_by_mru(&mut windows, &["b".to_string(), "a".to_string()]);
        assert_eq!(identifiers(&windows)[1], "a");
    }

    fn advertised(globals: &[(&str, u32)]) -> Vec<(String, u32)> {
        globals.iter().map(|&(i, v)| (i.to_string(), v)).collect()
    }

    fn line_for(globals: &[(&str, u32)], interface: &str) -> String {
        let statuses = if REQUIRED_GLOBALS.iter().any(|(i, _)| *i == interface) {
            globals_status(REQUIRED_GLOBALS, &advertised(globals))
        } else {
            globals_status(PREVIEW_GLOBALS, &advertised(globals))
        };
        statuses
            .iter()
            .map(ToString::to_string)
            .find(|l| l.starts_with(&format!("{interface}:")))
            .unwrap()
    }

    #[test]
    fn everything_offered_is_ok() {
        let globals = [
            ("zcosmic_toplevel_info_v1", 3),
            ("zcosmic_toplevel_manager_v1", 4),
            ("ext_foreign_toplevel_list_v1", 1),
            ("zwlr_layer_shell_v1", 5),
            ("wl_seat", 9),
            ("ext_foreign_toplevel_image_capture_source_manager_v1", 1),
            ("ext_image_copy_capture_manager_v1", 1),
            ("wl_shm", 1),
        ];
        assert!(
            globals_status(REQUIRED_GLOBALS, &advertised(&globals))
                .iter()
                .all(|s| s.ok())
        );
        assert!(
            globals_status(PREVIEW_GLOBALS, &advertised(&globals))
                .iter()
                .all(|s| s.ok())
        );
    }

    #[test]
    fn missing_global_is_reported() {
        let globals = [
            ("zcosmic_toplevel_info_v1", 3),
            ("ext_foreign_toplevel_list_v1", 1),
            ("zwlr_layer_shell_v1", 5),
            ("wl_seat", 9),
        ];
        let statuses = globals_status(REQUIRED_GLOBALS, &advertised(&globals));
        assert_eq!(statuses.iter().filter(|s| !s.ok()).count(), 1);
        assert_eq!(
            line_for(&globals, "zcosmic_toplevel_manager_v1"),
            "zcosmic_toplevel_manager_v1: NOT offered (needs v1)",
        );
    }

    #[test]
    fn version_below_the_one_we_bind_is_too_old() {
        let globals = [("wl_seat", 5)];
        assert_eq!(
            line_for(&globals, "wl_seat"),
            "wl_seat: TOO OLD (v5, needs v7)"
        );
    }

    #[test]
    fn toplevel_info_v1_is_too_old() {
        let globals = [("zcosmic_toplevel_info_v1", 1)];
        assert_eq!(
            line_for(&globals, "zcosmic_toplevel_info_v1"),
            "zcosmic_toplevel_info_v1: TOO OLD (v1, needs v2)",
        );
    }

    #[test]
    fn preview_globals_are_reported_separately() {
        let globals = [("wl_shm", 1)];
        let statuses = globals_status(PREVIEW_GLOBALS, &advertised(&globals));
        assert_eq!(statuses.iter().filter(|s| s.ok()).count(), 1);
        assert_eq!(statuses.len(), 3);
    }

    /// A seat advertised twice counts as its highest version.
    #[test]
    fn repeated_global_uses_highest_version() {
        let globals = [("wl_seat", 5), ("wl_seat", 8)];
        assert_eq!(line_for(&globals, "wl_seat"), "wl_seat: ok (v8, needs v7)");
    }
}
