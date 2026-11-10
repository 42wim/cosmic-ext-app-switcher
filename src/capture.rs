// SPDX-License-Identifier: GPL-3.0-only

//! Window preview capture.
//!
//! cosmic-comp exposes toplevels as `ext_image_capture_source_v1` sources and
//! renders them through `ext_image_copy_capture_v1`.  A resident worker keeps
//! one capture session and one buffer per window, refreshes them periodically,
//! and uses the capture frame damage to skip unchanged windows.  Frames are
//! downscaled, cached by ext-toplevel identifier, and streamed to the UI.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs, io,
    os::{fd::AsFd, unix::fs::MetadataExt},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use calloop::{
    EventLoop, channel,
    timer::{TimeoutAction, Timer},
};
use calloop_wayland_source::WaylandSource;
use cosmic::cctk::{
    GlobalData,
    screencopy::{
        CaptureFrame, CaptureOptions, CaptureSession, CaptureSource, FailureReason, Formats, Frame,
        ScreencopyFrameData, ScreencopyFrameDataExt, ScreencopyHandler, ScreencopySessionData,
        ScreencopySessionDataExt, ScreencopyState,
    },
    sctk::{
        dmabuf::{DmabufFeedback, DmabufHandler, DmabufState},
        registry::{ProvidesRegistryState, RegistryState},
        shm::{Shm, ShmHandler, raw::RawPool},
    },
    wayland_client::{
        Connection, Dispatch, QueueHandle, WEnum, delegate_noop,
        globals::registry_queue_init,
        protocol::{wl_buffer, wl_shm},
    },
    wayland_protocols::{
        ext::foreign_toplevel_list::v1::client::{
            ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
            ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
        },
        wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    },
};
use parking_lot::Mutex;

use crate::events::{BackendEvent, PreviewImage};

/// Longest side of a stored preview, in pixels.  Cards are much smaller; this
/// leaves headroom for HiDPI output without keeping full 4K frames in memory.
const MAX_PREVIEW_DIM: u32 = 512;

/// How often the worker asks each window for a new frame.  The compositor's
/// per-session damage tracker makes unchanged windows cheap.
const REFRESH_INTERVAL: Duration = Duration::from_secs(10);

/// Commands from the backend to the resident capture worker.
#[derive(Debug)]
pub enum CaptureCommand {
    /// The switcher is visible: pause periodic refreshes and capture only the
    /// windows whose previews are missing.
    Show {
        missing: Vec<String>,
    },
    /// The switcher is hidden: resume periodic refreshes.
    Hide,
    /// Capture these windows next.
    Refresh {
        identifiers: Vec<String>,
    },
    Shutdown,
}

struct SessionData {
    session_data: ScreencopySessionData,
    identifier: String,
}

impl ScreencopySessionDataExt for SessionData {
    fn screencopy_session_data(&self) -> &ScreencopySessionData {
        &self.session_data
    }
}

struct FrameData {
    frame_data: ScreencopyFrameData,
    identifier: String,
    frame_id: u64,
}

impl ScreencopyFrameDataExt for FrameData {
    fn screencopy_frame_data(&self) -> &ScreencopyFrameData {
        &self.frame_data
    }
}

enum CaptureStorage {
    Shm {
        pool: RawPool,
    },
    Dmabuf {
        // Drop order matters: the buffer object must go before its device.
        bo: gbm::BufferObject<()>,
        _device: gbm::Device<fs::File>,
        format: gbm::Format,
    },
}

struct CaptureBuffer {
    storage: CaptureStorage,
    _buffer: wl_buffer::WlBuffer,
    width: u32,
    height: u32,
}

#[derive(Default)]
struct HandleData {
    identifier: Mutex<String>,
}

struct CaptureApp {
    registry_state: RegistryState,
    _ext_list: ExtForeignToplevelListV1,
    handles: HashMap<String, ExtForeignToplevelHandleV1>,
    screencopy_state: ScreencopyState,
    dmabuf_state: DmabufState,
    shm_state: Shm,
    qh: QueueHandle<CaptureApp>,
    conn: Connection,

