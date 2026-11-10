// SPDX-License-Identifier: GPL-3.0-only

//! Entry point, daemon supervisor, and fatal Wayland-error handling.

mod app;
mod capture;
mod events;
mod icons;
mod ui;
mod wayland;

use anyhow::{Context, Result};
use clap::Parser;
use cosmic::cosmic_config::ConfigGet;
use std::{
    io::Write,
    os::unix::net::{UnixListener, UnixStream},
    time::Duration,
};
use switcher_config::{APP_ID, CONFIG_VERSION, Theme, WorkspaceScope};

pub fn socket_path() -> std::path::PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(dir).join("cosmic-ext-app-switcher.sock")
}

#[derive(Parser, Debug)]
#[command(name = "cosmic-ext-app-switcher", version)]
pub struct Args {
    #[arg(long, default_value_t = false)]
    pub reverse: bool,

    /// Which windows to list, overriding the configured scope for this invocation
    #[arg(long, value_enum)]
    pub scope: Option<ScopeArg>,

    /// Skip preview capture and show app icons only
    #[arg(long, default_value_t = false)]
    pub no_previews: bool,

    /// Maximum number of previews shown at once (1-18)
    #[arg(long, value_name = "N")]
    pub preview_count: Option<usize>,

    /// Report whether the compositor offers the Wayland interfaces the switcher needs, then exit
    #[arg(long, default_value_t = false)]
    pub check_compat: bool,

    /// Start the resident backend hidden, without showing the switcher
    #[arg(long, default_value_t = false)]
    pub daemon: bool,

    /// Stop a running resident switcher and exit
    #[arg(long, default_value_t = false)]
    pub quit: bool,
}

/// The scopes worth offering on the command line. `CurrentWorkspace` is deliberately absent
/// from the applet picker: cosmic-comp's ext workspace handles are only sent to clients that
/// bind the ext workspace manager, which the switcher does not need.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum ScopeArg {
    /// Every window, on every workspace and monitor
    All,
    /// Only windows on the monitor holding the window you're switching away from
    CurrentMonitor,
}

impl From<ScopeArg> for WorkspaceScope {
    fn from(arg: ScopeArg) -> Self {
        match arg {
            ScopeArg::All => WorkspaceScope::AllWorkspaces,
            ScopeArg::CurrentMonitor => WorkspaceScope::CurrentOutput,
        }
    }
}

pub(crate) fn load_theme() -> Theme {
    use cosmic::cosmic_config::Config;
    Config::new(APP_ID, CONFIG_VERSION)
        .ok()
        .and_then(|c| c.get::<Theme>("theme").ok())
        .unwrap_or_default()
}

fn load_scope() -> WorkspaceScope {
    use cosmic::cosmic_config::Config;
    Config::new(APP_ID, CONFIG_VERSION)
        .ok()
        .and_then(|c| c.get::<WorkspaceScope>("workspace_scope").ok())
        .unwrap_or_default()
}

/// Prints the required and preview-only Wayland globals.  Missing preview
/// globals are reported but do not fail the check.
fn check_compat() -> Result<()> {
    let report = wayland::probe_required_globals()?;
    for status in &report.required {
        println!("{status}");
    }
    if !report.required.iter().all(wayland::GlobalStatus::ok) {
        std::process::exit(1);
    }

    println!("previews:");
    for status in &report.preview {
        println!("  {status}");
    }
    if !report.preview.iter().all(wayland::GlobalStatus::ok) {
        println!("  previews unavailable — app icons will be used");
    }
    Ok(())
}

/// Exit immediately when a Wayland connection is lost.  `calloop-wayland-source`
/// treats a failed socket flush as fatal, but iced's SCTK thread drops the error
/// and can leave the daemon alive but unable to present.  Exiting lets the
/// launcher or supervisor start a fresh process instead of hanging.
fn is_fatal_wayland_error(level: log::Level, target: &str, message: &str) -> bool {
    level == log::Level::Error
        && target == "calloop_wayland_source"
        && message.contains("Error trying to flush the wayland display")
}

struct WaylandExitLogger(env_logger::Logger);

impl log::Log for WaylandExitLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.0.enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        self.0.log(record);

        if record.level() == log::Level::Error && record.target() == "calloop_wayland_source" {
            let message = record.args().to_string();
            if is_fatal_wayland_error(record.level(), record.target(), &message) {
                eprintln!("cosmic-ext-app-switcher: Wayland connection lost; exiting");
                std::process::exit(1);
            }
        }
    }

    fn flush(&self) {
        self.0.flush();
    }
}

fn init_logging() {
    let logger = env_logger::Builder::from_default_env()
        .format_timestamp_millis()
        .build();
    // Always let error records reach the wrapper below, even when RUST_LOG
    // filters the calloop target out of normal output.
    log::set_max_level(logger.filter().max(log::LevelFilter::Error));
    let _ = log::set_boxed_logger(Box::new(WaylandExitLogger(logger)));
}

fn command_bytes(args: &Args) -> &'static [u8] {
    if args.quit {
        b"quit"
    } else if args.reverse {
        b"prev"
    } else {
        b"next"
    }
}

