use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};


#[derive(Debug, Deserialize)]
struct Manifest {
    schema_version: u32,
    source: SourceManifest,
    build: BuildManifest,
    artifacts: ArtifactManifest,
}

#[derive(Debug, Deserialize)]
struct SourceManifest {
    signature_verified: bool,
}

#[derive(Debug, Deserialize)]
struct BuildManifest {
    effective_config_sha256: String,
}

#[derive(Debug, Deserialize)]
struct ArtifactManifest {
    kernel_release: String,
    kernel_image: String,
    kernel_image_sha256: String,
    system_map: Option<String>,
    system_map_sha256: Option<String>,
    modules_directory: String,
    modules_staged: bool,
    modules_tree_sha256: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PackageMetadata {
    schema_version: u32,
    package_name: String,
    package_version: String,
    architecture: String,
    kernel_release: String,
    source_signature_verified: bool,
    kernel_image_sha256: String,
    modules_tree_sha256: String,
    config_sha256: String,
    initramfs_backend: String,
}

fn valid_release(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
}

fn package_name_for_release(release: &str) -> Result<String> {
    if !valid_release(release) {
        bail!("invalid kernel release '{release}'");
    }
    let mut suffix = String::new();
    for character in release.chars() {
        match character {
            'A'..='Z' => {
                suffix.push_str("-u");
                suffix.push(character.to_ascii_lowercase());
            }
            'a'..='z' | '0'..='9' | '.' | '_' | '+' | '-' => suffix.push(character),
            _ => bail!("kernel release contains a character that cannot be represented in an Arch package name"),
        }
    }
    Ok(format!("linux-optimizer-{suffix}"))
}

fn valid_version(value: &str) -> bool {
    let parts = value.split('.').collect::<Vec<_>>();
    (2..=3).contains(&parts.len())
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

fn safe_relative_path(value: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("provenance contains an unsafe staged path: {value}");
    }
    Ok(path.to_path_buf())
}

fn resolve_staged_path(stage_dir: &Path, relative: &str) -> Result<PathBuf> {
    let relative = safe_relative_path(relative)?;
    let resolved = fs::canonicalize(stage_dir.join(relative))
        .context("resolving a path referenced by staged provenance")?;
    if !resolved.starts_with(stage_dir) {
        bail!("provenance path resolves outside the staging directory: {}", resolved.display());
    }
    Ok(resolved)
}

fn read_manifest(stage_dir: &Path, release: &str) -> Result<(Manifest, PathBuf, PathBuf)> {
    if !valid_release(release) {
        bail!("invalid kernel release '{release}'");
    }
    let manifest_path = stage_dir.join("provenance.json");
    let bytes = fs::read(&manifest_path)
        .with_context(|| format!("reading build provenance {}", manifest_path.display()))?;
    let manifest: Manifest = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing build provenance {}", manifest_path.display()))?;

    if manifest.schema_version != 2 {
        bail!(
            "unsupported provenance schema {}; expected schema 2",
            manifest.schema_version
        );
    }
    if !manifest.source.signature_verified {
        bail!("source signature is not marked verified in provenance; refusing to package");
    }
    if manifest.artifacts.kernel_release != release {
        bail!(
            "staged provenance is for {}, not requested release {release}",
            manifest.artifacts.kernel_release
        );
    }
    if !manifest.artifacts.modules_staged {
        bail!("provenance does not attest staged kernel modules");
    }

    let image = resolve_staged_path(stage_dir, &manifest.artifacts.kernel_image)?;
    let modules = resolve_staged_path(stage_dir, &manifest.artifacts.modules_directory)?;
    if !image.is_file() || !modules.is_dir() {
        bail!("staged kernel image or module tree referenced by provenance is missing");
    }

    Ok((manifest, image, modules))
}

fn validate_artifacts(
    stage_dir: &Path,
    release: &str,
) -> Result<(Manifest, PathBuf, PathBuf, PathBuf, Option<PathBuf>)> {
    let (manifest, image, modules) = read_manifest(stage_dir, release)?;
    let image_hash = crate::provenance::sha256_file(&image)?;
    if image_hash != manifest.artifacts.kernel_image_sha256 {
        bail!("staged kernel image hash does not match provenance");
    }

    let expected_modules_hash = manifest
        .artifacts
        .modules_tree_sha256
        .as_deref()
        .context("provenance does not contain a staged modules-tree hash")?;
    let (actual_modules_hash, _, _) = crate::provenance::sha256_tree(&modules)?;
    if actual_modules_hash != expected_modules_hash {
        bail!("staged modules tree hash does not match provenance");
    }

    for index in ["modules.dep", "modules.alias"] {
        let path = modules.join(index);
        if !path.is_file() || fs::metadata(&path)?.len() == 0 {
            bail!("staged module index {} is missing or empty", path.display());
        }
    }

    let config = resolve_staged_path(stage_dir, &format!("boot/config-{release}"))?;
    if !config.is_file() {
        bail!("staged kernel configuration is missing: {}", config.display());
    }
    let config_hash = crate::provenance::sha256_file(&config)?;
    if config_hash != manifest.build.effective_config_sha256 {
        bail!("staged kernel config hash does not match provenance");
    }
    let config_text = fs::read_to_string(&config)
        .with_context(|| format!("reading {}", config.display()))?;
    if config_text.lines().any(|line| line.trim() == "CONFIG_MODULE_SIG_FORCE=y") {
        bail!(
            "CONFIG_MODULE_SIG_FORCE=y is enabled, but this build has no attested module-signing step; refusing to package"
        );
    }

    let system_map = match (&manifest.artifacts.system_map, &manifest.artifacts.system_map_sha256) {
        (Some(relative), Some(expected_hash)) => {
            let path = resolve_staged_path(stage_dir, relative)?;
            if !path.is_file() || crate::provenance::sha256_file(&path)? != *expected_hash {
                bail!("staged System.map is missing or does not match provenance");
            }
            Some(path)
        }
        (None, None) => None,
        _ => bail!("provenance has incomplete System.map metadata"),
    };

    Ok((manifest, image, modules, config, system_map))
}

fn copy_module_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source).with_context(|| format!("reading {}", source.display()))? {
        let entry = entry?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&from)?;
        let file_type = metadata.file_type();

        if file_type.is_symlink() {
            let name = entry.file_name();
            if matches!(name.to_str(), Some("build" | "source")) {
                continue;
            }
            bail!(
                "unexpected symlink in staged module tree: {}; inspect it before packaging",
                from.display()
            );
        }
        if file_type.is_dir() {
            copy_module_tree(&from, &to)?;
        } else if file_type.is_file() {
            fs::copy(&from, &to)
                .with_context(|| format!("copying module artifact {}", from.display()))?;
            fs::set_permissions(&to, metadata.permissions())?;
        } else {
            bail!("unsupported special file in staged module tree: {}", from.display());
        }
    }
    Ok(())
}

