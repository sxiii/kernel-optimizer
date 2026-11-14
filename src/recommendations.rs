use crate::{hardware::{self, HardwareProfile}, tuning::Workload};
use serde::Serialize;
use std::{fs, path::Path};

#[derive(Debug, Serialize)]
pub struct Recommendation {
    pub category: String,
    pub component: String,
    pub current: String,
    pub suggestion: String,
    pub rationale: String,
    pub tradeoff: String,
    pub action: String,
}

#[derive(Debug, Serialize)]
pub struct RecommendationReport {
    pub profile: String,
    pub hardware: HardwareProfile,
    pub recommendations: Vec<Recommendation>,
}

fn scheduler_names(value: &str) -> Vec<&str> {
    value
        .split_whitespace()
        .map(|name| name.trim_matches(&['[', ']'][..]))
        .collect()
}

fn recommendation(
    category: &str,
    component: &str,
    current: String,
    suggestion: &str,
    rationale: &str,
    tradeoff: &str,
    action: &str,
) -> Recommendation {
    Recommendation {
        category: category.into(),
        component: component.into(),
        current,
        suggestion: suggestion.into(),
        rationale: rationale.into(),
        tradeoff: tradeoff.into(),
        action: action.into(),
    }
}

fn storage_recommendations(hw: &HardwareProfile, out: &mut Vec<Recommendation>) {
    for device in &hw.block_devices {
        if device.kind != "disk" {
            continue;
        }
        let available = scheduler_names(&device.scheduler);
        let current = device
            .scheduler
            .split_whitespace()
            .find(|name| name.starts_with('['))
            .map(|name| name.trim_matches(&['[', ']'][..]))
            .unwrap_or("kernel default");
        let choice = match device.media_type.as_str() {
            "NVMe" if available.contains(&"none") => Some("none"),
            "rotational HDD" if available.contains(&"bfq") => Some("bfq"),
            "rotational HDD" if available.contains(&"mq-deadline") => Some("mq-deadline"),
            "SATA SSD" if available.contains(&"mq-deadline") => Some("mq-deadline"),
            "SATA SSD" if available.contains(&"kyber") => Some("kyber"),
            _ => None,
        };
        if let Some(choice) = choice {
            out.push(recommendation(
                "storage",
                &format!("{} ({})", device.name, device.model),
                current.into(),
                choice,
                match device.media_type.as_str() {
                    "NVMe" => "NVMe devices often benefit from avoiding an additional software queue when the driver exposes the none scheduler.",
                    "rotational HDD" => "BFQ can provide fairer latency and responsiveness on rotational media; mq-deadline is a reasonable fallback.",
                    _ => "mq-deadline or kyber are available multi-queue schedulers for non-rotational SATA storage.",
                },
                "Scheduler results depend on the storage controller, firmware, queue depth and workload. Benchmark database, desktop and sequential workloads separately.",
                "Preview with kernel-optimizer tune --dry-run; changes are generated as per-device udev rules and require explicit root apply.",
            ));
        } else {
            out.push(recommendation(
                "storage",
                &device.name,
                current.into(),
                "Keep the current kernel-selected scheduler",
                "No suitable scheduler was advertised by this device, or its media type could not be identified safely.",
                "Forcing a scheduler not exposed by the device can fail or regress performance.",
                "No action recommended.",
            ));
        }
    }
}

fn cpu_recommendations(hw: &HardwareProfile, profile: Workload, out: &mut Vec<Recommendation>) {
    let microarch = &hw.cpu.microarchitecture;
    out.push(recommendation(
        "cpu",
        &hw.cpu.model,
        format!("{}; {} logical CPUs", microarch, hw.cpu.logical_cpus),
        "Keep the distribution-compatible kernel configuration as the baseline",
        "The detected microarchitecture informs optimization choices, but kernel Kconfig options and compiler support vary by kernel release. CPU flags are not sufficient evidence to disable other CPU or platform support.",
        "Aggressive architecture-specific builds may fail to boot on another CPU, under a rescue environment, or after a hardware change.",
        "Use explicit, release-specific Kconfig choices only after checking the kernel supported CPU options. Do not inject -march=native into a generally bootable kernel.",
    ));

    for policy in &hw.cpu_frequency_policies {
        let preferred = match profile {
            Workload::Laptop | Workload::Desktop => ["schedutil", "powersave"].into_iter().find(|name| policy.available_governors.iter().any(|g| g == name)),
            Workload::Gaming | Workload::Server => ["schedutil", "performance"].into_iter().find(|name| policy.available_governors.iter().any(|g| g == name)),
        };
        let suggestion = preferred.unwrap_or(policy.governor.as_str());
        out.push(recommendation(
            "power",
            &policy.name,
            format!(
                "governor={}, EPP={}, min={:?} MHz, max={:?} MHz, current={:?} MHz",
                policy.governor,
                policy.energy_performance_preference.as_deref().unwrap_or("not exposed"),
                policy.min_frequency_mhz,
                policy.max_frequency_mhz,
                policy.current_frequency_mhz
            ),
            suggestion,
            match profile {
                Workload::Laptop => "A dynamic governor is generally preferable for battery life and interactive responsiveness.",
                Workload::Desktop => "schedutil balances responsiveness with dynamic frequency scaling when the driver supports it.",
                Workload::Gaming => "Benchmark schedutil against performance under the actual game workload; a fixed maximum governor is not universally faster.",
                Workload::Server => "Dynamic frequency scaling usually provides a better efficiency baseline; validate latency-sensitive services before forcing performance.",
            },
            "Governor names and behavior depend on the cpufreq driver, firmware, thermal limits and platform power policy. This report does not change governor settings.",
            "Inspect available governors and compare latency, throughput, power draw and thermals before making a change.",
        ));
    }
}

