use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
mod tuning;
use tuning::Workload;
mod hardware;
mod benchmark;
mod recommendations;
mod tui;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{fs, path::{Path, PathBuf}, process::Command, time::Duration};

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
struct Profile {
    source: String, version: String, local_version: String, jobs: usize,
    config_base: String, compiler: String, optimization: String,
    use_mold: bool, ccache: bool, build_dir: PathBuf, stage_dir: PathBuf,
    enable_lto: bool, enable_bpf: bool, menuconfig: bool,
}
impl Default for Profile {
    fn default() -> Self { Self {
        source:"linux".into(), version:"stable".into(), local_version:"-ko".into(),
        jobs: std::thread::available_parallelism().map(|x| x.get()).unwrap_or(2),
        config_base:"current".into(), compiler:"gcc".into(), optimization:"native".into(),
        use_mold:false, ccache:true, build_dir:PathBuf::from("/var/tmp/kernel-optimizer/build"),
        stage_dir:PathBuf::from("/var/tmp/kernel-optimizer/stage"),
        enable_lto:false, enable_bpf:true, menuconfig:false,
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

    let checksum = Command::new("sha256sum")
        .arg(&image)
        .output()
        .context("running sha256sum on staged kernel image")?;
    if !checksum.status.success() {
        bail!("sha256sum failed for staged kernel image");
    }
    let digest = String::from_utf8_lossy(&checksum.stdout)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_owned();
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("could not parse SHA-256 digest for staged kernel image");
    }

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
    if p.build_dir == p.stage_dir { bail!("build_dir and stage_dir must be different directories"); }
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
    let required = ["make", "curl", "xz", "tar", "bc", "bison", "flex", "perl", "python3"];
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

fn build(profile_path:&Path,dry:bool)->Result<()> {
    let mut p:Profile=toml::from_str(&fs::read_to_string(profile_path)?)?;
    validate_profile(&p)?;
    if p.version=="stable" {p.version=latest_stable()?;}
    let src=p.build_dir.join(format!("linux-{}",p.version));
    let archive=p.build_dir.join(format!("linux-{}.tar.xz",p.version));
    let url=format!("https://cdn.kernel.org/pub/linux/kernel/v{}.x/linux-{}.tar.xz",p.version.split('.').next().unwrap(),p.version);
    println!("Kernel: {}{}",p.version,p.local_version);
    println!("CPU: {}",hardware::detect().cpu.model);
    println!("Jobs: {}",p.jobs);
    println!("Compiler: {} (ccache: {}, mold requested: {})",p.compiler,p.ccache,p.use_mold);
    println!("Optimization profile: {} (currently informational; no unsafe CPU-specific flags injected)",p.optimization);
    println!("Source: {url}");
    println!("Build dir: {}",src.display());
    if dry {println!("Dry run only. No files changed.");return Ok(());}
    fs::create_dir_all(&p.build_dir)?;
    ensure_kernel_archive(&archive, &url)?;
    if !src.exists(){run_checked(Command::new("tar").arg("-xf").arg(&archive).arg("-C").arg(&p.build_dir))?;}
    let config=src.join(".config");
    if !config.exists() {
        match p.config_base.as_str() {
            "current" => {
                let release = output("uname", &["-r"]);
                let boot_config = PathBuf::from(format!("/boot/config-{release}"));
                let module_config = PathBuf::from(format!("/lib/modules/{release}/build/.config"));
                if Path::new("/proc/config.gz").exists()
                    && let Ok(decoded) = Command::new("gzip").args(["-dc", "/proc/config.gz"]).output()
                    && decoded.status.success()
                {
                    fs::write(&config, decoded.stdout)?;
                }
                if !config.exists() {
                    for candidate in [&boot_config, &module_config] {
                        if candidate.is_file() {
                            fs::copy(candidate, &config)
                                .with_context(|| format!("copying current kernel config from {}", candidate.display()))?;
                            break;
                        }
                    }
                }
                if !config.exists() {
                    println!("No current kernel config found; falling back to defconfig.");
                    run_checked(Command::new("make").current_dir(&src).arg("defconfig"))?;
                }
            }
            "defconfig" => run_checked(Command::new("make").current_dir(&src).arg("defconfig"))?,
            other => bail!("unsupported config_base '{other}' (use current or defconfig)"),
        }
    }
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
    run_checked(&mut make)?;
    fs::create_dir_all(&p.stage_dir)?;
    let release = kernel_release(&src)?;
    run_checked(Command::new("make").current_dir(&src).arg("modules_install").env("INSTALL_MOD_PATH",p.stage_dir.join("root")))?;
    run_checked(Command::new("make").current_dir(&src).arg("install").env("INSTALL_PATH",p.stage_dir.join("boot")))?;
    let (image_name, image_sha256, modules_staged) = verify_staged_kernel(&p.stage_dir, &release)?;
    let metadata = format!(
        "version={}\nlocalversion={}\nkernel_release={}\nsource={}\nkernel_image={}\nkernel_image_sha256={}\nmodules_staged={}\n",
        p.version, p.local_version, release, p.source, image_name, image_sha256, modules_staged
    );
    fs::write(p.stage_dir.join("BUILD-METADATA.txt"), metadata)?;
    println!("Verified staged kernel image: {image_name} (SHA-256 {image_sha256})");
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
    }
}