fn arch_name() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("x86_64"),
        other => bail!(
            "the Arch package adapter currently supports x86_64 only; detected {other}"
        ),
    }
}

struct PackageSourceInput<'a> {
    workspace: &'a Path,
    stage_dir: &'a Path,
    release: &'a str,
    version: &'a str,
    metadata: &'a PackageMetadata,
    image: &'a Path,
    modules: &'a Path,
    config: &'a Path,
    system_map: Option<&'a Path>,
    preset: Option<&'a str>,
    initramfs_backend: &'a str,
    package_name: &'a str,
}

fn write_package_sources(input: PackageSourceInput<'_>) -> Result<()> {
    let PackageSourceInput {
        workspace,
        stage_dir,
        release,
        version,
        metadata,
        image,
        modules,
        config,
        system_map,
        preset,
        initramfs_backend,
        package_name,
    } = input;
    let payload = workspace.join("payload");
    let payload_modules = payload.join("modules");
    copy_module_tree(modules, &payload_modules)?;
    fs::copy(image, payload.join("vmlinuz"))?;
    fs::copy(config, payload.join("config"))?;
    if let Some(system_map) = system_map {
        fs::copy(system_map, payload.join("System.map"))?;
    }
    fs::copy(stage_dir.join("provenance.json"), payload.join("provenance.json"))?;
    let metadata_bytes = serde_json::to_vec_pretty(metadata)?;
    fs::write(payload.join("package.json"), metadata_bytes)?;
    let preset_filename = format!("{package_name}.preset");
    if let Some(preset) = preset {
        fs::write(payload.join(&preset_filename), preset)?;
    }

    let system_map_install = if system_map.is_some() {
        "  install -Dm644 \"$srcdir/payload/System.map\" \"$moddir/System.map\"\n"
    } else {
        ""
    };

    let preset_install = if preset.is_some() {
        format!(
            "  install -Dm644 \"$srcdir/payload/{preset_filename}\" \"$pkgdir/etc/mkinitcpio.d/{preset_filename}\"\n"
        )
    } else {
        String::new()
    };
    let kernel_image_path = format!("vmlinuz-{package_name}");
    let pkgbuild = format!(
        r#"pkgname={package_name}
pkgver={version}
pkgrel=1
pkgdesc='Custom upstream Linux kernel built and verified by kernel-optimizer'
arch=('x86_64')
url='https://github.com/sxiii/kernel-optimizer'
license=('GPL-2.0-only')
depends=('kmod' '{initramfs_backend}')
options=('!strip')
KERNEL_RELEASE='{release}'

package() {{
  local moddir="$pkgdir/usr/lib/modules/$KERNEL_RELEASE"
  install -d "$moddir"
  cp -a "$srcdir/payload/modules/." "$moddir/"
  rm -f "$moddir/build" "$moddir/source"
  install -Dm644 "$srcdir/payload/vmlinuz" "$moddir/vmlinuz"
  install -Dm644 "$srcdir/payload/vmlinuz" "$pkgdir/boot/{kernel_image_path}"
  install -Dm644 "$srcdir/payload/config" "$moddir/config"
{system_map_install}  printf '%s\\n' '{package_name}' > "$moddir/pkgbase"
{preset_install}  install -Dm644 "$srcdir/payload/provenance.json" "$pkgdir/usr/share/kernel-optimizer/provenance.json"
  install -Dm644 "$srcdir/payload/package.json" "$pkgdir/usr/share/kernel-optimizer/package.json"
}}
"#
    );
    fs::write(workspace.join("PKGBUILD"), pkgbuild)
        .context("writing Arch PKGBUILD")?;
    Ok(())
}

