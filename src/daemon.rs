//! Background daemon: owns the USB device, streams frames, performs uploads,
//! and serves newline-delimited JSON commands on a Unix socket.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use th_linux_lcd::metrics::Sampler;
use th_linux_lcd::overlay::{self, Layer};
use th_linux_lcd::shared::{self, Cmd, DaemonStatus, Metrics, OverlaySettings, Resp};
use th_linux_lcd::usb;

const DEVICE_RETRY: Duration = Duration::from_secs(2);
const PRESENCE_CHECK: Duration = Duration::from_secs(5);
const MAX_REQUEST_BYTES: u64 = 64 * 1024;
const MAX_CONNECTIONS: usize = 16;
const CONN_READ_TIMEOUT: Duration = Duration::from_secs(10);
const QUICK_REPLY_TIMEOUT: Duration = Duration::from_secs(10);
/// An upload that could not start within this time is dropped rather than
/// written to flash long after the user asked for it.
const UPLOAD_START_DEADLINE: Duration = Duration::from_secs(10);
const QUEUE_DEPTH: usize = 32;
/// How often sensors (coolant, CPU, GPU, RAM) are sampled.
const METRICS_POLL: Duration = Duration::from_secs(2);

static QUIT: AtomicBool = AtomicBool::new(false);

type Reply = SyncSender<Result<(), String>>;

enum UsbCmd {
    Play {
        frames: Vec<Vec<u8>>,
        fps: f32,
        id: String,
    },
    Stop,
    Screen(bool),
    SetOverlay(OverlaySettings),
    Upload {
        slot: usb::Slot,
        data: Vec<u8>,
        deadline: Instant,
    },
    /// libusb hotplug event for our VID:PID (true = plugged in).
    Hotplug(bool),
    Quit,
}

type Job = (UsbCmd, Option<Reply>);

/// Forwards libusb hotplug events to the USB thread so plugging / unplugging
/// is noticed immediately instead of at the next poll.
struct HotplugWaker(SyncSender<Job>);

impl rusb::Hotplug<rusb::GlobalContext> for HotplugWaker {
    fn device_arrived(&mut self, _device: rusb::Device<rusb::GlobalContext>) {
        let _ = self.0.try_send((UsbCmd::Hotplug(true), None));
    }

    fn device_left(&mut self, _device: rusb::Device<rusb::GlobalContext>) {
        let _ = self.0.try_send((UsbCmd::Hotplug(false), None));
    }
}

