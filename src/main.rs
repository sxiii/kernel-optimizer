use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
mod tuning;
use tuning::Workload;
mod hardware;
mod benchmark;
mod recommendations;
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
    /// Create or print an example build profile.
    Profile { #[arg(long, default_value="kernel-optimizer.toml")] file: PathBuf, #[arg(long)] create: bool },
    /// Fetch, configure, compile and stage a kernel. Installation requires a separate explicit step.
    Build { #[arg(long, default_value="kernel-optimizer.toml")] profile: PathBuf, #[arg(long)] dry_run: bool },
    /// Generate/apply workload-aware sysctl and I/O scheduler settings.
    Tune { #[arg(long, value_enum, default_value="desktop")] profile: Workload, #[arg(long)] dry_run: bool },
    /// Remove optimizer-managed sysctl and udev drop-ins.
    Revert { #[arg(long)] dry_run: bool },
    /// Install a previously staged kernel through the detected package/boot integration.
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
fn build(profile_path:&Path,dry:bool)->Result<()> {
    let mut p:Profile=toml::from_str(&fs::read_to_string(profile_path)?)?;
    if p.jobs==0 || p.jobs>4096 {bail!("jobs must be between 1 and 4096");}
    if p.source!="linux" {bail!("source '{}' is not supported by the safe builder yet; use source=linux",p.source);}
    if p.version=="stable" {p.version=latest_stable()?;}
    let version_re=Regex::new(r"^\d+\.\d+(\.\d+)?$")?;
    if !version_re.is_match(&p.version){bail!("invalid kernel version: {}",p.version);}
    if p.local_version.contains('/')||p.local_version.contains(' ') {bail!("local_version may not contain slash or spaces");}
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
    if !archive.exists(){run_checked(Command::new("curl").args(["--fail","--location","--retry","3","--output"]).arg(&archive).arg(&url))?;}
    // Verify archive integrity before extraction using xz test.
    run_checked(Command::new("xz").arg("-t").arg(&archive))?;
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
    run_checked(Command::new("make").current_dir(&src).arg("modules_install").env("INSTALL_MOD_PATH",p.stage_dir.join("root")))?;
    run_checked(Command::new("make").current_dir(&src).arg("install").env("INSTALL_PATH",p.stage_dir.join("boot")))?;
    fs::write(p.stage_dir.join("BUILD-METADATA.txt"),format!("version={}\nlocalversion={}\nsource={}\n",p.version,p.local_version,p.source))?;
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