    /// Persistent sessions and buffers, one per window.
    sessions: HashMap<String, CaptureSession>,
    buffers: HashMap<String, CaptureBuffer>,
    /// The single frame currently in flight: identifier, frame serial, frame.
    pending: Option<(String, u64, CaptureFrame)>,
    next_frame_id: u64,
    /// Missing previews requested while the switcher is visible.
    priority: VecDeque<String>,
    /// Periodic refreshes, only drained while the switcher is hidden.
    normal: VecDeque<String>,
    /// Identifiers already in a queue or in flight.
    queued: HashSet<String>,
    last_capture: HashMap<String, Instant>,
    paused: bool,
    quit: bool,

    current_generation: Arc<AtomicU64>,
    cache: Arc<Mutex<HashMap<String, Arc<PreviewImage>>>>,
    event_tx: tokio::sync::mpsc::UnboundedSender<BackendEvent>,
}

impl CaptureApp {
    fn handle_command(&mut self, command: CaptureCommand) {
        match command {
            CaptureCommand::Show { missing } => {
                self.paused = true;
                self.priority.clear();
                self.normal.clear();
                self.queued.clear();
                for identifier in missing {
                    self.enqueue(identifier, true);
                }
            }
            CaptureCommand::Hide => {
                self.paused = false;
                self.priority.clear();
                self.queued.clear();
                self.enqueue_all(false);
            }
            CaptureCommand::Refresh { identifiers } => {
                for identifier in identifiers {
                    self.enqueue(identifier, true);
                }
            }
            CaptureCommand::Shutdown => self.quit = true,
        }
    }

    fn on_tick(&mut self) {
        if self.paused {
            return;
        }
        self.enqueue_all(false);
    }

    fn enqueue_all(&mut self, force: bool) {
        let identifiers: Vec<String> = self.handles.keys().cloned().collect();
        for identifier in identifiers {
            let due = force
                || self
                    .last_capture
                    .get(&identifier)
                    .is_none_or(|last| last.elapsed() >= REFRESH_INTERVAL);
            if due {
                self.enqueue(identifier, false);
            }
        }
    }

    fn enqueue(&mut self, identifier: String, priority: bool) {
        if identifier.is_empty() || self.queued.contains(&identifier) {
            return;
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|(pending, _, _)| pending == &identifier)
        {
            return;
        }
        if !self.ensure_session(&identifier) {
            // A new session is created without a buffer yet; init_done will
            // enqueue it once the buffer exists.
            return;
        }
        self.queued.insert(identifier.clone());
        if priority {
            self.priority.push_back(identifier);
        } else {
            self.normal.push_back(identifier);
        }
        self.start_next();
    }

    /// Returns true when the session and its buffer are both ready.
    fn ensure_session(&mut self, identifier: &str) -> bool {
        if self.sessions.contains_key(identifier) {
            return self.buffers.contains_key(identifier);
        }

        let Some(handle) = self.handles.get(identifier).cloned() else {
            return false;
        };
        let qh = self.qh.clone();
        match self.screencopy_state.capturer().create_session(
            &CaptureSource::Toplevel(handle),
            CaptureOptions::empty(),
            &qh,
            SessionData {
                session_data: ScreencopySessionData::default(),
                identifier: identifier.to_string(),
            },
        ) {
            Ok(session) => {
                log::debug!("preview capture: session created for {identifier}");
                self.sessions.insert(identifier.to_string(), session);
                false
            }
            Err(err) => {
                log::debug!("preview capture unavailable for {identifier}: {err}");
                false
            }
        }
    }

    fn start_next(&mut self) {
        if self.pending.is_some() || self.quit {
            return;
        }

        loop {
            let next = self.priority.pop_front().or_else(|| {
                if self.paused {
                    None
                } else {
                    self.normal.pop_front()
                }
            });
            let Some(identifier) = next else {
                return;
            };
            self.queued.remove(&identifier);

            let Some(session) = self.sessions.get(&identifier).cloned() else {
                continue;
            };
            let Some(buffer) = self.buffers.get(&identifier) else {
                continue;
            };
            self.next_frame_id = self.next_frame_id.wrapping_add(1);
            let frame_id = self.next_frame_id;
            let frame = session.capture(
                &buffer._buffer,
                &[],
                &self.qh,
                FrameData {
                    frame_data: ScreencopyFrameData::default(),
                    identifier: identifier.clone(),
                    frame_id,
                },
            );
            log::trace!("preview capture: frame {frame_id} requested for {identifier}");
            self.pending = Some((identifier, frame_id, frame));
            self.conn.flush().ok();
            return;
        }
    }