/// Register for hotplug events and run the libusb event loop on its own
/// thread. Without hotplug support the USB thread's polling still works.
fn start_hotplug(tx: SyncSender<Job>) {
    if !rusb::has_hotplug() {
        eprintln!("[usb] libusb has no hotplug support; polling for the device instead");
        return;
    }
    let ctx = rusb::GlobalContext::default();
    let registration = rusb::HotplugBuilder::new()
        .vendor_id(usb::VID)
        .product_id(usb::PID)
        .enumerate(false)
        .register(ctx, Box::new(HotplugWaker(tx)));
    let registration = match registration {
        Ok(r) => r,
        Err(e) => return eprintln!("[usb] hotplug registration failed ({e}); polling instead"),
    };
    let spawned = std::thread::Builder::new().name("hotplug".into()).spawn(move || {
        let _registration = registration;
        loop {
            if let Err(e) = rusb::UsbContext::handle_events(&ctx, None) {
                eprintln!("[usb] hotplug event loop: {e}");
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    });
    if let Err(e) = spawned {
        eprintln!("[usb] cannot start hotplug thread: {e}");
    }
}

/// Read a regular file, refusing anything larger than `max` bytes.
fn read_limited(path: &Path, max: usize) -> Result<Vec<u8>, String> {
    let f = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let meta = f.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if meta.len() > max as u64 {
        return Err(format!("{} is {} bytes; the limit is {max}", path.display(), meta.len()));
    }
    let mut buf = Vec::with_capacity(meta.len() as usize);
    f.take(max as u64 + 1).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    if buf.len() > max {
        return Err(format!("{} grew past the {max}-byte limit", path.display()));
    }
    Ok(buf)
}

fn load_frames(dir: &Path, count: usize) -> Vec<Vec<u8>> {
    (1..=count.min(shared::MAX_STREAM_FRAMES))
        .filter_map(|i| read_limited(&shared::frame_file(dir, i), usb::MAX_FRAME_BYTES).ok())
        .filter(|f| !f.is_empty())
        .collect()
}

// ── USB thread ────────────────────────────────────────────────────────────────

struct Stream {
    frames: Vec<Vec<u8>>,
    idx: usize,
    period: Duration,
    next: Instant,
    fps: f32,
    id: String,
}

impl Stream {
    fn new(frames: Vec<Vec<u8>>, fps: f32, id: String) -> Self {
        Self { frames, idx: 0, period: Duration::from_secs_f32(1.0 / fps), next: Instant::now(), fps, id }
    }
}

struct FlashSettle {
    until: Instant,
    reply: Option<Reply>,
}

/// Overlay rendered for the current text, plus each stream frame with the
/// overlay applied (filled lazily, one frame at a time, as frames are sent).
#[derive(Default)]
struct OverlayCache {
    key: String,
    layer: Option<Layer>,
    frames: Vec<Option<Vec<u8>>>,
    quality: u8,
    error_logged: bool,
}

impl OverlayCache {
    fn reset_frames(&mut self, n: usize) {
        self.frames = vec![None; n];
    }
}

struct UsbThread {
    rx: Receiver<Job>,
    status: Arc<Mutex<DaemonStatus>>,
    published: DaemonStatus,
    device: Option<usb::Handle>,
    last_open_attempt: Option<Instant>,
    last_presence_check: Instant,
    last_open_error: Option<String>,
    stream: Option<Stream>,
    autoplay_checked: bool,
    settle: Option<FlashSettle>,
    quitting: bool,
    sampler: Sampler,
    metrics: Metrics,
    next_metrics: Instant,
    coolant_probed: bool,
    overlay: OverlaySettings,
    cache: OverlayCache,
}

impl UsbThread {
    fn new(rx: Receiver<Job>, status: Arc<Mutex<DaemonStatus>>) -> Self {
        Self {
            rx,
            status,
            published: DaemonStatus::default(),
            device: None,
            last_open_attempt: None,
            last_presence_check: Instant::now(),
            last_open_error: None,
            stream: None,
            autoplay_checked: false,
            settle: None,
            quitting: false,
            sampler: Sampler::default(),
            metrics: Metrics::default(),
            next_metrics: Instant::now(),
            coolant_probed: false,
            overlay: shared::load_overlay_settings(&shared::data_dir()),
            cache: OverlayCache { quality: overlay::initial_quality(), ..Default::default() },
        }
    }

    fn run(mut self) {
        loop {
            let now = Instant::now();
            self.ensure_device(now);
            self.finish_settle(now);
            self.poll_metrics(now);
            if QUIT.load(Ordering::Relaxed) {
                self.quitting = true;
            }
            if self.quitting && self.settle.is_none() {
                break;
            }
            self.publish_status();

            match self.rx.recv_timeout(self.next_wakeup(now)) {
                Ok((cmd, reply)) => self.handle(cmd, reply),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    self.quitting = true;
                    if let Some(s) = &self.settle {
                        std::thread::sleep(s.until.saturating_duration_since(Instant::now()));
                    }
                }
            }

            self.send_due_frame();
        }
        if self.settle.is_none() {
            eprintln!("[daemon] USB thread stopped");
        }
        // Dropping the handle releases the interfaces and re-attaches the kernel driver.
    }

    fn next_wakeup(&self, now: Instant) -> Duration {
        let mut wait = DEVICE_RETRY;
        if self.device.is_none() {
            if let Some(t) = self.last_open_attempt {
                wait = (t + DEVICE_RETRY).saturating_duration_since(now);
            }
        } else if let Some(s) = &self.settle {
            wait = wait.min(s.until.saturating_duration_since(now));
        } else if let Some(s) = &self.stream {
            wait = wait.min(s.next.saturating_duration_since(now));
        }
        wait.min(self.next_metrics.saturating_duration_since(now))
    }

    /// Sample host sensors and the pump's coolant temperature.
    fn poll_metrics(&mut self, now: Instant) {
        if now < self.next_metrics {
            return;
        }
        self.next_metrics = now + METRICS_POLL;
        let mut m = self.sampler.sample();
        if self.device.is_some() && self.settle.is_none() {
            let probe = !self.coolant_probed;
            match self.with_device(usb::read_coolant_temp) {
                Ok((temp, head)) => {
                    if probe {
                        // One-off diagnostics so the byte layout can be checked on other units.
                        eprintln!("[usb] status poll 0x82 reply: {head:02x?} -> coolant {temp:?} °C");
                        self.coolant_probed = true;
                    }
                    m.coolant = temp;
                }
                Err(e) => {
                    if probe {
                        eprintln!("[usb] coolant temperature unavailable: {e}");
                        self.coolant_probed = true;
                    }
                }
            }
        }
        self.metrics = m;
        self.refresh_overlay();
    }

    /// Re-render the overlay layer if the text or settings changed.
    fn refresh_overlay(&mut self) {
        let key = if self.overlay.enabled {
            overlay::key(&overlay::rows(&self.metrics, &self.overlay), self.overlay.position)
        } else {
            String::new()
        };
        if key == self.cache.key {
            return;
        }
        self.cache.layer = if self.overlay.enabled {
            Layer::render(&overlay::rows(&self.metrics, &self.overlay), self.overlay.position)
        } else {
            None
        };
        self.cache.key = key;
        self.cache.reset_frames(self.stream.as_ref().map_or(0, |s| s.frames.len()));
    }

    fn ensure_device(&mut self, now: Instant) {
        if let Some(dev) = &self.device {
            let idle = self.stream.is_none() && self.settle.is_none();
            if idle && now.duration_since(self.last_presence_check) >= PRESENCE_CHECK {
                self.last_presence_check = now;
                if !usb::still_connected(dev) {
                    eprintln!("[usb] device disconnected");
                    self.device = None;
                }
            }
            return;
        }
        if self.last_open_attempt.is_some_and(|t| now.duration_since(t) < DEVICE_RETRY) {
            return;
        }
        self.last_open_attempt = Some(now);
        match usb::open() {
            Ok(dev) => {
                eprintln!("[usb] device opened");
                self.device = Some(dev);
                self.coolant_probed = false;
                self.last_open_error = None;
                self.last_presence_check = now;
                if let Some(s) = &mut self.stream {
                    s.next = now;
                }
                self.restore_autoplay();
            }
            Err(e) => {
                // Log only on change, not every retry.
                if self.last_open_error.as_deref() != Some(e.as_str()) {
                    eprintln!("[usb] {e}");
                    self.last_open_error = Some(e);
                }
            }
        }
    }

    /// On the first successful open, resume the last video that was playing.
    fn restore_autoplay(&mut self) {
        if self.autoplay_checked || self.stream.is_some() {
            return;
        }
        self.autoplay_checked = true;
        let data = shared::data_dir();
        let Some(id) = shared::load_autoplay(&data) else { return };
        let (videos, _) = shared::load_videos(&data);
        let Some(v) = videos.iter().find(|v| v.id == id) else { return };
        let dir =
            if v.rotation != 0 { shared::rotated_frames_dir(&data, &v.id) } else { shared::frames_dir(&data, &v.id) };
        let frames = load_frames(&dir, v.frame_count);
        let fps = shared::sanitize_fps(v.fps).unwrap_or(15.0);
        if !frames.is_empty() {
            eprintln!("[daemon] autoplay: {} ({} frames @ {fps} fps)", v.name, frames.len());
            self.cache.reset_frames(frames.len());
            self.stream = Some(Stream::new(frames, fps, v.id.clone()));
        }
    }

    fn finish_settle(&mut self, now: Instant) {
        if self.settle.as_ref().is_some_and(|s| now >= s.until) {
            let s = self.settle.take().unwrap();
            eprintln!("[daemon] boot animation flash write complete");
            if let Some(reply) = s.reply {
                let _ = reply.send(Ok(()));
            }
            if let Some(st) = &mut self.stream {
                st.next = now;
            }
        }
    }

    fn with_device<T>(&mut self, f: impl FnOnce(&usb::Handle) -> usb::Result<T>) -> Result<T, String> {
        let Some(dev) = &self.device else { return Err("Device not found".into()) };
        f(dev).map_err(|e| {
            if e.device_gone {
                eprintln!("[usb] device lost: {e}");
                self.device = None;
            }
            e.msg
        })
    }

    fn handle(&mut self, cmd: UsbCmd, mut reply: Option<Reply>) {
        let data = shared::data_dir();
        let result = match cmd {
            UsbCmd::Play { frames, fps, id } => {
                eprintln!("[daemon] play: {} frames @ {fps} fps", frames.len());
                if let Err(e) = shared::save_autoplay(&data, Some(&id)) {
                    eprintln!("[daemon] autoplay not saved: {e}");
                }
                self.cache.reset_frames(frames.len());
                self.stream = Some(Stream::new(frames, fps, id));
                Ok(())
            }
            UsbCmd::Stop => {
                eprintln!("[daemon] stop");
                self.stream = None;
                if let Err(e) = shared::save_autoplay(&data, None) {
                    eprintln!("[daemon] autoplay not cleared: {e}");
                }
                Ok(())
            }
            UsbCmd::Screen(on) => {
                if self.settle.is_some() {
                    Err("The device is writing flash; try again in a few seconds".into())
                } else {
                    self.with_device(|d| usb::set_screen(d, on))
                }
            }
            UsbCmd::SetOverlay(settings) => {
                self.overlay = settings;
                self.refresh_overlay();
                shared::save_overlay_settings(&data, &settings)
            }
            UsbCmd::Upload { slot, data, deadline } => {
                if Instant::now() > deadline {
                    Err("Upload was queued too long and has been cancelled; try again".into())
                } else if self.settle.is_some() {
                    Err("The device is still writing the previous upload to flash; try again shortly".into())
                } else {
                    eprintln!("[daemon] upload {slot:?}: {} bytes", data.len());
                    match self.with_device(|d| usb::upload(d, slot, &data)) {
                        Ok(()) if slot == usb::Slot::Boot => {
                            eprintln!("[daemon] boot data committed; waiting for flash write");
                            self.settle = Some(FlashSettle {
                                until: Instant::now() + shared::BOOT_FLASH_SETTLE,
                                reply: reply.take(),
                            });
                            Ok(())
                        }
                        r => {
                            if let Err(e) = &r {
                                eprintln!("[daemon] upload failed: {e}");
                            }
                            r
                        }
                    }
                }
            }
            UsbCmd::Hotplug(true) => {
                if self.device.is_none() {
                    eprintln!("[usb] device plugged in");
                    self.last_open_attempt = None; // open on the next loop iteration
                }
                Ok(())
            }
            UsbCmd::Hotplug(false) => {
                if self.device.as_ref().is_some_and(|d| !usb::still_connected(d)) {
                    eprintln!("[usb] device unplugged");
                    self.device = None;
                }
                Ok(())
            }
            UsbCmd::Quit => {
                eprintln!("[daemon] shutting down");
                self.quitting = true;
                Ok(())
            }
        };
        if let Some(r) = reply {
            let _ = r.send(result);
        }
    }

    fn send_due_frame(&mut self) {
        if self.settle.is_some() {
            return;
        }
        let (Some(dev), Some(s)) = (&self.device, &mut self.stream) else { return };
        if s.frames.is_empty() {
            return;
        }
        let now = Instant::now();
        if now < s.next {
            return;
        }
        // With the overlay on, send the frame with stats drawn in (rendered once per text change).
        let OverlayCache { layer, frames, quality, error_logged, .. } = &mut self.cache;
        if let (Some(layer), Some(slot @ None)) = (layer.as_ref(), frames.get_mut(s.idx)) {
            *slot = Some(layer.apply_to_jpeg(&s.frames[s.idx], quality).unwrap_or_else(|e| {
                if !*error_logged {
                    eprintln!("[daemon] overlay skipped for a frame: {e}");
                    *error_logged = true;
                }
                s.frames[s.idx].clone()
            }));
        }
        let jpeg = match (layer, frames.get(s.idx)) {
            (Some(_), Some(Some(rendered))) => rendered.as_slice(),
            _ => s.frames[s.idx].as_slice(),
        };
        match usb::send_frame(dev, jpeg) {
            Ok(()) => {
                s.idx = (s.idx + 1) % s.frames.len();
                s.next += s.period;
                if s.next < now {
                    s.next = now + s.period; // fell behind; don't burst to catch up
                }
            }
            Err(e) => {
                eprintln!("[daemon] frame send failed ({e}); reopening device");
                self.device = None;
            }
        }
    }

    fn publish_status(&mut self) {
        let s = DaemonStatus {
            streaming: self.stream.is_some(),
            playing_id: self.stream.as_ref().map(|s| s.id.clone()),
            device_found: self.device.is_some(),
            fps: self.stream.as_ref().map_or(0.0, |s| s.fps),
            flash_busy: self.settle.is_some(),
            metrics: self.metrics,
            overlay: self.overlay,
        };
        if s != self.published {
            if let Ok(mut st) = self.status.lock() {
                *st = s.clone();
            }
            self.published = s;
        }
    }
}

// ── Socket server ─────────────────────────────────────────────────────────────

fn dispatch(cmd: Cmd, tx: &SyncSender<Job>, status: &Mutex<DaemonStatus>) -> Result<Resp, String> {
    let (job, timeout) = match cmd {
        Cmd::Status => return Ok(Resp::Status(status.lock().map(|s| s.clone()).unwrap_or_default())),
        Cmd::Play { frames_dir, fps, frame_count, id } => {
            if !shared::is_valid_id(&id) {
                return Err("invalid id".into());
            }
            let fps = shared::sanitize_fps(fps).ok_or("invalid fps")?;
            if frame_count == 0 || frame_count > shared::MAX_STREAM_FRAMES {
                return Err(format!("frame_count must be 1..={}", shared::MAX_STREAM_FRAMES));
            }
            let frames = load_frames(Path::new(&frames_dir), frame_count);
            if frames.is_empty() {
                return Err(format!("no frames found in {frames_dir}"));
            }
            (UsbCmd::Play { frames, fps, id }, QUICK_REPLY_TIMEOUT)
        }
        Cmd::Stop => (UsbCmd::Stop, QUICK_REPLY_TIMEOUT),
        Cmd::ScreenOn => (UsbCmd::Screen(true), QUICK_REPLY_TIMEOUT),
        Cmd::ScreenOff => (UsbCmd::Screen(false), QUICK_REPLY_TIMEOUT),
        Cmd::SetOverlay { settings } => (UsbCmd::SetOverlay(settings), QUICK_REPLY_TIMEOUT),
        Cmd::UploadStandby { path } => {
            let data = read_limited(Path::new(&path), shared::MAX_STANDBY_BYTES)?;
            shared::validate_standby_jpeg(&data)?;
            let deadline = Instant::now() + UPLOAD_START_DEADLINE;
            (UsbCmd::Upload { slot: usb::Slot::Standby, data, deadline }, shared::DAEMON_UPLOAD_TIMEOUT)
        }
        Cmd::UploadBoot { path } => {
            let data = read_limited(Path::new(&path), shared::MAX_BOOT_BYTES)?;
            shared::validate_boot_container(&data)?;
            let deadline = Instant::now() + UPLOAD_START_DEADLINE;
            (UsbCmd::Upload { slot: usb::Slot::Boot, data, deadline }, shared::DAEMON_UPLOAD_TIMEOUT)
        }
    };

    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    match tx.try_send((job, Some(reply_tx))) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => return Err("Daemon is busy; try again".into()),
        Err(TrySendError::Disconnected(_)) => return Err("Daemon is shutting down".into()),
    }
    match reply_rx.recv_timeout(timeout) {
        Ok(Ok(())) => Ok(Resp::Ok),
        Ok(Err(msg)) => Err(msg),
        Err(_) => Err("Timed out waiting for the device".into()),
    }
}

