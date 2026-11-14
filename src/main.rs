use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
mod tuning;
use tuning::Workload;
mod hardware;
mod benchmark;
mod recommendations;
mod tui;
mod provenance;
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
    /// Run a read-only pre-build environment and profile check.
    Doctor { #[arg(long, default_value="kernel-optimizer.toml")] profile: PathBuf },
    /// Create or print an example build profile.
    Profile { #[arg(long, default_value="kernel-optimizer.toml")] file: PathBuf, #[arg(long)] create: bool },
    /// Fetch, configure, compile and stage a kernel. Installation requires a separate explicit step.
    Build { #[arg(long, default_value="kernel-optimizer.toml")] profile: PathBuf, #[arg(long)] dry_run: bool },
    /// Generate/apply workload-aware sysctl and I/O scheduler settings.
    Tune { #[arg(long, value_enum, default_value="desktop")] profile: Workload, #[arg(long)] dry_run: bool },
    /// Remove optimizer-managed sysctl and udev drop-ins.
    Revert { #[arg(long)] dry_run: bool },
    /// Reserved: installation stays disabled until package and rollback adapters are available.
    Install { #[arg(long)] staged: Option<PathBuf>, #[arg(long)] yes: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Profile {
    source: String, version: String, local_version: String, jobs: usize,
    config_base: String, compiler: String, optimization: String,
    use_mold: bool, ccache: bool, build_dir: PathBuf, stage_dir: PathBuf,
    enable_lto: bool, enable_bpf: bool, menuconfig: bool,
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
        enable_lto:false, enable_bpf:true, menuconfig:false, gpg_home:None,
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
    println!("SOURCE            TRACK / PURPOSE");
    println!("linux             Latest stable upstream kernel (kernel.org)");
    println!("linux-lts          Long-term support upstream series");
    println!("linux-zen          Desktop responsiveness / low-latency patchset (Arch)");
    println!("linux-cachyos      CachyOS patches and scheduler/config profiles");
    println!("linux-tkg          Community kernel patch/config framework (TKG)");
    println!("linux-rt           PREEMPT_RT real-time patchset (specialized workloads)");
    println!("\nOnly upstream linux is fetched automatically in this initial release. Other tracks are listed as planned adapters; no untrusted patch URL is executed.");
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
fn latest_stable()->Result<String> {
    let client=reqwest::blocking::Client::builder().timeout(Duration::from_secs(20)).user_agent("kernel-optimizer/0.1").build()?;
    let body=client.get("https://www.kernel.org/releases.json").send()?.error_for_status()?.text()?;
    let v:serde_json::Value=serde_json::from_str(&body)?;
    let versions=v["releases"].as_array().context("kernel.org releases response missing releases")?;
    versions.iter().find(|x|x["moniker"]=="stable").and_then(|x|x["version"].as_str()).map(str::to_string).context("stable version not found")
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
    if p.source != "linux" { bail!("source '{}' is not supported by the safe builder yet; use source=linux", p.source); }
    if p.version != "stable" && !Regex::new(r"^\d+\.\d+(\.\d+)?$")?.is_match(&p.version) { bail!("invalid kernel version: {}", p.version); }
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
    let required = ["make", "curl", "xz", "tar", "gpg", "bc", "bison", "flex", "perl", "python3"];
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


fn validate_profile(p: &Profile) -> Result<()> {
    if !(1..=4096).contains(&p.jobs) { bail!("jobs must be between 1 and 4096"); }
    if p.source != "linux" { bail!("source '{}' is not supported by the safe builder yet; use source=linux", p.source); }
    if p.version != "stable" && !Regex::new(r"^\d+\.\d+(\.\d+)?$")?.is_match(&p.version) { bail!("invalid kernel version: {}", p.version); }
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
    let required = ["make", "curl", "xz", "tar", "gpg", "bc", "bison", "flex", "perl", "python3"];
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
    fs::create_dir(&temporary)?;
    let extraction = run_checked(
        Command::new("tar")
            .args(["--extract", "--file"])
            .arg(archive)
            .arg("--directory")
            .arg(&temporary)
            .args(["--no-same-owner", "--no-same-permissions"]),
    );
    if let Err(error) = extraction {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error).context("extracting signed kernel source archive");
    }
    let extracted_root = temporary.join(format!("linux-{version}"));
    if !extracted_root.is_dir() {
        let _ = fs::remove_dir_all(&temporary);
        bail!("signed archive did not contain expected top-level directory linux-{version}");
    }
    let backup = build_dir.join(format!(".kernel-optimizer-old-source-{}-{stamp}", std::process::id()));
    let had_source = source_metadata.is_some();
    if had_source {
        if let Err(error) = fs::rename(source, &backup) {
            let _ = fs::remove_dir_all(&temporary);
            return Err(error).with_context(|| format!("moving previous source tree {} aside", source.display()));
        }
    }
    if let Err(error) = fs::rename(&extracted_root, source) {
        if had_source {
            let _ = fs::rename(&backup, source);
        }
        let _ = fs::remove_dir_all(&temporary);
        return Err(error).with_context(|| format!("promoting fresh source tree to {}", source.display()));
    }
    if let Some(config) = saved_config.as_ref() {
        if let Err(error) = fs::write(source.join(".config"), config) {
            let _ = fs::remove_dir_all(source);
            if had_source {
                let _ = fs::rename(&backup, source);
            }
            let _ = fs::remove_dir_all(&temporary);
            return Err(error).context("restoring the prior kernel .config into the clean source tree");
        }
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
    gpg_version: Option<String>,
    xz_version: Option<String>,
    curl_version: Option<String>,
    tar_version: Option<String>,
}

#[derive(Debug, Serialize)]
struct BuildProvenance {
    started_unix: u64,
    completed_unix: u64,
    duration_seconds: f64,
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
    host: HostProvenance,
    artifacts: ArtifactProvenance,
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
    fs::create_dir(&temporary)?;
    let extraction = run_checked(
        Command::new("tar")
            .args(["--extract", "--file"])
            .arg(archive)
            .arg("--directory")
            .arg(&temporary)
            .args(["--no-same-owner", "--no-same-permissions"]),
    );
    if let Err(error) = extraction {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error).context("extracting signed kernel source archive");
    }
    let extracted_root = temporary.join(format!("linux-{version}"));
    if !extracted_root.is_dir() {
        let _ = fs::remove_dir_all(&temporary);
        bail!("signed archive did not contain expected top-level directory linux-{version}");
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
    gpg_version: Option<String>,
    xz_version: Option<String>,
    curl_version: Option<String>,
    tar_version: Option<String>,
}

#[derive(Debug, Serialize)]
struct BuildProvenance {
    started_unix: u64,
    completed_unix: u64,
    duration_seconds: f64,
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
    host: HostProvenance,
    artifacts: ArtifactProvenance,
}

fn build(profile_path:&Path,dry:bool)->Result<()> {
    let mut p:Profile=toml::from_str(&fs::read_to_string(profile_path)?)?;
    validate_profile(&p)?;
    if p.version=="stable" {p.version=latest_stable()?;}
    let src=p.build_dir.join(format!("linux-{}",p.version));
    let archive=p.build_dir.join(format!("linux-{}.tar.xz",p.version));
    let signature=p.build_dir.join(format!("linux-{}.tar.sign",p.version));
    let major=p.version.split('.').next().context("kernel version has no major component")?;
    let url=format!("https://cdn.kernel.org/pub/linux/kernel/v{major}.x/linux-{}.tar.xz",p.version);
    let signature_url=format!("https://cdn.kernel.org/pub/linux/kernel/v{major}.x/linux-{}.tar.sign",p.version);
    println!("Kernel: {}{}",p.version,p.local_version);
    println!("CPU: {}",hardware::detect().cpu.model);
    println!("Jobs: {}",p.jobs);
    println!("Compiler: {} (ccache: {}, mold requested: {})",p.compiler,p.ccache,p.use_mold);
    println!("Optimization profile: {} (currently informational; no unsafe CPU-specific flags injected)",p.optimization);
    println!("Source: {url}");
    println!("Signature: {signature_url}");
    println!("Build dir: {}",src.display());
    if dry {println!("Dry run only. No files changed.");return Ok(());}
    fs::create_dir_all(&p.build_dir)?;
    let started_unix=unix_timestamp();
    let build_timer=Instant::now();
    ensure_kernel_archive(&archive, &url)?;
    download_signature(&signature, &signature_url)?;
    let archive_sha256_before=provenance::sha256_file(&archive)?;
    let signature_sha256_before=provenance::sha256_file(&signature)?;
    let signature_verification=provenance::verify_kernel_archive(&archive,&signature,p.gpg_home.as_deref())?;
    let archive_sha256=provenance::sha256_file(&archive)?;
    let signature_sha256=provenance::sha256_file(&signature)?;
    if archive_sha256 != archive_sha256_before || signature_sha256 != signature_sha256_before {
        bail!("kernel archive or detached signature changed during verification; refusing to build");
    }
    println!("Verified source signature by {} ({})",
        signature_verification.signer_name,signature_verification.signer_primary_fingerprint);
    let preserved_config=refresh_source_tree(&archive,&src,&p.build_dir,&p.version)?;
    if provenance::sha256_file(&archive)? != archive_sha256 {
        bail!("kernel archive changed after signature verification; refusing to build");
    }
    println!("Extracted a clean source tree from the signed archive{}.",
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
    let gpg_version=provenance::command_version("gpg",&["--version"]);
    let xz_version=provenance::command_version("xz",&["--version"]);
    let curl_version=provenance::command_version("curl",&["--version"]);
    let tar_version=provenance::command_version("tar",&["--version"]);
    let rustc_version=provenance::command_version("rustc",&["--version"]);
    run_checked(&mut make)?;
    fs::create_dir_all(&p.stage_dir)?;
    let release = kernel_release(&src)?;
    run_checked(Command::new("make").current_dir(&src).arg("modules_install").env("INSTALL_MOD_PATH",p.stage_dir.join("root")))?;
    run_checked(Command::new("make").current_dir(&src).arg("install").env("INSTALL_PATH",p.stage_dir.join("boot")))?;
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
    let source_manifest=SourceProvenance {
        project:"Linux kernel.org upstream".into(),
        version:p.version.clone(),
        archive_url:url,
        archive_sha256,
        signature_url,
        signature_sha256,
        signature_verified:true,
        signature_verification_method:"xz --decompress --stdout piped to gpg --no-options --batch --no-auto-key-import --no-auto-key-retrieve --status-fd=1 --verify".into(),
        trust_reference:"https://www.kernel.org/signature.html".into(),
        signature_creation_date:signature_verification.signature_creation_date,
        signature_timestamp:signature_verification.signature_timestamp,
        signature_public_key_algorithm:signature_verification.public_key_algorithm,
        signature_hash_algorithm:signature_verification.hash_algorithm,
        signer_name:signature_verification.signer_name,
        signer_primary_fingerprint:signature_verification.signer_primary_fingerprint,
        gpg_version,
        xz_version,
        curl_version,
        tar_version,
    };
    let compiler_command=if p.ccache { format!("ccache {}",p.compiler) } else { p.compiler.clone() };
    let mut recorded_commands=vec![
        "curl --fail --location --retry 3 (download kernel.org archive and detached signature)".into(),
        "xz -t <archive>".into(),
        "xz --decompress --stdout <archive> | gpg --no-options --batch --no-auto-key-import --no-auto-key-retrieve --no-auto-key-locate --status-fd=1 --verify <signature> -".into(),
        "tar --extract --file <signature-verified archive> --no-same-owner --no-same-permissions".into(),
    ];
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
        format!("make install INSTALL_PATH={}",p.stage_dir.join("boot").display()),
    ]);
    let build_manifest=BuildProvenance {
        started_unix,
        completed_unix,
        duration_seconds:build_timer.elapsed().as_secs_f64(),
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
        schema_version:1,
        generated_unix:completed_unix,
        application_version:env!("CARGO_PKG_VERSION"),
        source:source_manifest,
        build:build_manifest,
        host:host_manifest,
        artifacts,
    };
    provenance::write_json_atomic(&p.stage_dir.join("provenance.json"),&manifest)?;
    let metadata = format!(
        "version={}\nlocalversion={}\nkernel_release={}\nsource={}\nsource_signature_verified=true\nsource_signer={}\nsource_signer_fingerprint={}\nsource_archive_sha256={}\nsource_signature_sha256={}\neffective_config_sha256={}\nkernel_image={}\nkernel_image_sha256={}\nmodules_staged={}\nprovenance=provenance.json\n",
        p.version, p.local_version, release, p.source,
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
fn main()->Result<()> {
    let cli=Cli::parse();
    match cli.command {
        Commands::Inspect=>println!("{}",serde_json::to_string_pretty(&hardware::detect())?),
        Commands::Hardware=>println!("{}",serde_json::to_string_pretty(&hardware::detect())?),
        Commands::Recommend{profile}=>{
            let report=recommendations::generate(profile);
            println!("{}",serde_json::to_string_pretty(&report)?);
        },
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
        Commands::Doctor{profile}=>doctor(&profile)?,
        Commands::Profile{file,create}=>profile(&file,create)?,
        Commands::Build{profile,dry_run}=>build(&profile,dry_run)?,
        Commands::Tune{profile,dry_run}=>tuning::tune(profile,dry_run)?,
        Commands::Revert{dry_run}=>tuning::revert(dry_run)?,
        Commands::Install{staged,yes}=>{
            let _=staged; let _=yes;
            bail!("installation/bootloader integration is intentionally not enabled in 0.1. Review staged artifacts and use your distribution's supported packaging tools; automated bootloader mutation will be added after distro-specific adapters and rollback checks.");
        }
    }
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;

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
