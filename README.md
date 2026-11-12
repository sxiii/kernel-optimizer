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
