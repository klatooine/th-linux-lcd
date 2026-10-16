//! GIF / image import via python3 + Pillow.
//!
//! The scripts are fixed strings; file paths and numbers are passed as argv,
//! never spliced into the source. GIFs are decoded one frame at a time so a
//! long animation never has to fit in memory.

use crate::shared::{
    self, frame_file, BOOT_FRAMES, BOOT_FRAME_BUDGET, MAX_BOOT_FPS, MAX_STANDBY_BYTES, MAX_STREAM_FRAMES,
    STREAM_FRAME_BUDGET,
};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// argv: src out_dir max_frames budget  →  stdout: {"n": int, "fps": float}
const VIDEO_SCRIPT: &str = r#"
import io, json, os, sys
from PIL import Image
src, out, max_frames, budget = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
os.makedirs(out, exist_ok=True)
img = Image.open(src)
total = getattr(img, 'n_frames', 1)
n = min(total, max_frames)
wanted = set(range(0, total, max(total // n, 1))[:n])
written = duration = 0
for i in range(total):
    img.seek(i)
    duration += img.info.get('duration') or 50
    if i not in wanted:
        continue
    frame = img.convert('RGB').resize((480, 480), Image.LANCZOS)
    q = 25
    while True:
        b = io.BytesIO()
        frame.save(b, 'JPEG', quality=q, subsampling='4:4:4', dpi=(96, 96))
        if len(b.getvalue()) <= budget or q <= 5:
            break
        q -= 5
    written += 1
    with open(os.path.join(out, f'f{written:05}.jpg'), 'wb') as f:
        f.write(b.getvalue())
fps = min(max(1000.0 * total / max(duration, 1), 5.0), 20.0)
print(json.dumps({"n": written, "fps": fps}))
"#;

/// argv: src dest budget  — first frame, 480×480, 4:4:4, written atomically.
const STANDBY_SCRIPT: &str = r#"
import io, os, sys
from PIL import Image
src, dest, budget = sys.argv[1], sys.argv[2], int(sys.argv[3])
img = Image.open(src)
img.seek(0)
img = img.convert('RGB').resize((480, 480), Image.LANCZOS)
q = 85
while True:
    b = io.BytesIO()
    img.save(b, 'JPEG', quality=q, subsampling='4:4:4', dpi=(96, 96))
    if len(b.getvalue()) <= budget:
        break
    if q <= 20:
        sys.exit(f'image is too detailed: {len(b.getvalue())} bytes at quality 20 exceeds {budget}')
    q -= 5
tmp = dest + '.tmp'
with open(tmp, 'wb') as f:
    f.write(b.getvalue())
os.replace(tmp, dest)
"#;

/// argv: src tmp_dir rotation frames budget max_fps  →  {"n": int, "fps": float}
/// Frames are f00000.jpg … (0-based), 4:2:0, padded to `frames` by repeating the last.
const BOOT_SCRIPT: &str = r#"
import io, json, os, shutil, sys
from PIL import Image
src, tmp = sys.argv[1], sys.argv[2]
rotation, target, budget, max_fps = int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5]), float(sys.argv[6])
os.makedirs(tmp, exist_ok=True)
img = Image.open(src)
total = getattr(img, 'n_frames', 1)
n = min(total, target)
wanted = set(range(0, total, max(total // n, 1))[:n])
written = duration = 0
for i in range(total):
    img.seek(i)
    duration += img.info.get('duration') or 80
    if i not in wanted:
        continue
    r = img.convert('RGB').resize((480, 480), Image.LANCZOS)
    if rotation:
        r = r.rotate(-rotation, expand=False)
    q = 75
    while True:
        b = io.BytesIO()
        r.save(b, 'JPEG', quality=q, subsampling=2, dpi=(96, 96))
        if len(b.getvalue()) <= budget:
            break
        if q <= 20:
            sys.exit(f'frame {i} is too detailed: {len(b.getvalue())} bytes at quality 20 exceeds {budget}')
        q -= 5
    with open(os.path.join(tmp, f'f{written:05}.jpg'), 'wb') as f:
        f.write(b.getvalue())
    written += 1
last = os.path.join(tmp, f'f{written - 1:05}.jpg')
while written < target:
    shutil.copyfile(last, os.path.join(tmp, f'f{written:05}.jpg'))
    written += 1
fps = min(max(1000.0 * total / max(duration, 1), 5.0), max_fps)
print(json.dumps({"n": written, "fps": fps}))
"#;

/// argv: src dst count rotation budget  — rotates f00001…fNNNNN.jpg into dst.
const ROTATE_SCRIPT: &str = r#"
import io, os, sys
from PIL import Image
src, dst, count, rot, budget = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5])
os.makedirs(dst, exist_ok=True)
for i in range(1, count + 1):
    p = os.path.join(src, f'f{i:05}.jpg')
    if not os.path.exists(p):
        continue
    img = Image.open(p).rotate(-rot, expand=False)
    q = 25
    while True:
        b = io.BytesIO()
        img.save(b, 'JPEG', quality=q, subsampling='4:4:4', dpi=(96, 96))
        if len(b.getvalue()) <= budget or q <= 5:
            break
        q -= 5
    with open(os.path.join(dst, f'f{i:05}.jpg'), 'wb') as f:
        f.write(b.getvalue())
"#;

fn run_python(script: &str, args: &[&OsStr]) -> Result<String, String> {
    let out = Command::new("python3")
        .arg("-c")
        .arg(script)
        .args(args)
        .output()
        .map_err(|e| format!("could not run python3: {e}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("No module named 'PIL'") {
        return Err("Python Pillow is not installed (python3-pillow / python-pillow)".into());
    }
    // A traceback's last line carries the actual error.
    Err(stderr.trim().lines().last().unwrap_or("python3 failed").to_string())
}

fn parse_n_fps(stdout: &str) -> Result<(usize, f32), String> {
    let v: serde_json::Value = serde_json::from_str(stdout).map_err(|e| format!("unexpected importer output: {e}"))?;
    let n = v["n"].as_u64().ok_or("importer reported no frame count")? as usize;
    let fps = v["fps"].as_f64().unwrap_or(15.0) as f32;
    Ok((n, fps))
}

/// Removes a directory when dropped (temp frame directories on every exit path).
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Extract up to `MAX_STREAM_FRAMES` streaming frames from a GIF into
/// `out_dir`. On failure `out_dir` is removed.
pub fn import_video(src: &Path, out_dir: &Path) -> Result<(usize, f32), String> {
    let result = run_python(
        VIDEO_SCRIPT,
        &[
            src.as_os_str(),
            out_dir.as_os_str(),
            OsStr::new(&MAX_STREAM_FRAMES.to_string()),
            OsStr::new(&STREAM_FRAME_BUDGET.to_string()),
        ],
    )
    .and_then(|s| parse_n_fps(&s))
    .and_then(|(n, fps)| if n == 0 { Err("GIF has no frames".into()) } else { Ok((n, fps)) });
    if result.is_err() {
        let _ = fs::remove_dir_all(out_dir);
    }
    result
}

/// Convert any image (first frame) to a 480×480 standby JPEG at `dest`.
pub fn import_standby(src: &Path, dest: &Path) -> Result<(), String> {
    run_python(STANDBY_SCRIPT, &[src.as_os_str(), dest.as_os_str(), OsStr::new(&MAX_STANDBY_BYTES.to_string())])?;
    let data = fs::read(dest).map_err(|e| e.to_string())?;
    shared::validate_standby_jpeg(&data).inspect_err(|_| {
        let _ = fs::remove_file(dest);
    })
}

/// Build a validated boot container from a GIF and write it atomically to
/// `out_path`. Returns (frames, fps, base64 thumbnail of the first frame).
pub fn build_boot_from_gif(src: &Path, rotation: i32, out_path: &Path) -> Result<(usize, f32, String), String> {
    let mut tmp_name = out_path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".frames");
    let tmp = TempDir(out_path.with_file_name(tmp_name));
    let _ = fs::remove_dir_all(&tmp.0);

    let stdout = run_python(
        BOOT_SCRIPT,
        &[
            src.as_os_str(),
            tmp.0.as_os_str(),
            OsStr::new(&rotation.rem_euclid(360).to_string()),
            OsStr::new(&BOOT_FRAMES.to_string()),
            OsStr::new(&BOOT_FRAME_BUDGET.to_string()),
            OsStr::new(&MAX_BOOT_FPS.to_string()),
        ],
    )?;
    let (n, fps) = parse_n_fps(&stdout)?;

    let frames = (0..n)
        .map(|i| fs::read(tmp.0.join(format!("f{i:05}.jpg"))).map_err(|e| e.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    let container = shared::build_boot_container(&frames, fps)?;
    shared::validate_boot_container(&container)?;
    shared::write_atomic(out_path, &container)?;
    let thumb = frames.first().map(|f| shared::thumbnail_from_bytes(f)).unwrap_or_default();
    Ok((n, fps, thumb))
}

/// Rotate streaming frames from `src_dir` into `dst_dir` (replaced atomically).
/// Reuses `dst_dir` if it already holds `count` frames.
pub fn rotate_frames(src_dir: &Path, dst_dir: &Path, count: usize, rotation: i32) -> Result<(), String> {
    if (1..=count).all(|i| frame_file(dst_dir, i).exists()) {
        return Ok(());
    }
    let mut tmp_name = dst_dir.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = TempDir(dst_dir.with_file_name(tmp_name));
    let _ = fs::remove_dir_all(&tmp.0);
    run_python(
        ROTATE_SCRIPT,
        &[
            src_dir.as_os_str(),
            tmp.0.as_os_str(),
            OsStr::new(&count.to_string()),
            OsStr::new(&rotation.rem_euclid(360).to_string()),
            OsStr::new(&STREAM_FRAME_BUDGET.to_string()),
        ],
    )?;
    let _ = fs::remove_dir_all(dst_dir);
    fs::rename(&tmp.0, dst_dir).map_err(|e| e.to_string())
}