    fn drop_window(&mut self, identifier: &str) {
        self.sessions.remove(identifier);
        self.buffers.remove(identifier);
        self.queued.remove(identifier);
        self.priority.retain(|known| known != identifier);
        self.normal.retain(|known| known != identifier);
        self.last_capture.remove(identifier);
        if self
            .pending
            .as_ref()
            .is_some_and(|(pending, _, _)| pending == identifier)
        {
            self.pending = None;
        }
        self.start_next();
    }

    fn finish_pending(&mut self, identifier: &str) {
        if self
            .pending
            .as_ref()
            .is_some_and(|(pending, _, _)| pending == identifier)
        {
            self.pending = None;
        }
        self.start_next();
    }
}

impl Dispatch<ExtForeignToplevelListV1, GlobalData> for CaptureApp {
    fn event(
        _state: &mut Self,
        _proxy: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_foreign_toplevel_list_v1::Event::Toplevel { .. }
            | ext_foreign_toplevel_list_v1::Event::Finished => {}
            _ => {}
        }
    }

    cosmic::cctk::wayland_client::event_created_child!(CaptureApp, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE =>
            (ExtForeignToplevelHandleV1, HandleData::default()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, HandleData> for CaptureApp {
    fn event(
        state: &mut Self,
        handle: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        data: &HandleData,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                *data.identifier.lock() = identifier.clone();
                if !identifier.is_empty() {
                    state.handles.insert(identifier.clone(), handle.clone());
                    state.enqueue(identifier, true);
                }
            }
            ext_foreign_toplevel_handle_v1::Event::Closed => {
                let identifier = data.identifier.lock().clone();
                state.handles.retain(|_, known| known != handle);
                state.drop_window(&identifier);
            }
            _ => {}
        }
    }
}

impl ProvidesRegistryState for CaptureApp {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    cosmic::cctk::sctk::registry_handlers!();
}

impl ShmHandler for CaptureApp {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm_state
    }
}

impl DmabufHandler for CaptureApp {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_feedback(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _proxy: &cosmic::cctk::wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
        _feedback: DmabufFeedback,
    ) {
    }

    fn created(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _params: &ZwpLinuxBufferParamsV1,
        _buffer: wl_buffer::WlBuffer,
    ) {
    }

    fn failed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _params: &ZwpLinuxBufferParamsV1,
    ) {
    }

    fn released(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _buffer: &wl_buffer::WlBuffer,
    ) {
    }
}

impl ScreencopyHandler for CaptureApp {
    fn screencopy_state(&mut self) -> &mut ScreencopyState {
        &mut self.screencopy_state
    }

    fn init_done(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        session: &CaptureSession,
        formats: &Formats,
    ) {
        let Some(session_data) = session.data::<SessionData>() else {
            return;
        };
        let identifier = session_data.identifier.clone();
        if !self.handles.contains_key(&identifier) {
            return;
        }

        // A resize sends init_done again with new constraints.  Replace the
        // buffer and any in-flight frame for this window.
        if self
            .pending
            .as_ref()
            .is_some_and(|(pending, _, _)| pending == &identifier)
        {
            self.pending = None;
        }
        self.buffers.remove(&identifier);
        self.queued.remove(&identifier);
        self.priority.retain(|known| known != &identifier);
        self.normal.retain(|known| known != &identifier);

        match create_capture_buffer(self, qh, formats) {
            Ok(buffer) => {
                self.buffers.insert(identifier.clone(), buffer);
                self.enqueue(identifier, true);
            }
            Err(err) => {
                log::debug!("preview capture: {identifier} cannot be captured: {err:#}");
                self.sessions.remove(&identifier);
                self.start_next();
            }
        }
    }

    fn stopped(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, session: &CaptureSession) {
        let Some(session_data) = session.data::<SessionData>() else {
            return;
        };
        let identifier = session_data.identifier.clone();
        log::debug!("preview capture stopped for {identifier}");
        self.drop_window(&identifier);
    }

