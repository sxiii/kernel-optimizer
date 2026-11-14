use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

const SYSCTL_PATH: &str = "/etc/sysctl.d/99-kernel-optimizer.conf";
const UDEV_PATH: &str = "/etc/udev/rules.d/60-kernel-optimizer-io-scheduler.rules";

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Workload {
    Desktop,
    Laptop,
    Server,
    Gaming,
}

impl Workload {
    fn label(self) -> &'static str {
        match self { Self::Desktop => "desktop", Self::Laptop => "laptop", Self::Server => "server", Self::Gaming => "gaming" }
    }
}

fn read(path: &str) -> String { fs::read_to_string(path).unwrap_or_default() }
fn command_output(program: &str, args: &[&str]) -> String {
    Command::new(program).args(args).output().ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).unwrap_or_default()
}
fn is_root() -> bool { unsafe { libc::geteuid() == 0 } }

fn detect_zram() -> bool {
    Path::new("/sys/block").read_dir().ok().into_iter().flatten().flatten()
        .any(|e| e.file_name().to_string_lossy().starts_with("zram"))
}
fn detect_virtualization() -> String {
    let detected = command_output("systemd-detect-virt", &[]);
    if detected.is_empty() { "none".into() } else { detected }
}
fn mem_total_kib() -> u64 {
    read("/proc/meminfo").lines().find(|l| l.starts_with("MemTotal:"))
        .and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse().ok()).unwrap_or(0)
}
fn scheduler_available(name: &str) -> bool {
    Path::new("/sys/block").read_dir().ok().into_iter().flatten().flatten().any(|e| {
        fs::read_to_string(e.path().join("queue/scheduler")).unwrap_or_default()
            .split_whitespace().any(|s| s.trim_matches(&['[', ']'][..]) == name)
    })
}

fn render_sysctl(profile: Workload) -> (String, Vec<String>) {
    let zram = detect_zram();
    let virt = detect_virtualization();
    let mem_kib = mem_total_kib();
    let min_free = ((mem_kib / 100).clamp(64 * 1024, 512 * 1024)).min(u64::MAX);
    let mut notes = vec![
        format!("Workload profile: {}", profile.label()),
        format!("Memory: {:.1} GiB", mem_kib as f64 / 1024.0 / 1024.0),
        format!("ZRAM detected: {zram}"),
        format!("Virtualization: {virt}"),
    ];

    let mut out = String::from("# Managed by kernel-optimizer. Changes are safe to regenerate/revert.\n");
    out.push_str("# Values are workload-aware suggestions, not universal performance guarantees.\n\n");

    // Swappiness is workload dependent: compressed RAM swap should be used more readily.
    if zram {
        out.push_str("# ZRAM: encourage reclaim into compressed RAM; disk-swap tuning is inappropriate here.\n");
        out.push_str("vm.swappiness = 150\nvm.page-cluster = 0\n");
    } else {
        out.push_str("# Disk swap: conservative swappiness avoids unnecessary paging on typical desktop/server systems.\n");
        let swappiness = match profile { Workload::Server => 10, Workload::Laptop => 30, Workload::Desktop => 20, Workload::Gaming => 10 };
        out.push_str(&format!("vm.swappiness = {swappiness}\n"));
    }

    // A bounded reserve avoids tiny reserves on large-memory machines and huge reserves on small systems.
    if mem_kib > 0 && virt == "none" {
        out.push_str("\n# Reserve roughly 1% of RAM for atomic allocations, bounded to 64-512 MiB.\n");
        out.push_str(&format!("vm.min_free_kbytes = {min_free}\n"));
    } else if virt != "none" {
        notes.push("Virtualization/container detected: skipped host memory-reserve tuning.".into());
    }

    // Only request BBR if the running kernel advertises it; never modprobe or assume BBRv2/v3.
    let available_cc = read("/proc/sys/net/ipv4/tcp_available_congestion_control");
    if available_cc.split_whitespace().any(|v| v == "bbr") {
        out.push_str("\n# BBR is advertised by this running kernel. BBR variant is kernel/build dependent.\n");
        out.push_str("net.core.default_qdisc = fq\nnet.ipv4.tcp_congestion_control = bbr\n");
    } else {
        notes.push("BBR not listed in tcp_available_congestion_control; leaving TCP congestion control unchanged.".into());
    }

    // Keep networking conservative for laptops and latency-sensitive gaming.
    if matches!(profile, Workload::Server) {
        out.push_str("\n# Server profile: moderate listen backlog for connection-heavy services.\n");
        out.push_str("net.core.somaxconn = 4096\n");
    }
    if matches!(profile, Workload::Laptop | Workload::Gaming) {
        notes.push("No aggressive network buffer or CPU power tweaks applied for latency/battery safety.".into());
    }

    // MGLRU is a runtime sysfs interface, not a sysctl. Report availability rather than
    // pretending a sysctl drop-in can persistently configure it.
    let mglru = Path::new("/sys/kernel/mm/lru_gen/enabled");
    if mglru.exists() {
        notes.push(format!("MGLRU supported (current value: {}). Runtime toggle is intentionally not changed automatically.", read(mglru.to_str().unwrap_or("")).trim()));
    } else {
        notes.push("MGLRU interface not present; kernel may not support it or it may be disabled.".into());
    }
    (out, notes)
}