fn main() -> Result<()> {
    init_logging();
    let args = Args::parse();

    if args.check_compat {
        return check_compat();
    }

    // If a switcher is already running, signal it to cycle/quit and exit.
    if forward_if_running(&args) {
        return Ok(());
    }
    if args.quit {
        return Ok(());
    }

    if args.daemon {
        // Supervise the autostarted daemon: a lost Wayland connection or a
        // backend/capture failure exits the app, then this loop starts a fresh
        // process and reconnects once the compositor is available again.
        loop {
            match run_daemon(&args) {
                Ok(()) => return Ok(()),
                Err(err) => {
                    log::error!("daemon: {err:#}; restarting in 2s");
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
        }
    }

    run_daemon(&args)
}

fn forward_if_running(args: &Args) -> bool {
    let Ok(mut stream) = UnixStream::connect(socket_path()) else {
        return false;
    };
    if !args.daemon {
        let _ = stream.write_all(command_bytes(args));
    }
    true
}

/// Bind the daemon socket.  Only remove an existing path after confirming that
/// nothing is listening on it, so a bind race cannot unlink a live daemon.
fn bind_daemon_socket() -> std::io::Result<UnixListener> {
    match UnixListener::bind(socket_path()) {
        Ok(listener) => Ok(listener),
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
            if UnixStream::connect(socket_path()).is_ok() {
                return Err(err);
            }
            let _ = std::fs::remove_file(socket_path());
            UnixListener::bind(socket_path())
        }
        Err(err) => Err(err),
    }
}

fn run_daemon(args: &Args) -> Result<()> {
    // A supervisor restart may race with a shortcut-started daemon.  If one is
    // already listening, forward the command instead of unlinking its socket.
    if forward_if_running(args) {
        return Ok(());
    }

    // Bind before preview capture: capture can take a few hundred
    // milliseconds, and subsequent Tab presses must find this instance rather
    // than starting a second overlay.
    let listener = match bind_daemon_socket() {
        Ok(listener) => listener,
        Err(err) => {
            if forward_if_running(args) {
                return Ok(());
            }
            return Err(err).with_context(|| format!("could not bind {}", socket_path().display()));
        }
    };
    listener.set_nonblocking(true)?;

    let theme = load_theme();
    // A --scope on the command line beats the applet's setting, so one binding can list
    // every window while another lists only this monitor's (#7).
    let scope = args.scope.map_or_else(load_scope, WorkspaceScope::from);
    let (cmd_tx, backend_rx, initial_output) =
        wayland::spawn_backend(scope, args.no_previews, args.preview_count)?;

    if !args.daemon {
        let _ = cmd_tx.send(events::BackendCommand::Show {
            reverse: args.reverse,
        });
    }

    app::run(cmd_tx, backend_rx, listener, theme, initial_output)
}

#[cfg(test)]
mod tests {
    use super::{Args, ScopeArg, is_fatal_wayland_error};
    use clap::Parser;
    use switcher_config::WorkspaceScope;

    #[test]
    fn fatal_wayland_flush_is_detected() {
        assert!(is_fatal_wayland_error(
            log::Level::Error,
            "calloop_wayland_source",
            "Error trying to flush the wayland display: Invalid argument (os error 22)"
        ));
        assert!(!is_fatal_wayland_error(
            log::Level::Debug,
            "calloop_wayland_source",
            "Error trying to flush the wayland display: Invalid argument (os error 22)"
        ));
        assert!(!is_fatal_wayland_error(
            log::Level::Error,
            "another_target",
            "Error trying to flush the wayland display: Invalid argument (os error 22)"
        ));
    }

    #[test]
    fn scope_is_optional_so_the_applet_setting_still_applies() {
        assert_eq!(Args::parse_from(["switcher"]).scope, None);
    }

    #[test]
    fn scope_values_map_to_the_config_scopes() {
        let parsed = Args::parse_from(["switcher", "--scope", "current-monitor"]);
        assert_eq!(parsed.scope, Some(ScopeArg::CurrentMonitor));
        assert_eq!(
            WorkspaceScope::from(parsed.scope.unwrap()),
            WorkspaceScope::CurrentOutput
        );

        let parsed = Args::parse_from(["switcher", "--scope", "all", "--reverse"]);
        assert_eq!(
            WorkspaceScope::from(parsed.scope.unwrap()),
            WorkspaceScope::AllWorkspaces
        );
        assert!(parsed.reverse);
    }

    #[test]
    fn no_previews_is_optional() {
        assert!(!Args::parse_from(["switcher"]).no_previews);
        assert!(Args::parse_from(["switcher", "--no-previews"]).no_previews);
    }

    #[test]
    fn preview_count_is_optional() {
        assert_eq!(Args::parse_from(["switcher"]).preview_count, None);
        assert_eq!(
            Args::parse_from(["switcher", "--preview-count", "6"]).preview_count,
            Some(6)
        );
    }

    #[test]
    fn daemon_and_quit_flags_are_optional() {
        let args = Args::parse_from(["switcher"]);
        assert!(!args.daemon);
        assert!(!args.quit);
        assert!(Args::parse_from(["switcher", "--daemon"]).daemon);
        assert!(Args::parse_from(["switcher", "--quit"]).quit);
    }

    #[test]
    fn unknown_scopes_are_rejected() {
        // Including the one the picker hides, so it can't look like a working option.
        assert!(Args::try_parse_from(["switcher", "--scope", "current-workspace"]).is_err());
        assert!(Args::try_parse_from(["switcher", "--scope", "nonsense"]).is_err());
    }
}
