// SPDX-License-Identifier: GPL-3.0-only

//! Resident UI state machine: layer surfaces, keyboard focus, and selection.

use anyhow::Result;
use cosmic::{
    Element,
    app::{Application, Core, Settings, Task},
    iced::platform_specific::runtime::wayland::layer_surface::SctkLayerSurfaceSettings,
    iced::platform_specific::shell::wayland::commands::layer_surface::{
        Anchor, KeyboardInteractivity, Layer, destroy_layer_surface, get_layer_surface,
    },
    iced::{
        self, Event, Length, Limits, Subscription, event,
        keyboard::{self, key::Named},
        window::Id as WindowId,
    },
    widget::{container, image::Handle, space},
};
use parking_lot::Mutex;
use std::{
    collections::{HashMap, HashSet},
    hash::{Hash, Hasher},
    os::unix::net::UnixListener,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::{
    events::{BackendCommand, BackendEvent, OutputTarget, PreviewImage, ShowData, ToplevelEntry},
    icons::{AppVisual, visual_for},
};
use switcher_config::{Theme, ThemeValues};

/// Create the persistent panel surface.  Keyboard focus is handled separately,
/// so this surface never takes keyboard interactivity.
fn panel_surface(id: WindowId, width: u32, height: u32) -> Task<Message> {
    get_layer_surface::<cosmic::Action<Message>>(SctkLayerSurfaceSettings {
        id,
        layer: Layer::Overlay,
        keyboard_interactivity: KeyboardInteractivity::None,
        anchor: Anchor::empty(),
        size: Some((Some(width), Some(height))),
        input_zone: Some(Vec::new()),
        exclusive_zone: -1,
        namespace: "cosmic-ext-app-switcher".into(),
        size_limits: Limits::NONE.min_width(1.0).min_height(1.0),
        ..Default::default()
    })
}

/// Create the tiny transparent surface that owns keyboard focus for one show.
fn keyboard_surface(id: WindowId) -> Task<Message> {
    get_layer_surface::<cosmic::Action<Message>>(SctkLayerSurfaceSettings {
        id,
        layer: Layer::Overlay,
        keyboard_interactivity: KeyboardInteractivity::Exclusive,
        anchor: Anchor::empty(),
        size: Some((Some(1), Some(1))),
        input_zone: Some(Vec::new()),
        exclusive_zone: -1,
        namespace: "cosmic-ext-app-switcher-focus".into(),
        size_limits: Limits::NONE.min_width(1.0).min_height(1.0),
        ..Default::default()
    })
}

/// Set the layer surface's input region.  `None` accepts input over the whole
/// surface; `Some(Vec::new())` makes it click-through.  The public iced command
/// set only exposes size/anchor/keyboard, so build the platform action here.
fn set_input_zone(id: WindowId, zone: Option<Vec<cosmic::iced::Rectangle>>) -> Task<Message> {
    use cosmic::iced::runtime::{
        Action,
        platform_specific::{self, wayland},
        task,
    };
    task::effect(Action::PlatformSpecific(
        platform_specific::Action::Wayland(wayland::Action::LayerSurface(
            wayland::layer_surface::Action::InputZone { id, zone },
        )),
    ))
}

/// A value that is consumed by exactly one subscription stream.  iced builds
/// subscriptions from shared keys, so the state lives behind `Arc<Mutex<_>>`;
/// this wrapper keeps the `Option::take` plumbing in one place.
struct TakeOnce<T>(Arc<Mutex<Option<T>>>);

impl<T> Clone for TakeOnce<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> TakeOnce<T> {
    fn new(value: T) -> Self {
        Self(Arc::new(Mutex::new(Some(value))))
    }

    fn take(&self) -> Option<T> {
        self.0.lock().take()
    }
}

pub struct AppFlags {
    cmd_tx: calloop::channel::Sender<BackendCommand>,
    backend_rx: UnboundedReceiver<BackendEvent>,
    socket_listener: UnixListener,
    theme: Theme,
    initial_output: Option<OutputTarget>,
    restart_required: Arc<AtomicBool>,
}

pub struct AppSwitcher {
    core: Core,
    pub(crate) toplevels: Vec<ToplevelEntry>,
    pub(crate) selected: usize,
    cmd_tx: calloop::channel::Sender<BackendCommand>,
    super_held: bool,
    alt_held: bool,
    surface_id: Option<WindowId>,
    keyboard_id: Option<WindowId>,
    surface_output: Option<OutputTarget>,
    restart_required: Arc<AtomicBool>,
    pub(crate) theme: ThemeValues,
    pub(crate) previews: HashMap<String, Handle>,
    preview_handles: HashMap<String, (Arc<PreviewImage>, Handle)>,
    pub(crate) preview_count: usize,
    pub(crate) screen_size: (f32, f32),
    visual_cache: Mutex<HashMap<String, AppVisual>>,
    pub(crate) show_started: Instant,
    socket_listener: TakeOnce<UnixListener>,
    backend_rx: TakeOnce<UnboundedReceiver<BackendEvent>>,
    generation: u64,
    activation_seq: u64,
    pub(crate) visible: bool,
    pending_show: bool,
}

#[derive(Debug, Clone)]
pub enum Message {
    Backend(BackendEvent),
    SelectNext,
    SelectPrev,
    SelectIndex(usize),
    Activate,
    ActivateSelected(u64, String),
    WindowUnfocused(WindowId),
    Quit,
    /// The compositor closed the layer surface (output removed, session locked).
    SurfaceDone(WindowId),
    KeyEvent(keyboard::Event),
}

impl Application for AppSwitcher {
    type Executor = cosmic::executor::Default;
    type Flags = AppFlags;
    type Message = Message;

    const APP_ID: &'static str = "io.github.cosmic-ext-app-switcher";

    fn core(&self) -> &Core {
        &self.core
    }
    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, flags: Self::Flags) -> (Self, Task<Message>) {
        let AppFlags {
            cmd_tx,
            backend_rx,
            socket_listener,
            theme: theme_preset,
            initial_output,
            restart_required,
        } = flags;
        let theme = theme_preset.values();
        let screen_size = initial_output
            .as_ref()
            .map(OutputTarget::size)
            .unwrap_or((1920.0, 1080.0));
        let (surface_w, surface_h) = crate::ui::panel_size(screen_size.0, screen_size.1);
        let surface_id = WindowId::unique();
        // Map the panel-sized surface once at daemon startup.  The compositor's
        // fractional-scale handshake finishes while it is transparent, so the
        // first show never sees the configure shrink from the pre-scale size.
        // The empty input region keeps the hidden surface click-through.
        let create = panel_surface(surface_id, surface_w, surface_h);

        (
            AppSwitcher {
                core,
                toplevels: Vec::new(),
                selected: 0,
                cmd_tx,
                super_held: false,
                alt_held: false,
                surface_id: Some(surface_id),
                keyboard_id: None,
                surface_output: initial_output,
                restart_required,
                theme,
                previews: HashMap::new(),
                preview_handles: HashMap::new(),
                preview_count: crate::events::MAX_PREVIEWS,
                screen_size,
                visual_cache: Mutex::new(HashMap::new()),
                show_started: Instant::now(),
                socket_listener: TakeOnce::new(socket_listener),
                backend_rx: TakeOnce::new(backend_rx),
                generation: 0,
                activation_seq: 0,
                visible: false,
                pending_show: false,
            },
            create,
        )
    }

    fn update(&mut self, msg: Message) -> Task<Message> {
        match msg {
            Message::Backend(event) => self.backend_event(event),
            Message::SelectNext => self.select(false),
            Message::SelectPrev => self.select(true),
            Message::SelectIndex(index) => self.select_index(index),
            Message::Activate => self.activate(),
            Message::ActivateSelected(seq, identifier) => self.activate_selected(seq, identifier),
            Message::WindowUnfocused(id) => self.window_unfocused(id),
            Message::Quit => self.quit(),
            Message::SurfaceDone(id) => self.surface_done(id),
            Message::KeyEvent(event) => self.key_event(event),
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        let key_sub = event::listen_with(|event, _status, window| match event {
            Event::Keyboard(ke) => Some(Message::KeyEvent(ke)),
            Event::Window(iced::window::Event::Unfocused) => Some(Message::WindowUnfocused(window)),
            Event::PlatformSpecific(cosmic::iced::event::PlatformSpecific::Wayland(
                cosmic::iced::event::wayland::Event::Layer(
                    cosmic::iced::event::wayland::LayerEvent::Focused,
                    _,
                    id,
                ),
            )) => {
                log::debug!("layer focused: {id:?}");
                None
            }
            Event::PlatformSpecific(cosmic::iced::event::PlatformSpecific::Wayland(
                cosmic::iced::event::wayland::Event::Layer(
                    cosmic::iced::event::wayland::LayerEvent::Unfocused,
                    _,
                    id,
                ),
            )) => {
                log::debug!("layer unfocused: {id:?}");
                None
            }
            Event::PlatformSpecific(cosmic::iced::event::PlatformSpecific::Wayland(
                cosmic::iced::event::wayland::Event::Layer(
                    cosmic::iced::event::wayland::LayerEvent::Done,
                    _,
                    id,
                ),
            )) => Some(Message::SurfaceDone(id)),
            _ => None,
        });

        let socket_sub = Subscription::run_with(
            SocketSub {
                listener: self.socket_listener.clone(),
            },
            socket_stream,
        );

        let backend_sub = Subscription::run_with(
            BackendSub {
                rx: self.backend_rx.clone(),
            },
            backend_stream,
        );

        Subscription::batch([key_sub, socket_sub, backend_sub])
    }

    fn view(&self) -> Element<'_, Message> {
        crate::ui::view(self)
    }

    fn view_window(&self, id: WindowId) -> Element<'_, Message> {
        if self.keyboard_id == Some(id) {
            return container(space())
                .width(Length::Fill)
                .height(Length::Fill)
                .into();
        }
        crate::ui::view(self)
    }
}

