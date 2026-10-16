//! System tray icon (freedesktop StatusNotifierItem over D-Bus).
//!
//! Runs as its own lightweight process (`th-linux-lcd --tray`, started at
//! login via XDG autostart). It shows the daemon status as the icon colour,
//! opens the GUI on click, and offers a few quick actions. It never touches
//! USB itself — everything goes through the daemon socket.

use ksni::blocking::TrayMethods;
use ksni::menu::{MenuItem, StandardItem};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use th_linux_lcd::shared::{self, Cmd, DaemonStatus, Resp};

const POLL: Duration = Duration::from_secs(2);
const ICON_SIZES: [i32; 4] = [22, 32, 48, 64];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    NoDaemon,
    NoDevice,
    WritingFlash,
    Streaming,
    Idle,
}

impl State {
    fn from_status(s: Option<&DaemonStatus>) -> Self {
        match s {
            None => State::NoDaemon,
            Some(s) if !s.device_found => State::NoDevice,
            Some(s) if s.flash_busy => State::WritingFlash,
            Some(s) if s.streaming => State::Streaming,
            Some(_) => State::Idle,
        }
    }

    fn label(self) -> &'static str {
        match self {
            State::NoDaemon => "Daemon not running",
            State::NoDevice => "No device",
            State::WritingFlash => "Writing flash — do not power off",
            State::Streaming => "Streaming",
            State::Idle => "Idle",
        }
    }

    /// Same palette as the GUI header.
    fn color(self) -> [u8; 3] {
        match self {
            State::NoDaemon => [210, 45, 45],
            State::NoDevice => [180, 70, 0],
            State::WritingFlash | State::Idle => [200, 150, 0],
            State::Streaming => [0, 210, 55],
        }
    }
}

/// Anti-aliased status icon: dark disc with a coloured ring and centre dot.
/// ARGB32, network byte order.
fn status_icon(size: i32, [r, g, b]: [u8; 3]) -> ksni::Icon {
    let n = size as usize;
    let c = size as f32 / 2.0;
    let outer = c - 0.5;
    let ring = (size as f32 * 0.14).max(2.0);
    let dot = size as f32 * 0.17;
    let mut data = Vec::with_capacity(n * n * 4);
    for y in 0..n {
        for x in 0..n {
            let d = (x as f32 + 0.5 - c).hypot(y as f32 + 0.5 - c);
            let alpha = (outer - d + 0.5).clamp(0.0, 1.0);
            let in_ring = (d - (outer - ring) + 0.5).clamp(0.0, 1.0);
            let in_dot = (dot - d + 0.5).clamp(0.0, 1.0);
            let t = in_ring.max(in_dot);
            let mix = |fg: u8| (5.0 + (f32::from(fg) - 5.0) * t).round() as u8;
            data.extend([(alpha * 255.0).round() as u8, mix(r), mix(g), mix(b)]);
        }
    }
    ksni::Icon { width: size, height: size, data }
}

/// Send a command to the daemon without blocking the D-Bus thread.
fn send(cmd: Cmd) {
    std::thread::spawn(move || match shared::daemon_cmd(&cmd) {
        Ok(Resp::Err { msg }) => eprintln!("[tray] {msg}"),
        Err(e) => eprintln!("[tray] {e}"),
        _ => {}
    });
}

struct Tray {
    state: State,
    playing: Option<String>,
    gui_children: Vec<Child>,
}

impl Tray {
    /// Launch the GUI. If one is already running, the new process asks it
    /// to come forward and exits immediately.
    fn open_gui(&mut self) {
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => return eprintln!("[tray] cannot find own executable: {e}"),
        };
        match Command::new(exe).stdin(Stdio::null()).process_group(0).spawn() {
            Ok(child) => self.gui_children.push(child),
            Err(e) => eprintln!("[tray] cannot start the GUI: {e}"),
        }
    }

    fn reap(&mut self) {
        self.gui_children.retain_mut(|c| matches!(c.try_wait(), Ok(None)));
    }

    fn status_text(&self) -> String {
        match (&self.state, &self.playing) {
            (State::Streaming, Some(name)) => format!("Streaming: {name}"),
            (state, _) => state.label().to_string(),
        }
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "th-linux-lcd".into()
    }

    fn title(&self) -> String {
        "TH Linux LCD".into()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::Hardware
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        ICON_SIZES.iter().map(|&s| status_icon(s, self.state.color())).collect()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip { title: "TH Linux LCD".into(), description: self.status_text(), ..Default::default() }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.open_gui();
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let device_ready = !matches!(self.state, State::NoDaemon | State::NoDevice | State::WritingFlash);
        vec![
            StandardItem {
                label: "Open TH Linux LCD".into(),
                icon_name: "window-new".into(),
                activate: Box::new(|t: &mut Self| t.open_gui()),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem { label: self.status_text(), enabled: false, ..Default::default() }.into(),
            StandardItem {
                label: "Stop streaming".into(),
                icon_name: "media-playback-stop".into(),
                enabled: self.state == State::Streaming,
                activate: Box::new(|_| send(Cmd::Stop)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Screen on".into(),
                enabled: device_ready,
                activate: Box::new(|_| send(Cmd::ScreenOn)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Screen off".into(),
                enabled: device_ready,
                activate: Box::new(|_| send(Cmd::ScreenOff)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit tray icon".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|_| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Runs the tray; returns the process exit code.
pub fn run() -> i32 {
    let _lock = match shared::lock_instance(shared::TRAY_LOCK) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            eprintln!("[tray] already running");
            return 0;
        }
        Err(e) => {
            eprintln!("[tray] {e}");
            return 1;
        }
    };

    let tray = Tray { state: State::NoDaemon, playing: None, gui_children: Vec::new() };
    // At login the panel may not be up yet; wait for it instead of failing.
    let handle = match tray.assume_sni_available(true).spawn() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("[tray] cannot create the tray icon: {e}");
            return 1;
        }
    };

    let data = shared::data_dir();
    let mut playing_id: Option<String> = None;
    let mut playing_name: Option<String> = None;
    while !handle.is_closed() {
        let status = match shared::daemon_cmd(&Cmd::Status) {
            Ok(Resp::Status(s)) => Some(s),
            _ => None,
        };
        let state = State::from_status(status.as_ref());
        let id = status.and_then(|s| s.playing_id);
        if id != playing_id {
            playing_name = id
                .as_ref()
                .and_then(|id| shared::load_videos(&data).0.into_iter().find(|v| &v.id == id).map(|v| v.name));
            playing_id = id;
        }
        let name = playing_name.clone();
        handle.update(move |t| {
            t.reap();
            t.state = state;
            t.playing = name;
        });
        std::thread::sleep(POLL);
    }
    0
}
