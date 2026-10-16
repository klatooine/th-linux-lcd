//! Standalone boot-animation upload tool for protocol debugging.
//!
//! Talks to the device directly, bypassing the daemon — stop it first:
//! `systemctl --user stop th-linux-lcd`.
//!
//! ```text
//! cargo run --example test_boot -- <input.gif>                [--dry-run] [--yes]
//! cargo run --example test_boot -- --binary <file.bootanim>   [--dry-run] [--yes]
//! ```
//!
//! The container is always validated before anything is sent, and the tool
//! asks for confirmation unless `--yes` is given.

use std::io::{BufRead, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use th_linux_lcd::{import, shared, usb};

fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn describe(container: &[u8]) {
    println!("    size     : {} bytes ({:.1} KiB)", container.len(), container.len() as f64 / 1024.0);
    if container.len() < 32 {
        return;
    }
    let timing = container[15];
    println!("    magic    : {:?}", String::from_utf8_lossy(&container[16..31]));
    println!("    checksum : 0x{:08x}", le_u32(&container[0..4]));
    println!("    n_frames : {}", container[14]);
    println!("    tbl_size : {} bytes", le_u32(&container[8..12]));
    println!("    timing   : {timing} ms/frame → {:.1} fps", 1000.0 / f32::from(timing.max(1)));
}

/// Build the container from a GIF inside a fresh private temp directory.
fn build_from_gif(gif: &Path) -> Result<Vec<u8>, String> {
    let dir = std::env::temp_dir().join(format!("th-lcd-test-boot-{}", uuid::Uuid::new_v4()));
    std::fs::DirBuilder::new().mode(0o700).create(&dir).map_err(|e| e.to_string())?;
    let out = dir.join("test.bootanim");
    let result = import::build_boot_from_gif(gif, 0, &out).and_then(|(n, fps, _)| {
        println!("    frames={n}  fps={fps:.1}");
        std::fs::read(&out).map_err(|e| e.to_string())
    });
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn confirm() -> bool {
    print!("\nThis writes to the device's boot flash slot. Type 'yes' to continue: ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).is_ok() && line.trim() == "yes"
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let assume_yes = args.iter().any(|a| a == "--yes");
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with("--") || *a == "--binary").collect();

    let container = match positional.as_slice() {
        [flag, path] if flag.as_str() == "--binary" => {
            println!("[1] Reading container {path}");
            match std::fs::read(path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("    [ERROR] {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        [gif] => {
            println!("[1] Building container from {gif}");
            match build_from_gif(&PathBuf::from(gif)) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("    [ERROR] {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        _ => {
            eprintln!("Usage: test_boot <input.gif | --binary file.bootanim> [--dry-run] [--yes]");
            return ExitCode::FAILURE;
        }
    };

    println!("\n[2] Container");
    describe(&container);
    if let Err(e) = shared::validate_boot_container(&container) {
        eprintln!("    [ERROR] invalid container: {e}");
        return ExitCode::FAILURE;
    }
    println!("    validation: ok");

    if dry_run {
        println!("\n[DRY RUN] Skipping USB upload.");
        return ExitCode::SUCCESS;
    }
    if !assume_yes && !confirm() {
        println!("Aborted.");
        return ExitCode::FAILURE;
    }

    println!("\n[3] Opening USB device {:04x}:{:04x}", usb::VID, usb::PID);
    let dev = match usb::open() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("    [ERROR] {e}");
            return ExitCode::FAILURE;
        }
    };

    println!("\n[4] Uploading");
    if let Err(e) = usb::upload(&dev, usb::Slot::Boot, &container) {
        eprintln!("\n✗ Upload failed (commit not sent): {e}");
        return ExitCode::FAILURE;
    }

    let secs = shared::BOOT_FLASH_SETTLE.as_secs();
    println!("    committed; waiting {secs}s for the flash write. Do not unplug or power off.");
    for left in (1..=secs).rev() {
        print!("\r    {left:>3}s ");
        let _ = std::io::stdout().flush();
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    println!("\r\n✓ Upload complete. Power-cycle the cooler to see the animation.");
    ExitCode::SUCCESS
}
