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

- [x] Generate a reviewable Arch package build description from verified staged artifacts.
- [x] Package modules, kernel image, System.map and config without installing or invoking pacman.
- [x] Detect existing package ownership and block conflicting module directories before installation.
- [ ] Test package build in a disposable clean Arch environment.
- [x] Keep package installation as a separate, explicit PolicyKit-authorized user action.

## Priority 4 — Bootability, signing and rollback

- [ ] Expand bootloader detection to verify EFI/BIOS mode, ESP mount state and current boot entries.
- [x] Detect supported mkinitcpio/Dracut package hooks and let the distribution generate the initramfs during package installation.
- [ ] Add kernel/module signing support; installation currently blocks when Secure Boot is enabled or cannot be verified.
- [ ] Create a new boot entry without replacing the known-good entry.
- [ ] Verify the fallback entry remains selectable and provide a tested rollback path.
- [ ] Require a dry-run plan and explicit confirmation before any boot-path mutation.

## Priority 5 — More kernel sources and broader validation

- [ ] Add linux-zen, CachyOS, TKG and PREEMPT_RT adapters with pinned patch sources,
  documented config policies and reproducible provenance.
- [ ] Test supported paths on Arch/Garuda, Debian/Ubuntu and Fedora.
- [ ] Add hardware-specific benchmark suites only when their results are reproducible
  and their limitations are clearly reported.
- [x] Add a native desktop GUI for every currently implemented CLI workflow, with command logs, JSON benchmark import/comparison charts, and privileged actions gated by PolicyKit.
- [x] Connect the GUI Installation page to Arch-family readiness, package creation and explicit installation actions.
- [ ] Keep bootloader entry creation and automated rollback disabled until tested distribution-specific adapters exist.

## Explicit non-goals for the current milestone

- No automatic replacement of the running kernel.
- No automatic bootloader configuration changes.
- No automatic Secure Boot key enrollment.
- No execution of arbitrary third-party patch scripts as root.
