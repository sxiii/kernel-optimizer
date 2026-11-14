# Kernel Optimizer

A Rust CLI prototype for inspecting Linux hardware, discovering the current upstream stable kernel, creating an explicit build profile, compiling a kernel and staging artifacts for review.

> **Safety:** this project does not replace your running kernel, install modules into the host, or modify GRUB/systemd-boot. Build output is staged separately. Bootloader integration is intentionally disabled until distribution-specific package and rollback adapters exist.

## Status

This is the first usable milestone, not yet the complete GUI/installer envisioned for the project.

- [x] Hardware inventory: CPU model/features, architecture, logical CPUs, RAM, PCI devices, block devices, current kernel and virtualization.
- [x] Kernel.org stable version discovery.
- [x] Configurable profile (source, version, config base, compiler, jobs, LTO/BPF preferences, paths).
- [x] Upstream kernel download, archive integrity check, config and compilation.
- [x] Stage modules and boot artifacts outside the host.
- [ ] Adapters for linux-zen, CachyOS, TKG, RT and distro kernel packaging.
- [ ] Post-build kernel/module verification and signing.
- [ ] Safe integration with GRUB/systemd-boot and boot-entry rollback.
- [ ] TUI/GUI.

## Requirements

Rust stable, a C toolchain, make, curl, xz, tar, bc, bison, flex, openssl development headers, elfutils, pahole, perl, python and the usual kernel build dependencies. On Arch/Garuda, start with:

```sh
sudo pacman -S --needed base-devel rust curl xz bc bison flex openssl elfutils pahole perl python ccache
```

Optional tools such as `lspci`, `lsblk`, `zcat` and `menuconfig` enhance inspection/configuration.

## Build the application

```sh
cargo build --release
sudo install -Dm755 target/release/kernel-optimizer /usr/local/bin/kernel-optimizer
```

## Usage

```sh
kernel-optimizer inspect
kernel-optimizer sources
kernel-optimizer profile --create
kernel-optimizer profile
kernel-optimizer build --dry-run
```

Edit `kernel-optimizer.toml` before a real build. The defaults use the current kernel config when available and build with the detected logical CPU count.

```toml
source = "linux"
version = "stable"
local_version = "-ko"
jobs = 16
config_base = "current" # current or defconfig
compiler = "gcc"        # gcc or clang
optimization = "native" # informational until safe Kconfig tuning is implemented
use_mold = false
ccache = true
build_dir = "/var/tmp/kernel-optimizer/build"
stage_dir = "/var/tmp/kernel-optimizer/stage"
enable_lto = false
enable_bpf = true
menuconfig = false
```

Builds can take considerable time and disk space. Use a dedicated filesystem with ample free space. Don't select `-march=native` for a kernel intended to boot on other machines. The `optimization` field is presently descriptive and does not silently alter kernel compiler flags.

## Design principles

1. Discover hardware as evidence, not as a reason to blindly disable drivers.
2. Keep the base kernel config and retain distribution-required options.
3. Never run arbitrary third-party patch scripts as root.
4. Compile and stage as an unprivileged user where possible.
5. Verify artifacts and create a distro package before touching the boot path.
6. Keep the currently bootable kernel as a known-good fallback.


## System tuning (sysctl and I/O scheduler)

The `tune` command generates a small managed configuration and supports preview, apply and revert:

```sh
# Preview only; no root required and no files are changed
kernel-optimizer tune --profile desktop --dry-run
kernel-optimizer tune --profile laptop --dry-run
kernel-optimizer tune --profile server --dry-run
kernel-optimizer tune --profile gaming --dry-run

# Apply (writes only the two named drop-ins)
sudo kernel-optimizer tune --profile desktop

# Remove our drop-ins and reload the remaining system configuration
sudo kernel-optimizer revert
kernel-optimizer revert --dry-run
```

The implementation writes `/etc/sysctl.d/99-kernel-optimizer.conf` and
`/etc/udev/rules.d/60-kernel-optimizer-io-scheduler.rules`. It does not edit
`/etc/sysctl.conf`, does not append duplicate lines, and replaces its own files
atomically. Before applying, it saves the current `sysctl -a` output under
`/var/backups/`. Unknown sysctl keys are tolerated during application.

### Detection and rationale

| Setting | Behavior | Trade-off / limitation |
|---|---|---|
| `vm.swappiness` | Active zram swap: 150 on Linux 5.8+ (100 on older kernels) and `vm.page-cluster=0`; otherwise profile-dependent conservative values | No universal optimum; zram is detected from active entries in `/proc/swaps` |
| `vm.min_free_kbytes` | Approximately 1% RAM, bounded to 64-512 MiB; skipped below 4 GiB RAM and in detected virtualization | Larger reserves reduce memory available to applications; value is a heuristic |
| MGLRU | Reports whether `/sys/kernel/mm/lru_gen/enabled` exists and its current value | MGLRU is a runtime sysfs interface, not sysctl; this version deliberately does not change it automatically |
| TCP BBR | Enables BBR plus `fq` only when the running kernel lists `bbr` in `tcp_available_congestion_control` | Does not load modules or guess BBRv2/v3; the kernel's advertised implementation is used |
| I/O schedulers | Generates udev rules only for scheduler names exposed by the running kernel | Rules affect matching devices on add/change; NVMe, SATA/SAS and virtual devices differ by driver |

