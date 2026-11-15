use crate::provenance;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

const PACKAGE_PREFIX: &str = "kernel-optimizer-";
const MIN_PACKAGE_FREE_SPACE: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageArtifact {
    pub path: String,
    pub package_name: String,
    pub kernel_release: String,
    pub stage_directory: String,
    pub sha256: String,
    pub source_signer: String,
    pub modules_file_count: u64,
    pub modules_total_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstallReadiness {
    pub stage_directory: String,
    pub distro: String,
    pub architecture: String,
    pub firmware_mode: String,
    pub current_kernel: String,
    pub kernel_release: String,
    pub package_name: String,
    pub kernel_image_size_bytes: u64,
    pub modules_file_count: u64,
    pub modules_total_bytes: u64,
    pub secure_boot: String,
    pub module_signing: String,
    pub bootloader: String,
    pub initramfs_backend: String,
    pub boot_mount: String,
    pub package_build_ready: bool,
    pub installation_ready: bool,
    pub blockers: Vec<String>,
    pub warnings: Vec<String>,
}

struct ValidatedStage {
    stage_directory: String,
    release: String,
    package_name: String,
    kernel_image: PathBuf,
    config: PathBuf,
    system_map: Option<PathBuf>,
    modules: PathBuf,
    signer: String,
    pkgver: String,
    modules_file_count: u64,
    modules_total_bytes: u64,
    kernel_image_size_bytes: u64,
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    })
}

fn command_available(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn validate_release(release: &str) -> Result<()> {
    if release.is_empty()
        || release.len() > 96
        || !release
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".+_-".contains(&byte))
        || release == "."
        || release == ".."
    {
        bail!("staged kernel release contains unsafe characters: {release:?}");
    }
    Ok(())
}

fn package_slug(release: &str) -> String {
    let mut slug = String::new();
    let mut previous_dash = false;
    for character in release.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            slug.push(character);
            previous_dash = false;
        } else if !previous_dash {
            slug.push('-');
            previous_dash = true;
        }
    }
    slug.trim_matches('-').to_owned()
}

fn package_name(release: &str) -> Result<String> {
    validate_release(release)?;
    let name = format!("{PACKAGE_PREFIX}{}", package_slug(release));
    if name.len() > 63 {
        bail!("generated Arch package name is too long: {name}");
    }
    Ok(name)
}

fn safe_stage_path(stage: &Path, relative: &str) -> Result<PathBuf> {
    let relative_path = Path::new(relative);
    if relative_path.is_absolute()
        || relative_path.components().any(|component| {
            !matches!(component, Component::Normal(_))
        })
    {
        bail!("provenance contains an unsafe staged path: {relative:?}");
    }
    Ok(stage.join(relative_path))
}

fn validate_module_symlinks(modules: &Path) -> Result<()> {
    let mut pending = vec![modules.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)
            .with_context(|| format!("reading staged modules directory {}", directory.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let file_type = fs::symlink_metadata(&path)?.file_type();
            let relative = path.strip_prefix(modules)?;
            if relative == Path::new("build") || relative == Path::new("source") {
                if !file_type.is_symlink() {
                    bail!("staged modules build/source entry must be a symlink: {}", path.display());
                }
            }
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_symlink() {
                let relative = path.strip_prefix(modules)?;
                let is_known_build_link = relative == Path::new("build")
                    || relative == Path::new("source");
                if !is_known_build_link {
                    bail!(
                        "unexpected symlink in staged module tree: {}",
                        path.display()
                    );
                }
            }
        }
    }
    Ok(())
}

