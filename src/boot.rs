use serde::Serialize;
use serde_json::Value;
use std::{
    fs,
    path::Path,
    process::Command,
};

#[derive(Debug, Clone, Serialize, Default)]
pub struct BootMount {
    pub target: String,
    pub source: String,
    pub filesystem: String,
    pub is_efi_system_partition: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct BootEntry {
    pub id: String,
    pub title: String,
    pub source: String,
    pub kernel_image: Option<String>,
    pub is_default: bool,
    pub is_selected: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct BootInventory {
    /// "UEFI" when EFI firmware variables are exposed, otherwise a conservative
    /// description that does not assume a container is booted through legacy BIOS.
    pub firmware_mode: String,
    pub detected_bootloaders: Vec<String>,
    pub boot_entries: Vec<BootEntry>,
    pub boot_mounts: Vec<BootMount>,
    pub efi_system_partition: Option<BootMount>,
    pub current_boot_entry: Option<String>,
    pub current_kernel_image: Option<String>,
    pub efi_boot_current: Option<String>,
    pub efi_boot_entries: Vec<String>,
    pub notes: Vec<String>,
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn read_optional(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok()
}

fn bool_field(value: &Value, names: &[&str]) -> bool {
    names.iter().any(|name| value.get(*name).and_then(Value::as_bool).unwrap_or(false))
}

pub fn parse_systemd_boot_entries_json(input: &str) -> Vec<BootEntry> {
    let Ok(value) = serde_json::from_str::<Value>(input) else {
        return Vec::new();
    };
    let entries = value
        .as_array()
        .or_else(|| value.get("entries").and_then(Value::as_array));
    entries.into_iter().flatten().filter_map(|entry| {
        let id = entry.get("id").and_then(Value::as_str).unwrap_or_default();
        let title = entry.get("title").and_then(Value::as_str).unwrap_or(id);
        if id.is_empty() && title.is_empty() {
            return None;
        }
        Some(BootEntry {
            id: if id.is_empty() { title.to_owned() } else { id.to_owned() },
            title: title.to_owned(),
            source: "systemd-boot".into(),
            kernel_image: entry.get("linux").and_then(Value::as_str).map(str::to_owned)
                .or_else(|| entry.get("efi").and_then(Value::as_str).map(str::to_owned)),
            is_default: bool_field(entry, &["isDefault", "default"]),
            is_selected: bool_field(entry, &["isSelected", "selected", "isCurrent"]),
        })
    }).collect()
}

pub fn parse_systemd_boot_entries_text(input: &str) -> Vec<BootEntry> {
    let mut entries = Vec::new();
    let mut current = BootEntry {
        source: "systemd-boot".into(),
        ..BootEntry::default()
    };
    let mut has_data = false;
    let flush = |current: &mut BootEntry, has_data: &mut bool, entries: &mut Vec<BootEntry>| {
        if *has_data {
            if current.id.is_empty() {
                current.id = current.title.clone();
            }
            if current.title.is_empty() {
                current.title = current.id.clone();
            }
            if !current.id.is_empty() || !current.title.is_empty() {
                entries.push(current.clone());
            }
        }
        *current = BootEntry {
            source: "systemd-boot".into(),
            ..BootEntry::default()
        };
        *has_data = false;
    };

    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            flush(&mut current, &mut has_data, &mut entries);
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else { continue };
        let value = value.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "title" => current.title = value.to_owned(),
            "id" => current.id = value.to_owned(),
            "source" => current.source = value.to_owned(),
            "linux" => current.kernel_image = Some(value.to_owned()),
            "efi" => current.kernel_image = Some(value.to_owned()),
            "default" => current.is_default = value.eq_ignore_ascii_case("yes") || value == "*",
            "selected" | "current" => current.is_selected = value.eq_ignore_ascii_case("yes") || value == "*",
            _ => continue,
        }
        has_data = true;
    }
    flush(&mut current, &mut has_data, &mut entries);
    entries
}

fn quoted_value(value: &str) -> Option<String> {
    let first = value.find(['\'', '"'])?;
    let quote = value.as_bytes()[first] as char;
    let rest = &value[first + 1..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_owned())
}

pub fn parse_grub_entries(input: &str) -> Vec<BootEntry> {
    let mut entries = Vec::new();
    for (index, line) in input.lines().enumerate() {
        let line = line.trim_start();
        let (kind, rest) = if let Some(rest) = line.strip_prefix("menuentry ") {
            ("menuentry", rest)
        } else if let Some(rest) = line.strip_prefix("submenu ") {
            ("submenu", rest)
        } else {
            continue;
        };
        if let Some(title) = quoted_value(rest) {
            let id = format!("{kind}-{}", entries.len());
            entries.push(BootEntry {
                id,
                title,
                source: "GRUB".into(),
                kernel_image: None,
                is_default: false,
                is_selected: false,
            });
        } else {
            let _ = index;
        }
    }
    entries
}

fn collect_filesystems(value: &Value, output: &mut Vec<BootMount>) {
    if let Some(items) = value.as_array() {
        for item in items {
            collect_filesystems(item, output);
        }
        return;
    }
    if !value.is_object() {
        return;
    }
    let target = value.get("target").and_then(Value::as_str).unwrap_or_default();
    if matches!(target, "/boot" | "/efi" | "/boot/efi" | "/boot/esp" | "/esp") {
        let source = value.get("source").and_then(Value::as_str).unwrap_or_default();
        let filesystem = value.get("fstype").and_then(Value::as_str).unwrap_or_default();
        let is_fat = matches!(
            filesystem.to_ascii_lowercase().as_str(),
            "vfat" | "fat" | "fat32" | "msdos"
        );
        output.push(BootMount {
            target: target.to_owned(),
            source: source.to_owned(),
            filesystem: filesystem.to_owned(),
            is_efi_system_partition: is_fat,
        });
    }
    if let Some(children) = value.get("children") {
        collect_filesystems(children, output);
    }
}

pub fn parse_mounts_json(input: &str) -> Vec<BootMount> {
    let Ok(value) = serde_json::from_str::<Value>(input) else {
        return Vec::new();
    };
    let mut output = Vec::new();
    if let Some(filesystems) = value.get("filesystems") {
        collect_filesystems(filesystems, &mut output);
    }
    output.sort_by(|left, right| left.target.cmp(&right.target));
    output.dedup_by(|left, right| left.target == right.target);
    output
}

pub fn parse_efi_boot_entries(input: &str) -> (Option<String>, Vec<String>) {
    let current = input.lines().find_map(|line| {
        line.strip_prefix("BootCurrent:")
            .map(|value| format!("Boot{}", value.trim()))
    });
    let entries = input.lines().filter_map(|line| {
        let line = line.trim();
        if !line.starts_with("Boot") || line.len() < 8 {
            return None;
        }
        let id = &line[..8];
        let valid_id = id.starts_with("Boot")
            && id[4..].bytes().all(|byte| byte.is_ascii_hexdigit());
        valid_id.then(|| line.to_owned())
    }).collect();
    (current, entries)
}

fn current_kernel_image() -> Option<String> {
    let command_line = read_optional(Path::new("/proc/cmdline"))?;
    command_line.split_whitespace().find_map(|token| {
        token.strip_prefix("BOOT_IMAGE=").map(str::to_owned)
    })
}

fn path_exists_any(paths: &[&str]) -> bool {
    paths.iter().any(|path| Path::new(path).exists())
}

fn detect_systemd_boot() -> (bool, Vec<BootEntry>) {
    let loader_conf = [
        "/boot/loader/loader.conf",
        "/efi/loader/loader.conf",
        "/boot/efi/loader/loader.conf",
    ];
    let json = command_output("bootctl", &["--json=short", "list"]);
    let mut entries = json.as_deref().map(parse_systemd_boot_entries_json).unwrap_or_default();
    if entries.is_empty() {
        if let Some(text) = command_output("bootctl", &["list"]) {
            entries = parse_systemd_boot_entries_text(&text);
        }
    }
    let status = command_output("bootctl", &["status"]).unwrap_or_default();
    let detected = path_exists_any(&loader_conf)
        || !entries.is_empty()
        || status.to_ascii_lowercase().contains("systemd-boot");
    (detected, entries)
}

fn detect_grub() -> (bool, Vec<BootEntry>) {
    let paths = [
        "/boot/grub/grub.cfg",
        "/boot/grub2/grub.cfg",
        "/efi/grub/grub.cfg",
        "/boot/efi/grub/grub.cfg",
    ];
    let mut entries = Vec::new();
    let mut detected = false;
    for path in paths {
        if let Some(contents) = read_optional(Path::new(path)) {
            detected = true;
            entries.extend(parse_grub_entries(&contents));
        }
    }
    (detected, entries)
}

fn detect_refind() -> bool {
    path_exists_any(&[
        "/boot/EFI/refind/refind.conf",
        "/efi/EFI/refind/refind.conf",
        "/boot/efi/EFI/refind/refind.conf",
        "/boot/refind_linux.conf",
    ])
}

fn detect_syslinux() -> bool {
    path_exists_any(&[
        "/boot/syslinux/syslinux.cfg",
        "/boot/extlinux/extlinux.conf",
    ])
}

pub fn detect() -> BootInventory {
    let firmware_mode = if Path::new("/sys/firmware/efi").is_dir() {
        "UEFI".to_owned()
    } else {
        "Legacy BIOS or firmware data not exposed".to_owned()
    };

    let (systemd_boot_detected, mut entries) = detect_systemd_boot();
    let (grub_detected, grub_entries) = detect_grub();
    entries.extend(grub_entries);

    let mut detected_bootloaders = Vec::new();
    if systemd_boot_detected {
        detected_bootloaders.push("systemd-boot".to_owned());
    }
    if grub_detected {
        detected_bootloaders.push("GRUB".to_owned());
    }
    if detect_refind() {
        detected_bootloaders.push("rEFInd".to_owned());
    }
    if detect_syslinux() {
        detected_bootloaders.push("Syslinux".to_owned());
    }

    let mount_json = command_output("findmnt", &["--json", "--output", "TARGET,SOURCE,FSTYPE"])
        .unwrap_or_default();
    let boot_mounts = parse_mounts_json(&mount_json);
    let efi_system_partition = if firmware_mode == "UEFI" {
        boot_mounts.iter().find(|mount| mount.is_efi_system_partition).cloned()
    } else {
        None
    };

    let (efi_boot_current, efi_boot_entries) = command_output("efibootmgr", &["-v"])
        .map(|output| parse_efi_boot_entries(&output))
        .unwrap_or_default();

    let current_boot_entry = entries.iter()
        .find(|entry| entry.is_selected)
        .map(|entry| entry.title.clone())
        .or_else(|| efi_boot_current.clone());

    let mut notes = Vec::new();
    if detected_bootloaders.is_empty() {
        notes.push("No recognised bootloader configuration was found in the standard paths; this can be normal in containers or custom layouts.".into());
    }
    if firmware_mode == "UEFI" && efi_system_partition.is_none() {
        notes.push("UEFI firmware is exposed, but no mounted FAT ESP was found at /boot, /efi, /boot/efi, /boot/esp or /esp.".into());
    }
    if firmware_mode != "UEFI" {
        notes.push("Firmware mode is not asserted as legacy BIOS because EFI sysfs data can be hidden in containers and virtualized environments.".into());
    }
    if current_boot_entry.is_none() {
        notes.push("The currently selected boot entry could not be identified. The running kernel image may still be available from /proc/cmdline.".into());
    }

    BootInventory {
        firmware_mode,
        detected_bootloaders,
        boot_entries: entries,
        boot_mounts,
        efi_system_partition,
        current_boot_entry,
        current_kernel_image: current_kernel_image(),
        efi_boot_current,
        efi_boot_entries,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_systemd_boot_json_entries_and_selection() {
        let input = r#"[
            {"id":"arch.conf","title":"Arch Linux","linux":"/vmlinuz-linux","isDefault":true,"isSelected":true},
            {"id":"fallback.conf","title":"Arch fallback","linux":"/vmlinuz-linux"}
        ]"#;
        let entries = parse_systemd_boot_entries_json(input);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].title, "Arch Linux");
        assert!(entries[0].is_default);
        assert!(entries[0].is_selected);
        assert_eq!(entries[1].kernel_image.as_deref(), Some("/vmlinuz-linux"));
    }

    #[test]
    fn parses_systemd_boot_text_entries() {
        let input = "title: Arch Linux\nid: arch.conf\nsource: /boot/loader/entries/arch.conf\nlinux: /vmlinuz-linux\n\n";
        let entries = parse_systemd_boot_entries_text(input);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "Arch Linux");
        assert_eq!(entries[0].id, "arch.conf");
    }

    #[test]
    fn parses_grub_menuentries_and_submenus() {
        let input = "menuentry 'Arch Linux' --class arch {\nsubmenu \"Advanced options\" {\n menuentry 'Arch Linux fallback' {\n";
        let entries = parse_grub_entries(input);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].title, "Arch Linux");
        assert_eq!(entries[1].title, "Advanced options");
        assert_eq!(entries[2].title, "Arch Linux fallback");
    }

    #[test]
    fn identifies_mounted_efi_system_partition_from_findmnt_json() {
        let input = r#"{
            "filesystems": [
                {"target":"/","source":"/dev/nvme0n1p2","fstype":"btrfs","children":[
                    {"target":"/boot","source":"/dev/nvme0n1p1","fstype":"vfat"}
                ]},
                {"target":"/efi","source":"/dev/sdb1","fstype":"vfat"}
            ]
        }"#;
        let mounts = parse_mounts_json(input);
        assert_eq!(mounts.len(), 2);
        assert!(mounts.iter().find(|mount| mount.target == "/boot").unwrap().is_efi_system_partition);
        assert!(mounts.iter().find(|mount| mount.target == "/efi").unwrap().is_efi_system_partition);
    }

    #[test]
    fn parses_current_efi_boot_entry() {
        let input = "BootCurrent: 0002\nBootOrder: 0002,0001\nBoot0001* Linux Firmware Entry\nBoot0002* Linux Boot Manager\n";
        let (current, entries) = parse_efi_boot_entries(input);
        assert_eq!(current.as_deref(), Some("Boot0002"));
        assert_eq!(entries.len(), 2);
        assert!(entries[1].contains("Linux Boot Manager"));
    }
}