fn handle_conn(stream: UnixStream, tx: SyncSender<Job>, status: Arc<Mutex<DaemonStatus>>) {
    let _ = stream.set_read_timeout(Some(CONN_READ_TIMEOUT));
    let Ok(mut writer) = stream.try_clone() else { return };
    let reader = BufReader::new(stream.take(MAX_REQUEST_BYTES));

    for line in reader.lines() {
        let Ok(line) = line else { break };
        let resp = match serde_json::from_str::<Cmd>(&line) {
            Ok(cmd) => dispatch(cmd, &tx, &status).unwrap_or_else(|msg| Resp::Err { msg }),
            Err(e) => Resp::Err { msg: format!("bad request: {e}") },
        };
        let Ok(json) = serde_json::to_string(&resp) else { break };
        if writeln!(writer, "{json}").is_err() {
            break;
        }
    }
}

struct ConnSlot(Arc<AtomicUsize>);

impl Drop for ConnSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

fn accept_loop(listener: UnixListener, tx: SyncSender<Job>, status: Arc<Mutex<DaemonStatus>>) {
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let s = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[daemon] accept error: {e}");
                continue;
            }
        };
        if active.fetch_add(1, Ordering::Relaxed) >= MAX_CONNECTIONS {
            active.fetch_sub(1, Ordering::Relaxed);
            continue; // dropping `s` closes it
        }
        let slot = ConnSlot(Arc::clone(&active));
        let (tx, st) = (tx.clone(), Arc::clone(&status));
        std::thread::spawn(move || {
            let _slot = slot;
            handle_conn(s, tx, st);
        });
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Runs the daemon; returns the process exit code.
pub fn run() -> i32 {
    eprintln!("[daemon] starting (PID {})", std::process::id());

    // Single instance: hold an exclusive lock for the lifetime of the process.
    let lock = match shared::lock_instance(shared::DAEMON_LOCK) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            eprintln!("[daemon] another daemon is already running");
            return 0;
        }
        Err(e) => {
            eprintln!("[daemon] {e}");
            return 1;
        }
    };

    // We hold the lock, so any existing socket is stale.
    let sock_path = shared::socket_path();
    let _ = fs::remove_file(&sock_path);
    let listener = match UnixListener::bind(&sock_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[daemon] cannot bind {}: {e}", sock_path.display());
            return 1;
        }
    };
    let _ = fs::set_permissions(&sock_path, fs::Permissions::from_mode(0o600));
    eprintln!("[daemon] listening on {}", sock_path.display());

    let status = Arc::new(Mutex::new(DaemonStatus::default()));
    let (tx, rx) = mpsc::sync_channel::<Job>(QUEUE_DEPTH);

    let usb_thread = {
        let status = Arc::clone(&status);
        match std::thread::Builder::new().name("usb".into()).spawn(move || UsbThread::new(rx, status).run()) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("[daemon] cannot start USB thread: {e}");
                return 1;
            }
        }
    };

    // SIGINT/SIGTERM/SIGHUP: let the USB thread finish any upload in progress
    // (including the flash settle time) before exiting.
    {
        let tx = tx.clone();
        if let Err(e) = ctrlc::set_handler(move || {
            QUIT.store(true, Ordering::Relaxed);
            let _ = tx.try_send((UsbCmd::Quit, None));
        }) {
            eprintln!("[daemon] cannot install signal handler: {e}");
        }
    }

    start_hotplug(tx.clone());

    {
        let status = Arc::clone(&status);
        std::thread::spawn(move || accept_loop(listener, tx, status));
    }

    let code = match usb_thread.join() {
        Ok(()) => 0,
        Err(_) => {
            eprintln!("[daemon] USB thread panicked");
            1
        }
    };
    let _ = fs::remove_file(&sock_path);
    drop(lock);
    code
}
