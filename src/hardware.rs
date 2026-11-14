use serde::Serialize;
use std::{fs, path::Path, process::Command};

#[derive(Debug, Serialize, Clone)]
pub struct CpuInfo {
    pub vendor: String,
    pub model: String,
    pub family: Option<String>,
    pub model_id: Option<String>,
    pub stepping: Option<String>,
    pub microarchitecture: String,
    pub logical_cpus: usize,
    pub physical_cores: Option<usize>,
    pub sockets: Option<usize>,
    pub flags: Vec<String>,
}
#[derive(Debug, Serialize, Clone)]
pub struct BlockDevice {
    pub name: String, pub kind: String, pub size: String, pub model: String,
    pub transport: String, pub rotational: Option<bool>, pub scheduler: String,
    pub media_type: String,
}
#[derive(Debug, Serialize, Clone)]
pub struct CpuFreqPolicy {
    pub name: String,
    pub affected_cpus: String,
    pub governor: String,
    pub available_governors: Vec<String>,
    pub energy_performance_preference: Option<String>,
    pub min_frequency_mhz: Option<f64>,
    pub max_frequency_mhz: Option<f64>,
    pub current_frequency_mhz: Option<f64>,
}
#[derive(Debug, Serialize, Clone)]
pub struct HardwareProfile {
    pub kernel_release: String, pub architecture: String, pub cpu: CpuInfo,
    pub memory_kib: u64, pub virtualization: String, pub block_devices: Vec<BlockDevice>,
    pub gpus: Vec<String>, pub network_controllers: Vec<String>,
    pub power_governors: Vec<String>, pub cpu_frequency_policies: Vec<CpuFreqPolicy>,
    pub current_cmdline: String,
}
fn read(path: impl AsRef<Path>) -> String { fs::read_to_string(path).unwrap_or_default() }
fn cmd(program: &str, args: &[&str]) -> String {
    Command::new(program).args(args).output().ok().filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).unwrap_or_default()
}
fn cpu_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| line.split_once(':')
        .filter(|(k, _)| k.trim() == key).map(|(_, v)| v.trim().to_owned()))
}
fn microarch(vendor: &str, family: Option<&str>, model: Option<&str>) -> String {
    let (Some(family), Some(model)) = (family, model) else { return "unknown".into() };
    let Ok(family) = family.parse::<u32>() else { return "unknown".into() };
    let model = if let Some(hex) = model.strip_prefix("0x") {
        u32::from_str_radix(hex, 16)
    } else {
        model.parse::<u32>()
    };
    let Ok(model) = model else { return "unknown".into() };
    if vendor == "AuthenticAMD" && family == 0x19 {
        return match model { 0x00..=0x0f => "Zen 3 (Family 19h)".into(), 0x10..=0x1f => "Zen 4 (Family 19h)".into(), 0x60..=0x7f => "Zen 4/5 family (model-dependent)".into(), 0xa0..=0xaf => "AMD Family 19h model 0xA* (microarchitecture requires full CPUID decode)".into(), _ => "AMD Family 19h (Zen 3/4/5 variant; exact CPUID mapping needed)".into() };
    }
    if vendor == "AuthenticAMD" {
        return match family { 0x17 => "Zen / Zen+ (Family 17h)".into(), 0x18 => "AMD Family 18h".into(), 0x1a => "Zen 5 (AMD Family 1Ah)".into(), _ => format!("AMD family {family:#x}") };
    }
    if vendor == "GenuineIntel" {
        return match family { 6 => match model { 0x8e | 0x9e => "Intel Skylake-derived (Kaby/Coffee/Whiskey Lake)".into(), 0x97 | 0x9a => "Intel Alder Lake".into(), 0xb7 | 0xba | 0xbf => "Intel Raptor Lake family".into(), 0xaa | 0xac => "Intel Meteor Lake".into(), 0xc5 | 0xc6 => "Intel Lunar Lake family".into(), _ => format!("Intel family 6 model {model:#x}") }, _ => format!("Intel family {family}") };
    }
    format!("{vendor} family {family} model {model:#x}")
}
fn block_devices() -> Vec<BlockDevice> {
    let json = cmd("lsblk", &["-J", "-b", "-o", "NAME,TYPE,SIZE,MODEL,TRAN,ROTA"]);
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&json) else { return Vec::new() };
    fn walk(v: &serde_json::Value, out: &mut Vec<BlockDevice>) {
        let Some(items) = v.as_array() else { return };
        for item in items {
            let name = item["name"].as_str().unwrap_or("?").to_owned();
            let path = format!("/sys/block/{name}");
            let scheduler = read(format!("{path}/queue/scheduler")).trim().to_owned();
            let kind = item["type"].as_str().unwrap_or("?").to_owned();
            let transport = item["tran"].as_str().unwrap_or("").to_owned();
            let rotational = item["rota"].as_u64().map(|v| v != 0);
            let media_type = if kind != "disk" {
                "partition or virtual volume".to_owned()
            } else if name.starts_with("nvme") || transport.eq_ignore_ascii_case("nvme") {
                "NVMe".to_owned()
            } else if rotational == Some(true) {
                "rotational HDD".to_owned()
            } else if rotational == Some(false) && transport.eq_ignore_ascii_case("sata") {
                "SATA SSD".to_owned()
            } else if rotational == Some(false) {
                "non-rotational block device".to_owned()
            } else {
                "unknown".to_owned()
            };
            out.push(BlockDevice {
                name, kind,
                size: item["size"].as_str().map(str::to_owned).unwrap_or_else(|| item["size"].to_string()),
                model: item["model"].as_str().unwrap_or("").trim().into(),
                transport,
                rotational, scheduler, media_type,
            });
            walk(&item["children"], out);
        }
    }
    let mut out = Vec::new(); walk(&value["blockdevices"], &mut out); out
}
pub fn detect_virtualization() -> String {
    let detected = cmd("systemd-detect-virt", &[]);
    if !detected.is_empty() && detected != "none" {
        return detected;
    }
    if Path::new("/.dockerenv").exists() {
        return "docker".into();
    }
    if Path::new("/run/.containerenv").exists() {
        return "podman".into();
    }
    let cgroup = read("/proc/1/cgroup").to_lowercase();
    for marker in ["kubepods", "docker", "containerd", "libpod", "lxc"] {
        if cgroup.contains(marker) {
            return "container".into();
        }
    }
    let environment = read("/proc/1/environ");
    if environment.contains("container=") {
        return "container".into();
    }
    "none".into()
}
fn memory_kib() -> u64 {
    read("/proc/meminfo").lines().find(|l| l.starts_with("MemTotal:"))
        .and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse().ok()).unwrap_or(0)
}
fn controller_lines(class: &str) -> Vec<String> {
    cmd("lspci", &["-nn"]).lines().filter(|line| line.to_lowercase().contains(class))
        .map(str::to_owned).collect()
}
fn frequency_mhz(path: &Path) -> Option<f64> {
    fs::read_to_string(path).ok()?.trim().parse::<f64>().ok().map(|khz| khz / 1000.0)
}
fn cpu_frequency_policies() -> Vec<CpuFreqPolicy> {
    let mut policies = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/devices/system/cpu/cpufreq") else {
        return policies;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("policy") {
            continue;
        }
        let path = entry.path();
        let available = read(path.join("scaling_available_governors"));
        let epp = read(path.join("energy_performance_preference"));
        policies.push(CpuFreqPolicy {
            name,
            affected_cpus: read(path.join("affected_cpus")).trim().to_owned(),
            governor: read(path.join("scaling_governor")).trim().to_owned(),
            available_governors: available.split_whitespace().map(str::to_owned).collect(),
            energy_performance_preference: if epp.trim().is_empty() { None } else { Some(epp.trim().to_owned()) },
            min_frequency_mhz: frequency_mhz(&path.join("cpuinfo_min_freq")),
            max_frequency_mhz: frequency_mhz(&path.join("cpuinfo_max_freq")),
            current_frequency_mhz: frequency_mhz(&path.join("scaling_cur_freq")),
        });
    }
    policies
}
fn power_governors() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/devices/system/cpu") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("cpu") && name[3..].parse::<usize>().is_ok() {
                let governor = read(entry.path().join("cpufreq/scaling_governor"));
                if !governor.trim().is_empty() && !out.contains(&governor.trim().to_owned()) { out.push(governor.trim().to_owned()); }
            }
        }
    }
    out
}
pub fn detect() -> HardwareProfile {
    let cpuinfo = read("/proc/cpuinfo");
    let first_cpu = cpuinfo.split("\n\n").next().unwrap_or("");
    let vendor = cpu_value(first_cpu, "vendor_id").or_else(|| cpu_value(first_cpu, "CPU implementer")).unwrap_or_else(|| "unknown".into());
    let model = cpu_value(first_cpu, "model name").or_else(|| cpu_value(first_cpu, "Hardware")).unwrap_or_else(|| "Unknown CPU".into());
    let family = cpu_value(first_cpu, "cpu family");
    let model_id = cpu_value(first_cpu, "model");
    let stepping = cpu_value(first_cpu, "stepping");
    let flags = first_cpu.lines().find(|l| l.starts_with("flags") || l.starts_with("Features"))
        .and_then(|l| l.split_once(':')).map(|(_, v)| v.split_whitespace().map(str::to_owned).collect()).unwrap_or_default();
    let logical = std::thread::available_parallelism().map(|v| v.get()).unwrap_or(1);
    let sockets = cpuinfo.lines().filter(|l| l.starts_with("physical id")).filter_map(|l| l.split_once(':').map(|(_,v)|v.trim().to_owned())).collect::<std::collections::HashSet<_>>().len();
    let cores = cpuinfo.lines().filter(|l| l.starts_with("cpu cores")).find_map(|l| l.split_once(':').and_then(|(_,v)|v.trim().parse::<usize>().ok()));
    let virt = detect_virtualization();
    HardwareProfile {
        kernel_release: cmd("uname", &["-r"]), architecture: cmd("uname", &["-m"]),
        cpu: CpuInfo { microarchitecture: microarch(&vendor, family.as_deref(), model_id.as_deref()), vendor, model, family, model_id, stepping, logical_cpus: logical, physical_cores: cores.map(|c| c * sockets.max(1)), sockets: if sockets == 0 { None } else { Some(sockets) }, flags },
        memory_kib: memory_kib(), virtualization: virt,
        block_devices: block_devices(), gpus: controller_lines("vga").into_iter().chain(controller_lines("3d controller")).collect(),
        network_controllers: controller_lines("ethernet").into_iter().chain(controller_lines("network controller")).collect(),
        power_governors: power_governors(), cpu_frequency_policies: cpu_frequency_policies(),
        current_cmdline: read("/proc/cmdline").trim().into(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn classifies_amd_zen_families() {
        assert!(microarch("AuthenticAMD", Some("23"), Some("1")).contains("Zen"));
        assert!(microarch("AuthenticAMD", Some("25"), Some("0x61")).contains("Zen"));
    }
    #[test] fn parses_proc_cpuinfo_decimal_model() {
        assert!(microarch("AuthenticAMD", Some("25"), Some("97")).contains("Zen 4/5"));
    }
    #[test] fn classifies_amd_zen5_family() {
        assert!(microarch("AuthenticAMD", Some("26"), Some("1")).contains("Zen 5"));
    }
    #[test] fn unknown_cpu_is_safe() { assert_eq!(microarch("unknown", None, None), "unknown"); }
}
