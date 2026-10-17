//! Host metrics for the LCD overlay: CPU/GPU temperature and load, memory use.
//! CPU and memory come from sysfs/procfs. GPU comes from NVML (NVIDIA, loaded
//! at runtime if the driver is present) or amdgpu sysfs.

use std::fs;
use std::path::PathBuf;

/// Samples host metrics; keeps state between calls for CPU load deltas.
#[derive(Default)]
pub struct Sampler {
    cpu_prev: Option<(u64, u64)>,
    /// `None` = not searched yet; `Some(None)` = no sensor found.
    cpu_temp_path: Option<Option<PathBuf>>,
    nvml: Option<Option<nvml_wrapper::Nvml>>,
    /// amdgpu (busy_percent, temp input) paths.
    amd: Option<Option<(PathBuf, Option<PathBuf>)>>,
}

impl Sampler {
    /// CPU package temperature in °C.
    pub fn cpu_temp(&mut self) -> Option<f32> {
        let path = self.cpu_temp_path.get_or_insert_with(find_cpu_temp_sensor).as_ref()?;
        let milli: f32 = fs::read_to_string(path).ok()?.trim().parse().ok()?;
        Some(milli / 1000.0)
    }

    /// Whole-system CPU load in percent since the previous call (None on the first call).
    pub fn cpu_load(&mut self) -> Option<f32> {
        let stat = fs::read_to_string("/proc/stat").ok()?;
        let fields: Vec<u64> = stat.lines().next()?.split_whitespace().skip(1).filter_map(|v| v.parse().ok()).collect();
        if fields.len() < 4 {
            return None;
        }
        // idle + iowait count as idle time
        let idle = fields[3] + fields.get(4).copied().unwrap_or(0);
        let total: u64 = fields.iter().take(8).sum();
        let prev = self.cpu_prev.replace((idle, total));
        let (pidle, ptotal) = prev?;
        let dt = total.saturating_sub(ptotal);
        if dt == 0 {
            return None;
        }
        let busy = dt.saturating_sub(idle.saturating_sub(pidle));
        Some(busy as f32 * 100.0 / dt as f32)
    }
}

impl Sampler {
    /// GPU (temperature °C, load %) for the first GPU found.
    pub fn gpu(&mut self) -> (Option<f32>, Option<f32>) {
        if let Some(nvml) = self.nvml.get_or_insert_with(|| nvml_wrapper::Nvml::init().ok()) {
            if let Ok(dev) = nvml.device_by_index(0) {
                let temp =
                    dev.temperature(nvml_wrapper::enum_wrappers::device::TemperatureSensor::Gpu).ok().map(|t| t as f32);
                let load = dev.utilization_rates().ok().map(|u| u.gpu as f32);
                if temp.is_some() || load.is_some() {
                    return (temp, load);
                }
            }
        }
        let Some((busy, temp)) = self.amd.get_or_insert_with(find_amdgpu) else { return (None, None) };
        let read = |p: &PathBuf| fs::read_to_string(p).ok()?.trim().parse::<f32>().ok();
        (temp.as_ref().and_then(read).map(|t| t / 1000.0), read(busy))
    }
}

/// First amdgpu card: (gpu_busy_percent, hwmon temp1_input).
fn find_amdgpu() -> Option<(PathBuf, Option<PathBuf>)> {
    for card in fs::read_dir("/sys/class/drm").ok()?.flatten() {
        let dev = card.path().join("device");
        let busy = dev.join("gpu_busy_percent");
        if !busy.exists() {
            continue;
        }
        let temp = fs::read_dir(dev.join("hwmon"))
            .ok()
            .and_then(|mut d| d.next())
            .and_then(|e| e.ok())
            .map(|e| e.path().join("temp1_input"))
            .filter(|p| p.exists());
        return Some((busy, temp));
    }
    None
}

/// Memory in use (MemTotal − MemAvailable) in percent.
pub fn ram_used_percent() -> Option<f32> {
    let info = fs::read_to_string("/proc/meminfo").ok()?;
    let field = |name: &str| -> Option<f32> {
        info.lines().find(|l| l.starts_with(name))?.split_whitespace().nth(1)?.parse().ok()
    };
    let total = field("MemTotal:")?;
    let avail = field("MemAvailable:")?;
    (total > 0.0).then(|| (total - avail) * 100.0 / total)
}

/// Find the hwmon input for the CPU package temperature: Intel `coretemp`
/// "Package id 0", AMD `k10temp`/`zenpower` "Tctl"/"Tdie", then thermal zones.
fn find_cpu_temp_sensor() -> Option<PathBuf> {
    let mut best: Option<(u8, PathBuf)> = None;
    for hw in fs::read_dir("/sys/class/hwmon").ok()?.flatten() {
        let dir = hw.path();
        let name = fs::read_to_string(dir.join("name")).unwrap_or_default();
        let name = name.trim();
        if !matches!(name, "coretemp" | "k10temp" | "zenpower" | "cpu_thermal") {
            continue;
        }
        for i in 1..=64 {
            let input = dir.join(format!("temp{i}_input"));
            if !input.exists() {
                continue;
            }
            let label = fs::read_to_string(dir.join(format!("temp{i}_label"))).unwrap_or_default();
            let rank = match label.trim() {
                "Package id 0" | "Tdie" => 0,
                "Tctl" => 1,
                _ if i == 1 => 2,
                _ => continue,
            };
            if best.as_ref().is_none_or(|(r, _)| rank < *r) {
                best = Some((rank, input));
            }
        }
    }
    if let Some((_, p)) = best {
        return Some(p);
    }
    // Fallback: a thermal zone that looks like the CPU package.
    for tz in fs::read_dir("/sys/class/thermal").ok()?.flatten() {
        let kind = fs::read_to_string(tz.path().join("type")).unwrap_or_default();
        if matches!(kind.trim(), "x86_pkg_temp" | "cpu-thermal" | "cpu_thermal") {
            return Some(tz.path().join("temp"));
        }
    }
    None
}

impl Sampler {
    /// Host-side readings (coolant comes from the device, so it is left empty).
    pub fn sample(&mut self) -> crate::shared::Metrics {
        let (gpu_temp, gpu_load) = self.gpu();
        crate::shared::Metrics {
            cpu_temp: self.cpu_temp(),
            coolant: None,
            gpu_temp,
            cpu_load: self.cpu_load(),
            gpu_load,
            ram: ram_used_percent(),
        }
    }
}