    fn ready(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        capture_frame: &CaptureFrame,
        frame: Frame,
    ) {
        let Some(frame_data) = capture_frame.data::<FrameData>() else {
            self.pending = None;
            self.start_next();
            return;
        };
        let identifier = frame_data.identifier.clone();

        let matches = self.pending.as_ref().is_some_and(|(pending, frame_id, _)| {
            pending == &identifier && *frame_id == frame_data.frame_id
        });
        if !matches {
            log::debug!(
                "preview capture: stale frame {} for {identifier} completed",
                frame_data.frame_id
            );
            if self.pending.is_none() {
                self.start_next();
            }
            return;
        }
        self.pending = None;

        let current = self.current_generation.load(Ordering::SeqCst);
        let cached = self.cache.lock().contains_key(&identifier);

        // The compositor's per-session damage tracker reports empty damage for
        // unchanged windows.  Keep the cached preview and skip readback and
        // downscale in that case.
        if (!frame.damage.is_empty() || !cached)
            && let Some(capture) = self.buffers.get_mut(&identifier)
            && let Some(rgba) = read_capture(capture)
            && let Some(preview) = downscale(capture.width, capture.height, rgba)
        {
            let image = Arc::new(preview);
            self.cache.lock().insert(identifier.clone(), image.clone());
            let _ = self.event_tx.send(BackendEvent::Preview {
                generation: current,
                identifier: identifier.clone(),
                image,
            });
        } else {
            log::trace!("preview capture: {identifier} unchanged");
        }

        self.last_capture.insert(identifier.clone(), Instant::now());
        self.finish_pending(&identifier);
    }

    fn failed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        capture_frame: &CaptureFrame,
        reason: WEnum<FailureReason>,
    ) {
        let Some(frame_data) = capture_frame.data::<FrameData>() else {
            self.pending = None;
            self.start_next();
            return;
        };
        let identifier = frame_data.identifier.clone();
        log::debug!("preview capture failed for {identifier}: {reason:?}");
        if self.pending.as_ref().is_some_and(|(pending, frame_id, _)| {
            pending == &identifier && *frame_id == frame_data.frame_id
        }) {
            self.pending = None;
        }
        if matches!(reason, WEnum::Value(FailureReason::Stopped)) {
            self.drop_window(&identifier);
        } else {
            self.start_next();
        }
    }
}

fn create_capture_buffer(
    app: &mut CaptureApp,
    qh: &QueueHandle<CaptureApp>,
    formats: &Formats,
) -> Result<CaptureBuffer> {
    let (width, height) = formats.buffer_size;
    let dmabuf = choose_dmabuf_format(formats);
    log::debug!(
        "preview capture: formats {width}x{height}, shm={:?}, dmabuf_device={:?}, linear_dmabuf={}",
        formats.shm_formats,
        formats.dmabuf_device,
        dmabuf.is_some()
    );
    if width == 0 || height == 0 {
        bail!("invalid buffer size {width}x{height}");
    }

    if let Some((format, modifier)) = dmabuf
        && let Some(dev_id) = formats.dmabuf_device
    {
        match create_dmabuf_capture(app, qh, width, height, dev_id, format, modifier) {
            Ok(buffer) => {
                log::debug!("preview capture: using dmabuf {format:?} {modifier:?}");
                return Ok(buffer);
            }
            Err(err) => {
                log::debug!("preview capture: dmabuf unavailable ({err:#}), falling back to shm");
            }
        }
    }

    if !formats.shm_formats.contains(&wl_shm::Format::Abgr8888) {
        bail!("Abgr8888 shm is not offered");
    }

    let len = width as usize * height as usize * 4;
    let mut pool = RawPool::new(len, &app.shm_state)?;
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        width as i32 * 4,
        wl_shm::Format::Abgr8888,
        (),
        qh,
    );
    Ok(CaptureBuffer {
        storage: CaptureStorage::Shm { pool },
        _buffer: buffer,
        width,
        height,
    })
}