fn validate_stage(stage: &Path) -> Result<ValidatedStage> {
    if !stage.is_dir() {
        bail!("staging directory does not exist: {}", stage.display());
    }
    let stage = stage.canonicalize().with_context(|| format!("resolving staging directory {}", stage.display()))?;
    let manifest_path = stage.join("provenance.json");
    let manifest: Value = serde_json::from_slice(
        &fs::read(&manifest_path)
            .with_context(|| format!("reading {}", manifest_path.display()))?,
    )
    .context("parsing provenance.json")?;

    if manifest["schema_version"].as_u64() != Some(2) {
        bail!("unsupported or missing provenance schema; finalize the staged build first");
    }
    if manifest["source"]["signature_verified"].as_bool() != Some(true) {
        bail!("kernel source signature is not verified; refusing to package this build");
    }

    let release = manifest["artifacts"]["kernel_release"]
        .as_str()
        .context("provenance is missing artifacts.kernel_release")?
        .to_owned();
    validate_release(&release)?;
    let expected_image_name = format!("vmlinuz-{release}");
    let expected_image_rel = format!("boot/{expected_image_name}");
    let expected_modules_rel = format!("root/lib/modules/{release}");

    let image_rel = manifest["artifacts"]["kernel_image"]
        .as_str()
        .context("provenance is missing artifacts.kernel_image")?;
    let modules_rel = manifest["artifacts"]["modules_directory"]
        .as_str()
        .context("provenance is missing artifacts.modules_directory")?;
    if image_rel != expected_image_rel || modules_rel != expected_modules_rel {
        bail!("provenance artifact paths do not match the expected staged kernel layout");
    }

    let kernel_image = safe_stage_path(&stage, image_rel)?;
    let modules = safe_stage_path(&stage, modules_rel)?;
    let config = stage.join("boot").join(format!("config-{release}"));
    let system_map_candidate = stage.join("boot").join(format!("System.map-{release}"));
    let manifest_has_system_map = !manifest["artifacts"]["system_map"].is_null();
    if manifest_has_system_map != system_map_candidate.is_file() {
        bail!("staged System.map presence does not match the provenance manifest");
    }
    if system_map_candidate.is_file()
        && fs::symlink_metadata(&system_map_candidate)?.file_type().is_symlink()
    {
        bail!("staged System.map must not be a symlink");
    }
    let system_map = system_map_candidate.is_file().then_some(system_map_candidate);

    if !kernel_image.is_file() || fs::symlink_metadata(&kernel_image)?.file_type().is_symlink() {
        bail!("staged kernel image is missing or is a symlink: {}", kernel_image.display());
    }
    if !config.is_file() || fs::symlink_metadata(&config)?.file_type().is_symlink() {
        bail!("staged kernel config is missing or is a symlink: {}", config.display());
    }
    if !modules.is_dir() || fs::symlink_metadata(&modules)?.file_type().is_symlink() {
        bail!("staged modules directory is missing or is a symlink: {}", modules.display());
    }
    for artifact in [&kernel_image, &config, &modules] {
        let canonical = artifact.canonicalize()
            .with_context(|| format!("resolving staged artifact {}", artifact.display()))?;
        if !canonical.starts_with(&stage) {
            bail!("staged artifact resolves outside the staging directory: {}", artifact.display());
        }
    }
    if !stage.join("BUILD-METADATA.txt").is_file() {
        bail!("BUILD-METADATA.txt is missing; run kernel-optimizer finalize first");
    }
    validate_module_symlinks(&modules)?;

    for index in ["modules.dep", "modules.alias"] {
        let path = modules.join(index);
        if !path.is_file() || fs::metadata(&path)?.len() == 0 {
            bail!("staged module index is missing or empty: {}", path.display());
        }
    }

    let image_hash = provenance::sha256_file(&kernel_image)?;
    let expected_image_hash = manifest["artifacts"]["kernel_image_sha256"]
        .as_str()
        .context("provenance is missing the kernel image SHA-256")?;
    if image_hash != expected_image_hash {
        bail!("staged kernel image does not match its recorded SHA-256");
    }

    let (modules_hash, modules_file_count, modules_total_bytes) =
        provenance::sha256_tree(&modules).context("hashing staged module tree")?;
    let expected_modules_hash = manifest["artifacts"]["modules_tree_sha256"]
        .as_str()
        .context("provenance has no modules-tree SHA-256; run finalize again")?;
    if modules_hash != expected_modules_hash {
        bail!("staged modules do not match the verified modules-tree SHA-256");
    }
    if modules_file_count == 0 {
        bail!("staged module directory contains no regular files");
    }

    let signer = manifest["source"]["signer_name"]
        .as_str()
        .unwrap_or("Pinned kernel.org signer")
        .to_owned();
    let pkgver = manifest["source"]["version"]
        .as_str()
        .filter(|version| {
            !version.is_empty()
                && version
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || byte == b'.')
        })
        .context("provenance contains an invalid upstream kernel version")?
        .to_owned();
    if !(2..=3).contains(&pkgver.split('.').count())
        || pkgver.split('.').any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        bail!("provenance source version is not a valid X.Y or X.Y.Z package version");
    }

    let expected_config_hash = manifest["build"]["effective_config_sha256"]
        .as_str()
        .context("provenance is missing the effective kernel config hash")?;
    if provenance::sha256_file(&config)? != expected_config_hash {
        bail!("staged kernel config does not match its recorded SHA-256");
    }
    if let Some(system_map) = &system_map {
        let expected_hash = manifest["artifacts"]["system_map_sha256"]
            .as_str()
            .context("staged System.map exists but provenance has no recorded hash")?;
        if provenance::sha256_file(system_map)? != expected_hash {
            bail!("staged System.map does not match its recorded SHA-256");
        }
    }

    Ok(ValidatedStage {
        stage_directory: stage.display().to_string(),
        release: release.clone(),
        package_name: package_name(&release)?,
        kernel_image,
        config,
        system_map,
        modules,
        signer,
        pkgver,
        modules_file_count,
        modules_total_bytes,
        kernel_image_size_bytes: fs::metadata(stage.join("boot").join(expected_image_name))?.len(),
    })
}

