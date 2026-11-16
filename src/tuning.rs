use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    os::unix::fs::OpenOptionsExt,
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
    // A zram device can exist without being initialized or enabled as swap.
    read("/proc/swaps").lines().skip(1).any(|line| line.split_whitespace().next()
        .is_some_and(|name| name.starts_with("/dev/zram")))
}
fn detect_virtualization() -> String {
    crate::hardware::detect_virtualization()
}
fn supports_extended_swappiness() -> bool {
    let release = command_output("uname", &["-r"]);
    let mut parts = release.split('.').filter_map(|part| {
        part.split(|character: char| !character.is_ascii_digit())
            .next()
            .and_then(|value| value.parse::<u32>().ok())
    });
    matches!((parts.next(), parts.next()), (Some(major), Some(minor)) if major > 5 || (major == 5 && minor >= 8))
}
fn mem_total_kib() -> u64 {
    read("/proc/meminfo").lines().find(|l| l.starts_with("MemTotal:"))
        .and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse().ok()).unwrap_or(0)
}

fn render_sysctl(profile: Workload) -> (String, Vec<String>) {
    let zram = detect_zram();
    let virt = detect_virtualization();
    let mem_kib = mem_total_kib();
    let min_free = (mem_kib / 100).clamp(64 * 1024, 512 * 1024);
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
        // Linux 5.8 raised the accepted swappiness range from 0-100 to 0-200.
        let swappiness = if supports_extended_swappiness() { 150 } else { 100 };
        out.push_str("# ZRAM: encourage reclaim into compressed RAM; disk-swap tuning is inappropriate here.\n");
        out.push_str(&format!("vm.swappiness = {swappiness}\nvm.page-cluster = 0\n"));
    } else {
        out.push_str("# Disk swap: conservative swappiness avoids unnecessary paging on typical desktop/server systems.\n");
        let swappiness = match profile { Workload::Server => 10, Workload::Laptop => 30, Workload::Desktop => 20, Workload::Gaming => 10 };
        out.push_str(&format!("vm.swappiness = {swappiness}\n"));
    }

    // A 64 MiB floor can consume a large share of RAM on small systems.
    if mem_kib >= 4 * 1024 * 1024 && virt == "none" {
        out.push_str("\n# Reserve roughly 1% of RAM for atomic allocations, bounded to 64-512 MiB.\n");
        out.push_str(&format!("vm.min_free_kbytes = {min_free}\n"));
    } else if mem_kib > 0 && mem_kib < 4 * 1024 * 1024 {
        notes.push("Less than 4 GiB RAM detected: skipped the 64 MiB minimum memory-reserve heuristic to avoid consuming an excessive share of memory.".into());
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
    if detect_virtualization() != "none" {
        return String::from(
            "# Skipped: virtualization/container detected; host I/O scheduler policy is not assumed.\n",
        );
    }

    let mut rules = String::from(
        "# Managed by kernel-optimizer. Per-device rules use only schedulers exposed by that device.\n",
    );
    let Ok(devices) = fs::read_dir("/sys/block") else {
        return rules;
    };
    let mut rule_count = 0_usize;

    for device in devices.flatten() {
        let name = device.file_name().to_string_lossy().to_string();
        let queue = device.path().join("queue");
        let scheduler_path = queue.join("scheduler");
        let rotational_path = queue.join("rotational");
        let schedulers = fs::read_to_string(&scheduler_path)
            .unwrap_or_default()
            .split_whitespace()
            .map(|value| value.trim_matches(&['[', ']'][..]).to_owned())
            .collect::<Vec<_>>();
        if schedulers.is_empty() {
            continue;
        }

        let rotational = fs::read_to_string(rotational_path)
            .ok()
            .and_then(|value| value.trim().parse::<u8>().ok());
        let selected = if name.starts_with("nvme") {
            schedulers.iter().find(|value| value.as_str() == "none")
        } else if rotational == Some(1) {
            schedulers
                .iter()
                .find(|value| value.as_str() == "bfq")
                .or_else(|| schedulers.iter().find(|value| value.as_str() == "mq-deadline"))
        } else if rotational == Some(0) {
            schedulers
                .iter()
                .find(|value| value.as_str() == "mq-deadline")
                .or_else(|| schedulers.iter().find(|value| value.as_str() == "kyber"))
        } else {
            None
        };

        if let Some(selected) = selected {
            rules.push_str(&format!(
                "# {name}: rotational={rotational:?}; available={schedulers:?}\n"
            ));
            rules.push_str(&format!(
                "ACTION==\"add|change\", SUBSYSTEM==\"block\", KERNEL==\"{name}\", ATTR{{queue/scheduler}}=\"{selected}\"\n"
            ));
            rule_count += 1;
        }
    }

    if rule_count == 0 {
        rules.push_str("# No eligible block devices with a suitable scheduler were detected.\n");
    }
    rules
}