fn choose_dmabuf_format(formats: &Formats) -> Option<(gbm::Format, gbm::Modifier)> {
    for format in [
        gbm::Format::Argb8888,
        gbm::Format::Xrgb8888,
        gbm::Format::Abgr8888,
        gbm::Format::Xbgr8888,
    ] {
        let Some((_, modifiers)) = formats
            .dmabuf_formats
            .iter()
            .find(|(fourcc, _)| *fourcc == format as u32)
        else {
            continue;
        };
        if modifiers
            .iter()
            .any(|modifier| gbm::Modifier::from(*modifier) == gbm::Modifier::Linear)
        {
            return Some((format, gbm::Modifier::Linear));
        }
    }
    None
}

fn create_dmabuf_capture(
    app: &mut CaptureApp,
    qh: &QueueHandle<CaptureApp>,
    width: u32,
    height: u32,
    dev_id: libc::dev_t,
    format: gbm::Format,
    modifier: gbm::Modifier,
) -> Result<CaptureBuffer> {
    let device =
        find_gbm_device(dev_id)?.context("no gbm device for the compositor render node")?;
    let bo = device.create_buffer_object_with_modifiers2::<()>(
        width,
        height,
        format,
        std::iter::once(modifier),
        gbm::BufferObjectFlags::empty(),
    )?;
    let params = app
        .dmabuf_state
        .create_params(qh)
        .context("linux-dmabuf params unavailable")?;
    for plane in 0..bo.plane_count() as i32 {
        let fd = bo.fd_for_plane(plane)?;
        params.add(
            fd.as_fd(),
            plane as u32,
            bo.offset(plane),
            bo.stride_for_plane(plane),
            modifier.into(),
        );
    }
    let (buffer, _) = params.create_immed(
        width as i32,
        height as i32,
        format as u32,
        zwp_linux_buffer_params_v1::Flags::empty(),
        qh,
    );
    Ok(CaptureBuffer {
        storage: CaptureStorage::Dmabuf {
            bo,
            _device: device,
            format,
        },
        _buffer: buffer,
        width,
        height,
    })
}

fn find_gbm_device(dev: libc::dev_t) -> io::Result<Option<gbm::Device<fs::File>>> {
    for entry in fs::read_dir("/dev/dri")? {
        let entry = entry?;
        if entry.metadata()?.rdev() == dev {
            let file = fs::File::options()
                .read(true)
                .write(true)
                .open(entry.path())?;
            return Ok(Some(gbm::Device::new(file)?));
        }
    }
    Ok(None)
}

fn read_capture(capture: &mut CaptureBuffer) -> Option<Vec<u8>> {
    let width = capture.width as usize;
    let height = capture.height as usize;
    match &mut capture.storage {
        CaptureStorage::Shm { pool } => Some(pool.mmap().to_vec()),
        CaptureStorage::Dmabuf { bo, format, .. } => {
            let mut rgba = Vec::with_capacity(width * height * 4);
            let result = bo.map(0, 0, capture.width, capture.height, |mapped| {
                let stride = mapped.stride() as usize;
                let row_len = width * 4;
                for row in mapped.buffer().chunks(stride).take(height) {
                    if row.len() < row_len {
                        break;
                    }
                    rgba.extend_from_slice(&row[..row_len]);
                }
            });
            if let Err(err) = result {
                log::debug!("preview capture: dmabuf map failed: {err}");
                return None;
            }
            if rgba.len() != width * height * 4 {
                log::debug!("preview capture: dmabuf returned a short buffer");
                return None;
            }
            convert_to_rgba(*format, &mut rgba);
            Some(rgba)
        }
    }
}

fn convert_to_rgba(format: gbm::Format, pixels: &mut [u8]) {
    match format {
        gbm::Format::Argb8888 => {
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
        }
        gbm::Format::Xrgb8888 => {
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
                pixel[3] = 255;
            }
        }
        gbm::Format::Abgr8888 => {}
        gbm::Format::Xbgr8888 => {
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel[3] = 255;
            }
        }
        _ => {}
    }
}