impl AppSwitcher {
    fn backend_event(&mut self, event: BackendEvent) -> Task<Message> {
        match event {
            BackendEvent::Show(data) => self.show(data),
            BackendEvent::Fatal(err) => {
                log::error!("{err}");
                self.restart_required.store(true, Ordering::Relaxed);
                Task::batch([self.destroy_surface(), iced::exit()])
            }
            BackendEvent::Preview {
                generation,
                identifier,
                image,
            } => {
                if generation == self.generation {
                    let handle = Handle::from_rgba(image.width, image.height, image.rgba.clone());
                    self.previews.insert(identifier.clone(), handle.clone());
                    self.preview_handles.insert(identifier, (image, handle));
                }
                Task::none()
            }
        }
    }

    fn select(&mut self, reverse: bool) -> Task<Message> {
        if !self.visible {
            if !self.pending_show {
                self.pending_show = true;
                let _ = self.cmd_tx.send(BackendCommand::Show { reverse });
            }
        } else if !self.toplevels.is_empty() {
            self.selected = if reverse {
                (self.selected + self.toplevels.len() - 1) % self.toplevels.len()
            } else {
                (self.selected + 1) % self.toplevels.len()
            };
        }
        Task::none()
    }

    fn select_index(&mut self, index: usize) -> Task<Message> {
        if self.visible && index < self.toplevels.len() {
            self.selected = index;
        }
        Task::none()
    }