fn system_recommendations(hw: &HardwareProfile, out: &mut Vec<Recommendation>) {
    let zram_active = fs::read_to_string("/proc/swaps")
        .map(|text| {
            text.lines().skip(1).any(|line| {
                line.split_whitespace()
                    .next()
                    .is_some_and(|device| device.starts_with("/dev/zram"))
            })
        })
        .unwrap_or(false);
    out.push(recommendation(
        "memory",
        "swap",
        if zram_active { "active zram swap".into() } else { "no active zram swap".into() },
        if zram_active { "higher swappiness can suit compressed RAM swap" } else { "use conservative disk-swap defaults unless workload measurements suggest otherwise" },
        "The appropriate reclaim policy depends on whether swap is compressed RAM or persistent storage.",
        "Swappiness values are workload dependent; a value that helps zram can be undesirable with slow disk swap.",
        "Review kernel-optimizer tune --dry-run before applying any sysctl profile.",
    ));

    let mglru = Path::new("/sys/kernel/mm/lru_gen/enabled");
    out.push(recommendation(
        "memory",
        "MGLRU",
        if mglru.exists() {
            fs::read_to_string(mglru).unwrap_or_default().trim().to_owned()
        } else {
            "sysfs interface not present".into()
        },
        "Enable only after confirming kernel support and reviewing the runtime feature bits",
        "MGLRU is a runtime sysfs control, not a sysctl key; persistence and bit semantics differ from ordinary sysctl drop-ins.",
        "Incorrect feature-bit handling or unsupported kernels may reject the write. The optimizer intentionally does not toggle it automatically.",
        "Inspect the kernel documentation and current value; no automatic change is made.",
    ));

    let bbr = fs::read_to_string("/proc/sys/net/ipv4/tcp_available_congestion_control")
        .unwrap_or_default()
        .split_whitespace()
        .any(|name| name == "bbr");
    out.push(recommendation(
        "network",
        "TCP congestion control",
        if bbr { "BBR advertised by running kernel".into() } else { "BBR not advertised".into() },
        if bbr { "consider BBR with fq for throughput-oriented clients or servers" } else { "keep current congestion control" },
        "BBR can improve throughput on some paths when supported by the running kernel; fq is commonly paired with BBR.",
        "Network path, bufferbloat, VPNs and traffic mix matter. Do not infer BBRv2/v3 from the distribution or kernel name.",
        "Only the tune command adds BBR when the running kernel advertises it; compare with the existing congestion control.",
    ));

    if hw.virtualization != "none" {
        out.push(recommendation(
            "platform",
            "virtualization",
            hw.virtualization.clone(),
            "Avoid host-specific device and memory tuning inside this guest/container",
            "The guest may not control the physical device queues or host memory reclaim behavior.",
            "Changing guest settings can conflict with host-level scheduling and may have no effect.",
            "Use hardware-specific tuning on the hypervisor or bare-metal host when appropriate.",
        ));
    }
}

pub fn generate(profile: Workload) -> RecommendationReport {
    generate_from_hardware(hardware::detect(), profile)
}

pub fn generate_from_hardware(hw: HardwareProfile, profile: Workload) -> RecommendationReport {
    let mut recommendations = Vec::new();
    cpu_recommendations(&hw, profile, &mut recommendations);
    storage_recommendations(&hw, &mut recommendations);
    system_recommendations(&hw, &mut recommendations);
    RecommendationReport {
        profile: format!("{profile:?}").to_lowercase(),
        hardware: hw,
        recommendations,
    }
}