fn unique_backup_path() -> PathBuf {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    PathBuf::from(format!("/var/backups/kernel-optimizer-sysctl-{stamp}-{}.conf", std::process::id()))
}
fn backup_sysctl() -> Result<PathBuf> {
    fs::create_dir_all("/var/backups").context("creating /var/backups")?;
    let path = unique_backup_path();
    let output = Command::new("sysctl").args(["-a"]).output().context("running sysctl -a")?;
    if !output.status.success() {
        bail!("sysctl -a failed; refusing to apply settings without a complete backup");
    }
    let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
    file.write_all(&output.stdout)?;
    file.sync_all()?;
    Ok(path)
}
fn write_atomic(path: &str, contents: &str) -> Result<()> {
    let target = Path::new(path);
    let parent = target.parent().context("configuration path has no parent")?;
    fs::create_dir_all(parent)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let tmp = parent.join(format!(".kernel-optimizer-{}-{stamp}.tmp", std::process::id()));
    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).mode(0o644).open(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, target)
            .with_context(|| format!("atomically replacing {}", target.display()))?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write_result
}

fn previous_file(path: &str) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading existing {}", path)),
    }
}
fn restore_file(path: &str, previous: Option<&str>) -> Result<()> {
    match previous {
        Some(contents) => write_atomic(path, contents),
        None => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("removing {}", path)),
        },
    }
}

fn rollback_managed_files(
    sysctl_path: &str,
    udev_path: &str,
    previous_sysctl: Option<&str>,
    previous_udev: Option<&str>,
    reload_sysctl: &mut impl FnMut() -> Result<()>,
    reload_udev: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    let mut failures = Vec::new();
    if let Err(error) = restore_file(sysctl_path, previous_sysctl) {
        failures.push(format!("restoring {sysctl_path}: {error:#}"));
    }
    if let Err(error) = restore_file(udev_path, previous_udev) {
        failures.push(format!("restoring {udev_path}: {error:#}"));
    }
    if let Err(error) = reload_sysctl() {
        failures.push(format!("reloading sysctl after rollback: {error:#}"));
    }
    if let Err(error) = reload_udev() {
        failures.push(format!("reloading udev after rollback: {error:#}"));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!("rollback was incomplete: {}", failures.join("; "));
    }
}

fn apply_managed_files_with_reloader(
    sysctl_path: &str,
    udev_path: &str,
    sysctl_contents: &str,
    udev_contents: &str,
    mut reload_sysctl: impl FnMut() -> Result<()>,
    mut reload_udev: impl FnMut() -> Result<()>,
) -> Result<()> {
    let previous_sysctl = previous_file(sysctl_path)?;
    let previous_udev = previous_file(udev_path)?;

    write_atomic(sysctl_path, sysctl_contents)?;
    if let Err(error) = write_atomic(udev_path, udev_contents) {
        return match restore_file(sysctl_path, previous_sysctl.as_deref()) {
            Ok(()) => Err(error).context("writing udev rules; restored the previous sysctl drop-in"),
            Err(rollback_error) => Err(error).context(format!(
                "writing udev rules failed and restoring the previous sysctl drop-in also failed: {rollback_error:#}"
            )),
        };
    }

    if let Err(error) = reload_sysctl() {
        return match rollback_managed_files(
            sysctl_path,
            udev_path,
            previous_sysctl.as_deref(),
            previous_udev.as_deref(),
            &mut reload_sysctl,
            &mut reload_udev,
        ) {
            Ok(()) => Err(error).context("sysctl reload failed; restored the previous optimizer drop-ins"),
            Err(rollback_error) => Err(error).context(format!(
                "sysctl reload failed and rollback reported an error: {rollback_error:#}"
            )),
        };
    }

    if udev_contents.contains("ACTION==")
        && let Err(error) = reload_udev()
    {
        return match rollback_managed_files(
            sysctl_path,
            udev_path,
            previous_sysctl.as_deref(),
            previous_udev.as_deref(),
            &mut reload_sysctl,
            &mut reload_udev,
        ) {
            Ok(()) => Err(error).context("udev rule reload failed; restored the previous optimizer drop-ins"),
            Err(rollback_error) => Err(error).context(format!(
                "udev rule reload failed and rollback reported an error: {rollback_error:#}"
            )),
        };
    }
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

    apply_managed_files_with_reloader(
        SYSCTL_PATH,
        UDEV_PATH,
        &sysctl,
        &udev,
        || {
            let status = Command::new("sysctl").args(["-e", "-p", SYSCTL_PATH]).status()
                .context("starting sysctl reload")?;
            if !status.success() {
                bail!("sysctl reload returned {status}");
            }
            Ok(())
        },
        || {
            let status = Command::new("udevadm").args(["control", "--reload-rules"]).status()
                .context("starting udev rule reload")?;
            if !status.success() {
                bail!("udevadm reload returned {status}");
            }
            Ok(())
        },
    )?;
    println!("Applied. Reboot is not required for sysctl values; scheduler rules apply to future device events.");
    Ok(())
}

