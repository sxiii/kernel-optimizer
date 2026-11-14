# Kernel Optimizer roadmap

This roadmap prioritizes reliability and recoverability before adding more kernel sources
or enabling host installation. The project must never treat a staged kernel image as
sufficient proof that a machine can boot it.

## Priority 1 — Build reliability and diagnostics

- [x] Add a read-only doctor command to validate profile settings, required tools,
  selected compiler/cache, current-kernel config availability, and free space.
- [x] Reject invalid build-profile values before downloading or compiling.
- [x] Make benchmark comparison validate report schemas and metric data.
- [x] Report baseline metrics missing from a candidate and candidate-only metrics.
- [ ] Expand tests around archive corruption/re-download, staged-image selection,
  failed staging, tuning transaction rollback and filesystem errors.
- [ ] Run integration tests against representative GCC and LLVM kernel build environments.

## Priority 2 — Source integrity and build provenance

- [ ] Verify kernel.org source signatures using a pinned, documented maintainer-key
  trust process; do not silently trust a key fetched from the same download endpoint.
- [ ] Record source archive SHA-256, signature verification result, compiler/tool versions,
  effective kernel config hash, build command and build timestamp in build metadata.
- [ ] Preserve a machine-readable manifest for the staged image and modules.
- [ ] Add deterministic checks that the staged image release matches the staged module tree.

## Priority 3 — Distribution packaging (Arch/Garuda first)

- [ ] Generate a reviewable Arch package build description from staged artifacts.
- [ ] Package modules, kernel image, System.map and config without invoking pacman.
- [ ] Detect existing kernel package names and prevent accidental file/package collisions.
- [ ] Test package build in a disposable clean Arch environment.
- [ ] Keep package installation as a separate, explicit user action.

## Priority 4 — Bootability, signing and rollback

- [ ] Detect the actual bootloader, EFI/BIOS mode, ESP, mount state and current boot entries.
- [ ] Detect initramfs tooling and generate an initramfs only through a supported adapter.
- [ ] Check module signing and Secure Boot state before proposing installation.
- [ ] Create a new boot entry without replacing the known-good entry.
- [ ] Verify the fallback entry remains selectable and provide a tested rollback path.
- [ ] Require a dry-run plan and explicit confirmation before any boot-path mutation.

## Priority 5 — More kernel sources and broader validation

- [ ] Add linux-zen, CachyOS, TKG and PREEMPT_RT adapters with pinned patch sources,
  documented config policies and reproducible provenance.
- [ ] Test supported paths on Arch/Garuda, Debian/Ubuntu and Fedora.
- [ ] Add hardware-specific benchmark suites only when their results are reproducible
  and their limitations are clearly reported.
- [ ] Consider a graphical interface after CLI/TUI workflows and safety guarantees stabilize.

## Explicit non-goals for the current milestone

- No automatic replacement of the running kernel.
- No automatic bootloader configuration changes.
- No automatic Secure Boot key enrollment.
- No execution of arbitrary third-party patch scripts as root.
