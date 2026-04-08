//! Starla tray app — system tray icon showing probe status

use anyhow::Result;
use starla_common::status::ProbeStatus;
use std::sync::{Arc, Mutex};
use tray_icon::menu::{AboutMetadata, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::event_loop::{ActiveEventLoop, EventLoop};

/// Generate a colored circle icon (16x16 RGBA)
fn make_icon(r: u8, g: u8, b: u8) -> Icon {
    let size = 16u32;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    let center = size as f32 / 2.0;
    let radius = center - 1.0;

    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let dist = (dx * dx + dy * dy).sqrt();
            if dist <= radius {
                rgba.extend_from_slice(&[r, g, b, 255]);
            } else if dist <= radius + 1.0 {
                let alpha = ((radius + 1.0 - dist) * 255.0) as u8;
                rgba.extend_from_slice(&[r, g, b, alpha]);
            } else {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }

    Icon::from_rgba(rgba, size, size).expect("Failed to create icon")
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

struct App {
    icon_green: Icon,
    icon_red: Icon,
    status: Arc<Mutex<Option<ProbeStatus>>>,
    copy_key_id: tray_icon::menu::MenuId,
    open_atlas_id: tray_icon::menu::MenuId,
    quit_id: tray_icon::menu::MenuId,
}

impl App {
    fn build_menu(&self) -> Menu {
        let menu = Menu::new();
        let status = self.status.lock().unwrap();

        if let Some(ref s) = *status {
            let state_label = if s.connected {
                "Connected"
            } else {
                "Disconnected"
            };

            let _ = menu.append(&MenuItem::new(format!("Probe {}", s.probe_id), false, None));
            let _ = menu.append(&MenuItem::new(state_label, false, None));
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
        } else {
            let _ = menu.append(&MenuItem::new("Probe not running", false, None));
        }

        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&MenuItem::with_id(
            self.copy_key_id.clone(),
            "Copy Public Key",
            true,
            None,
        ));
        let _ = menu.append(&MenuItem::with_id(
            self.open_atlas_id.clone(),
            "Open RIPE Atlas",
            true,
            None,
        ));
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&MenuItem::with_id(self.quit_id.clone(), "Quit", true, None));

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
            if event.id == self.quit_id {
                event_loop.exit();
            } else if event.id == self.copy_key_id {
                let status = self.status.lock().unwrap();
                if let Some(ref s) = *status {
                    if let Some(ref key) = s.public_key {
                        if let Ok(mut clipboard) = arboard::Clipboard::new() {
                            let _ = clipboard.set_text(key.clone());
                        }
                    }
                }
            } else if event.id == self.open_atlas_id {
                let status = self.status.lock().unwrap();
                if let Some(ref s) = *status {
                    if s.probe_id != 0 {
                        let _ =
                            open::that(format!("https://atlas.ripe.net/probes/{}/", s.probe_id));
                    } else {
                        let _ = open::that("https://atlas.ripe.net/apply/swprobe/");
                    }
                }
            }
        }
    }
}

fn main() -> Result<()> {
    let event_loop = EventLoop::new()?;

    let icon_green = make_icon(76, 175, 80);
    let icon_red = make_icon(244, 67, 54);

    let status: Arc<Mutex<Option<ProbeStatus>>> = Arc::new(Mutex::new(read_status()));
    let connected = status
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.connected)
        .unwrap_or(false);

    let copy_key_id = tray_icon::menu::MenuId::new("copy_key");
    let open_atlas_id = tray_icon::menu::MenuId::new("open_atlas");
    let quit_id = tray_icon::menu::MenuId::new("quit");

    let mut app = App {
        icon_green: icon_green.clone(),
        icon_red: icon_red.clone(),
        status: status.clone(),
        copy_key_id,
        open_atlas_id,
        quit_id,
    };

    let initial_icon = if connected {
        icon_green.clone()
    } else {
        icon_red.clone()
    };

    let tooltip = status
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| {
            format!(
                "Starla — Probe {} — {}",
                s.probe_id,
                if s.connected {
                    "Connected"
                } else {
                    "Disconnected"
                }
            )
        })
        .unwrap_or_else(|| "Starla — Probe not running".to_string());

    let _tray = TrayIconBuilder::new()
        .with_tooltip(&tooltip)
        .with_icon(initial_icon)
        .with_menu(Box::new(app.build_menu()))
        .build()?;

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