fn downscale(width: u32, height: u32, rgba: Vec<u8>) -> Option<PreviewImage> {
    if width == 0 || height == 0 || rgba.len() != width as usize * height as usize * 4 {
        return None;
    }

    if width <= MAX_PREVIEW_DIM && height <= MAX_PREVIEW_DIM {
        return Some(PreviewImage {
            width,
            height,
            rgba,
        });
    }

    let image = image::RgbaImage::from_raw(width, height, rgba)?;
    let ratio = (MAX_PREVIEW_DIM as f32 / width as f32).min(MAX_PREVIEW_DIM as f32 / height as f32);
    let new_width = ((width as f32 * ratio).round() as u32).max(1);
    let new_height = ((height as f32 * ratio).round() as u32).max(1);
    let thumb = image::imageops::resize(
        &image,
        new_width,
        new_height,
        image::imageops::FilterType::Triangle,
    );
    Some(PreviewImage {
        width: thumb.width(),
        height: thumb.height(),
        rgba: thumb.into_raw(),
    })
}

/// Start the resident capture worker.  It keeps its own Wayland connection and
/// one capture session/buffer per window alive, and refreshes them on a timer.
pub fn spawn_worker(
    cache: Arc<Mutex<HashMap<String, Arc<PreviewImage>>>>,
    current_generation: Arc<AtomicU64>,
    event_tx: tokio::sync::mpsc::UnboundedSender<BackendEvent>,
) -> Result<channel::Sender<CaptureCommand>> {
    let (tx, rx) = channel::channel();
    let fatal_tx = event_tx.clone();
    std::thread::spawn(move || {
        if let Err(err) = worker_main(rx, cache, current_generation, event_tx) {
            log::error!("capture worker: {err:#}");
            let _ = fatal_tx.send(BackendEvent::Fatal(format!(
                "capture worker lost its Wayland connection: {err:#}"
            )));
        }
    });
    Ok(tx)
}

fn worker_main(
    cmd_rx: channel::Channel<CaptureCommand>,
    cache: Arc<Mutex<HashMap<String, Arc<PreviewImage>>>>,
    current_generation: Arc<AtomicU64>,
    event_tx: tokio::sync::mpsc::UnboundedSender<BackendEvent>,
) -> Result<()> {
    let conn =
        Connection::connect_to_env().context("capture worker could not connect to Wayland")?;
    let (globals, mut queue) = registry_queue_init::<CaptureApp>(&conn)?;
    let qh = queue.handle();

    let advertised: Vec<String> = globals
        .contents()
        .clone_list()
        .into_iter()
        .map(|global| global.interface)
        .collect();
    let has_global = |name: &str| advertised.iter().any(|interface| interface == name);
    if !has_global("ext_foreign_toplevel_image_capture_source_manager_v1")
        || !has_global("ext_image_copy_capture_manager_v1")
        || !has_global("wl_shm")
    {
        log::info!("compositor does not offer window preview capture; using app icons");
        return Ok(());
    }

    let registry_state = RegistryState::new(&globals);
    let ext_list = registry_state
        .bind_one::<ExtForeignToplevelListV1, _, _>(&qh, 1..=1, GlobalData)
        .context("compositor does not offer ext_foreign_toplevel_list_v1")?;
    let screencopy_state = ScreencopyState::new(&globals, &qh);
    let dmabuf_state = DmabufState::new(&globals, &qh);
    let shm_state = Shm::bind(&globals, &qh).context("compositor does not offer wl_shm")?;

    let mut app = CaptureApp {
        registry_state,
        _ext_list: ext_list,
        handles: HashMap::new(),
        screencopy_state,
        dmabuf_state,
        shm_state,
        qh: qh.clone(),
        conn: conn.clone(),
        sessions: HashMap::new(),
        buffers: HashMap::new(),
        pending: None,
        next_frame_id: 0,
        priority: VecDeque::new(),
        normal: VecDeque::new(),
        queued: HashSet::new(),
        last_capture: HashMap::new(),
        paused: false,
        quit: false,
        current_generation,
        cache,
        event_tx,
    };

    // Let the ext-foreign-toplevel list and each handle's identifier events
    // arrive before the event loop starts.
    for _ in 0..5 {
        queue.roundtrip(&mut app)?;
    }

    let mut event_loop = EventLoop::<CaptureApp>::try_new()?;
    WaylandSource::new(conn, queue)
        .insert(event_loop.handle())
        .map_err(|err| anyhow::anyhow!("could not insert capture Wayland source: {err:?}"))?;
    event_loop
        .handle()
        .insert_source(cmd_rx, |event, _, app| match event {
            channel::Event::Msg(command) => app.handle_command(command),
            channel::Event::Closed => app.quit = true,
        })
        .map_err(|err| anyhow::anyhow!("could not insert capture command channel: {err:?}"))?;

    let timer = Timer::from_duration(REFRESH_INTERVAL);
    event_loop
        .handle()
        .insert_source(timer, |_, _, app| {
            app.on_tick();
            TimeoutAction::ToDuration(REFRESH_INTERVAL)
        })
        .map_err(|err| anyhow::anyhow!("could not insert capture timer: {err:?}"))?;

    // Warm the cache immediately, then keep refreshing on the timer.
    app.on_tick();
    while !app.quit {
        event_loop.dispatch(None, &mut app)?;
    }

    Ok(())
}

