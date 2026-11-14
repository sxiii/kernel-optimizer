use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
mod tuning;
use tuning::Workload;
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

#[derive(Debug, Serialize)]
struct Hardware {
    uname: String, architecture: String, cpu_model: String, logical_cpus: usize,
    cpu_flags: Vec<String>, memory_kib: Option<u64>, pci_devices: Vec<String>,
    block_devices: Vec<String>, current_cmdline: String, virtualization: String,
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

fn output(program:&str,args:&[&str])->String {
    Command::new(program).args(args).output().ok().filter(|o|o.status.success())
        .map(|o|String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}
fn read(path:&str)->String { fs::read_to_string(path).unwrap_or_default() }
fn hardware()->Hardware {
    let cpuinfo=read("/proc/cpuinfo");
    let model=cpuinfo.lines().find_map(|l|l.strip_prefix("model name\t:").or_else(||l.strip_prefix("Hardware\t:")).map(str::trim)).unwrap_or("Unknown CPU").to_string();
    let flags=cpuinfo.lines().find(|l|l.starts_with("flags")||l.starts_with("Features")).and_then(|l|l.split_once(':')).map(|(_,v)|v.split_whitespace().map(str::to_string).collect()).unwrap_or_default();
    let mem=read("/proc/meminfo").lines().find(|l|l.starts_with("MemTotal:")).and_then(|l|l.split_whitespace().nth(1)).and_then(|v|v.parse().ok());
    let pci=output("lspci",&["-nn"]).lines().map(str::to_string).collect();
    let blocks=output("lsblk",&["-J","-o","NAME,TYPE,SIZE,MODEL,TRAN"]).parse::<serde_json::Value>().ok()
        .and_then(|v|v["blockdevices"].as_array().cloned()).unwrap_or_default().iter().map(|v|format!("{} {} {} {}",v["name"].as_str().unwrap_or("?"),v["type"].as_str().unwrap_or("?"),v["size"].as_str().unwrap_or("?"),v["model"].as_str().unwrap_or(""))).collect();
    let virt=output("systemd-detect-virt",&[]); 
    Hardware { uname:output("uname",&["-r"]),architecture:output("uname",&["-m"]),cpu_model:model,logical_cpus:std::thread::available_parallelism().map(|x|x.get()).unwrap_or(1),cpu_flags:flags,memory_kib:mem,pci_devices:pci,block_devices:blocks,current_cmdline:read("/proc/cmdline").trim().into(),virtualization:if virt.is_empty(){"none".into()}else{virt} }
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
    println!("CPU: {}",hardware().cpu_model);
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
            "current" => { let current=Path::new("/proc/config.gz"); if current.exists(){run_checked(Command::new("sh").arg("-c").arg(format!("zcat /proc/config.gz > '{}'",config.display())))?;} else {run_checked(Command::new("make").current_dir(&src).arg("defconfig"))?;} },
            "defconfig" => run_checked(Command::new("make").current_dir(&src).arg("defconfig"))?,
            other => bail!("unsupported config_base '{other}' (use current or defconfig)"),
        }
    }
    let local=p.local_version.clone();
    run_checked(Command::new("scripts/config").current_dir(&src).args(["--set-str","LOCALVERSION",&local]))?;
    if p.enable_bpf {run_checked(Command::new("scripts/config").current_dir(&src).args(["--enable","BPF","--enable","BPF_SYSCALL"]))?;}
    if p.enable_lto {println!("LTO requested: kernel config must support a compatible LLVM toolchain; not forcing incompatible LTO options.");}
    if p.menuconfig {run_checked(Command::new("make").current_dir(&src).arg("menuconfig"))?;}
    let mut make=Command::new("make"); make.current_dir(&src).arg(format!("-j{}",p.jobs));
    if p.compiler=="clang" {make.env("LLVM","1");}
    if p.ccache {make.env("CC","ccache gcc");}
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
        Commands::Inspect=>println!("{}",serde_json::to_string_pretty(&hardware())?),
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