    fn activate(&mut self) -> Task<Message> {
        if !self.visible || self.toplevels.is_empty() {
            return Task::none();
        }

        let identifier = self.toplevels[self.selected].identifier.clone();
        log::debug!(
            "activate selected {} of {} ({identifier})",
            self.selected + 1,
            self.toplevels.len()
        );
        // Drop the overlay and its keyboard-focus surface first, then ask the
        // compositor to activate the target.  Activating while an exclusive
        // layer surface still owns the keyboard can be ignored.
        self.activation_seq = self.activation_seq.wrapping_add(1);
        let seq = self.activation_seq;
        let hide = self.hide();
        let activate = Task::perform(
            async move {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                (seq, identifier)
            },
            |(seq, identifier)| cosmic::Action::App(Message::ActivateSelected(seq, identifier)),
        );
        Task::batch([hide, activate])
    }

    fn activate_selected(&mut self, seq: u64, identifier: String) -> Task<Message> {
        if seq == self.activation_seq {
            let _ = self.cmd_tx.send(BackendCommand::Activate { identifier });
        }
        Task::none()
    }

    fn window_unfocused(&mut self, id: WindowId) -> Task<Message> {
        // Taking keyboard focus for the small focus surface makes the panel
        // surface emit Unfocused too.  Only cancel when the surface that
        // actually owns keyboard focus is lost.
        if self.keyboard_id == Some(id)
            || (self.keyboard_id.is_none() && self.surface_id == Some(id))
        {
            return self.cancel();
        }
        Task::none()
    }

