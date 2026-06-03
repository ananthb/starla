//! Starla tray app: system tray icon showing probe status

mod daemon;

use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use starla_common::pause::PauseState;
use starla_common::status::ProbeStatus;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::event_loop::{ActiveEventLoop, EventLoop};

const TRAY_ICON_PNG: &[u8] = include_bytes!("../tray.png");

/// Decode the embedded PNG into the `Icon` shape tray-icon expects.
/// macOS reads only the alpha mask when the icon is marked as a template
/// (see `with_icon_as_template`), so the source RGB values are ignored
/// there. Other platforms keep the source colours.
fn load_icon() -> Icon {
    let decoder = png::Decoder::new(std::io::Cursor::new(TRAY_ICON_PNG));
    let mut reader = decoder.read_info().expect("Failed to read PNG header");
    let mut buf = vec![0u8; reader.output_buffer_size().expect("PNG too large")];
    let info = reader.next_frame(&mut buf).expect("Failed to decode PNG");
    buf.truncate(info.buffer_size());

    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => {
            let mut rgba = Vec::with_capacity(buf.len() / 3 * 4);
            for chunk in buf.chunks(3) {
                rgba.extend_from_slice(chunk);
                rgba.push(255);
            }
            rgba
        }
        _ => panic!("Unexpected PNG color type: {:?}", info.color_type),
    };

    Icon::from_rgba(rgba, info.width, info.height).expect("Failed to create icon")
}

/// Read probe status from the Unix domain socket
fn read_status() -> Option<ProbeStatus> {
    #[cfg(unix)]
    {
        use std::io::Read;
        use std::os::unix::net::UnixStream;
        use std::time::Duration;

        let socket_path = starla_common::status_socket_path();
        let mut stream = UnixStream::connect(&socket_path).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;

        let mut buf = String::new();
        stream.read_to_string(&mut buf).ok()?;
        serde_json::from_str(&buf).ok()
    }
    #[cfg(not(unix))]
    {
        None
    }
}

fn format_uptime(secs: u64) -> String {
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    if days > 0 {
        format!("{}d {}h {}m", days, hours, mins)
    } else if hours > 0 {
        format!("{}h {}m", hours, mins)
    } else {
        format!("{}m", mins)
    }
}

fn format_pause(state: &PauseState) -> String {
    match state {
        PauseState::Indefinite => "Paused indefinitely".to_string(),
        PauseState::Until(t) => {
            let local: DateTime<Local> = (*t).into();
            format!("Paused until {}", local.format("%H:%M"))
        }
    }
}

/// Status line: connection-or-pause state, plus a "why" if disconnected.
/// Returns (header, optional second line with detail).
fn status_lines(s: &ProbeStatus) -> (String, Option<String>) {
    if let Some(ref p) = s.pause {
        return (format_pause(p), None);
    }
    if s.connected {
        return ("Connected".to_string(), None);
    }
    if s.probe_id == 0 {
        return (
            "Not registered".to_string(),
            Some("Register the public key at atlas.ripe.net/apply/swprobe".to_string()),
        );
    }
    let detail = s
        .last_connection_error
        .clone()
        .unwrap_or_else(|| "controller unreachable".to_string());
    ("Disconnected".to_string(), Some(detail))
}

/// Pause duration options shown in the tray submenu, in order.
fn pause_options() -> Vec<(&'static str, &'static str, Option<chrono::Duration>)> {
    vec![
        (
            "pause_30m",
            "30 minutes",
            Some(chrono::Duration::minutes(30)),
        ),
        ("pause_1h", "1 hour", Some(chrono::Duration::hours(1))),
        ("pause_4h", "4 hours", Some(chrono::Duration::hours(4))),
        ("pause_8h", "8 hours", Some(chrono::Duration::hours(8))),
        ("pause_24h", "24 hours", Some(chrono::Duration::hours(24))),
        ("pause_indefinite", "Indefinitely", None),
    ]
}

struct Ids {
    copy_key: MenuId,
    open_atlas: MenuId,
    quit: MenuId,
    resume: MenuId,
    start_daemon: MenuId,
    restart_daemon: MenuId,
    stop_daemon: MenuId,
}

struct App {
    _icon: Icon,
    status: Arc<Mutex<Option<ProbeStatus>>>,
    /// Bumped by the background refresh thread and by menu event handlers
    /// whenever the cached status changes. The main loop notices a bump
    /// and rebuilds the menu — without this the menu is frozen to whatever
    /// the status was when the tray launched.
    status_version: Arc<AtomicU64>,
    last_built_version: u64,
    tray: Option<TrayIcon>,
    ids: Ids,
}

