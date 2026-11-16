# Kernel Optimizer roadmap

This roadmap prioritizes reliability and recoverability before automating boot-path
changes. Kernel package installation exists as a separate, explicit operation; the
project must never treat a staged or installed image as proof that a machine can boot it.

## Priority 1 — Build reliability and diagnostics

- [x] Add a read-only doctor command to validate profile settings, required tools,
  selected compiler/cache, current-kernel config availability, and free space.
- [x] Reject invalid build-profile values before downloading or compiling.
- [x] Make benchmark comparison validate report schemas and metric data.
- [x] Report baseline metrics missing from a candidate and candidate-only metrics.
- [x] Expand hardware-independent tests for corrupt-cache re-download, failed download cleanup,
  staged-image selection and preflight failures, tuning transaction rollback and benchmark edge cases.
- [ ] Run integration tests against representative GCC and LLVM kernel build environments.

## Priority 2 — Source integrity and build provenance

- [x] Verify kernel.org source tarball signatures against a pinned maintainer-key
  allowlist, and fail closed for missing, unknown, expired or revoked signatures.
- [x] Record source archive/signature hashes, signer identity and fingerprint, signature
  time/algorithms, tool versions, effective kernel config hash, build commands and timing.
- [x] Write a machine-readable provenance manifest with kernel image, System.map and
  modules-tree hashes plus host/build metadata.
- [x] Re-extract the verified archive on each build, preserving only the previous .config.
- [ ] Add an end-to-end verification test using a test keyring and deliberately altered
  archives/signatures; keep the test signer separate from the production allowlist.
- [ ] Add a documented maintenance procedure for reviewing signer-key revocations and
  updating the pinned fingerprint set from kernel.org's published source.

## Priority 3 — Distribution packaging (Arch/Garuda first)

- [x] Generate a reviewable Arch package build description from staged artifacts.
- [x] Package modules, kernel image, System.map and config without invoking pacman.
- [x] Detect conflicting staged/installed module directories and owned boot-image/preset files before package installation.
- [ ] Test package build in a disposable clean Arch environment.
- [x] Keep package installation as a separate, explicit user action.

## Priority 4 — Bootability, signing and rollback

- [ ] Detect the actual bootloader, EFI/BIOS mode, ESP, mount state and current boot entries.
- [x] Detect mkinitcpio/dracut and their active pacman hooks; package installation delegates
  initramfs generation to the detected distribution hook rather than invoking an ad-hoc command.
- [x] Check Secure Boot state before Arch package installation and fail closed when enabled or
  indeterminate; reject package paths that require forced module-signature enforcement.
- [ ] Add module signing support and a complete Secure Boot signing/enrollment workflow.
- [ ] Create a new boot entry without replacing the known-good entry.
- [ ] Verify the fallback entry remains selectable and provide a tested rollback path.
- [ ] Require a dry-run plan and explicit confirmation before any boot-path mutation.

## Priority 5 — More kernel sources and broader validation

- [x] Add Linux Zen, CachyOS, TKG and PREEMPT_RT adapters with pinned patch sources,
  recorded hashes/signature state and explicit opt-in for unsigned CachyOS/TKG material.
- [ ] Test supported paths on Arch/Garuda, Debian/Ubuntu and Fedora.
- [ ] Add hardware-specific benchmark suites only when their results are reproducible
  and their limitations are clearly reported.
- [x] Add a native desktop GUI for every currently implemented CLI workflow, with command logs, JSON benchmark import/comparison charts, and privileged actions gated by PolicyKit.
- [x] Connect the GUI Installation page to package creation and explicitly authorized pacman installation.
- [ ] Add automated integration tests in a disposable clean Arch/Garuda environment; bootloader entry management and Secure Boot signing remain deliberately manual/unsupported.

All six kernel source tracks have now been successfully built on the project maintainer's
host. This is useful runtime validation of the source adapters, but it does not establish
cross-distribution compatibility, reproducible builds or successful bootability.

## Explicit non-goals for the current milestone

- No automatic replacement of the running kernel.
- No automatic bootloader configuration changes.
- No automatic Secure Boot key enrollment.
- No execution of arbitrary third-party patch scripts as root.
