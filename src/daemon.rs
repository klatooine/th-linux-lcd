use crate::shared::{socket_path, Cmd, DaemonStatus, Resp};
use crate::usb;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ── USB thread ────────────────────────────────────────────────────────────────

struct UsbState {
    playing: bool,
    frames: Vec<Vec<u8>>,
    frame_idx: usize,
    fps: f32,
    playing_id: Option<String>,
    next_frame: Instant,
}

impl Default for UsbState {
    fn default() -> Self {
        Self {
            playing: false,
            frames: Vec::new(),
            frame_idx: 0,
            fps: 15.0,
            playing_id: None,
            next_frame: Instant::now(),
        }
    }
}

pub enum UsbCmd {
    Play { frames_dir: String, fps: f32, frame_count: usize, id: String },
    Stop,
    ScreenOn,
    ScreenOff,
    UploadStandby { path: String, done_tx: std::sync::mpsc::SyncSender<Result<(), String>> },
    Quit,
}

fn load_frames(dir: &str, count: usize) -> Vec<Vec<u8>> {
    (1..=count)
        .filter_map(|i| fs::read(format!("{}/f{:05}.jpg", dir, i)).ok())
        .collect()
}

fn usb_thread(
    rx: std::sync::mpsc::Receiver<UsbCmd>,
    status: Arc<Mutex<DaemonStatus>>,
) {
    let mut state = UsbState::default();
    let mut device: Option<rusb::DeviceHandle<rusb::GlobalContext>> = None;

    loop {
        // Try to open device if we don't have one
        if device.is_none() {
            device = usb::open();
            let found = device.is_some();
            if let Ok(mut s) = status.lock() {
                s.device_found = found;
            }
            if device.is_none() {
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        }

        // Drain all pending commands (non-blocking)
        loop {
            match rx.try_recv() {
                Ok(cmd) => match cmd {
                    UsbCmd::Play { frames_dir, fps, frame_count, id } => {
                        state.frames = load_frames(&frames_dir, frame_count);
                        state.fps = fps;
                        state.frame_idx = 0;
                        state.playing = true;
                        state.playing_id = Some(id);
                        state.next_frame = Instant::now();
                        eprintln!("[daemon] play: {} frames @ {}fps", state.frames.len(), fps);
                    }
                    UsbCmd::Stop => {
                        state.playing = false;
                        state.playing_id = None;
                        state.frames.clear();
                        eprintln!("[daemon] stop");
                    }
                    UsbCmd::ScreenOn => {
                        if let Some(ref dev) = device {
                            usb::screen_on(dev);
                        }
                    }
                    UsbCmd::ScreenOff => {
                        if let Some(ref dev) = device {
                            usb::screen_off(dev);
                        }
                    }
                    UsbCmd::UploadStandby { path, done_tx } => {
                        let was_playing = state.playing;
                        state.playing = false;
                        let result = if let Some(ref dev) = device {
                            fs::read(&path)
                                .map_err(|e| e.to_string())
                                .and_then(|data| usb::upload_standby(dev, &data))
                        } else {
                            Err("Device not found".into())
                        };
                        let _ = done_tx.send(result);
                        state.playing = was_playing;
                    }
                    UsbCmd::Quit => {
                        eprintln!("[daemon] quit");
                        return;
                    }
                },
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
            }
        }

        // Update status
        if let Ok(mut s) = status.lock() {
            s.streaming = state.playing;
            s.playing_id = state.playing_id.clone();
            s.fps = state.fps;
        }

        // Send next frame if due
        if state.playing && !state.frames.is_empty() {
            let now = Instant::now();
            if now >= state.next_frame {
                if let Some(ref dev) = device {
                    if !usb::send_frame(dev, &state.frames[state.frame_idx]) {
                        eprintln!("[daemon] device lost during frame send");
                        device = None;
                        if let Ok(mut s) = status.lock() {
                            s.device_found = false;
                        }
                    } else {
                        state.frame_idx = (state.frame_idx + 1) % state.frames.len();
                        state.next_frame = now + Duration::from_secs_f32(1.0 / state.fps);
                    }
                }
            } else {
                // sleep only up to next frame, capped at 5ms so commands stay responsive
                let wait = (state.next_frame - now).min(Duration::from_millis(5));
                std::thread::sleep(wait);
            }
        } else {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

// ── Socket server ─────────────────────────────────────────────────────────────

fn handle_conn(
    stream: UnixStream,
    usb_tx: Arc<std::sync::mpsc::SyncSender<UsbCmd>>,
    status: Arc<Mutex<DaemonStatus>>,
) {
    let mut writer = stream.try_clone().expect("clone stream");
    let reader = BufReader::new(stream);

    for line in reader.lines() {
        let Ok(line) = line else { break };
        let cmd: Cmd = match serde_json::from_str(&line) {
            Ok(c) => c,
            Err(e) => {
                let r = Resp::Err { msg: e.to_string() };
                let _ = writeln!(writer, "{}", serde_json::to_string(&r).unwrap());
                continue;
            }
        };

        let resp = match cmd {
            Cmd::Play { frames_dir, fps, frame_count, id } => {
                let _ = usb_tx.send(UsbCmd::Play { frames_dir, fps, frame_count, id });
                Resp::Ok
            }
            Cmd::Stop => {
                let _ = usb_tx.send(UsbCmd::Stop);
                Resp::Ok
            }
            Cmd::ScreenOn => {
                let _ = usb_tx.send(UsbCmd::ScreenOn);
                Resp::Ok
            }
            Cmd::ScreenOff => {
                let _ = usb_tx.send(UsbCmd::ScreenOff);
                Resp::Ok
            }
            Cmd::UploadStandby { path } => {
                let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
                let _ = usb_tx.send(UsbCmd::UploadStandby { path, done_tx });
                match done_rx.recv_timeout(Duration::from_secs(60)) {
                    Ok(Ok(())) => Resp::Ok,
                    Ok(Err(e)) => Resp::Err { msg: e },
                    Err(_) => Resp::Err { msg: "Upload timed out".into() },
                }
            }
            Cmd::Status => {
                let s = status.lock().map(|s| s.clone()).unwrap_or_default();
                Resp::Status(s)
            }
        };

        let _ = writeln!(writer, "{}", serde_json::to_string(&resp).unwrap());
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run() {
    eprintln!("[daemon] starting (PID {})", std::process::id());

    let sock_path = socket_path();

    // Remove stale socket
    if sock_path.exists() {
        let _ = fs::remove_file(&sock_path);
    }

    let listener = UnixListener::bind(&sock_path).expect("bind socket");
    eprintln!("[daemon] listening on {:?}", sock_path);

    let status: Arc<Mutex<DaemonStatus>> = Arc::new(Mutex::new(DaemonStatus::default()));

    // USB thread
    let (usb_tx, usb_rx) = std::sync::mpsc::sync_channel::<UsbCmd>(32);
    let usb_tx = Arc::new(usb_tx);
    {
        let status = Arc::clone(&status);
        std::thread::spawn(move || usb_thread(usb_rx, status));
    }

    // Cleanup on Ctrl-C
    {
        let sock_path = sock_path.clone();
        let _ = ctrlc::set_handler(move || {
            let _ = fs::remove_file(&sock_path);
            std::process::exit(0);
        });
    }

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let tx = Arc::clone(&usb_tx);
                let st = Arc::clone(&status);
                std::thread::spawn(move || handle_conn(s, tx, st));
            }
            Err(e) => eprintln!("[daemon] accept error: {}", e),
        }
    }
}