impl App {
    fn build_menu(&self) -> Menu {
        let menu = Menu::new();
        let status = self.status.lock().unwrap();

        if let Some(ref s) = *status {
            let (header, detail) = status_lines(s);
            let _ = menu.append(&MenuItem::new(header, false, None));
            if let Some(line) = detail {
                let _ = menu.append(&MenuItem::new(line, false, None));
            }
            if s.probe_id != 0 {
                let _ = menu.append(&MenuItem::new(format!("Probe {}", s.probe_id), false, None));
            }
            let _ = menu.append(&MenuItem::new(
                format!("Uptime: {}", format_uptime(s.uptime_secs)),
                false,
                None,
            ));
            let _ = menu.append(&PredefinedMenuItem::separator());

            let total: u64 = s.measurements.values().sum();
            let _ = menu.append(&MenuItem::new(
                format!("Measurements: {}", total),
                false,
                None,
            ));
            let mut types: Vec<_> = s.measurements.iter().collect();
            types.sort_by(|a, b| b.1.cmp(a.1));
            for (name, count) in types {
                let _ = menu.append(&MenuItem::new(
                    format!("  {}: {}", name, count),
                    false,
                    None,
                ));
            }
            let _ = menu.append(&PredefinedMenuItem::separator());

            if s.pause.is_some() {
                let _ = menu.append(&MenuItem::with_id(
                    self.ids.resume.clone(),
                    "Resume measurements",
                    true,
                    None,
                ));
            } else {
                let submenu = Submenu::new("Pause measurements", true);
                for (id, label, _) in pause_options() {
                    let _ = submenu.append(&MenuItem::with_id(MenuId::new(id), label, true, None));
                }
                let _ = menu.append(&submenu);
            }

            if daemon::SUPPORTED {
                let _ = menu.append(&PredefinedMenuItem::separator());
                let _ = menu.append(&MenuItem::with_id(
                    self.ids.restart_daemon.clone(),
                    "Restart probe",
                    true,
                    None,
                ));
                let _ = menu.append(&MenuItem::with_id(
                    self.ids.stop_daemon.clone(),
                    "Stop probe",
                    true,
                    None,
                ));
            }
        } else {
            let _ = menu.append(&MenuItem::new("Probe not running", false, None));
            if daemon::SUPPORTED {
                let _ = menu.append(&MenuItem::with_id(
                    self.ids.start_daemon.clone(),
                    "Start probe",
                    true,
                    None,
                ));
            }
        }

        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&MenuItem::with_id(
            self.ids.copy_key.clone(),
            "Copy Public Key",
            true,
            None,
        ));
        let _ = menu.append(&MenuItem::with_id(
            self.ids.open_atlas.clone(),
            "Open RIPE Atlas",
            true,
            None,
        ));
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&MenuItem::with_id(
            self.ids.quit.clone(),
            "Quit",
            true,
            None,
        ));

        menu
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}
    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _id: winit::window::WindowId,
        _event: winit::event::WindowEvent,
    ) {
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == self.ids.quit {
                event_loop.exit();
            } else if event.id == self.ids.copy_key {
                let status = self.status.lock().unwrap();
                if let Some(ref s) = *status {
                    if let Some(ref key) = s.public_key {
                        if let Ok(mut clipboard) = arboard::Clipboard::new() {
                            let _ = clipboard.set_text(key.clone());
                        }
                    }
                }
            } else if event.id == self.ids.open_atlas {
                let status = self.status.lock().unwrap();
                if let Some(ref s) = *status {
                    if s.probe_id != 0 {
                        let _ =
                            open::that(format!("https://atlas.ripe.net/probes/{}/", s.probe_id));
                    } else {
                        let _ = open::that("https://atlas.ripe.net/apply/swprobe/");
                    }
                }
            } else if event.id == self.ids.resume {
                let _ = starla_common::write_pause_state(None);
                refresh_status_from_disk(&self.status);
                self.status_version.fetch_add(1, Ordering::Release);
            } else if event.id == self.ids.start_daemon {
                if let Err(e) = daemon::start() {
                    eprintln!("Failed to start probe: {}", e);
                }
                self.bump_after_daemon_command();
            } else if event.id == self.ids.restart_daemon {
                if let Err(e) = daemon::restart() {
                    eprintln!("Failed to restart probe: {}", e);
                }
                self.bump_after_daemon_command();
            } else if event.id == self.ids.stop_daemon {
                if let Err(e) = daemon::stop() {
                    eprintln!("Failed to stop probe: {}", e);
                }
                self.bump_after_daemon_command();
            } else if let Some((_, _, dur)) = pause_options()
                .into_iter()
                .find(|(id, _, _)| event.id == *id)
            {
                let new_state = match dur {
                    Some(d) => PauseState::Until(Utc::now() + d),
                    None => PauseState::Indefinite,
                };
                let _ = starla_common::write_pause_state(Some(new_state));
                refresh_status_from_disk(&self.status);
                self.status_version.fetch_add(1, Ordering::Release);
            }
        }

        // Rebuild the menu if the cached status has changed since we last
        // rendered. Without this, the menu only ever reflects what was
        // true at tray startup.
        let v = self.status_version.load(Ordering::Acquire);
        if v != self.last_built_version {
            self.last_built_version = v;
            let menu = self.build_menu();
            if let Some(tray) = self.tray.as_ref() {
                tray.set_menu(Some(Box::new(menu)));
            }
        }
    }
}

