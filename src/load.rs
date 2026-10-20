//! Background sampling of CPU and GPU utilisation.
//!
//! Everything here runs unprivileged: CPU comes from `sysinfo`, NVIDIA GPUs from NVML
//! (loaded dynamically at runtime), and AMD/Intel GPUs on Linux from sysfs.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Latest load readings, as fractions in [0, 1] stored as f32 bits.
/// GPU is `NaN` when no GPU source is available.
#[derive(Default)]
pub struct Load {
    cpu: AtomicU32,
    gpu: AtomicU32,
}

impl Load {
    pub fn cpu(&self) -> f32 {
        f32::from_bits(self.cpu.load(Ordering::Relaxed))
    }

    pub fn gpu(&self) -> Option<f32> {
        let g = f32::from_bits(self.gpu.load(Ordering::Relaxed));
        (!g.is_nan()).then_some(g)
    }

    fn set(&self, cpu: f32, gpu: Option<f32>) {
        self.cpu.store(cpu.to_bits(), Ordering::Relaxed);
        self.gpu.store(gpu.unwrap_or(f32::NAN).to_bits(), Ordering::Relaxed);
    }
}

/// Spawns the sampling thread and returns the shared readings.
pub fn spawn_monitor(interval: Duration) -> Arc<Load> {
    let load = Arc::new(Load::default());
    load.set(0.0, None);
    let shared = Arc::clone(&load);
    thread::spawn(move || {
        let mut sys = sysinfo::System::new();
        let mut gpu = GpuProbe::new();
        sys.refresh_cpu_usage();
        loop {
            thread::sleep(interval.max(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL));
            sys.refresh_cpu_usage();
            let cpu = (sys.global_cpu_usage() / 100.0).clamp(0.0, 1.0);
            shared.set(cpu, gpu.sample());
        }
    });
    load
}

/// Busiest-GPU utilisation from whichever backends are available.
struct GpuProbe {
    #[cfg(feature = "nvidia")]
    nvml: Option<nvml_wrapper::Nvml>,
    sysfs: Vec<std::path::PathBuf>,
}

impl GpuProbe {
    fn new() -> Self {
        Self {
            #[cfg(feature = "nvidia")]
            nvml: nvml_wrapper::Nvml::init()
                .ok()
                .filter(|n| n.device_count().is_ok_and(|c| c > 0)),
            sysfs: sysfs_gpu_files(),
        }
    }

    fn sample(&mut self) -> Option<f32> {
        let mut best: Option<f32> = None;
        let mut push = |v: f32| best = Some(best.map_or(v, |b| b.max(v)));

        #[cfg(feature = "nvidia")]
        if let Some(nvml) = &self.nvml {
            let count = nvml.device_count().unwrap_or(0);
            for i in 0..count {
                if let Ok(rates) = nvml.device_by_index(i).and_then(|d| d.utilization_rates()) {
                    push(rates.gpu as f32 / 100.0);
                }
            }
        }

        for path in &self.sysfs {
            if let Some(v) = std::fs::read_to_string(path)
                .ok()
                .and_then(|s| s.trim().parse::<f32>().ok())
            {
                push(v / 100.0);
            }
        }

        best.map(|v| v.clamp(0.0, 1.0))
    }
}

/// `gpu_busy_percent` files exposed by amdgpu (and some other DRM drivers) on Linux.
fn sysfs_gpu_files() -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };
    let mut files: Vec<_> = entries
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with("card") && !name.contains('-')
        })
        .map(|e| e.path().join("device/gpu_busy_percent"))
        .filter(|p| std::fs::read_to_string(p).is_ok())
        .collect();
    files.sort();
    files
}