fn make_preset(release: &str, package_name: &str) -> String {
    format!(
        "# Generated by kernel-optimizer. Review before changing boot layout.\n\\
PRESETS=('default' 'fallback')\n\\
ALL_kver='/boot/vmlinuz-{package_name}'\n\\
ALL_kerneldest='/boot/vmlinuz-{package_name}'\n\\
default_image='/boot/initramfs-{package_name}.img'\n\\
fallback_image='/boot/initramfs-{package_name}-fallback.img'\n\\
fallback_options='-S autodetect'\n\\
# Kernel release: {release}\n"
    )
}

fn tool_available(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn ensure_tool(program: &str) -> Result<()> {
    if !tool_available(program) {
        bail!("required tool '{program}' is not available");
    }
    Ok(())
}

fn active_pacman_hook(name: &str) -> Option<PathBuf> {
    let system = PathBuf::from("/usr/share/libalpm/hooks").join(name);
    let local = PathBuf::from("/etc/pacman.d/hooks").join(name);

    // A local hook with the same name takes precedence. Respect /dev/null masks.
    match fs::symlink_metadata(&local) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                && fs::canonicalize(&local).is_ok_and(|target| target == Path::new("/dev/null"))
            {
                None
            } else if local.is_file() {
                Some(local)
            } else {
                None
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => system.is_file().then_some(system),
        Err(_) => None,
    }
}

fn hook_exec(hook: &Path) -> Option<PathBuf> {
    let contents = fs::read_to_string(hook).ok()?;
    contents.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim() == "Exec").then(|| PathBuf::from(value.trim()))
    })
}