    fn cancel(&mut self) -> Task<Message> {
        if !self.visible {
            return Task::none();
        }
        self.activation_seq = self.activation_seq.wrapping_add(1);
        let _ = self.cmd_tx.send(BackendCommand::Cancel);
        self.hide()
    }

    fn quit(&mut self) -> Task<Message> {
        let _ = self.cmd_tx.send(BackendCommand::Quit);
        Task::batch([self.destroy_surface(), iced::exit()])
    }

    fn surface_done(&mut self, id: WindowId) -> Task<Message> {
        if self.keyboard_id == Some(id) {
            log::debug!("keyboard focus surface was closed");
            self.keyboard_id = None;
            if self.visible {
                return self.hide();
            }
        }
        if self.surface_id == Some(id) {
            self.surface_id = None;
            if self.visible {
                return self.hide();
            }
        }
        Task::none()
    }

    fn key_event(&mut self, event: keyboard::Event) -> Task<Message> {
        match event {
            keyboard::Event::KeyPressed { key, modifiers, .. } => match key {
                iced::keyboard::Key::Named(Named::Tab) => self.select(modifiers.shift()),
                iced::keyboard::Key::Named(Named::Escape) => self.cancel(),
                iced::keyboard::Key::Named(Named::Enter) => self.activate(),
                _ => Task::none(),
            },
            keyboard::Event::ModifiersChanged(mods) => {
                let activate = (self.super_held && !mods.logo()) || (self.alt_held && !mods.alt());
                self.super_held = mods.logo();
                self.alt_held = mods.alt();
                if activate {
                    log::debug!(
                        "modifier release: super_held={} alt_held={}",
                        self.super_held,
                        self.alt_held
                    );
                    return self.activate();
                }
                Task::none()
            }
            _ => Task::none(),
        }
    }

    fn destroy_surface(&mut self) -> Task<Message> {
        self.visible = false;
        let panel = self
            .surface_id
            .take()
            .map(destroy_layer_surface)
            .unwrap_or_else(Task::none);
        let keyboard = self
            .keyboard_id
            .take()
            .map(destroy_layer_surface)
            .unwrap_or_else(Task::none);
        Task::batch([panel, keyboard])
    }

    pub(crate) fn visual(&self, entry: &ToplevelEntry) -> AppVisual {
        if let Some(visual) = self.visual_cache.lock().get(&entry.app_id).cloned() {
            return visual;
        }
        let visual = visual_for(&entry.app_id, self.theme.icon_size.max(96));
        self.visual_cache
            .lock()
            .insert(entry.app_id.clone(), visual.clone());
        visual
    }