fn os_release() -> String {
    fs::read_to_string("/etc/os-release").unwrap_or_default()
}

fn detect_arch_family() -> (bool, String) {
    let release = os_release();
    let mut id = String::new();
    let mut id_like = String::new();
    for line in release.lines() {
        if let Some(value) = line.strip_prefix("ID=") {
            id = value.trim_matches('"').to_owned();
        } else if let Some(value) = line.strip_prefix("ID_LIKE=") {
            id_like = value.trim_matches('"').to_owned();
        }
    }
    let arch_family = Path::new("/etc/arch-release").exists()
        || id.eq_ignore_ascii_case("arch")
        || id_like
            .split_whitespace()
            .any(|item| item.eq_ignore_ascii_case("arch"));
    let distro = if id.is_empty() {
        if arch_family { "Arch Linux".into() } else { "Unknown".into() }
    } else {
        id
    };
    (arch_family, distro)
}

fn secure_boot_state() -> Option<bool> {
    if let Some(status) = command_output("mokutil", &["--sb-state"]) {
        let status = status.to_ascii_lowercase();
        if status.contains("enabled") {
            return Some(true);
        }
        if status.contains("disabled") {
            return Some(false);
        }
    }

    let efivars = Path::new("/sys/firmware/efi/efivars");
    if !Path::new("/sys/firmware/efi").exists() {
        return Some(false);
    }
    if let Ok(entries) = fs::read_dir(efivars) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with("SecureBoot-")
                && let Ok(bytes) = fs::read(entry.path())
                && bytes.len() >= 5
            {
                return Some(bytes[4] == 1);
            }
        }
    }
    None
}

fn detect_bootloader() -> String {
    if Path::new("/boot/grub/grub.cfg").is_file()
        || Path::new("/usr/bin/grub-mkconfig").is_file()
    {
        "GRUB detected; boot menu will need a manual refresh".into()
    } else if [
        "/boot/loader/loader.conf",
        "/efi/loader/loader.conf",
        "/boot/efi/loader/loader.conf",
    ]
    .iter()
    .any(|path| Path::new(path).is_file())
    {
        "systemd-boot detected; a loader entry must be added manually".into()
    } else if Path::new("/boot/EFI/refind/refind.conf").is_file()
        || Path::new("/boot/efi/EFI/refind/refind.conf").is_file()
    {
        "rEFInd detected; boot entry must be added manually".into()
    } else {
        "Unknown; bootloader configuration will not be changed".into()
    }
}