impl App {
    /// Daemon commands (start/stop/restart) don't update the cached
    /// status themselves — the socket may take a moment to come up or
    /// go away. Re-read it now so the menu reflects the new state on
    /// the next about_to_wait tick, even before the 30s refresh fires.
    fn bump_after_daemon_command(&self) {
        let new_status = read_status();
        let mut guard = self.status.lock().unwrap();
        *guard = new_status;
        drop(guard);
        self.status_version.fetch_add(1, Ordering::Release);
    }
}

/// Mirror the pause file into the cached status so the menu reflects
/// the change before the probe's next status push.
fn refresh_status_from_disk(status: &Arc<Mutex<Option<ProbeStatus>>>) {
    let pause = starla_common::read_pause_state().and_then(|s| {
        if s.is_active(Utc::now()) {
            Some(s)
        } else {
            None
        }
    });
    let mut guard = status.lock().unwrap();
    if let Some(ref mut s) = *guard {
        s.pause = pause;
    }
}

fn main() -> Result<()> {
    let event_loop = EventLoop::new()?;

    let icon = load_icon();

    let status: Arc<Mutex<Option<ProbeStatus>>> = Arc::new(Mutex::new(read_status()));
    let status_version = Arc::new(AtomicU64::new(1));

    let ids = Ids {
        copy_key: MenuId::new("copy_key"),
        open_atlas: MenuId::new("open_atlas"),
        quit: MenuId::new("quit"),
        resume: MenuId::new("resume"),
        start_daemon: MenuId::new("start_daemon"),
        restart_daemon: MenuId::new("restart_daemon"),
        stop_daemon: MenuId::new("stop_daemon"),
    };

    let mut app = App {
        _icon: icon.clone(),
        status: status.clone(),
        status_version: status_version.clone(),
        last_built_version: 1,
        tray: None,
        ids,
    };

    let tooltip = status
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| {
            let (header, _) = status_lines(s);
            if s.probe_id != 0 {
                format!("Starla {}: {}", s.probe_id, header)
            } else {
                format!("Starla: {}", header)
            }
        })
        .unwrap_or_else(|| "Starla: probe not running".to_string());

    let tray_builder = TrayIconBuilder::new()
        .with_tooltip(&tooltip)
        .with_icon(icon)
        .with_menu(Box::new(app.build_menu()));

    #[cfg(target_os = "macos")]
    let tray_builder = tray_builder.with_icon_as_template(true);

    app.tray = Some(tray_builder.build()?);

    // Background thread: refresh status every 30s.
    // Bump the version on every refresh so the main loop knows to
    // re-render the menu; this is what lets the menu recover after the
    // daemon comes back from a restart or crash.
    let bg_status = status.clone();
    let bg_version = status_version.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(30));
        let new_status = read_status();
        let mut guard = bg_status.lock().unwrap();
        let changed = match (&*guard, &new_status) {
            (None, None) => false,
            (Some(a), Some(b)) => !status_equivalent(a, b),
            _ => true,
        };
        *guard = new_status;
        drop(guard);
        if changed {
            bg_version.fetch_add(1, Ordering::Release);
        }
    });

    event_loop.run_app(&mut app)?;
    Ok(())
}

/// Compare two statuses for "would render the same menu". Uptime is
/// compared at minute resolution because that's what `format_uptime`
/// displays — finer-grained comparison would rebuild the menu twice
/// per minute for no visible change. `queue_depth` is excluded because
/// it isn't shown in the menu.
fn status_equivalent(a: &ProbeStatus, b: &ProbeStatus) -> bool {
    a.probe_id == b.probe_id
        && a.connected == b.connected
        && a.pause == b.pause
        && a.last_connection_error == b.last_connection_error
        && a.public_key == b.public_key
        && a.uptime_secs / 60 == b.uptime_secs / 60
        && a.measurements == b.measurements
}