    fn show(&mut self, data: ShowData) -> Task<Message> {
        let output = self.apply_show_data(data);

        if self.toplevels.is_empty() {
            return self.hide();
        }

        self.visible = true;
        self.sync_surfaces(output)
    }

    fn apply_show_data(&mut self, data: ShowData) -> Option<OutputTarget> {
        let ShowData {
            generation,
            entries,
            previews,
            reverse,
            preview_count,
            output,
        } = data;
        self.pending_show = false;
        self.generation = generation;
        self.activation_seq = self.activation_seq.wrapping_add(1);
        self.show_started = Instant::now();
        log::debug!("app show start: {} windows", entries.len());
        self.preview_count = preview_count
            .unwrap_or(crate::events::MAX_PREVIEWS)
            .clamp(1, crate::events::MAX_PREVIEWS);
        self.screen_size = output
            .as_ref()
            .map(OutputTarget::size)
            .unwrap_or((1920.0, 1080.0));
        self.toplevels = entries;
        self.selected = if reverse {
            self.toplevels.len().saturating_sub(1)
        } else {
            1.min(self.toplevels.len().saturating_sub(1))
        };

        // Reuse image handles across shows.  Recreating them forces the
        // renderer to upload every cached preview again, which makes the
        // first frame and the scale transition unnecessarily expensive.
        let live: HashSet<&str> = self
            .toplevels
            .iter()
            .map(|entry| entry.identifier.as_str())
            .collect();
        self.preview_handles
            .retain(|identifier, _| live.contains(identifier.as_str()));
        self.previews.clear();
        for (identifier, image) in previews {
            let handle = match self.preview_handles.get(&identifier) {
                Some((known, handle)) if Arc::ptr_eq(known, &image) => handle.clone(),
                _ => {
                    let handle = Handle::from_rgba(image.width, image.height, image.rgba.clone());
                    self.preview_handles
                        .insert(identifier.clone(), (image, handle.clone()));
                    handle
                }
            };
            self.previews.insert(identifier, handle);
        }

        // Pick up applet theme changes without restarting the daemon.  The
        // app-icon cache is keyed by app_id, so only the visible cards/title
        // are resolved on show, not every window.
        let old_icon_size = self.theme.icon_size;
        self.theme = crate::load_theme().values();
        if self.theme.icon_size != old_icon_size {
            self.visual_cache.lock().clear();
        }

        output
    }

    fn sync_surfaces(&mut self, output: Option<OutputTarget>) -> Task<Message> {
        let (surface_w, surface_h) = crate::ui::panel_size(self.screen_size.0, self.screen_size.1);
        log::debug!(
            "app show ready after {:?}, surface {surface_w}x{surface_h}",
            self.show_started.elapsed()
        );

        // Normally the persistent panel is already correct.  Recreate only
        // when the active output changed, since an un-anchored layer surface
        // stays on the output where it was created.
        let recreate = self.surface_id.is_none() || self.surface_output != output;

        // Keyboard focus gets its own tiny surface, created fresh on every
        // show with Exclusive interactivity at creation.  The persistent panel
        // stays KeyboardInteractivity::None.
        let old_keyboard = self
            .keyboard_id
            .take()
            .map(destroy_layer_surface)
            .unwrap_or_else(Task::none);
        let keyboard_id = WindowId::unique();
        self.keyboard_id = Some(keyboard_id);
        let keyboard = keyboard_surface(keyboard_id);

        let panel = if recreate {
            log::debug!("recreating layer surface for output {output:?}");
            let destroy = self
                .surface_id
                .take()
                .map(destroy_layer_surface)
                .unwrap_or_else(Task::none);
            let surface_id = WindowId::unique();
            self.surface_id = Some(surface_id);
            self.surface_output = output;
            Task::batch([destroy, panel_surface(surface_id, surface_w, surface_h)])
        } else {
            let Some(surface_id) = self.surface_id else {
                return Task::none();
            };
            set_input_zone(surface_id, None)
        };

        Task::batch([old_keyboard, panel, keyboard])
    }