fn detect_initramfs_backend() -> String {
    let hook_dirs = ["/usr/share/libalpm/hooks", "/etc/pacman.d/hooks"];
    let mut hooks = Vec::new();
    for directory in hook_dirs {
        if let Ok(entries) = fs::read_dir(directory) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                if name.ends_with(".hook") {
                    hooks.push(name);
                }
            }
        }
    }
    let has_mkinitcpio_hook = hooks.iter().any(|name| name.contains("mkinitcpio"));
    let has_dracut_hook = hooks.iter().any(|name| name.contains("dracut"));
    let garuda_dracut_support = Command::new("pacman")
        .args(["-Q", "garuda-dracut-support"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
        && command_available("dracut")
        && Path::new("/usr/bin/dracut-rebuild").is_file();

    if command_available("mkinitcpio") && has_mkinitcpio_hook {
        "mkinitcpio pacman hook".into()
    } else if command_available("dracut") && has_dracut_hook {
        "dracut pacman hook".into()
    } else if garuda_dracut_support {
        "Garuda dracut support".into()
    } else {
        "No supported automatic initramfs hook detected".into()
    }
}

fn boot_mount() -> (String, Option<bool>) {
    let active = command_output("findmnt", &["-rn", "-o", "TARGET", "--target", "/boot"]);
    let expected = command_output("findmnt", &["--fstab", "-rn", "-o", "TARGET", "--target", "/boot"]);
    match (active, expected) {
        (Some(active), Some(expected)) if expected == "/boot" && active != "/boot" => (
            format!("Expected /boot from fstab, but active mount resolves to {active}"),
            Some(false),
        ),
        (Some(active), _) => (active, Some(true)),
        (None, _) => ("Could not inspect /boot mount (findmnt unavailable)".into(), None),
    }
}

fn package_owner(path: &Path) -> Option<String> {
    let path = path.to_str()?;
    let output = Command::new("pacman")
        .args(["-Qo", "--", path])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().next()?;
    let owner = line.split(" is owned by ").nth(1)?;
    owner.split_whitespace().next().map(str::to_owned)
}

fn module_install_state(release: &str, package_name: &str) -> Result<(String, bool)> {
    let directory = PathBuf::from("/usr/lib/modules").join(release);
    if !directory.exists() {
        return Ok(("Not installed".into(), true));
    }
    if let Some(owner) = package_owner(&directory) {
        if owner == package_name {
            return Ok((format!("Already managed by {owner}; package update is possible"), true));
        }
        return Ok((
            format!("Directory is owned by package {owner}"),
            false,
        ));
    }
    Ok((
        "Unowned module directory already exists; pacman would encounter file conflicts".into(),
        false,
    ))
}

fn available_bytes(path: &Path) -> Option<u64> {
    let mut existing = path;
    while !existing.exists() {
        existing = existing.parent()?;
    }
    let output = Command::new("df").args(["-Pk"]).arg(existing).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&output.stdout).lines().last()?.to_owned();
    let fields = line.split_whitespace().collect::<Vec<_>>();
    fields.get(3)?.parse::<u64>().ok().map(|kib| kib * 1024)
}