fn executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && (metadata.permissions().mode() & 0o111) != 0)
}

fn hook_targets_kernel_image_or_pkgbase(hook: &Path) -> bool {
    fs::read_to_string(hook).is_ok_and(|contents| {
        contents.lines().any(|line| {
            let line = line.trim();
            line.starts_with("Target")
                && (line.contains("usr/lib/modules/*/vmlinuz")
                    || line.contains("usr/lib/modules/*/pkgbase"))
        })
    })
}

fn mkinitcpio_hook_exists() -> bool {
    active_pacman_hook("90-mkinitcpio-install.hook")
        .and_then(|hook| {
            let exec = hook_exec(&hook)?;
            (hook_targets_kernel_image_or_pkgbase(&hook) && executable_file(&exec)).then_some(hook)
        })
        .is_some()
}

fn dracut_hook_exists() -> bool {
    active_pacman_hook("90-dracut-install.hook")
        .and_then(|hook| {
            let exec = hook_exec(&hook)?;
            (hook_targets_kernel_image_or_pkgbase(&hook) && executable_file(&exec)).then_some(hook)
        })
        .is_some()
}
fn detect_initramfs_backend() -> Result<&'static str> {
    let mkinitcpio = tool_available("mkinitcpio") && mkinitcpio_hook_exists();
    let dracut = tool_available("dracut") && dracut_hook_exists();
    match (mkinitcpio, dracut) {
        (true, false) => Ok("mkinitcpio"),
        (false, true) => Ok("dracut"),
        (false, false) => bail!(
            "no supported initramfs integration found; need mkinitcpio with its pacman hook or dracut with its pacman hook"
        ),
        (true, true) => bail!(
            "both mkinitcpio and dracut pacman integrations are active; disable one before building a kernel package to avoid duplicate initramfs generation"
        ),
    }
}

fn validate_initramfs_backend(backend: &str) -> Result<()> {
    match backend {
        "mkinitcpio" if tool_available("mkinitcpio") && mkinitcpio_hook_exists() => Ok(()),
        "dracut" if tool_available("dracut") && dracut_hook_exists() => Ok(()),
        "mkinitcpio" => bail!("the package expects mkinitcpio, but its executable or pacman hook is missing"),
        "dracut" => bail!("the package expects dracut, but its executable or pacman hook is missing"),
        other => bail!("unsupported initramfs backend in package metadata: {other}"),
    }
}

