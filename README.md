# Kernel Optimizer

A Rust CLI prototype for inspecting Linux hardware, discovering the current upstream stable kernel, creating an explicit build profile, compiling a kernel and staging artifacts for review.

> **Safety:** this project does not replace your running kernel, install modules into the host, or modify GRUB/systemd-boot. Build output is staged separately. Bootloader integration is intentionally disabled until distribution-specific package and rollback adapters exist.

## Status

The current milestone is a read-only hardware advisor, a conservative system-tuning tool,
a repeatable benchmark/report workflow, and an upstream kernel build-and-stage prototype.
The running kernel and bootloader are not modified by the build command.

**Implemented**

- [x] Hardware inventory and workload-aware, read-only recommendations.
- [x] Interactive read-only terminal dashboard.
- [x] Upstream stable kernel discovery, configurable build profiles, and staged builds.
- [x] Staged kernel image verification, pinned OpenPGP source-signature verification, and SHA-256 build provenance.
- [x] Read-only build preflight (doctor) for dependencies, config source and disk space.
- [x] Workload-aware sysctl/I/O-scheduler preview, apply and revert.
- [x] CPU, memory, optional fio/iperf3 benchmarks and report comparison.
- [x] Automated unit tests and CI lint/test checks.

**Next**

- [ ] Add broader hardware-independent tests for build staging, tuning rollback and benchmark edge cases.
- [ ] Add distro packaging adapters (Arch/Garuda first) that produce installable packages without changing the boot path.
- [ ] Add initramfs, module-signing and Secure Boot preflight/integration.
- [ ] Add boot-entry creation only with a known-good fallback and tested rollback.
- [ ] Test builds and tuning behavior across multiple distributions and kernel configurations.
- [ ] Add additional kernel source adapters (linux-zen, CachyOS, TKG and PREEMPT_RT) after their patch/config provenance is defined.

The install subcommand is intentionally disabled. Package installation and bootloader
mutation remain separate future milestones; a staged image is not an installable or
bootable system by itself.

## Requirements

Rust stable, a C toolchain, make, curl, xz, tar, GnuPG 2.4 or later, bc, bison, flex, OpenSSL development headers, elfutils, pahole, perl, python and the usual kernel build dependencies. On Arch/Garuda, start with:

```sh
sudo pacman -S --needed base-devel rust curl xz gnupg bc bison flex openssl elfutils pahole perl python ccache
```

Optional tools such as `lspci`, `lsblk`, `zcat` and `menuconfig` enhance inspection/configuration.

## Verified upstream kernel sources

The upstream builder downloads both `linux-X.Y.Z.tar.xz` and its detached
`linux-X.Y.Z.tar.sign` signature. Kernel.org signs the **uncompressed tar stream**,
not the compressed XZ file. The application therefore streams
`xz --decompress --stdout` into GnuPG's detached-signature verifier.

Verification is mandatory. It is not possible to disable it in the profile. The
GnuPG machine-readable `VALIDSIG` result must identify exactly one primary key
from this compiled-in allowlist:

| Release signer | Pinned primary-key fingerprint |
|---|---|
| Linus Torvalds | `ABAF11C65A2970B130ABE3C479BE3E4300411886` |
| Greg Kroah-Hartman | `647F28654894E3BD457199BE38DBBDC86092693E` |
| Sasha Levin | `E27E5D8A3403A2EF66873BBCDEA66FF797772CDC` |
| Ben Hutchings | `AC2B29BD34A6AFDDB3F68F35E7BFC8EC95861109` |

The fingerprints are maintained in `src/provenance.rs` and are based on the
[official kernel.org signature guidance](https://www.kernel.org/signature.html).
A key downloaded from the same server as the tarball is **not** trusted just
because it was imported. The application compares the full primary fingerprint
against the allowlist. It also disables automatic key retrieval during verification
and fails on bad, expired or revoked signature status.

### Prepare the GnuPG keyring once

Import the signer keys into the keyring of the unprivileged account that will run
the build. Kernel.org documents WKD lookup with GnuPG; for example:

```sh
gpg --locate-keys torvalds@kernel.org gregkh@kernel.org
gpg --fingerprint torvalds@kernel.org gregkh@kernel.org

# When using a dedicated keyring configured as gpg_home:
gpg --homedir /path/to/keyring --locate-keys torvalds@kernel.org gregkh@kernel.org
gpg --homedir /path/to/keyring --fingerprint torvalds@kernel.org gregkh@kernel.org
```

Compare the **complete primary fingerprints** with the allowlist above and with
kernel.org's published page before relying on an imported key. GnuPG may display
an owner-trust warning for a key that has not been assigned local trust; the
application does not use owner-trust as identity proof, but it does require a
cryptographically valid signature from one of the exact pinned primary keys.
Do not use a keyserver result or a short key ID as a substitute for checking the
full fingerprint.

By default, the application uses the current user's normal GnuPG home. Set
`gpg_home` in `kernel-optimizer.toml` to use a dedicated keyring. Do not run the
build with `sudo` merely to gain access to a keyring; prepare the keyring for the
same user that builds the kernel.

The signer allowlist is intentionally static and fail-closed. If kernel.org changes
its published release signers or fingerprints, the application will reject a new
signer until the allowlist is reviewed and updated in a code change.

### Clean extraction and local configuration

After signature verification, each build extracts a fresh source tree from the
verified archive. It preserves only the previous `.config`; edits to source files
or local patches inside the extracted tree are discarded. This prevents unverified
changes left in a reused build directory from being compiled as if they were part
of the signed release. Custom patch sets need a future explicit, separately
verified source-adapter workflow.

## Build the application

```sh
cargo build --release
sudo install -Dm755 target/release/kernel-optimizer /usr/local/bin/kernel-optimizer
```

## Usage

```sh
kernel-optimizer inspect
kernel-optimizer sources
kernel-optimizer doctor
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
# Optional: use a dedicated GnuPG keyring instead of the current user keyring.
# gpg_home = "/home/your-user/.gnupg"
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

Before extracting or compiling, the builder verifies the detached signature against
the pinned kernel.org signer fingerprints and records the source archive and
signature SHA-256 hashes. After compiling, it runs `make kernelrelease`, locates
the matching staged kernel image, hashes the effective `.config`, image,
`System.map` (when present), and staged modules tree, and writes both
`BUILD-METADATA.txt` and a machine-readable `provenance.json`.

The JSON manifest records the source and signature URLs/hashes, signer identity and
primary fingerprint, signature creation time and algorithm identifiers, GnuPG/XZ/
curl/tar versions, profile (without the private GnuPG home path), compiler and
ccache versions, make/rustc versions, build timestamps and commands, host kernel/
architecture/CPU, effective config hash/origin, kernel release and artifact hashes.
The modules tree hash uses the documented internal `sha256-tree-v1` encoding;
it hashes relative paths, entry types, file contents and symlink targets without
following symlinks.

These records improve traceability; they do **not** claim a reproducible build,
prove the compiler or host was uncompromised, or replace Secure Boot/module
signing. A checksum detects later artifact changes; source authenticity comes
from the verified detached developer signature.

The build remains staged outside the live boot path. Initramfs generation,
distribution package creation, module signing, Secure Boot enrollment, bootloader
entry creation and rollback integration are not yet automated. Keep a known-good
boot entry and review the staged image before installing it.