fn inspect_validated_stage(validated: &ValidatedStage) -> Result<InstallReadiness> {
    let (arch_family, distro) = detect_arch_family();
    let architecture = std::env::consts::ARCH.to_owned();
    let current_kernel = command_output("uname", &["-r"]).unwrap_or_else(|| "unknown".into());
    let secure_boot = secure_boot_state();
    let kernel_config = fs::read_to_string(&validated.config)
        .context("reading staged kernel config for module-signing checks")?;
    let module_sig_force = kernel_config.lines().any(|line| line.trim() == "CONFIG_MODULE_SIG_FORCE=y");
    let module_sig_all = kernel_config.lines().any(|line| line.trim() == "CONFIG_MODULE_SIG_ALL=y");
    let module_signing = if module_sig_force {
        "Forced module signatures (trust not verified)".to_owned()
    } else if module_sig_all {
        "All modules are signed by the build; signer trust is not verified".to_owned()
    } else if kernel_config.lines().any(|line| line.trim() == "CONFIG_MODULE_SIG=y") {
        "Optional module signatures".to_owned()
    } else {
        "Disabled".to_owned()
    };
    let initramfs_backend = detect_initramfs_backend();
    let (module_state, module_directory_writable) =
        module_install_state(&validated.release, &validated.package_name)?;

    let mut blockers = Vec::new();
    let mut warnings = Vec::new();

    if !arch_family {
        blockers.push("The Arch package adapter supports Arch Linux and Arch-based distributions only.".into());
    }
    if architecture != "x86_64" {
        blockers.push(format!(
            "The first Arch package adapter currently supports x86_64 only, not {architecture}."
        ));
    }
    for tool in ["makepkg", "fakeroot", "zstd", "bsdtar"] {
        if !command_available(tool) {
            blockers.push(format!("Required packaging tool '{tool}' is missing."));
        }
    }
    if secure_boot == Some(true) {
        blockers.push("Secure Boot is enabled. This build has not been signed for your firmware trust chain; installation is disabled.".into());
    } else if secure_boot.is_none() && Path::new("/sys/firmware/efi").exists() {
        blockers.push("Secure Boot state is unknown on this UEFI system. Install mokutil or make the EFI SecureBoot variable readable, then check again.".into());
    }
    if module_sig_force {
        blockers.push("The kernel enforces module signatures, but this installer cannot verify the module-signing certificate trust chain. Installation is blocked.".into());
    }
    if current_kernel == validated.release {
        blockers.push("This is the currently running kernel. Boot the known-good kernel before installing or replacing this release.".into());
    }
    if !module_directory_writable {
        blockers.push(module_state.clone());
    }
    if initramfs_backend.starts_with("No supported") {
        blockers.push("No supported initramfs pacman hook was detected. Install the distribution's mkinitcpio or dracut integration before installing this kernel.".into());
    }
    if !command_available("pacman") {
        blockers.push("pacman is not available.".into());
    }
    let (boot, boot_mount_ready) = boot_mount();
    if boot_mount_ready == Some(false) {
        blockers.push("The /boot mount declared in fstab is not active. Mount the boot filesystem before installing a kernel.".into());
    } else if boot_mount_ready.is_none() {
        warnings.push("Could not verify the /boot mount. Confirm the boot filesystem is mounted before installation.".into());
    }

    let package_build_ready = arch_family
        && architecture == "x86_64"
        && ["makepkg", "fakeroot", "zstd", "bsdtar"]
            .iter()
            .all(|tool| command_available(tool));
    let installation_ready = blockers.is_empty();

    Ok(InstallReadiness {
        stage_directory: validated.stage_directory.clone(),
        distro,
        architecture,
        firmware_mode: if Path::new("/sys/firmware/efi").is_dir() {
            "UEFI".into()
        } else {
            "Legacy BIOS or EFI variables unavailable".into()
        },
        current_kernel,
        kernel_release: validated.release.clone(),
        package_name: validated.package_name.clone(),
        kernel_image_size_bytes: validated.kernel_image_size_bytes,
        modules_file_count: validated.modules_file_count,
        modules_total_bytes: validated.modules_total_bytes,
        secure_boot: match secure_boot {
            Some(true) => "Enabled (not supported for unsigned builds)".into(),
            Some(false) => "Disabled".into(),
            None => "Unknown".into(),
        },
        module_signing,
        bootloader: detect_bootloader(),
        initramfs_backend,
        boot_mount: boot,
        package_build_ready,
        installation_ready,
        blockers,
        warnings,
    })
}