pub fn build(
    stage_dir: &Path,
    release: &str,
    version: &str,
    output_dir: &Path,
) -> Result<PathBuf> {
    if !valid_version(version) {
        bail!("invalid package version '{version}'");
    }
    if std::env::consts::OS != "linux" {
        bail!("the Arch package adapter can only run on Linux");
    }
    if unsafe { libc::geteuid() } == 0 {
        bail!("do not build an Arch package as root; run makepkg as your normal user");
    }
    let architecture = arch_name()?;
    ensure_tool("makepkg")?;
    ensure_tool("pacman")?;
    let initramfs_backend = detect_initramfs_backend()?;

    let stage_dir = fs::canonicalize(stage_dir)
        .with_context(|| format!("resolving staging directory {}", stage_dir.display()))?;
    let (manifest, image, modules, config, system_map) =
        validate_artifacts(&stage_dir, release)?;

    let output_dir = if output_dir.is_absolute() {
        output_dir.to_path_buf()
    } else {
        std::env::current_dir()?.join(output_dir)
    };
    fs::create_dir_all(&output_dir)
        .with_context(|| format!("creating package output directory {}", output_dir.display()))?;
    let output_dir = fs::canonicalize(&output_dir)?;

    let package_name = package_name_for_release(release)?;
    let metadata = PackageMetadata {
        schema_version: 1,
        package_name: package_name.clone(),
        package_version: version.into(),
        architecture: architecture.into(),
        kernel_release: release.into(),
        source_signature_verified: manifest.source.signature_verified,
        kernel_image_sha256: manifest.artifacts.kernel_image_sha256.clone(),
        modules_tree_sha256: manifest.artifacts.modules_tree_sha256.clone().unwrap_or_default(),
        config_sha256: crate::provenance::sha256_file(&config)?,
        initramfs_backend: initramfs_backend.into(),
    };
    let workspace = output_dir.join(format!(".{package_name}-build-{}-{}", release, std::process::id()));
    if workspace.exists() {
        fs::remove_dir_all(&workspace)
            .with_context(|| format!("removing stale package workspace {}", workspace.display()))?;
    }
    fs::create_dir_all(&workspace)?;

    let preset = if initramfs_backend == "mkinitcpio" {
        Some(make_preset(release, &package_name))
    } else {
        None
    };
    let build_result = (|| -> Result<PathBuf> {
        write_package_sources(PackageSourceInput {
            workspace: &workspace,
            stage_dir: &stage_dir,
            release,
            version,
            metadata: &metadata,
            image: &image,
            modules: &modules,
            config: &config,
            system_map: system_map.as_deref(),
            preset: preset.as_deref(),
            initramfs_backend,
            package_name: &package_name,
        })?;

        let listed = Command::new("makepkg")
            .arg("--packagelist")
            .current_dir(&workspace)
            .env("PKGDEST", &output_dir)
            .output()
            .context("asking makepkg for the output package path")?;
        if !listed.status.success() {
            bail!(
                "makepkg --packagelist failed: {}",
                String::from_utf8_lossy(&listed.stderr).trim()
            );
        }
        let listed_text = String::from_utf8_lossy(&listed.stdout);
        let package_path = listed_text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .next_back()
            .map(PathBuf::from)
            .context("makepkg did not report an output package path")?;
        if package_path.exists() {
            bail!(
                "package already exists at {}; remove it explicitly before rebuilding",
                package_path.display()
            );
        }

        println!("Building Arch package with makepkg...");
        let status = Command::new("makepkg")
            .args(["--noconfirm"])
            .current_dir(&workspace)
            .env("PKGDEST", &output_dir)
            .status()
            .context("running makepkg")?;
        if !status.success() {
            bail!("makepkg failed with status {status}");
        }
        if !package_path.is_file() || fs::metadata(&package_path)?.len() == 0 {
            bail!("makepkg succeeded but the expected package file is missing");
        }
        Ok(package_path)
    })();

    match build_result {
        Ok(path) => {
            let _ = fs::remove_dir_all(&workspace);
            println!("KERNEL_OPTIMIZER_PACKAGE={}", path.display());
            Ok(path)
        }
        Err(error) => {
            eprintln!("Package workspace retained for inspection: {}", workspace.display());
            Err(error)
        }
    }
}