cosmic::cctk::sctk::delegate_registry!(CaptureApp);
cosmic::cctk::sctk::delegate_shm!(CaptureApp);
cosmic::cctk::sctk::delegate_dmabuf!(CaptureApp);
cosmic::cctk::delegate_screencopy!(CaptureApp);
delegate_noop!(CaptureApp: ignore wl_buffer::WlBuffer);

#[cfg(test)]
mod tests {
    use super::{MAX_PREVIEW_DIM, choose_dmabuf_format, convert_to_rgba, downscale};
    use cosmic::cctk::screencopy::Formats;

    #[test]
    fn dmabuf_uses_linear_when_offered() {
        let linear: u64 = gbm::Modifier::Linear.into();
        let formats = Formats {
            buffer_size: (10, 10),
            shm_formats: Vec::new(),
            dmabuf_device: Some(0),
            dmabuf_formats: vec![(gbm::Format::Argb8888 as u32, vec![linear])],
        };
        assert_eq!(
            choose_dmabuf_format(&formats),
            Some((gbm::Format::Argb8888, gbm::Modifier::Linear))
        );
    }

    #[test]
    fn dmabuf_rejects_tiled_only_formats() {
        let tiled: u64 = gbm::Modifier::I915_y_tiled.into();
        let formats = Formats {
            buffer_size: (10, 10),
            shm_formats: Vec::new(),
            dmabuf_device: Some(0),
            dmabuf_formats: vec![(gbm::Format::Argb8888 as u32, vec![tiled])],
        };
        assert_eq!(choose_dmabuf_format(&formats), None);
    }

    #[test]
    fn dmabuf_pixels_become_rgba() {
        let mut argb = vec![10, 20, 30, 40];
        convert_to_rgba(gbm::Format::Argb8888, &mut argb);
        assert_eq!(argb, [30, 20, 10, 40]);

        let mut xrgb = vec![10, 20, 30, 0];
        convert_to_rgba(gbm::Format::Xrgb8888, &mut xrgb);
        assert_eq!(xrgb, [30, 20, 10, 255]);

        let mut abgr = vec![10, 20, 30, 40];
        convert_to_rgba(gbm::Format::Abgr8888, &mut abgr);
        assert_eq!(abgr, [10, 20, 30, 40]);
    }

    #[test]
    fn small_previews_are_not_upscaled() {
        let pixels = vec![255; 4 * 4 * 4];
        let preview = downscale(4, 4, pixels).unwrap();
        assert_eq!((preview.width, preview.height), (4, 4));
    }

    #[test]
    fn large_previews_are_downscaled_with_aspect_ratio() {
        let pixels = vec![255; 1600 * 900 * 4];
        let preview = downscale(1600, 900, pixels).unwrap();
        assert!(preview.width <= MAX_PREVIEW_DIM);
        assert!(preview.height <= MAX_PREVIEW_DIM);
        assert_eq!(preview.width * 9, preview.height * 16);
        assert_eq!(
            preview.rgba.len(),
            preview.width as usize * preview.height as usize * 4
        );
    }

    #[test]
    fn invalid_buffers_are_rejected() {
        assert!(downscale(2, 2, vec![0; 3]).is_none());
        assert!(downscale(0, 2, Vec::new()).is_none());
    }
}
