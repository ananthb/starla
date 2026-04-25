//! Starla tray app: system tray icon showing probe status

use anyhow::Result;
use starla_common::status::ProbeStatus;
use std::sync::{Arc, Mutex};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::event_loop::{ActiveEventLoop, EventLoop};

/// Embedded tray icon (star with signal arcs, RGBA PNG).
const TRAY_ICON_PNG: &[u8] = include_bytes!("../../../assets/tray.png");

/// Tint the embedded icon's opaque pixels with the given RGB color, preserving
/// alpha.
fn tint_icon(r: u8, g: u8, b: u8) -> Icon {
    let decoder = png::Decoder::new(TRAY_ICON_PNG);
    let mut reader = decoder.read_info().expect("Failed to read PNG header");
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).expect("Failed to decode PNG");
    buf.truncate(info.buffer_size());

    let mut rgba = match info.color_type {
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

    for pixel in rgba.chunks_exact_mut(4) {
        if pixel[3] > 0 {
            pixel[0] = r;
            pixel[1] = g;
            pixel[2] = b;
        }
    }

    Icon::from_rgba(rgba, info.width, info.height).expect("Failed to create tinted icon")
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
    _icon_green: Icon,
    _icon_red: Icon,
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

    let icon_green = tint_icon(76, 175, 80);
    let icon_red = tint_icon(244, 67, 54);

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
        _icon_green: icon_green.clone(),
        _icon_red: icon_red.clone(),
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
                "Starla: Probe {}: {}",
                s.probe_id,
                if s.connected {
                    "Connected"
                } else {
                    "Disconnected"
                }
            )
        })
        .unwrap_or_else(|| "Starla: Probe not running".to_string());

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
