//! Starla tray app: system tray icon showing probe status

use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use starla_common::pause::PauseState;
use starla_common::status::ProbeStatus;
use std::sync::{Arc, Mutex};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIconBuilder};
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
}

struct App {
    _icon: Icon,
    status: Arc<Mutex<Option<ProbeStatus>>>,
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
        } else {
            let _ = menu.append(&MenuItem::new("Probe not running", false, None));
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
            }
        }
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

    let ids = Ids {
        copy_key: MenuId::new("copy_key"),
        open_atlas: MenuId::new("open_atlas"),
        quit: MenuId::new("quit"),
        resume: MenuId::new("resume"),
    };

    let mut app = App {
        _icon: icon.clone(),
        status: status.clone(),
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

    let mut tray_builder = TrayIconBuilder::new()
        .with_tooltip(&tooltip)
        .with_icon(icon)
        .with_menu(Box::new(app.build_menu()));

    #[cfg(target_os = "macos")]
    {
        tray_builder = tray_builder.with_icon_as_template(true);
    }

    let _tray = tray_builder.build()?;

    // Background thread: refresh status every 30s
    let bg_status = status.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(30));
        if let Some(new_status) = read_status() {
            *bg_status.lock().unwrap() = Some(new_status);
        } else {
            *bg_status.lock().unwrap() = None;
        }
    });

    event_loop.run_app(&mut app)?;
    Ok(())
}