fn render_udev() -> String {
    let mut rules = String::from("# Managed by kernel-optimizer. Rules only target devices with known queue capabilities.\n");
    rules.push_str("# NVMe: use none when the kernel exposes it.\n");
    rules.push_str("ACTION==\"add|change\", SUBSYSTEM==\"block\", KERNEL==\"nvme[0-9]*n[0-9]*\", ATTR{queue/scheduler}=\"none\"\n");
    if scheduler_available("mq-deadline") {
        rules.push_str("# Non-rotational SCSI/SATA SSDs: mq-deadline when available.\n");
        rules.push_str("ACTION==\"add|change\", SUBSYSTEM==\"block\", KERNEL==\"sd[a-z]\", ATTR{queue/rotational}==\"0\", ATTR{queue/scheduler}=\"mq-deadline\"\n");
    }
    if scheduler_available("bfq") {
        rules.push_str("# Rotational disks: BFQ when the running kernel provides it.\n");
        rules.push_str("ACTION==\"add|change\", SUBSYSTEM==\"block\", KERNEL==\"sd[a-z]\", ATTR{queue/rotational}==\"1\", ATTR{queue/scheduler}=\"bfq\"\n");
    }
    rules
}

fn unique_backup_path() -> PathBuf {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    PathBuf::from(format!("/var/backups/kernel-optimizer-sysctl-{stamp}-{}.conf", std::process::id()))
}
fn backup_sysctl() -> Result<PathBuf> {
    fs::create_dir_all("/var/backups").context("creating /var/backups")?;
    let path = unique_backup_path();
    let output = Command::new("sysctl").args(["-a"]).output().context("running sysctl -a")?;
    let mut file = OpenOptions::new().write(true).create_new(true).open(&path)?;
    file.write_all(&output.stdout)?;
    Ok(path)
}
fn write_atomic(path: &str, contents: &str) -> Result<()> {
    let target = Path::new(path);
    let parent = target.parent().context("configuration path has no parent")?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".kernel-optimizer-{}.tmp", std::process::id()));
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, target)?;
    Ok(())
}

pub fn tune(profile: Workload, dry_run: bool) -> Result<()> {
    let (sysctl, notes) = render_sysctl(profile);
    let udev = render_udev();
    println!("--- {SYSCTL_PATH} ---\n{sysctl}\n--- {UDEV_PATH} ---\n{udev}");
    for note in notes { println!("NOTE: {note}"); }
    if dry_run { println!("Dry run: no system files changed."); return Ok(()); }
    if !is_root() { bail!("applying system tuning requires root; rerun with sudo (or use --dry-run)"); }

    let backup = backup_sysctl()?;
    println!("Saved current sysctl state to {}", backup.display());
    write_atomic(SYSCTL_PATH, &sysctl)?;
    write_atomic(UDEV_PATH, &udev)?;

    // sysctl -e tolerates keys unavailable in this kernel. If reload fails, remove our
    // drop-in and reload the previous configuration to avoid leaving a partial state.
    let status = Command::new("sysctl").args(["-e", "--load", SYSCTL_PATH]).status()
        .context("reloading optimizer sysctl drop-in")?;
    if !status.success() {
        let _ = fs::remove_file(SYSCTL_PATH);
        let _ = Command::new("sysctl").arg("--system").status();
        bail!("sysctl reload failed; removed our drop-in and attempted to restore system defaults");
    }
    let _ = Command::new("udevadm").args(["control", "--reload-rules"]).status();
    println!("Applied. Reboot is not required for sysctl values; scheduler rules apply to future device events.");
    Ok(())
}

pub fn revert(dry_run: bool) -> Result<()> {
    println!("Would remove: {SYSCTL_PATH}");
    println!("Would remove: {UDEV_PATH}");
    if dry_run { println!("Dry run: no system files changed."); return Ok(()); }
    if !is_root() { bail!("reverting system tuning requires root; rerun with sudo (or use --dry-run)"); }
    for path in [SYSCTL_PATH, UDEV_PATH] {
        match fs::remove_file(path) {
            Ok(()) => println!("Removed {path}"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("Already absent: {path}"),
            Err(e) => return Err(e).with_context(|| format!("removing {path}")),
        }
    }
    let status = Command::new("sysctl").arg("--system").status().context("reloading sysctl configuration")?;
    if !status.success() { bail!("sysctl --system reported an error after removing optimizer settings"); }
    let _ = Command::new("udevadm").args(["control", "--reload-rules"]).status();
    println!("Optimizer drop-ins removed. Note: this restores values from remaining sysctl configuration files, not necessarily the exact pre-optimization runtime state.");
    Ok(())
}
