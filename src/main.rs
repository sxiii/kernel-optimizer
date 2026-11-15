use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
mod tuning;
use tuning::Workload;
mod hardware;
mod benchmark;
mod recommendations;
mod tui;
mod provenance;
mod source_adapters;
mod arch_package;
mod gui;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{fs, path::{Path, PathBuf}, process::Command, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};

#[derive(Parser)]
#[command(name="kernel-optimizer", version, about="Hardware-aware Linux kernel builder")]
struct Cli { #[command(subcommand)] command: Commands }

#[derive(Subcommand)]
enum Commands {
    /// Inventory CPU, memory, PCI devices, block devices and current boot.
    Inspect,
    /// List supported upstream and optimized kernel source tracks.
    Sources,
    /// Print structured hardware and power-management inventory.
    Hardware,
    /// Generate hardware-aware recommendations without applying changes.
    Recommend { #[arg(long, value_enum, default_value="desktop")] profile: Workload },
    /// Open the native desktop interface.
    Gui,
    /// Open the interactive, read-only terminal dashboard.
    Tui { #[arg(long, value_enum, default_value="desktop")] profile: Workload },
    /// Run CPU and memory benchmarks; optionally test an iperf3 server.
    Benchmark {
        #[arg(long, default_value="kernel-optimizer-benchmark.json")]
        output: PathBuf,
        #[arg(long, default_value_t=5)]
        iterations: usize,
        #[arg(long, default_value="manual")]
        profile: String,
        #[arg(long, default_value_t=5_000_000)]
        cpu_rounds: u64,
        #[arg(long, default_value_t=64)]
        memory_mib: usize,
        /// Run fio against a temporary file in this existing directory.
        #[arg(long)]
        fio_path: Option<PathBuf>,
        #[arg(long, default_value_t=256)]
        fio_size_mib: usize,
        #[arg(long, default_value_t=10)]
        fio_runtime_seconds: usize,
        /// Run iperf3 against an explicitly supplied server.
        #[arg(long)]
        iperf_server: Option<String>,
    },
    /// Compare two saved benchmark reports.
    Compare {
        #[arg(long)]
        baseline: PathBuf,
        #[arg(long)]
        candidate: PathBuf,
    },
    /// Run a read-only pre-build environment and profile check.
    Doctor { #[arg(long, default_value="kernel-optimizer.toml")] profile: PathBuf },
    /// Create or print an example build profile.
    Profile { #[arg(long, default_value="kernel-optimizer.toml")] file: PathBuf, #[arg(long)] create: bool },
    /// Fetch, configure, compile and stage a kernel. Installation requires a separate explicit step.
    Build { #[arg(long, default_value="kernel-optimizer.toml")] profile: PathBuf, #[arg(long)] dry_run: bool },
    /// Verify and write metadata for an already-built kernel in the staging directory; does not compile.
    Finalize {
        #[arg(long, default_value="kernel-optimizer.toml")]
        profile: PathBuf,
        /// Upstream kernel version (for example 7.3.4); inferred from staged modules when omitted.
        #[arg(long)]
        version: Option<String>,
    },
    /// Generate/apply workload-aware sysctl and I/O scheduler settings.
    Tune { #[arg(long, value_enum, default_value="desktop")] profile: Workload, #[arg(long)] dry_run: bool },
    /// Remove optimizer-managed sysctl and udev drop-ins.
    Revert { #[arg(long)] dry_run: bool },
    /// Build a reviewable Arch Linux package from verified staged kernel artifacts.
    ArchPackage {
        #[arg(long, default_value="kernel-optimizer.toml")]
        profile: PathBuf,
        /// Upstream kernel version (inferred when omitted).
        #[arg(long)]
        version: Option<String>,
        /// Package output directory (defaults to <stage_dir>/packages).
        #[arg(long)]
        output_dir: Option<PathBuf>,
    },
    /// Install a previously built kernel-optimizer Arch package via pacman.
    Install {
        #[arg(long)]
        package: PathBuf,
        /// Acknowledge the package transaction after reviewing the plan.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Profile {
    source: String, version: String, local_version: String, jobs: usize,
    config_base: String, compiler: String, optimization: String,
    use_mold: bool, ccache: bool, build_dir: PathBuf, stage_dir: PathBuf,
    enable_lto: bool, enable_bpf: bool, menuconfig: bool,
    allow_unverified_sources: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    gpg_home: Option<PathBuf>,
}
impl Default for Profile {
    fn default() -> Self { Self {
        source:"linux".into(), version:"stable".into(), local_version:"-ko".into(),
        jobs: std::thread::available_parallelism().map(|x| x.get()).unwrap_or(2),
        config_base:"current".into(), compiler:"gcc".into(), optimization:"native".into(),
        use_mold:false, ccache:true, build_dir:PathBuf::from("/var/tmp/kernel-optimizer/build"),
        stage_dir:PathBuf::from("/var/tmp/kernel-optimizer/stage"),
        enable_lto:false, enable_bpf:true, menuconfig:false, allow_unverified_sources:false, gpg_home:None,
    }}
}

fn output(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|result| result.status.success())
        .map(|result| String::from_utf8_lossy(&result.stdout).trim().to_owned())
        .unwrap_or_default()
}

fn sources() {
    println!("SOURCE            STATUS       TRACK / PURPOSE");
    println!("linux             SUPPORTED    Latest stable upstream kernel (kernel.org; signed tarball)");
    println!("linux-lts          SUPPORTED    Latest longterm upstream kernel (kernel.org; signed tarball)");
    println!("linux-zen          SUPPORTED    Kernel.org base + signed Zen patch");
    println!("linux-rt           SUPPORTED    Kernel.org base + signed PREEMPT_RT patch");
    println!("linux-cachyos      OPT-IN       CachyOS release archive; explicit unverified-source opt-in required");
    println!("linux-tkg          OPT-IN       Pinned TKG EEVDF patch subset; explicit unverified-source opt-in required");
    println!("\nCachyOS and TKG require allow_unverified_sources = true and are excluded from verified package builds.");
}
fn profile(file:&Path,create:bool)->Result<()> {
    if create { let p=toml::to_string_pretty(&Profile::default())?; fs::write(file,p).with_context(||format!("writing {}",file.display()))?; println!("Created {}",file.display()); }
    else { println!("{}",fs::read_to_string(file).with_context(||format!("reading {}",file.display()))?); }
    Ok(())
}
fn run_checked(cmd:&mut Command)->Result<()> {
    let shown=format!("{cmd:?}"); let status=cmd.status().with_context(||format!("starting {shown}"))?;
    if !status.success(){bail!("command failed ({status}): {shown}");} Ok(())
}
fn latest_release(moniker: &str)->Result<String> {
    let client=reqwest::blocking::Client::builder().timeout(Duration::from_secs(20)).user_agent("kernel-optimizer/0.2").build()?;
    let body=client.get("https://www.kernel.org/releases.json").send()?.error_for_status()?.text()?;
    let v:serde_json::Value=serde_json::from_str(&body)?;
    let versions=v["releases"].as_array().context("kernel.org releases response missing releases")?;
    versions.iter().find(|x|x["moniker"].as_str()==Some(moniker))
        .and_then(|x|x["version"].as_str()).map(str::to_string)
        .with_context(|| format!("kernel.org {moniker} release not found"))
}

fn release_moniker(source: &str)->Result<&'static str> {
    match source {
        "linux" => Ok("stable"),
        "linux-lts" => Ok("longterm"),
        "linux-zen" | "linux-cachyos" | "linux-tkg" | "linux-rt" => Ok("stable"),
        other => bail!("unknown kernel source '{other}'"),
    }
}
fn ensure_kernel_archive(archive: &Path, url: &str) -> Result<()> {
    let cached_is_valid = archive.exists()
        && Command::new("xz")
            .arg("-t")
            .arg(archive)
            .status()
            .is_ok_and(|status| status.success());
    if cached_is_valid {
        return Ok(());
    }
    if archive.exists() {
        eprintln!(
            "Cached kernel archive {} failed integrity testing; downloading a fresh copy.",
            archive.display()
        );
        fs::remove_file(archive)
            .with_context(|| format!("removing invalid archive {}", archive.display()))?;
    }

    let temporary = archive.with_extension("download");
    if temporary.exists() {
        fs::remove_file(&temporary)?;
    }
    let download_result = (|| -> Result<()> {
        run_checked(
            Command::new("curl")
                .args(["--fail", "--location", "--retry", "3", "--output"])
                .arg(&temporary)
                .arg(url),
        )?;
        run_checked(Command::new("xz").arg("-t").arg(&temporary))?;
        fs::rename(&temporary, archive)
            .with_context(|| format!("promoting verified archive to {}", archive.display()))?;
        Ok(())
    })();
    if download_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    download_result
}

fn kernel_release(source: &Path) -> Result<String> {
    let output = Command::new("make")
        .current_dir(source)
        .arg("kernelrelease")
        .output()
        .context("running make kernelrelease")?;
    if !output.status.success() {
        bail!("make kernelrelease failed");
    }
    let release = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if release.is_empty() {
        bail!("kernel build returned an empty kernel release string");
    }
    Ok(release)
}

fn kernel_image_candidates(architecture: &str) -> Result<&'static [&'static str]> {
    match architecture {
        "x86_64" | "x86" => Ok(&["arch/x86/boot/bzImage"]),
        "aarch64" => Ok(&["arch/arm64/boot/Image", "arch/arm64/boot/Image.gz"]),
        "arm" => Ok(&["arch/arm/boot/zImage", "arch/arm/boot/Image"]),
        "riscv64" => Ok(&["arch/riscv/boot/Image", "arch/riscv/boot/Image.gz"]),
        "powerpc64" | "powerpc64le" => Ok(&["arch/powerpc/boot/zImage"]),
        "s390x" => Ok(&["arch/s390/boot/bzImage"]),
        other => bail!(
            "automatic boot-image staging is not yet supported for architecture '{other}'"
        ),
    }
}

// Do not call the kernel's "make install" target here. On distributions such as
// Arch it invokes installkernel/kernel-install hooks, which may run dracut against
// the live host's /lib/modules tree instead of this build's staged module tree.
fn stage_kernel_boot_artifacts(
    source: &Path,
    boot_dir: &Path,
    release: &str,
    architecture: &str,
) -> Result<String> {
    let candidates = kernel_image_candidates(architecture)?;
    let image_source = candidates
        .iter()
        .map(|relative| source.join(relative))
        .find(|candidate| candidate.is_file())
        .with_context(|| {
            format!(
                "could not find a built kernel image for {architecture}; checked: {}",
                candidates.join(", ")
            )
        })?;

    fs::create_dir_all(boot_dir)
        .with_context(|| format!("creating staged boot directory {}", boot_dir.display()))?;

    let image_name = format!("vmlinuz-{release}");
    let image_destination = boot_dir.join(&image_name);
    fs::copy(&image_source, &image_destination).with_context(|| {
        format!(
            "copying built kernel image {} to {}",
            image_source.display(),
            image_destination.display()
        )
    })?;

    let config_source = source.join(".config");
    if !config_source.is_file() {
        bail!("built kernel configuration is missing: {}", config_source.display());
    }
    let config_destination = boot_dir.join(format!("config-{release}"));
    fs::copy(&config_source, &config_destination).with_context(|| {
        format!(
            "copying kernel config {} to {}",
            config_source.display(),
            config_destination.display()
        )
    })?;

    let system_map_source = source.join("System.map");
    if system_map_source.is_file() {
        let system_map_destination = boot_dir.join(format!("System.map-{release}"));
        fs::copy(&system_map_source, &system_map_destination).with_context(|| {
            format!(
                "copying System.map {} to {}",
                system_map_source.display(),
                system_map_destination.display()
            )
        })?;
    }

    Ok(image_name)
}

fn verify_staged_kernel(stage: &Path, release: &str) -> Result<(String, String, bool)> {
    let boot_dir = stage.join("boot");
    let entries = fs::read_dir(&boot_dir)
        .with_context(|| format!("reading staged boot directory {}", boot_dir.display()))?;
    let mut images = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let looks_like_image = name.starts_with("vmlinuz")
            || name == "Image"
            || name == "Image.gz"
            || name == "bzImage"
            || name == "zImage";
        if looks_like_image && path.is_file() {
            images.push(path);
        }
    }
    let image = images
        .iter()
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().contains(release))
        })
        .cloned()
        .or_else(|| {
            if images.len() == 1 {
                images.into_iter().next()
            } else {
                None
            }
        })
        .context(
            "could not uniquely identify the just-built kernel image in staging; clear old images and rebuild",
        )?;
    if fs::metadata(&image)?.len() == 0 {
        bail!("staged kernel image is empty: {}", image.display());
    }

    let digest = provenance::sha256_file(&image)
        .with_context(|| format!("hashing staged kernel image {}", image.display()))?;

    let modules_dir = stage.join("root/lib/modules").join(release);
    let modules_staged = modules_dir.is_dir();
    Ok((
        image.file_name().unwrap_or_default().to_string_lossy().to_string(),
        digest,
        modules_staged,
    ))
}

fn validate_profile(p: &Profile) -> Result<()> {
    if !(1..=4096).contains(&p.jobs) { bail!("jobs must be between 1 and 4096"); }
    let _ = release_moniker(&p.source)?;
    if matches!(p.source.as_str(), "linux-cachyos" | "linux-tkg") && !p.allow_unverified_sources {
        bail!("{} requires allow_unverified_sources = true; its source material does not meet the pinned detached-signature policy",
            if p.source == "linux-cachyos" { "CachyOS" } else { "Linux TKG" });
    }
    if p.version != "stable" && p.version != "longterm" && !Regex::new(r"^\d+\.\d+(\.\d+)?$")?.is_match(&p.version) { bail!("invalid kernel version: {}", p.version); }
    if p.local_version.contains('/') || p.local_version.contains(' ') || p.local_version.contains('\n') { bail!("local_version may not contain slash, spaces or newlines"); }
    if !matches!(p.config_base.as_str(), "current" | "defconfig") { bail!("unsupported config_base '{}' (use current or defconfig)", p.config_base); }
    if !matches!(p.compiler.as_str(), "gcc" | "clang") { bail!("unsupported compiler {}; use gcc or clang", p.compiler); }
    if p.build_dir == p.stage_dir
        || p.build_dir.starts_with(&p.stage_dir)
        || p.stage_dir.starts_with(&p.build_dir)
    {
        bail!("build_dir and stage_dir must be separate, non-overlapping directories");
    }
    Ok(())
}

fn command_available(program: &str) -> bool {
    Command::new(program).arg("--version").stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output().is_ok()
}

fn nearest_existing_path(path: &Path) -> Option<PathBuf> {
    let mut candidate = path;
    loop {
        if candidate.exists() { return Some(candidate.to_path_buf()); }
        candidate = candidate.parent()?;
    }
}

fn available_gib(path: &Path) -> Option<f64> {
    let existing = nearest_existing_path(path)?;
    let output = Command::new("df").args(["-Pk"]).arg(existing).output().ok()?;
    if !output.status.success() { return None; }
    let line = String::from_utf8_lossy(&output.stdout).lines().last()?.to_owned();
    let fields = line.split_whitespace().collect::<Vec<_>>();
    let available_kib = fields.get(3)?.parse::<u64>().ok()?;
    Some(available_kib as f64 / 1024.0 / 1024.0)
}

fn doctor(profile_path: &Path) -> Result<()> {
    let (p, profile_loaded) = if profile_path.is_file() {
        (toml::from_str::<Profile>(&fs::read_to_string(profile_path).with_context(|| format!("reading {}", profile_path.display()))?)?, true)
    } else {
        (Profile::default(), false)
    };
    validate_profile(&p).context("invalid build profile")?;

    println!("Kernel build preflight (read-only)\nProfile: {}", if profile_loaded { profile_path.display().to_string() } else { "built-in defaults".into() });
    let mut failures = 0usize;
    let mut warnings = 0usize;
    let mut required = vec!["make", "curl", "xz", "tar", "gpg", "bc", "bison", "flex", "perl", "python3"];
    if matches!(p.source.as_str(), "linux-zen" | "linux-tkg") {
        required.push("patch");
    }
    if matches!(p.source.as_str(), "linux-tkg") {
        required.push("git");
    }
    if matches!(p.source.as_str(), "linux-zen") {
        required.push("zstd");
    }
    if matches!(p.source.as_str(), "linux-cachyos") {
        required.push("gzip");
    }
    if matches!(p.source.as_str(), "linux-rt") {
        required.push("patch");
    }
    for tool in required {
        if command_available(tool) { println!("[ ok ] {tool} available"); }
        else { println!("[FAIL] {tool} not found in PATH"); failures += 1; }
    }
    if command_available(&p.compiler) { println!("[ ok ] {} compiler available", p.compiler); }
    else { println!("[FAIL] {} compiler not found in PATH", p.compiler); failures += 1; }
    if p.ccache {
        if command_available("ccache") { println!("[ ok ] ccache available (enabled in profile)"); }
        else { println!("[FAIL] ccache is enabled in the profile but not installed"); failures += 1; }
    } else if command_available("ccache") { println!("[ ok ] ccache available (not required)"); }
    match provenance::trusted_key_available(p.gpg_home.as_deref()) {
        Ok(Some(name)) => println!("[ ok ] pinned kernel.org signer key available in GPG keyring ({name})"),
        Ok(None) => { println!("[WARN] no pinned kernel.org signer key found in GPG keyring; import and verify a documented key before building"); warnings += 1; }
        Err(error) => { println!("[WARN] could not inspect GPG keyring: {error}"); warnings += 1; }
    }

    let patch_signer = match p.source.as_str() {
        "linux-zen" => Some(("Linux Zen", "83BC8889351B5DEBBB68416EB8AC08600F108CDF")),
        "linux-rt" => Some(("PREEMPT_RT", "64254695FFF0AA4466CC19E67B96E8162A8CF5D1")),
        _ => None,
    };
    if let Some((name, fingerprint)) = patch_signer {
        match provenance::fingerprint_available(p.gpg_home.as_deref(), fingerprint) {
            Ok(true) => println!("[ ok ] pinned {name} patch signer key available"),
            Ok(false) => { println!("[WARN] pinned {name} patch signer key {fingerprint} is missing from the configured GPG keyring"); warnings += 1; }
            Err(error) => { println!("[WARN] could not inspect {name} patch signer key: {error}"); warnings += 1; }
        }
    }
    if command_available("pahole") { println!("[ ok ] pahole available"); }
    else { println!("[WARN] pahole missing; BTF-enabled kernel configurations may fail"); warnings += 1; }
    if Path::new("/usr/include/openssl/ssl.h").is_file() || Path::new("/usr/local/include/openssl/ssl.h").is_file() {
        println!("[ ok ] OpenSSL development headers found");
    } else { println!("[WARN] OpenSSL development headers not found in standard include paths"); warnings += 1; }

    let release = output("uname", &["-r"]);
    let current_config = Path::new("/proc/config.gz").is_file()
        || PathBuf::from(format!("/boot/config-{release}")).is_file()
        || PathBuf::from(format!("/lib/modules/{release}/build/.config")).is_file();
    if p.config_base == "current" && current_config { println!("[ ok ] current kernel config source found"); }
    else if p.config_base == "current" { println!("[WARN] current kernel config unavailable; build will fall back to defconfig"); warnings += 1; }
    else { println!("[ ok ] defconfig selected explicitly"); }

    for (label, path, floor) in [("build", &p.build_dir, 20.0_f64), ("stage", &p.stage_dir, 2.0_f64)] {
        match available_gib(path) {
            Some(gib) if gib >= floor => println!("[ ok ] {label} filesystem: {gib:.1} GiB available ({})", path.display()),
            Some(gib) => { println!("[WARN] {label} filesystem has only {gib:.1} GiB free; heuristic recommendation is at least {floor:.0} GiB ({})", path.display()); warnings += 1; }
            None => { println!("[WARN] unable to determine free space for {label} path {}", path.display()); warnings += 1; }
        }
    }
    if unsafe { libc::geteuid() } == 0 { println!("[WARN] running as root; prefer an unprivileged build user"); warnings += 1; }
    if !profile_loaded { println!("[INFO] {} not found; checked built-in defaults. Create it with kernel-optimizer profile --create.", profile_path.display()); }
    println!("\nPreflight summary: {failures} failure(s), {warnings} warning(s).");
    if failures > 0 { bail!("preflight failed; install the missing build dependencies and run doctor again"); }
    if warnings > 0 { println!("Review the warnings before starting a long kernel build."); }
    else { println!("Environment looks ready for a kernel build."); }
    Ok(())
}


fn download_signature(path: &Path, url: &str) -> Result<()> {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let temporary = path.with_extension(format!("download-{}-{stamp}", std::process::id()));
    let result = (|| -> Result<()> {
        run_checked(
            Command::new("curl")
                .args(["--fail", "--location", "--retry", "3", "--output"])
                .arg(&temporary)
                .arg(url),
        )?;
        if fs::metadata(&temporary)?.len() == 0 {
            bail!("downloaded kernel signature is empty");
        }
        fs::rename(&temporary, path)
            .with_context(|| format!("promoting verified download to {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

// Re-extract the signed source archive on every build. Only the previous .config is
// retained; local edits to source files cannot silently survive outside the signature.
fn refresh_source_tree(archive: &Path, source: &Path, build_dir: &Path, version: &str) -> Result<bool> {
    refresh_source_tree_with_layout(archive, source, build_dir, version, false)
}

fn refresh_source_tree_with_layout(
    archive: &Path,
    source: &Path,
    build_dir: &Path,
    version: &str,
    strip_components: bool,
) -> Result<bool> {
    let source_metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("inspecting source tree path {}", source.display())),
    };
    if source_metadata.as_ref().is_some_and(|metadata| metadata.file_type().is_symlink()) {
        bail!("refusing to replace a symlink at source tree path {}", source.display());
    }
    if source_metadata.as_ref().is_some_and(|metadata| !metadata.is_dir()) {
        bail!("source tree path exists but is not a directory: {}", source.display());
    }
    let saved_config = if source.join(".config").is_file() {
        Some(fs::read(source.join(".config")).context("reading existing kernel .config for preservation")?)
    } else {
        None
    };
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let temporary = build_dir.join(format!(".kernel-optimizer-extract-{}-{stamp}", std::process::id()));
    if temporary.exists() {
        fs::remove_dir_all(&temporary)?;
    }
    let extraction_root = temporary.join("payload");
    fs::create_dir_all(&extraction_root)?;
    let mut tar = Command::new("tar");
    tar.args(["--extract", "--file"])
        .arg(archive)
        .arg("--directory")
        .arg(&extraction_root)
        .args(["--no-same-owner", "--no-same-permissions"]);
    if strip_components {
        tar.arg("--strip-components=1");
    }
    let extraction = run_checked(&mut tar);
    if let Err(error) = extraction {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error).context("extracting signed kernel source archive");
    }
    let extracted_root = if strip_components {
        extraction_root.clone()
    } else {
        extraction_root.join(format!("linux-{version}"))
    };
    if !extracted_root.is_dir() || !extracted_root.join("Makefile").is_file() {
        let _ = fs::remove_dir_all(&temporary);
        bail!("source archive does not contain the expected kernel source tree");
    }
    let backup = build_dir.join(format!(".kernel-optimizer-old-source-{}-{stamp}", std::process::id()));
    let had_source = source_metadata.is_some();
    if had_source && let Err(error) = fs::rename(source, &backup) {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error).with_context(|| format!("moving previous source tree {} aside", source.display()));
    }
    if let Err(error) = fs::rename(&extracted_root, source) {
        if had_source {
            let _ = fs::rename(&backup, source);
        }
        let _ = fs::remove_dir_all(&temporary);
        return Err(error).with_context(|| format!("promoting fresh source tree to {}", source.display()));
    }
    if let Some(config) = saved_config.as_ref()
        && let Err(error) = fs::write(source.join(".config"), config)
    {
        let _ = fs::remove_dir_all(source);
        if had_source {
            let _ = fs::rename(&backup, source);
        }
        let _ = fs::remove_dir_all(&temporary);
        return Err(error).context("restoring the prior kernel .config into the clean source tree");
    }
    fs::remove_dir_all(&temporary)?;
    if had_source {
        fs::remove_dir_all(&backup)
            .with_context(|| format!("removing superseded source tree {}", backup.display()))?;
    }
    Ok(saved_config.is_some())
}

fn unix_timestamp() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

#[derive(Debug, Serialize)]
struct SourceProvenance {
    project: String,
    version: String,
    archive_url: String,
    archive_sha256: String,
    signature_url: String,
    signature_sha256: String,
    signature_verified: bool,
    signature_verification_method: String,
    trust_reference: String,
    signature_creation_date: String,
    signature_timestamp: String,
    signature_public_key_algorithm: String,
    signature_hash_algorithm: String,
    signer_name: String,
    signer_primary_fingerprint: String,
    patches: Vec<source_adapters::PatchProvenance>,
    gpg_version: Option<String>,
    xz_version: Option<String>,
    curl_version: Option<String>,
    tar_version: Option<String>,
}

#[derive(Debug, Serialize)]
struct BuildProvenance {
    // Null when recovering metadata because the original build timestamps were not recorded.
    started_unix: Option<u64>,
    completed_unix: Option<u64>,
    duration_seconds: Option<f64>,
    metadata_generated_unix: u64,
    metadata_recovered: bool,
    recovery_note: Option<String>,
    recovery_actions: Vec<String>,
    profile: Profile,
    compiler_command: String,
    compiler_environment: String,
    compiler_version: Option<String>,
    ccache_version: Option<String>,
    make_version: Option<String>,
    rustc_version: Option<String>,
    config_origin: String,
    effective_config_path: String,
    effective_config_sha256: String,
    commands: Vec<String>,
}

#[derive(Debug, Serialize)]
struct HostProvenance {
    host_kernel_release: String,
    architecture: String,
    cpu_model: String,
    cpu_microarchitecture: String,
    logical_cpus: usize,
}

#[derive(Debug, Serialize)]
struct ArtifactProvenance {
    kernel_release: String,
    kernel_image: String,
    kernel_image_sha256: String,
    system_map: Option<String>,
    system_map_sha256: Option<String>,
    modules_directory: String,
    modules_staged: bool,
    modules_tree_sha256: Option<String>,
    modules_file_count: u64,
    modules_total_bytes: u64,
}

#[derive(Debug, Serialize)]
struct BuildManifest {
    schema_version: u32,
    generated_unix: u64,
    application_version: &'static str,
    source: SourceProvenance,
    build: BuildProvenance,
    // Absent for recovered manifests: the original build host could not be attested.
    host: Option<HostProvenance>,
    artifacts: ArtifactProvenance,
}

fn build(profile_path:&Path,dry:bool)->Result<()> {
    let mut p:Profile=toml::from_str(&fs::read_to_string(profile_path)?)?;
    validate_profile(&p)?;
    let source_plan = source_adapters::resolve(&p.source, &p.version, p.allow_unverified_sources)
        .context("resolving selected kernel source")?;
    if let Some(plan) = source_plan.as_ref() {
        p.version = plan.version.clone();
    } else if p.version=="stable" || p.version=="longterm" {
        let moniker = release_moniker(&p.source)?;
        p.version=latest_release(moniker)?;
    }

    let src=p.build_dir.join(format!("linux-{}",p.version));
    let (url, archive_name, signature_url, signature_name, archive_format) =
        if let Some(plan) = source_plan.as_ref() {
            (plan.archive_url.clone(), plan.archive_name.clone(),
             plan.signature_url.clone(), plan.signature_name.clone(), plan.archive_format)
        } else {
            let major=p.version.split('.').next().context("kernel version has no major component")?;
            let base=format!("https://cdn.kernel.org/pub/linux/kernel/v{major}.x");
            (format!("{base}/linux-{}.tar.xz",p.version),
             format!("linux-{}.tar.xz",p.version),
             Some(format!("{base}/linux-{}.tar.sign",p.version)),
             Some(format!("linux-{}.tar.sign",p.version)),
             source_adapters::ArchiveFormat::KernelXz)
        };
    let archive=p.build_dir.join(&archive_name);
    let signature=signature_name.as_ref().map(|name| p.build_dir.join(name));
    let signature_url_text=signature_url.clone().unwrap_or_default();

    println!("Kernel: {}{}",p.version,p.local_version);
    println!("Source: {}", source_plan.as_ref().map(|plan| plan.project.as_str()).unwrap_or("Linux kernel.org"));
    println!("CPU: {}",hardware::detect().cpu.model);
    println!("Jobs: {}",p.jobs);
    println!("Compiler: {} (ccache: {}, mold requested: {})",p.compiler,p.ccache,p.use_mold);
    println!("Optimization profile: {} (currently informational; no unsafe CPU-specific flags injected)",p.optimization);
    println!("Source archive: {url}");
    if let Some(signature_url) = signature_url.as_deref() { println!("Signature: {signature_url}"); }
    if let Some(plan) = source_plan.as_ref() {
        println!("Source provenance: {}", plan.trust_reference);
        if !matches!(p.source.as_str(), "linux-zen" | "linux-rt") {
            println!("WARNING: this source track is not independently signature-verified; it cannot be packaged by the verified Arch package workflow.");
        }
    }
    println!("Build dir: {}",src.display());
    if dry {println!("Dry run only. No files changed.");return Ok(());}

    fs::create_dir_all(&p.build_dir)?;
    let started_unix=unix_timestamp();
    let build_timer=Instant::now();
    match archive_format {
        source_adapters::ArchiveFormat::KernelXz => ensure_kernel_archive(&archive, &url)?,
        source_adapters::ArchiveFormat::GzipTar => {
            download_signature(&archive, &url)?;
            run_checked(Command::new("gzip").arg("-t").arg(&archive))?;
        }
    }
    let archive_sha256_before=provenance::sha256_file(&archive)?;
    if let (Some(signature_path), Some(signature_url)) = (signature.as_deref(), signature_url.as_deref()) {
        download_signature(signature_path, signature_url)?;
    }
    let signature_sha256_before=signature.as_deref().map(provenance::sha256_file).transpose()?;
    let base_signature_verification = if let Some(signature_path) = signature.as_deref() {
        Some(provenance::verify_kernel_archive(&archive,signature_path,p.gpg_home.as_deref())?)
    } else {
        None
    };
    let archive_sha256=source_adapters::verify_archive_digest(
        &archive,
        source_plan.as_ref().and_then(|plan| plan.expected_archive_sha256.as_deref()),
    )?;
    let signature_sha256=signature.as_deref().map(provenance::sha256_file).transpose()?.unwrap_or_default();
    if archive_sha256 != archive_sha256_before
        || signature_sha256_before.as_ref().is_some_and(|before| before != &signature_sha256)
    {
        bail!("kernel archive or detached signature changed during verification; refusing to build");
    }
    if let Some(verification) = base_signature_verification.as_ref() {
        println!("Verified base source signature by {} ({})",
            verification.signer_name,verification.signer_primary_fingerprint);
    } else {
        println!("Source archive digest matched the upstream GitHub release metadata; no trusted detached signature was available.");
    }

    let preserved_config=if archive_format == source_adapters::ArchiveFormat::GzipTar {
        refresh_source_tree_with_layout(&archive,&src,&p.build_dir,&p.version,true)?
    } else {
        refresh_source_tree(&archive,&src,&p.build_dir,&p.version)?
    };
    if provenance::sha256_file(&archive)? != archive_sha256 {
        bail!("source archive changed after verification; refusing to build");
    }
    let patch_provenance = if let Some(plan) = source_plan.as_ref() {
        source_adapters::download_and_apply_patches(plan, &src, &p.build_dir, p.gpg_home.as_deref())?
    } else {
        Vec::new()
    };
    if !patch_provenance.is_empty() {
        println!("Applied {} source patch(es) with the configured provenance policy.", patch_provenance.len());
    }
    println!("Extracted a clean source tree{}.",
        if preserved_config { "; preserved the prior .config" } else { "" });
    let config=src.join(".config");
    let config_origin = if preserved_config {
        "preserved_previous_config".to_owned()
    } else {
        let origin = match p.config_base.as_str() {
            "current" => {
                let release = output("uname", &["-r"]);
                let boot_config = PathBuf::from(format!("/boot/config-{release}"));
                let module_config = PathBuf::from(format!("/lib/modules/{release}/build/.config"));
                let mut origin = None;
                if Path::new("/proc/config.gz").exists()
                    && let Ok(decoded) = Command::new("gzip").args(["-dc", "/proc/config.gz"]).output()
                    && decoded.status.success()
                {
                    fs::write(&config, decoded.stdout)?;
                    origin = Some("proc_config_gz");
                }
                if !config.exists() {
                    for candidate in [&boot_config, &module_config] {
                        if candidate.is_file() {
                            fs::copy(candidate, &config)
                                .with_context(|| format!("copying current kernel config from {}", candidate.display()))?;
                            origin = Some(if candidate == &boot_config { "boot_config" } else { "module_build_config" });
                            break;
                        }
                    }
                }
                if !config.exists() {
                    println!("No current kernel config found; falling back to defconfig.");
                    run_checked(Command::new("make").current_dir(&src).arg("defconfig"))?;
                    "defconfig_fallback"
                } else {
                    origin.unwrap_or("current_kernel_config")
                }
            }
            "defconfig" => {
                run_checked(Command::new("make").current_dir(&src).arg("defconfig"))?;
                "defconfig"
            }
            other => bail!("unsupported config_base '{other}' (use current or defconfig)"),
        };
        origin.to_owned()
    };
    let local=p.local_version.clone();
    run_checked(Command::new("scripts/config").current_dir(&src).args(["--set-str","LOCALVERSION",&local]))?;
    if p.enable_bpf {run_checked(Command::new("scripts/config").current_dir(&src).args(["--enable","BPF","--enable","BPF_SYSCALL"]))?;}
    if p.enable_lto {println!("LTO requested: kernel config must support a compatible LLVM toolchain; not forcing incompatible LTO options.");}
    if p.menuconfig {run_checked(Command::new("make").current_dir(&src).arg("menuconfig"))?;}
    let mut make=Command::new("make");
    make.current_dir(&src).arg(format!("-j{}",p.jobs));
    match p.compiler.as_str() {
        "gcc" => {
            make.env("CC", if p.ccache { "ccache gcc" } else { "gcc" });
        }
        "clang" => {
            make.env("LLVM", "1").env("CC", if p.ccache { "ccache clang" } else { "clang" });
        }
        other => bail!("unsupported compiler {other}; use gcc or clang"),
    }
    if p.use_mold {
        println!("mold requested, but this build adapter does not force an unsupported linker override.");
    }
    run_checked(Command::new("make").current_dir(&src).arg("olddefconfig"))?;
    let effective_config_sha256=provenance::sha256_file(&config)
        .with_context(|| format!("hashing effective kernel config {}",config.display()))?;
    let make_version=provenance::command_version("make",&["--version"]);
    let compiler_version=provenance::command_version(&p.compiler,&["--version"]);
    let ccache_version=if p.ccache { provenance::command_version("ccache",&["--version"]) } else { None };
    let rustc_version=provenance::command_version("rustc",&["--version"]);
    run_checked(&mut make)?;
    fs::create_dir_all(&p.stage_dir)?;
    let release = kernel_release(&src)?;
    run_checked(Command::new("make").current_dir(&src).arg("modules_install").env("INSTALL_MOD_PATH",p.stage_dir.join("root")))?;
    let staged_image_name = stage_kernel_boot_artifacts(
        &src,
        &p.stage_dir.join("boot"),
        &release,
        std::env::consts::ARCH,
    )?;
    println!("Staged kernel image: {staged_image_name} (distribution install hooks were not run)");
    let (image_name, image_sha256, modules_staged) = verify_staged_kernel(&p.stage_dir, &release)?;
    let image_relative=PathBuf::from("boot").join(&image_name).to_string_lossy().to_string();
    let system_map_path=p.stage_dir.join("boot").join(format!("System.map-{release}"));
    let system_map_sha256=if system_map_path.is_file() {
        Some(provenance::sha256_file(&system_map_path)?)
    } else { None };
    let modules_path=p.stage_dir.join("root/lib/modules").join(&release);
    let modules_relative=PathBuf::from("root/lib/modules").join(&release).to_string_lossy().to_string();
    let (modules_tree_sha256,modules_file_count,modules_total_bytes)=if modules_staged {
        let (hash,count,bytes)=provenance::sha256_tree(&modules_path)?;
        (Some(hash),count,bytes)
    } else { (None,0,0) };
    let completed_unix=unix_timestamp();
    let host=hardware::detect();
    let mut recorded_profile=p.clone();
    recorded_profile.gpg_home=None;
    let verified_source = base_signature_verification.is_some()
        && patch_provenance.iter().all(|patch| patch.signature_verified);
    let base_verification = base_signature_verification.as_ref();
    let source_manifest=SourceProvenance {
        project: source_plan.as_ref().map(|plan| plan.project.clone())
            .unwrap_or_else(|| format!("Linux kernel.org {} track", if p.source == "linux-lts" { "longterm" } else { "stable" })),
        version:p.version.clone(),
        archive_url:url,
        archive_sha256,
        signature_url:signature_url_text,
        signature_sha256,
        signature_verified:verified_source,
        signature_verification_method: if verified_source {
            "kernel.org base archive signature plus all source patch signatures verified against pinned signer fingerprints".into()
        } else {
            "NOT FULLY VERIFIED: explicit opt-in source or unsigned patch provenance; do not use for package installation".into()
        },
        trust_reference:source_plan.as_ref().map(|plan| plan.trust_reference.clone())
            .unwrap_or_else(|| "https://www.kernel.org/signature.html".into()),
        signature_creation_date:base_verification.map(|v| v.signature_creation_date.clone()).unwrap_or_default(),
        signature_timestamp:base_verification.map(|v| v.signature_timestamp.clone()).unwrap_or_default(),
        signature_public_key_algorithm:base_verification.map(|v| v.public_key_algorithm.clone()).unwrap_or_default(),
        signature_hash_algorithm:base_verification.map(|v| v.hash_algorithm.clone()).unwrap_or_default(),
        signer_name:base_verification.map(|v| v.signer_name.clone()).unwrap_or_else(|| "not independently verified".into()),
        signer_primary_fingerprint:base_verification.map(|v| v.signer_primary_fingerprint.clone()).unwrap_or_default(),
        patches:patch_provenance,
        gpg_version:provenance::command_version("gpg",&["--version"]),
        xz_version:provenance::command_version("xz",&["--version"]),
        curl_version:provenance::command_version("curl",&["--version"]),
        tar_version:provenance::command_version("tar",&["--version"]),
    };
    let compiler_command=if p.ccache { format!("ccache {}",p.compiler) } else { p.compiler.clone() };
    let mut recorded_commands=vec![
        format!("curl --fail --location --retry 3 (download source archive from {url})"),
        match (signature_url.as_deref(), source_plan.as_ref().and_then(|plan| plan.expected_archive_sha256.as_deref())) {
            (Some(_), _) => "verify base archive detached signature against the pinned signer allowlist".into(),
            (None, Some(_)) => "verify source archive SHA-256 against the digest advertised by the upstream GitHub release".into(),
            (None, None) => "no detached source signature or upstream digest was available".into(),
        },
        if archive_format == source_adapters::ArchiveFormat::GzipTar {
            "gzip -t <source archive>; extract with --strip-components=1".into()
        } else {
            "xz -t <source archive>; extract the signed kernel.org source archive".into()
        },
    ];
    for patch in &manifest.source.patches {
        recorded_commands.push(format!(
            "download and apply source patch {} (sha256 {}; signature verified: {}; Git blob: {})",
            patch.name, patch.sha256, patch.signature_verified, patch.git_blob_sha1
        ));
    }
    if config_origin == "defconfig" || config_origin == "defconfig_fallback" {
        recorded_commands.push("make defconfig".into());
    }
    recorded_commands.extend([
        format!("scripts/config --set-str LOCALVERSION {}",p.local_version),
        if p.enable_bpf { "scripts/config --enable BPF --enable BPF_SYSCALL".into() } else { "BPF flags unchanged".into() },
        if p.menuconfig { "make menuconfig (interactive)".into() } else { "menuconfig disabled".into() },
        "make olddefconfig".into(),
        format!("make -j{}",p.jobs),
        "make kernelrelease".into(),
        format!("make modules_install INSTALL_MOD_PATH={}",p.stage_dir.join("root").display()),
        format!("copy built kernel image, .config and System.map into {}",p.stage_dir.join("boot").display()),
    ]);
    let build_manifest=BuildProvenance {
        started_unix:Some(started_unix),
        completed_unix:Some(completed_unix),
        duration_seconds:Some(build_timer.elapsed().as_secs_f64()),
        metadata_generated_unix:completed_unix,
        metadata_recovered:false,
        recovery_note:None,
        recovery_actions:Vec::new(),
        profile:recorded_profile,
        compiler_command:compiler_command.clone(),
        compiler_environment:if p.compiler == "clang" { format!("CC={compiler_command}; LLVM=1") } else { format!("CC={compiler_command}") },
        compiler_version,
        ccache_version,
        make_version,
        rustc_version,
        config_origin,
        effective_config_path:".config".into(),
        effective_config_sha256:effective_config_sha256.clone(),
        commands:recorded_commands,
    };
    let host_manifest=HostProvenance {
        host_kernel_release:host.kernel_release,
        architecture:host.architecture,
        cpu_model:host.cpu.model,
        cpu_microarchitecture:host.cpu.microarchitecture,
        logical_cpus:host.cpu.logical_cpus,
    };
    let artifacts=ArtifactProvenance {
        kernel_release:release.clone(),
        kernel_image:image_relative.clone(),
        kernel_image_sha256:image_sha256.clone(),
        system_map:if system_map_sha256.is_some() {
            Some(PathBuf::from("boot").join(format!("System.map-{release}")).to_string_lossy().to_string())
        } else { None },
        system_map_sha256,
        modules_directory:modules_relative,
        modules_staged,
        modules_tree_sha256,
        modules_file_count,
        modules_total_bytes,
    };
    let manifest=BuildManifest {
        schema_version:3,
        generated_unix:completed_unix,
        application_version:env!("CARGO_PKG_VERSION"),
        source:source_manifest,
        build:build_manifest,
        host:Some(host_manifest),
        artifacts,
    };
    provenance::write_json_atomic(&p.stage_dir.join("provenance.json"),&manifest)?;
    let metadata = format!(
        "version={}\nlocalversion={}\nkernel_release={}\nsource={}\nsource_signature_verified={}\nsource_signer={}\nsource_signer_fingerprint={}\nsource_archive_sha256={}\nsource_signature_sha256={}\neffective_config_sha256={}\nkernel_image={}\nkernel_image_sha256={}\nmodules_staged={}\nprovenance=provenance.json\n",
        p.version, p.local_version, release, p.source, manifest.source.signature_verified,
        manifest.source.signer_name,manifest.source.signer_primary_fingerprint,
        manifest.source.archive_sha256,manifest.source.signature_sha256,
        effective_config_sha256,image_name,image_sha256,modules_staged
    );
    fs::write(p.stage_dir.join("BUILD-METADATA.txt"), metadata)?;
    println!("Verified staged kernel image: {image_name} (SHA-256 {image_sha256})");
    println!("Build provenance written to {}",p.stage_dir.join("provenance.json").display());
    if !modules_staged {
        println!("WARNING: no staged module directory found for {release}; this kernel may be fully built-in or module staging may be incomplete.");
    }
    println!("Build staged at {}. Not installed or added to bootloader.",p.stage_dir.display());
    Ok(())
}

fn infer_finalize_version(p: &Profile, override_version: Option<&str>) -> Result<String> {
    let version_pattern = Regex::new(r"^\d+\.\d+(\.\d+)?$")?;
    if let Some(version) = override_version {
        if !version_pattern.is_match(version) {
            bail!("invalid kernel version '{version}'; expected X.Y or X.Y.Z");
        }
        if p.version != "stable" && p.version != "longterm" && p.version != version {
            bail!(
                "requested version {version} does not match profile version {}; update the profile or omit --version",
                p.version
            );
        }
        return Ok(version.to_owned());
    }

    let modules_root = p.stage_dir.join("root/lib/modules");
    let entries = fs::read_dir(&modules_root)
        .with_context(|| format!("reading staged modules directory {}", modules_root.display()))?;
    let mut versions = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let release = entry.file_name().to_string_lossy().to_string();
        let Some(version) = release.strip_suffix(&p.local_version) else {
            continue;
        };
        if version_pattern.is_match(version) {
            versions.push(version.to_owned());
        }
    }
    versions.sort();
    versions.dedup();
    match versions.as_slice() {
        [version] => Ok(version.clone()),
        [] => bail!(
            "could not infer the kernel version from staged modules under {}; pass --version X.Y.Z",
            modules_root.display()
        ),
        _ => bail!(
            "multiple staged kernel versions were found ({}); pass --version explicitly",
            versions.join(", ")
        ),
    }
}

fn finalize(profile_path: &Path, version_override: Option<&str>) -> Result<()> {
    let mut p: Profile = toml::from_str(
        &fs::read_to_string(profile_path)
            .with_context(|| format!("reading build profile {}", profile_path.display()))?
    )?;
    validate_profile(&p)?;
    if !matches!(p.source.as_str(), "linux" | "linux-lts") {
        bail!("metadata recovery currently supports only Linux stable/LTS. Variant sources require re-verifying their exact patch assets; rebuild the selected variant to recreate complete provenance.");
    }

    let version = infer_finalize_version(&p, version_override)?;
    p.version = version.clone();
    let release = format!("{version}{}", p.local_version);
    let source_dir = p.build_dir.join(format!("linux-{version}"));
    let stage_boot = p.stage_dir.join("boot");
    let modules_dir = p.stage_dir.join("root/lib/modules").join(&release);

    if !source_dir.is_dir() {
        bail!("existing kernel source/build directory is missing: {}", source_dir.display());
    }
    if !source_dir.join(".config").is_file() {
        bail!("built kernel .config is missing: {}", source_dir.join(".config").display());
    }
    if !modules_dir.is_dir() {
        bail!("staged modules for {release} are missing: {}", modules_dir.display());
    }

    println!("Finalizing existing kernel build {release}; no compilation or module installation will be run.");

    // kernelrelease reads generated build metadata; it does not compile the kernel.
    let actual_release = kernel_release(&source_dir)?;
    if actual_release != release {
        bail!(
            "source tree reports kernel release {actual_release}, but staged module directory/profile expects {release}"
        );
    }

    for index in ["modules.dep", "modules.alias"] {
        let path = modules_dir.join(index);
        if !path.is_file() || fs::metadata(&path)?.len() == 0 {
            bail!(
                "staged module index {} is missing or empty; do not finalize an incomplete modules_install",
                path.display()
            );
        }
    }

    let (image_name, image_sha256, modules_staged) = verify_staged_kernel(&p.stage_dir, &release)?;
    if !modules_staged {
        bail!("staged module directory for {release} was not found");
    }
    let expected_image_name = format!("vmlinuz-{release}");
    if image_name != expected_image_name {
        bail!(
            "staged image is named {image_name}, expected {expected_image_name}; refusing to finalize ambiguous artifacts"
        );
    }

    let source_image = kernel_image_candidates(std::env::consts::ARCH)?
        .iter()
        .map(|relative| source_dir.join(relative))
        .find(|path| path.is_file())
        .context("built kernel image is missing from the source/build directory")?;
    let source_image_sha256 = provenance::sha256_file(&source_image)?;
    if source_image_sha256 != image_sha256 {
        bail!(
            "staged kernel image hash does not match the built image at {}; refusing to attest mismatched artifacts",
            source_image.display()
        );
    }

    let config_path = source_dir.join(".config");
    let staged_config_path = stage_boot.join(format!("config-{release}"));
    if !staged_config_path.is_file() {
        bail!("staged kernel config is missing: {}", staged_config_path.display());
    }
    let effective_config_sha256 = provenance::sha256_file(&config_path)?;
    if provenance::sha256_file(&staged_config_path)? != effective_config_sha256 {
        bail!("staged config-{release} does not match the build tree .config");
    }

    let system_map_source = source_dir.join("System.map");
    let system_map_staged = stage_boot.join(format!("System.map-{release}"));
    let system_map_sha256 = if system_map_source.is_file() {
        if !system_map_staged.is_file() {
            bail!("System.map exists in the build tree but is missing from staging");
        }
        let source_hash = provenance::sha256_file(&system_map_source)?;
        if provenance::sha256_file(&system_map_staged)? != source_hash {
            bail!("staged System.map-{release} does not match the build tree System.map");
        }
        Some(source_hash)
    } else {
        None
    };

    let archive = p.build_dir.join(format!("linux-{version}.tar.xz"));
    let signature = p.build_dir.join(format!("linux-{version}.tar.sign"));
    let major = version.split('.').next().context("kernel version has no major component")?;
    let archive_url = format!("https://cdn.kernel.org/pub/linux/kernel/v{major}.x/linux-{version}.tar.xz");
    let signature_url = format!("https://cdn.kernel.org/pub/linux/kernel/v{major}.x/linux-{version}.tar.sign");

    // A previous build may have completed compilation but stopped before its
    // metadata step, and the small detached signature may have been removed.
    // Fetch it from the same kernel.org release directory if missing/empty;
    // cryptographic verification against the pinned signer allowlist remains
    // mandatory before any recovered provenance is written.
    if !signature.is_file() || fs::metadata(&signature)?.len() == 0 {
        println!("Detached signature is missing; downloading the official kernel.org signature.");
        download_signature(&signature, &signature_url)
            .with_context(|| format!("downloading detached signature from {signature_url}"))?;
    }

    // Re-verify the signature now. Hashes are checked before and after
    // verification to detect replacement while the archive is being read.
    let archive_sha256_before = provenance::sha256_file(&archive)
        .with_context(|| format!("reading cached source archive {}", archive.display()))?;
    let signature_sha256_before = provenance::sha256_file(&signature)
        .with_context(|| format!("reading detached signature {}", signature.display()))?;
    let signature_verification =
        provenance::verify_kernel_archive(&archive, &signature, p.gpg_home.as_deref())?;
    let archive_sha256 = provenance::sha256_file(&archive)?;
    let signature_sha256 = provenance::sha256_file(&signature)?;
    if archive_sha256 != archive_sha256_before || signature_sha256 != signature_sha256_before {
        bail!("kernel archive or detached signature changed during finalization; refusing to write provenance");
    }

    println!(
        "Re-verified kernel.org source signature by {} ({})",
        signature_verification.signer_name,
        signature_verification.signer_primary_fingerprint
    );

    let (modules_tree_sha256, modules_file_count, modules_total_bytes) =
        provenance::sha256_tree(&modules_dir)
            .context("hashing staged kernel modules tree")?;
    let generated_unix = unix_timestamp();

    let source_manifest = SourceProvenance {
        project: format!("Linux kernel.org {} track (recovered)", if p.source == "linux-lts" { "longterm" } else { "stable" }),
        version: version.clone(),
        archive_url,
        archive_sha256,
        signature_url,
        signature_sha256,
        signature_verified: true,
        patches: Vec::new(),
        signature_verification_method: "re-verified during metadata recovery: xz --decompress --stdout piped to gpg --no-options --batch --no-auto-key-import --no-auto-key-retrieve --no-auto-key-locate --status-fd=1 --verify".into(),
        trust_reference: "https://www.kernel.org/signature.html".into(),
        signature_creation_date: signature_verification.signature_creation_date,
        signature_timestamp: signature_verification.signature_timestamp,
        signature_public_key_algorithm: signature_verification.public_key_algorithm,
        signature_hash_algorithm: signature_verification.hash_algorithm,
        signer_name: signature_verification.signer_name,
        signer_primary_fingerprint: signature_verification.signer_primary_fingerprint,
        gpg_version: provenance::command_version("gpg", &["--version"]),
        xz_version: provenance::command_version("xz", &["--version"]),
        curl_version: provenance::command_version("curl", &["--version"]),
        tar_version: provenance::command_version("tar", &["--version"]),
    };

    let mut recorded_profile = p.clone();
    recorded_profile.gpg_home = None;
    let compiler_command = if p.ccache {
        format!("ccache {}", p.compiler)
    } else {
        p.compiler.clone()
    };
    let build_manifest = BuildProvenance {
        started_unix: None,
        completed_unix: None,
        duration_seconds: None,
        metadata_generated_unix: generated_unix,
        metadata_recovered: true,
        recovery_note: Some(
            "Metadata was reconstructed after the build command stopped before writing its manifest. The original build start/completion times, duration, host inventory, exact compiler/tool versions, config origin, and original command history were not persisted and are therefore not asserted. Source authenticity was re-verified against the pinned kernel.org signer allowlist. Matching hashes confirm that the staged image, config, and System.map (when present) match the surviving build-tree outputs; this recovery does not independently prove that no source-tree edits occurred after extraction."
                .into(),
        ),
        recovery_actions: vec![
            "make kernelrelease (validation only; no compilation)".into(),
            "verify staged kernel image against the build-tree image SHA-256".into(),
            "verify staged config and System.map against the build-tree files".into(),
            "download the kernel.org detached signature if missing, then re-verify it against the pinned signer allowlist".into(),
            "hash the staged modules tree".into(),
        ],
        profile: recorded_profile,
        compiler_command,
        compiler_environment: if p.compiler == "clang" {
            format!("CC={}; LLVM=1", if p.ccache { "ccache clang" } else { "clang" })
        } else {
            format!("CC={}", if p.ccache { "ccache gcc" } else { "gcc" })
        },
        compiler_version: None,
        ccache_version: None,
        make_version: None,
        rustc_version: None,
        config_origin: "unknown_recovered_from_existing_build_tree".into(),
        effective_config_path: ".config".into(),
        effective_config_sha256: effective_config_sha256.clone(),
        commands: Vec::new(),
    };

    let artifacts = ArtifactProvenance {
        kernel_release: release.clone(),
        kernel_image: PathBuf::from("boot").join(&image_name).to_string_lossy().to_string(),
        kernel_image_sha256: image_sha256.clone(),
        system_map: if system_map_sha256.is_some() {
            Some(PathBuf::from("boot").join(format!("System.map-{release}")).to_string_lossy().to_string())
        } else {
            None
        },
        system_map_sha256,
        modules_directory: PathBuf::from("root/lib/modules").join(&release).to_string_lossy().to_string(),
        modules_staged: true,
        modules_tree_sha256: Some(modules_tree_sha256),
        modules_file_count,
        modules_total_bytes,
    };

    let manifest = BuildManifest {
        schema_version: 3,
        generated_unix,
        application_version: env!("CARGO_PKG_VERSION"),
        source: source_manifest,
        build: build_manifest,
        host: None,
        artifacts,
    };
    provenance::write_json_atomic(&p.stage_dir.join("provenance.json"), &manifest)?;

    let metadata = format!(
        "version={version}\nlocalversion={}\nkernel_release={release}\nsource={}\nmetadata_recovered=true\nbuild_timing_known=false\nsource_signature_verified=true\nsource_signer={}\nsource_signer_fingerprint={}\nsource_archive_sha256={}\nsource_signature_sha256={}\neffective_config_sha256={}\nkernel_image={image_name}\nkernel_image_sha256={image_sha256}\nmodules_staged=true\nmodules_tree_sha256={}\nmodules_file_count={modules_file_count}\nmodules_total_bytes={modules_total_bytes}\nprovenance=provenance.json\n",
        p.local_version,
        p.source,
        manifest.source.signer_name,
        manifest.source.signer_primary_fingerprint,
        manifest.source.archive_sha256,
        manifest.source.signature_sha256,
        effective_config_sha256,
        manifest.artifacts.modules_tree_sha256.as_deref().unwrap_or(""),
    );
    fs::write(p.stage_dir.join("BUILD-METADATA.txt"), metadata)
        .context("writing recovered BUILD-METADATA.txt")?;

    println!("Recovered metadata written to {}", p.stage_dir.join("provenance.json").display());
    println!("Build metadata written to {}", p.stage_dir.join("BUILD-METADATA.txt").display());
    println!("No kernel compilation or modules_install operation was performed.");
    println!("Note: original build timing and host/toolchain-at-build details are unavailable and marked as unknown.");
    Ok(())
}

fn main()->Result<()> {
    let cli=Cli::parse();
    match cli.command {
        Commands::Inspect=>println!("{}",serde_json::to_string_pretty(&hardware::detect())?),
        Commands::Hardware=>println!("{}",serde_json::to_string_pretty(&hardware::detect())?),
        Commands::Recommend{profile}=>{
            let report=recommendations::generate(profile);
            println!("{}",serde_json::to_string_pretty(&report)?);
        },
        Commands::Gui=>gui::run()?,
        Commands::Tui{profile}=>{
            tui::run(profile)?;
        },
        Commands::Benchmark{output,iterations,profile,cpu_rounds,memory_mib,fio_path,fio_size_mib,fio_runtime_seconds,iperf_server}=>{
            let report=benchmark::run(
                &output,
                benchmark::BenchmarkOptions {
                    iterations,
                    profile: &profile,
                    cpu_rounds,
                    memory_mib,
                    fio_path: fio_path.as_deref(),
                    fio_size_mib,
                    fio_runtime_seconds,
                    iperf_server: iperf_server.as_deref(),
                },
            )?;
            println!("{}",serde_json::to_string_pretty(&report)?);
            eprintln!("Saved benchmark report to {}",output.display());
        },
        Commands::Compare{baseline,candidate}=>{
            print!("{}",benchmark::compare(&baseline,&candidate)?);
        },
        Commands::Sources=>sources(),
        Commands::Doctor{profile}=>doctor(&profile)?,
        Commands::Profile{file,create}=>profile(&file,create)?,
        Commands::Build{profile,dry_run}=>build(&profile,dry_run)?,
        Commands::Finalize{profile,version}=>finalize(&profile,version.as_deref())?,
        Commands::ArchPackage{profile,version,output_dir}=>{
            let p:Profile=toml::from_str(&fs::read_to_string(&profile)
                .with_context(||format!("reading build profile {}",profile.display()))?)?;
            validate_profile(&p)?;
            let version=infer_finalize_version(&p,version.as_deref())?;
            let release=format!("{version}{}",p.local_version);
            let output_dir=output_dir.unwrap_or_else(||p.stage_dir.join("packages"));
            let package=arch_package::build(&p.stage_dir,&release,&version,&output_dir)?;
            println!("Arch package created at {}",package.display());
        },
        Commands::Install{package,yes}=>arch_package::install(&package,yes)?,
        Commands::Tune{profile,dry_run}=>tuning::tune(profile,dry_run)?,
        Commands::Revert{dry_run}=>tuning::revert(dry_run)?,
    }
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finalize_version_is_inferred_from_a_single_staged_release() {
        let root = std::env::temp_dir().join(format!(
            "kernel-optimizer-finalize-version-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos()
        ));
        let stage_dir = root.join("stage");
        fs::create_dir_all(stage_dir.join("root/lib/modules/7.3.4-ko")).unwrap();

        let profile = Profile {
            stage_dir: stage_dir.clone(),
            ..Profile::default()
        };
        assert_eq!(infer_finalize_version(&profile, None).unwrap(), "7.3.4");
        assert_eq!(
            infer_finalize_version(&profile, Some("7.3.4")).unwrap(),
            "7.3.4"
        );

        fs::create_dir_all(stage_dir.join("root/lib/modules/7.3.5-ko")).unwrap();
        assert!(infer_finalize_version(&profile, None).is_err());
        assert_eq!(
            infer_finalize_version(&profile, Some("7.3.4")).unwrap(),
            "7.3.4"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finalize_version_rejects_a_profile_version_mismatch() {
        let profile = Profile {
            version: "7.3.5".into(),
            ..Profile::default()
        };
        assert!(infer_finalize_version(&profile, Some("7.3.4")).is_err());
    }

    #[test]
    fn default_profile_passes_preflight_validation() {
        assert!(validate_profile(&Profile::default()).is_ok());
    }

    #[test]
    fn profile_validation_rejects_unsupported_compiler_and_sources() {
        let profile = Profile {
            compiler: "zig".into(),
            ..Profile::default()
        };
        assert!(validate_profile(&profile).is_err());

        let profile = Profile {
            source: "linux-zen".into(),
            ..Profile::default()
        };
        assert!(validate_profile(&profile).is_ok());
        let profile = Profile { source: "linux-tkg".into(), ..Profile::default() };
        assert!(validate_profile(&profile).is_err());
        let profile = Profile { source: "linux-tkg".into(), allow_unverified_sources: true, ..Profile::default() };
        assert!(validate_profile(&profile).is_ok());
        let profile = Profile { source: "linux-cachyos".into(), ..Profile::default() };
        assert!(validate_profile(&profile).is_err());
        assert_eq!(release_moniker("linux-zen").unwrap(), "stable");
        assert_eq!(release_moniker("linux-rt").unwrap(), "stable");
        assert_eq!(release_moniker("linux-cachyos").unwrap(), "stable");
        assert_eq!(release_moniker("linux-tkg").unwrap(), "stable");
        let profile = Profile { source: "unknown".into(), ..Profile::default() };
        assert!(validate_profile(&profile).is_err());
    }

    #[test]
    fn profile_validation_rejects_unsafe_local_version_and_shared_paths() {
        let profile = Profile {
            local_version: "-custom/../../boot".into(),
            ..Profile::default()
        };
        assert!(validate_profile(&profile).is_err());

        let defaults = Profile::default();
        let profile = Profile {
            stage_dir: defaults.build_dir.clone(),
            ..defaults
        };
        assert!(validate_profile(&profile).is_err());

        let defaults = Profile::default();
        let profile = Profile {
            stage_dir: defaults.build_dir.join("stage"),
            ..defaults
        };
        assert!(validate_profile(&profile).is_err());
    }

    #[test]
    fn boot_artifacts_are_copied_to_stage_without_install_hooks() {
        let root = std::env::temp_dir().join(format!(
            "kernel-optimizer-boot-stage-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos()
        ));
        let source = root.join("source");
        let stage_root = root.join("stage-root");
        let boot_dir = stage_root.join("boot");
        let image_relative = kernel_image_candidates(std::env::consts::ARCH).unwrap()[0];
        let image_source = source.join(image_relative);
        fs::create_dir_all(image_source.parent().unwrap()).unwrap();
        fs::write(&image_source, b"test kernel image").unwrap();
        fs::write(source.join(".config"), b"CONFIG_TEST=y\n").unwrap();
        fs::write(source.join("System.map"), b"test system map\n").unwrap();

        let release = "6.1.1-ko";
        let image_name = stage_kernel_boot_artifacts(
            &source,
            &boot_dir,
            release,
            std::env::consts::ARCH,
        )
        .unwrap();

        assert_eq!(image_name, format!("vmlinuz-{release}"));
        assert_eq!(fs::read(boot_dir.join(&image_name)).unwrap(), b"test kernel image");
        assert_eq!(
            fs::read(boot_dir.join(format!("config-{release}"))).unwrap(),
            b"CONFIG_TEST=y\n"
        );
        assert_eq!(
            fs::read(boot_dir.join(format!("System.map-{release}"))).unwrap(),
            b"test system map\n"
        );
        let (_, _, modules_staged) = verify_staged_kernel(&stage_root, release).unwrap();
        assert!(!modules_staged);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clean_source_refresh_discards_edits_but_preserves_config() {
        let root = std::env::temp_dir().join(format!(
            "kernel-optimizer-source-refresh-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos()
        ));
        let build_dir = root.join("build");
        let input_dir = root.join("input");
        let source = build_dir.join("linux-6.1.1");
        let archive = build_dir.join("linux-6.1.1.tar");
        let archived_source = input_dir.join("linux-6.1.1");
        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&archived_source).unwrap();
        fs::create_dir_all(&source).unwrap();
        fs::write(archived_source.join("Makefile"), "official source").unwrap();
        fs::write(source.join("Makefile"), "locally modified source").unwrap();
        fs::write(source.join(".config"), "user kernel config").unwrap();
        fs::write(source.join("untrusted.patch"), "not part of signed archive").unwrap();

        let status = Command::new("tar")
            .args(["--create", "--file"])
            .arg(&archive)
            .arg("--directory")
            .arg(&input_dir)
            .arg("linux-6.1.1")
            .status()
            .unwrap();
        assert!(status.success());

        let preserved = refresh_source_tree(&archive, &source, &build_dir, "6.1.1").unwrap();
        assert!(preserved);
        assert_eq!(fs::read_to_string(source.join("Makefile")).unwrap(), "official source");
        assert_eq!(fs::read_to_string(source.join(".config")).unwrap(), "user kernel config");
        assert!(!source.join("untrusted.patch").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn clean_source_refresh_refuses_symlink_source_directory() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!(
            "kernel-optimizer-source-symlink-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos()
        ));
        let build_dir = root.join("build");
        let outside = root.join("outside");
        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let source = build_dir.join("linux-6.1.1");
        symlink(&outside, &source).unwrap();
        let archive = build_dir.join("missing.tar");
        let result = refresh_source_tree(&archive, &source, &build_dir, "6.1.1");
        assert!(result.is_err());
        assert!(source.is_symlink());
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod source_adapter_tests {
    use super::*;

    #[test]
    fn stable_and_lts_tracks_resolve_to_distinct_kernel_org_monikers() {
        assert_eq!(release_moniker("linux").unwrap(), "stable");
        assert_eq!(release_moniker("linux-lts").unwrap(), "longterm");
        for source in ["linux-zen", "linux-cachyos", "linux-tkg", "linux-rt"] {
            assert_eq!(release_moniker(source).unwrap(), "stable");
        }
    }
}