pub fn inspect_stage(stage: &Path) -> Result<InstallReadiness> {
    let validated = validate_stage(stage)?;
    inspect_validated_stage(&validated)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn make_pkgbuild(stage: &ValidatedStage) -> String {
    let release = shell_quote(&stage.release);
    let package = shell_quote(&stage.package_name);
    let modules = shell_quote(&stage.modules.display().to_string());
    let image = shell_quote(&stage.kernel_image.display().to_string());
    let config = shell_quote(&stage.config.display().to_string());
    let system_map = stage
        .system_map
        .as_ref()
        .map(|path| shell_quote(&path.display().to_string()));

    let mut body = format!(
        r#"pkgname={package}
pkgver={}
pkgrel=1
pkgdesc='Linux kernel {} built by kernel-optimizer'
arch=('x86_64')
url='https://www.kernel.org/'
license=('GPL-2.0-only')
source=()
sha256sums=()
depends=('kmod')
options=('!strip')

package() {{
    local release={release}
    local pkgbase={package}
    local target="$pkgdir/usr/lib/modules/$release"
    local staged_modules={modules}
    install -d "$target"
    cp -a "$staged_modules"/. "$target"/
    if [[ -L "$target/build" ]]; then rm -f "$target/build"; fi
    if [[ -L "$target/source" ]]; then rm -f "$target/source"; fi
    install -Dm644 {image} "$target/vmlinuz"
    install -Dm644 {config} "$target/config"
    printf '%s\n' "$pkgbase" > "$target/pkgbase"
"#,
        stage.pkgver,
        stage.release,
    );
    if let Some(system_map) = system_map {
        body.push_str(&format!(
            "    install -Dm644 {system_map} \"$target/System.map\"\n"
        ));
    }
    body.push_str("}\n");
    body
}

struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_package_manifest(path: &Path, artifact: &PackageArtifact) -> Result<()> {
    let sidecar = PathBuf::from(format!("{}.manifest.json", path.display()));
    let contents = serde_json::to_vec_pretty(artifact)?;
    fs::write(&sidecar, contents)
        .with_context(|| format!("writing package manifest {}", sidecar.display()))
}

pub fn build_arch_package(stage: &Path, output_directory: Option<&Path>) -> Result<PackageArtifact> {
    let validated = validate_stage(stage)?;
    let readiness = inspect_validated_stage(&validated)?;
    if !readiness.package_build_ready {
        bail!(
            "Arch package prerequisites are not met: {}",
            readiness.blockers.join(" ")
        );
    }

    if unsafe { libc::geteuid() } == 0 {
        bail!("build the Arch package as an unprivileged user; do not run makepkg as root");
    }
    let output_dir = output_directory
        .map(Path::to_path_buf)
        .unwrap_or_else(|| stage.join("packages"));
    fs::create_dir_all(&output_dir)
        .with_context(|| format!("creating package output directory {}", output_dir.display()))?;
    let output_dir = output_dir.canonicalize()
        .with_context(|| format!("resolving package output directory {}", output_dir.display()))?;
    if let Some(free) = available_bytes(&output_dir) {
        let required = validated
            .modules_total_bytes
            .saturating_add(MIN_PACKAGE_FREE_SPACE);
        if free < required {
            bail!(
                "not enough free space to package the staged modules: {:.2} GiB available, approximately {:.2} GiB required",
                free as f64 / 1024.0_f64.powi(3),
                required as f64 / 1024.0_f64.powi(3)
            );
        }
    }

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let work_dir = Path::new(&validated.stage_directory).join(format!(".package-{}-{stamp}", std::process::id()));
    fs::create_dir(&work_dir)
        .with_context(|| format!("creating package workspace {}", work_dir.display()))?;
    let _cleanup = RemoveOnDrop(work_dir.clone());
    let pkgbuild = make_pkgbuild(&validated);
    fs::write(work_dir.join("PKGBUILD"), pkgbuild)?;

    let output = Command::new("makepkg")
        .args(["--force", "--nodeps", "--noconfirm"])
        .current_dir(&work_dir)
        .env("PKGDEST", &output_dir)
        .env("PKGEXT", ".pkg.tar.zst")
        .status()
        .context("starting makepkg; install base-devel on Arch/Garuda")?;
    if !output.success() {
        bail!("makepkg failed ({output})");
    }

    let prefix = format!("{}-", validated.package_name);
    let mut packages = fs::read_dir(&output_dir)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
                && path.to_string_lossy().ends_with(".pkg.tar.zst")
        })
        .collect::<Vec<_>>();
    packages.sort_by_key(|path| {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(UNIX_EPOCH)
    });
    let package_path = packages
        .pop()
        .context("makepkg completed but did not create the expected .pkg.tar.zst file")?;
    let sha256 = provenance::sha256_file(&package_path)?;
    let artifact = PackageArtifact {
        path: package_path.display().to_string(),
        package_name: validated.package_name,
        kernel_release: validated.release,
        stage_directory: validated.stage_directory.clone(),
        sha256,
        source_signer: validated.signer,
        modules_file_count: validated.modules_file_count,
        modules_total_bytes: validated.modules_total_bytes,
    };
    write_package_manifest(&package_path, &artifact)?;
    Ok(artifact)
}

fn archive_member(package: &Path, member: &str) -> Result<Vec<u8>> {
    let output = Command::new("bsdtar")
        .args(["-xOf"])
        .arg(package)
        .arg(member)
        .output()
        .context("reading generated package with bsdtar")?;
    if !output.status.success() {
        bail!(
            "could not read package member {member}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn validate_package_archive(package: &Path, manifest: &PackageArtifact) -> Result<()> {
    if !command_available("bsdtar") {
        bail!("bsdtar is required to inspect the package before installing it");
    }
    let listing = Command::new("bsdtar")
        .args(["-tf"])
        .arg(package)
        .output()
        .context("listing generated package contents with bsdtar")?;
    if !listing.status.success() {
        bail!(
            "package archive is not readable: {}",
            String::from_utf8_lossy(&listing.stderr).trim()
        );
    }

    let files = String::from_utf8_lossy(&listing.stdout)
        .lines()
        .map(|line| line.trim_start_matches("./").to_owned())
        .collect::<Vec<_>>();
    for entry in &files {
        let path = Path::new(entry);
        if path.is_absolute()
            || path.components().any(|component| matches!(component, Component::ParentDir))
        {
            bail!("package contains an unsafe archive path: {entry}");
        }
    }

    let prefix = format!("usr/lib/modules/{}/", manifest.kernel_release);
    for required in [
        format!("{prefix}vmlinuz"),
        format!("{prefix}config"),
        format!("{prefix}pkgbase"),
        format!("{prefix}modules.dep"),
        format!("{prefix}modules.alias"),
    ] {
        if !files.iter().any(|entry| entry.trim_end_matches('/') == required) {
            bail!("package is missing the required kernel artifact {required}");
        }
    }

    let pkginfo = String::from_utf8(archive_member(package, ".PKGINFO")?)
        .context("package .PKGINFO is not UTF-8")?;
    let package_name = pkginfo.lines()
        .find_map(|line| line.strip_prefix("pkgname = "))
        .context("package .PKGINFO has no pkgname")?;
    let architecture = pkginfo.lines()
        .find_map(|line| line.strip_prefix("arch = "))
        .context("package .PKGINFO has no architecture")?;
    if package_name != manifest.package_name || architecture != "x86_64" {
        bail!("package metadata does not match the expected name and x86_64 architecture");
    }

    let pkgbase = String::from_utf8(archive_member(package, &format!("{prefix}pkgbase"))?)
        .context("kernel pkgbase marker is not UTF-8")?;
    if pkgbase.trim() != manifest.package_name {
        bail!("kernel pkgbase marker does not match the package name");
    }
    Ok(())
}

fn package_manifest_path(package: &Path) -> PathBuf {
    PathBuf::from(format!("{}.manifest.json", package.display()))
}

pub fn install_arch_package(package: &Path, confirmed: bool) -> Result<()> {
    if !confirmed {
        bail!("installation requires explicit confirmation (--yes)");
    }
    if unsafe { libc::geteuid() } != 0 {
        bail!("install must run through an administrator authorization prompt (for example pkexec)");
    }
    if !Path::new("/etc/arch-release").exists()
        && !detect_arch_family().0
    {
        bail!("pacman installation is supported only on Arch Linux and Arch-based distributions");
    }
    if !command_available("pacman") {
        bail!("pacman is not available");
    }
    let package = package
        .canonicalize()
        .with_context(|| format!("resolving package path {}", package.display()))?;
    if !package.is_file() || !package.to_string_lossy().ends_with(".pkg.tar.zst") {
        bail!("expected a local .pkg.tar.zst package: {}", package.display());
    }

    let sidecar = package_manifest_path(&package);
    let manifest: PackageArtifact = serde_json::from_slice(
        &fs::read(&sidecar)
            .with_context(|| format!("reading generated package manifest {}", sidecar.display()))?,
    )
    .context("parsing generated package manifest")?;
    let manifest_package_path = PathBuf::from(&manifest.path).canonicalize()
        .context("resolving package path stored in generated package manifest")?;
    if manifest_package_path != package {
        bail!("package path does not match the generated package manifest");
    }
    if provenance::sha256_file(&package)? != manifest.sha256 {
        bail!("package SHA-256 does not match its generated manifest");
    }
    validate_package_archive(&package, &manifest)?;
    validate_release(&manifest.kernel_release)?;
    let readiness = inspect_stage(Path::new(&manifest.stage_directory))
        .context("rechecking staged build and local installation safety before pacman runs")?;
    if readiness.kernel_release != manifest.kernel_release || readiness.package_name != manifest.package_name {
        bail!("package manifest no longer matches the staged build it was created from");
    }
    if !readiness.installation_ready {
        bail!(
            "installation safety checks are no longer satisfied: {}",
            readiness.blockers.join(" ")
        );
    }
    if command_output("uname", &["-r"]).as_deref() == Some(&manifest.kernel_release) {
        bail!("refusing to replace the currently running kernel's module directory");
    }
    if secure_boot_state() != Some(false) {
        bail!("Secure Boot is enabled or could not be verified; unsigned kernel installation is blocked");
    }

    let query = Command::new("pacman")
        .args(["-Qp", "--print-format", "%n"])
        .arg(&package)
        .output()
        .context("querying local package metadata with pacman")?;
    if !query.status.success() {
        bail!(
            "pacman rejected the local package: {}",
            String::from_utf8_lossy(&query.stderr).trim()
        );
    }
    let detected_name = String::from_utf8_lossy(&query.stdout).trim().to_owned();
    if detected_name != manifest.package_name
        || !detected_name.starts_with(PACKAGE_PREFIX)
    {
        bail!(
            "package name mismatch: expected {}, found {}",
            manifest.package_name,
            detected_name
        );
    }

    let status = Command::new("pacman")
        .args(["-U", "--noconfirm"])
        .arg(&package)
        .status()
        .context("starting pacman package installation")?;
    if !status.success() {
        bail!("pacman failed to install {} ({status})", package.display());
    }
    println!("Installed {} for kernel release {}.", manifest.package_name, manifest.kernel_release);
    println!("The distribution's package hooks were allowed to run.");
    println!("No GRUB, systemd-boot, rEFInd or Secure Boot configuration was changed.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_release_validation_rejects_path_injection() {
        assert!(validate_release("7.3.4-ko").is_ok());
        assert!(validate_release("7.3.4+custom_1").is_ok());
        assert!(validate_release("../boot").is_err());
        assert!(validate_release("7.3.4/../../x").is_err());
        assert!(validate_release("").is_err());
        assert!(validate_release("release with spaces").is_err());
    }

    #[test]
    fn generated_package_names_are_arch_safe_and_release_specific() {
        assert_eq!(
            package_name("7.3.4-ko").unwrap(),
            "kernel-optimizer-7-3-4-ko"
        );
        assert_ne!(
            package_name("7.3.4-ko").unwrap(),
            package_name("7.3.4-ko2").unwrap()
        );
    }

    #[test]
    fn shell_quote_preserves_single_quotes() {
        assert_eq!(shell_quote("/tmp/a'b"), "'/tmp/a'\\''b'");
    }

    #[test]
    fn generated_pkgbuild_copies_modules_and_uses_the_pacman_hook_layout() {
        let root = std::env::temp_dir().join(format!(
            "kernel-optimizer-pkgbuild-test-{}",
            std::process::id()
        ));
        let stage = ValidatedStage {
            stage_directory: root.display().to_string(),
            release: "7.3.4-ko".into(),
            package_name: "kernel-optimizer-7-3-4-ko".into(),
            kernel_image: root.join("boot/vmlinuz-7.3.4-ko"),
            config: root.join("boot/config-7.3.4-ko"),
            system_map: Some(root.join("boot/System.map-7.3.4-ko")),
            modules: root.join("root/lib/modules/7.3.4-ko"),
            signer: "test signer".into(),
            pkgver: "7.3.4".into(),
            modules_file_count: 1,
            modules_total_bytes: 1,
            kernel_image_size_bytes: 1,
        };
        let build = make_pkgbuild(&stage);
        assert!(build.contains("install -Dm644"));
        assert!(build.contains("$target/vmlinuz"));
        assert!(build.contains("pkgbase"));
        assert!(build.contains("rm -f \"$target/build\""));
        assert!(build.contains("kernel-optimizer-7-3-4-ko"));
        assert!(build.contains("source=()"));
    }
}