fn run_text(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn ensure_boot_mount_ready() -> Result<()> {
    if Path::new("/efi/loader/loader.conf").is_file()
        && !Path::new("/boot/loader/loader.conf").is_file()
    {
        bail!("systemd-boot is using /efi as its ESP; this package adapter installs files under /boot and does not manage /efi entries");
    }
    let configured = run_text(
        "findmnt",
        &["--fstab", "--noheadings", "--output", "TARGET", "--target", "/boot"],
    )
    .unwrap_or_default();
    if configured.trim() == "/boot" {
        let mounted = run_text(
            "findmnt",
            &["--noheadings", "--output", "TARGET", "--target", "/boot"],
        )
        .unwrap_or_default();
        if mounted.trim() != "/boot" {
            bail!(
                "/boot is configured as a separate filesystem but is not mounted; mount it before installing the kernel package"
            );
        }
    }
    Ok(())
}

fn secure_boot_is_enabled() -> Result<bool> {
    let output = match Command::new("mokutil").arg("--sb-state").output() {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout).to_lowercase(),
        Ok(_) if Path::new("/sys/firmware/efi/efivars").is_dir() => {
            bail!("mokutil could not determine Secure Boot state; install or repair mokutil and retry")
        }
        Ok(_) => return Ok(false),
        Err(_) if Path::new("/sys/firmware/efi/efivars").is_dir() => {
            bail!("mokutil is required to check Secure Boot on this UEFI system; install mokutil and retry")
        }
        Err(_) => return Ok(false),
    };
    if output.contains("secureboot enabled") || output.contains("secure boot enabled") {
        return Ok(true);
    }
    if output.contains("secureboot disabled") || output.contains("secure boot disabled") {
        return Ok(false);
    }
    if Path::new("/sys/firmware/efi/efivars").is_dir() {
        bail!("could not determine Secure Boot state from mokutil; install mokutil and retry");
    }
    Ok(false)
}