    fn hide(&mut self) -> Task<Message> {
        self.visible = false;
        self.pending_show = false;
        let _ = self.cmd_tx.send(BackendCommand::Hide);
        let panel = self
            .surface_id
            .map(|surface_id| set_input_zone(surface_id, Some(Vec::new())))
            .unwrap_or_else(Task::none);
        let keyboard = self
            .keyboard_id
            .take()
            .map(destroy_layer_surface)
            .unwrap_or_else(Task::none);
        Task::batch([panel, keyboard])
    }
}

#[derive(Clone)]
struct SocketSub {
    listener: TakeOnce<UnixListener>,
}

impl Hash for SocketSub {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::any::TypeId::of::<Self>().hash(state);
    }
}

fn socket_stream(sub: &SocketSub) -> cosmic::iced::futures::stream::BoxStream<'static, Message> {
    let socket_listener = sub.listener.clone();
    Box::pin(cosmic::iced::stream::channel(
        16,
        move |mut tx: cosmic::iced::futures::channel::mpsc::Sender<Message>| async move {
            use cosmic::iced::futures::SinkExt;
            use tokio::io::AsyncReadExt;

            let Some(std_listener) = socket_listener.take() else {
                std::future::pending::<()>().await;
                unreachable!();
            };
            let Ok(listener) = tokio::net::UnixListener::from_std(std_listener) else {
                std::future::pending::<()>().await;
                unreachable!();
            };

            loop {
                if let Ok((mut stream, _)) = listener.accept().await {
                    let mut buf = Vec::new();
                    let _ = stream.read_to_end(&mut buf).await;
                    let msg = match buf.as_slice() {
                        b"next" => Message::SelectNext,
                        b"prev" => Message::SelectPrev,
                        b"quit" => Message::Quit,
                        _ => continue,
                    };
                    log::debug!("socket command: {msg:?}");
                    let _ = tx.send(msg).await;
                }
            }
        },
    ))
}

#[derive(Clone)]
struct BackendSub {
    rx: TakeOnce<UnboundedReceiver<BackendEvent>>,
}

impl Hash for BackendSub {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::any::TypeId::of::<Self>().hash(state);
    }
}

fn backend_stream(sub: &BackendSub) -> cosmic::iced::futures::stream::BoxStream<'static, Message> {
    let rx = sub.rx.take();
    Box::pin(cosmic::iced::stream::channel(
        32,
        move |mut tx: cosmic::iced::futures::channel::mpsc::Sender<Message>| async move {
            use cosmic::iced::futures::SinkExt;

            let Some(mut rx) = rx else {
                std::future::pending::<()>().await;
                unreachable!();
            };
            while let Some(event) = rx.recv().await {
                if tx.send(Message::Backend(event)).await.is_err() {
                    break;
                }
            }
            // The backend thread is gone; exit so the next invocation can
            // start a fresh daemon instead of talking to a dead socket.
            let _ = tx.send(Message::Quit).await;
        },
    ))
}

pub fn run(
    cmd_tx: calloop::channel::Sender<BackendCommand>,
    backend_rx: UnboundedReceiver<BackendEvent>,
    socket_listener: UnixListener,
    theme: Theme,
    initial_output: Option<OutputTarget>,
) -> Result<()> {
    let restart_required = Arc::new(AtomicBool::new(false));
    let settings = Settings::default()
        .no_main_window(true)
        .exit_on_close(false);
    let result = cosmic::app::run::<AppSwitcher>(
        settings,
        AppFlags {
            cmd_tx,
            backend_rx,
            socket_listener,
            theme,
            initial_output,
            restart_required: restart_required.clone(),
        },
    );
    if restart_required.load(Ordering::Relaxed) {
        return Err(anyhow::anyhow!(
            "Wayland connection lost; restarting daemon"
        ));
    }
    result.map_err(|e| anyhow::anyhow!("{e:?}"))
}