The desktop, laptop, server and gaming profiles intentionally remain conservative.
Laptop and gaming profiles avoid aggressive network buffers and CPU power changes.
The server profile only adds a moderate listen backlog. Hardware-specific memory
reserve tuning is skipped when virtualization is detected. Container environments
may restrict sysctl writes; apply only after reviewing the preview.

### Rollback semantics

`revert` deletes only the two files owned by this application, then runs
`sysctl --system` and reloads udev rules. This restores values from the remaining
configuration files; it cannot guarantee the exact pre-apply runtime state if
other services changed settings concurrently. The timestamped backup is a
reference snapshot, not an automatically replayed restore file. Reverting the
udev rule does not necessarily change the scheduler already selected for a
currently running device; select its scheduler manually or re-trigger the device
after reviewing its current state.

### Benchmark before claiming an optimization

No performance improvement is claimed without measurements on the target
machine. Record baseline and tuned results using the same workload and kernel:

- CPU: `sysbench cpu --threads=N run`
- Storage: `fio` with a workload representative of the actual application
- Network: `iperf3` plus latency-under-load measurements
- Scheduler/latency: `schbench` or application-specific latency tests

Repeat runs, report median and variance, and include power consumption and
thermals for laptop/gaming comparisons. Avoid comparing results from different
kernel versions, CPU governors, or background-load conditions as if they were
caused by a single sysctl.

## CI

GitHub Actions checks Rust with Clippy and tests. A separate
ShellCheck/shfmt job runs on shell scripts if any are introduced.


## Hardware report and recommendations

```sh
kernel-optimizer hardware
kernel-optimizer recommend --profile desktop
kernel-optimizer recommend --profile laptop
kernel-optimizer recommend --profile server
kernel-optimizer recommend --profile gaming
```

The hardware report includes CPU vendor/family/model and a conservative microarchitecture
label, feature flags, logical/physical topology, RAM, virtualization, PCI GPU and network
controllers, block-device media type and available I/O schedulers, plus cpufreq policies
(governor, available governors, EPP and exposed frequency limits/current frequency).

Recommendations are read-only. They explain why a per-device scheduler might be suitable,
show the scheduler choices each disk actually advertises, and describe CPU power-policy
trade-offs for the selected workload. They do not change governors, Kconfig, sysfs, or
sysctl values. Microarchitecture decoding is intentionally marked as unknown or
family/model-specific when /proc/cpuinfo is insufficient for an exact CPUID interpretation.

## Benchmark and compare

The benchmark command runs repeatable single-thread integer-throughput and memory-copy
tests. Optional disk and network tests require explicit targets:

```sh
# Baseline report on the currently running kernel
kernel-optimizer benchmark --profile baseline --output baseline.json

# Candidate report after booting the other kernel/settings
kernel-optimizer benchmark --profile candidate --output candidate.json

# Optional: test sequential direct-I/O on a filesystem directory (fio required)
kernel-optimizer benchmark --profile candidate --fio-path /mnt/test-ssd \
  --fio-size-mib 256 --fio-runtime-seconds 10 --output candidate-ssd.json

# Optional: run iperf3 against a server you control
kernel-optimizer benchmark --profile candidate --iperf-server 192.0.2.10

# Compare two reports with identical workloads
kernel-optimizer compare --baseline baseline.json --candidate candidate.json
```

The fio test creates and removes a uniquely named temporary file in the supplied directory;
it does not overwrite a raw device or an existing file. It requires space for the selected
test-file size. The iperf3 test connects only to the server explicitly supplied by the user.
CPU and memory tests use the current process's available CPU set and a bounded memory buffer.

Reports record the kernel, architecture, CPU model/microarchitecture, logical CPU count,
RAM and cpufreq governors. The comparison command checks hardware/governor metadata and
requires matching workload parameters before comparing each metric. A higher throughput
number is better, but results remain sensitive to temperature, power limits, background
load, storage cache and network conditions. Repeat tests and compare medians; do not treat
small differences as statistically meaningful.


## Interactive terminal dashboard

```sh
kernel-optimizer tui
kernel-optimizer tui --profile laptop
kernel-optimizer tui --profile server
```

The Ratatui dashboard is read-only. Use `1` / `2` or `Tab` to switch between
hardware inventory and recommendations, arrow keys or PageUp/PageDown to scroll,
and `q` or `Esc` to exit. It does not apply sysctl values, change CPU governors,
edit kernel configuration, or run privileged commands.

## Kernel build staging verification

After compiling, the build pipeline runs `make kernelrelease`, locates the matching
staged kernel image, checks that it is non-empty, computes its SHA-256 digest, and
records the image name, release, digest and module-staging status in
`BUILD-METADATA.txt`. If more than one image exists and the newly built release
cannot be identified, verification fails rather than silently selecting an older
image. A checksum detects accidental corruption; it is not a signature or proof
of source authenticity.

The build remains staged outside the live boot path. Initramfs generation,
distribution package creation, module signing, Secure Boot enrollment, bootloader
entry creation and rollback integration are not yet automated. Keep a known-good
boot entry and review the staged image before installing it.