pub fn install(package_path: &Path, yes: bool) -> Result<()> {
    if !yes {
        bail!("package installation changes the live system; review the plan and repeat with --yes");
    }
    if unsafe { libc::geteuid() } != 0 {
        bail!("package installation requires administrator privileges; use pkexec or sudo");
    }
    if !package_path.is_file() {
        bail!("package file does not exist: {}", package_path.display());
    }
    ensure_tool("pacman")?;
    if secure_boot_is_enabled()? {
        bail!("Secure Boot is enabled; kernel/module signing is not implemented by this installer");
    }
    ensure_boot_mount_ready()?;

    let package_path = fs::canonicalize(package_path)?;
    let pkg_info = run_text(
        "pacman",
        &[
            "-Qp",
            "--print-format",
            "%n|%v|%a",
            package_path.to_str().context("package path is not UTF-8")?,
        ],
    )?;
    let mut fields = pkg_info.split('|');
    let name = fields.next().unwrap_or_default();
    let _version = fields.next().unwrap_or_default();
    let arch = fields.next().unwrap_or_default();
    if arch != arch_name()? {
        bail!("package architecture mismatch: expected {}, got {pkg_info}", arch_name()?);
    }

    let metadata_bytes = Command::new("bsdtar")
        .args(["-xOf"])
        .arg(&package_path)
        .arg("usr/share/kernel-optimizer/package.json")
        .output()
        .context("reading kernel-optimizer package metadata from archive")?;
    if !metadata_bytes.status.success() {
        bail!("package does not contain kernel-optimizer metadata");
    }
    let metadata: PackageMetadata = serde_json::from_slice(&metadata_bytes.stdout)
        .context("parsing kernel-optimizer package metadata")?;
    let expected_package_name = package_name_for_release(&metadata.kernel_release)?;
    if metadata.schema_version != 1
        || metadata.package_name != expected_package_name
        || name != metadata.package_name.as_str()
        || metadata.architecture != arch_name()?
        || !metadata.source_signature_verified
        || !valid_release(&metadata.kernel_release)
    {
        bail!("package metadata or package name failed validation; refusing installation");
    }
    validate_initramfs_backend(&metadata.initramfs_backend)?;

    let installed = Command::new("pacman")
        .arg("-Q")
        .arg(&metadata.package_name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    let modules_dir = PathBuf::from("/usr/lib/modules").join(&metadata.kernel_release);
    if modules_dir.exists() && !installed {
        bail!(
            "module directory {} already exists but is not owned by an installed {} package; refusing a potentially destructive file conflict",
            modules_dir.display(),
            metadata.package_name
        );
    }
    let preset = PathBuf::from("/etc/mkinitcpio.d").join(format!("{}.preset", metadata.package_name));
    if metadata.initramfs_backend == "mkinitcpio" && preset.exists() && !installed {
        bail!(
            "{} already exists; move or back it up explicitly before installing",
            preset.display()
        );
    }
    let boot_image = PathBuf::from("/boot").join(format!("vmlinuz-{}", metadata.package_name));
    if boot_image.exists() && !installed {
        bail!(
            "{} already exists but the {} package is not installed; refusing to overwrite it",
            boot_image.display(),
            metadata.package_name
        );
    }
    for image in [
        PathBuf::from("/boot").join(format!("initramfs-{}.img", metadata.package_name)),
        PathBuf::from("/boot").join(format!("initramfs-{}-fallback.img", metadata.package_name)),
    ] {
        if image.exists() && !installed {
            bail!(
                "{} already exists but the {} package is not installed; refusing to overwrite it",
                image.display(),
                metadata.package_name
            );
        }
    }

    println!("Installing {} for kernel release {} using pacman.", metadata.package_name, metadata.kernel_release);
    println!(
        "Pacman will run the {} hook to generate initramfs images.",
        metadata.initramfs_backend
    );
    println!("No GRUB, systemd-boot, EFI or Secure Boot settings will be changed.");
    let status = Command::new("pacman")
        .args(["-U", "--noconfirm"])
        .arg(&package_path)
        .status()
        .context("starting pacman package installation")?;
    if !status.success() {
        bail!("pacman package installation failed with status {status}");
    }
    println!("Kernel package installed. Review /boot and add or update a bootloader entry manually before rebooting.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_and_version_validation_rejects_shell_and_path_syntax() {
        assert!(valid_release("7.3.4-ko"));
        assert!(valid_release("7.3.4-custom+1"));
        assert!(!valid_release("../boot"));
        assert!(!valid_release("7.3.4; touch /tmp/x"));
        assert!(valid_version("7.3.4"));
        assert!(!valid_version("7.3.4-ko"));
        assert!(!valid_version("7.3.4/../../"));
    }

    #[test]
    fn provenance_paths_cannot_escape_staging() {
        assert!(safe_relative_path("boot/vmlinuz-7.3.4").is_ok());
        assert!(safe_relative_path("../etc/shadow").is_err());
        assert!(safe_relative_path("/boot/vmlinuz").is_err());
    }

    #[test]
    fn hook_target_match_requires_kernel_vmlinuz_or_pkgbase() {
        let root = std::env::temp_dir().join(format!(
            "kernel-optimizer-hook-target-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let hook = root.join("90-dracut-install.hook");
        fs::write(
            &hook,
            "[Trigger]\nType = Path\nTarget = usr/lib/modules/*/vmlinuz\n[Action]\nExec = /usr/bin/dracut\nNeedsTargets\n",
        )
        .unwrap();
        assert!(hook_targets_kernel_image_or_pkgbase(&hook));
        fs::write(
            &hook,
            "[Trigger]\nType = Path\nTarget = usr/share/something\n[Action]\nExec = /usr/bin/dracut\n",
        )
        .unwrap();
        assert!(!hook_targets_kernel_image_or_pkgbase(&hook));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn package_names_are_unique_per_kernel_release() {
        assert_eq!(package_name_for_release("7.3.4-ko").unwrap(), "linux-optimizer-7.3.4-ko");
        assert_ne!(
            package_name_for_release("7.3.4+ko").unwrap(),
            package_name_for_release("7.3.4-ko").unwrap()
        );
        assert_ne!(
            package_name_for_release("7.3.4-ko").unwrap(),
            package_name_for_release("7.3.5-ko").unwrap()
        );
    }

    #[test]
    fn mkinitcpio_preset_uses_unique_kernel_and_initramfs_names() {
        let package_name = package_name_for_release("7.3.4-ko").unwrap();
        let preset = make_preset("7.3.4-ko", &package_name);
        assert!(preset.contains("ALL_kver='/boot/vmlinuz-linux-optimizer-7-3-4-ko'"));
        assert!(preset.contains("default_image='/boot/initramfs-linux-optimizer-7-3-4-ko.img'"));
        assert!(preset.contains("fallback_image='/boot/initramfs-linux-optimizer-7-3-4-ko-fallback.img'"));
    }
}