pub fn revert(dry_run: bool) -> Result<()> {
    println!("Would remove: {SYSCTL_PATH}");
    println!("Would remove: {UDEV_PATH}");
    if dry_run {
        println!("Dry run: no system files changed.");
        return Ok(());
    }
    if !is_root() {
        bail!("reverting system tuning requires root; rerun with sudo (or use --dry-run)");
    }

    let previous_sysctl = previous_file(SYSCTL_PATH)?;
    let previous_udev = previous_file(UDEV_PATH)?;

    let removal_result = (|| -> Result<()> {
        for path in [SYSCTL_PATH, UDEV_PATH] {
            match fs::remove_file(path) {
                Ok(()) => println!("Removed {path}"),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    println!("Already absent: {path}");
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("removing {path}"));
                }
            }
        }

        let status = Command::new("sysctl")
            .arg("--system")
            .status()
            .context("reloading sysctl configuration")?;
        if !status.success() {
            bail!("sysctl --system reported an error after removing optimizer settings");
        }
        Ok(())
    })();

    if let Err(error) = removal_result {
        restore_file(SYSCTL_PATH, previous_sysctl.as_deref())?;
        restore_file(UDEV_PATH, previous_udev.as_deref())?;
        let _ = Command::new("sysctl").arg("--system").status();
        let _ = Command::new("udevadm")
            .args(["control", "--reload-rules"])
            .status();
        return Err(error).context("revert failed; restored previous optimizer drop-ins");
    }

    match Command::new("udevadm")
        .args(["control", "--reload-rules"])
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!(
            "Warning: udev rule reload returned {status}; the optimizer rule file was removed"
        ),
        Err(error) => eprintln!(
            "Warning: could not reload udev rules after revert: {error}"
        ),
    }

    println!(
        "Optimizer drop-ins removed. This reloads the remaining sysctl files; it does not guarantee restoration of runtime changes made concurrently by other services."
    );
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

    fn temp_root(name: &str) -> PathBuf {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "kernel-optimizer-tuning-{name}-{}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn path_string(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn atomic_write_replaces_content_without_leaving_temporary_files() {
        let root = temp_root("atomic-write");
        let target = root.join("nested/managed.conf");
        let target_text = path_string(&target);
        write_atomic(&target_text, "before\n").unwrap();
        write_atomic(&target_text, "after\n").unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "after\n");
        let entries = fs::read_dir(target.parent().unwrap()).unwrap().count();
        assert_eq!(entries, 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_sysctl_reload_restores_both_managed_files() {
        let root = temp_root("sysctl-rollback");
        let sysctl = root.join("sysctl.conf");
        let udev = root.join("udev.rules");
        fs::write(&sysctl, "previous sysctl\n").unwrap();
        fs::write(&udev, "previous udev\n").unwrap();
        let sysctl_text = path_string(&sysctl);
        let udev_text = path_string(&udev);

        let result = apply_managed_files_with_reloader(
            &sysctl_text,
            &udev_text,
            "new sysctl\n",
            "new udev\n",
            || bail!("simulated sysctl reload failure"),
            || Ok(()),
        );

        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&sysctl).unwrap(), "previous sysctl\n");
        assert_eq!(fs::read_to_string(&udev).unwrap(), "previous udev\n");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_udev_reload_restores_both_managed_files() {
        let root = temp_root("udev-rollback");
        let sysctl = root.join("sysctl.conf");
        let udev = root.join("udev.rules");
        fs::write(&sysctl, "previous sysctl\n").unwrap();
        fs::write(&udev, "previous udev\n").unwrap();
        let sysctl_text = path_string(&sysctl);
        let udev_text = path_string(&udev);
        let reloads = AtomicU64::new(0);

        let result = apply_managed_files_with_reloader(
            &sysctl_text,
            &udev_text,
            "new sysctl\n",
            "ACTION==\"add\", TEST==\"device\"\n",
            || Ok(()),
            || {
                if reloads.fetch_add(1, Ordering::Relaxed) == 0 {
                    bail!("simulated udev reload failure");
                }
                Ok(())
            },
        );

        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&sysctl).unwrap(), "previous sysctl\n");
        assert_eq!(fs::read_to_string(&udev).unwrap(), "previous udev\n");
        assert_eq!(reloads.load(Ordering::Relaxed), 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rollback_removes_managed_files_that_did_not_exist_before_apply() {
        let root = temp_root("rollback-new-files");
        let sysctl = root.join("sysctl.conf");
        let udev = root.join("udev.rules");
        let sysctl_text = path_string(&sysctl);
        let udev_text = path_string(&udev);

        let result = apply_managed_files_with_reloader(
            &sysctl_text,
            &udev_text,
            "new sysctl\n",
            "new udev\n",
            || bail!("simulated sysctl reload failure"),
            || Ok(()),
        );

        assert!(result.is_err());
        assert!(!sysctl.exists());
        assert!(!udev.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
