use anyhow::{bail, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureVerification {
    pub signer_name: String,
    pub signer_primary_fingerprint: String,
    pub signature_creation_date: String,
    pub signature_timestamp: String,
    pub public_key_algorithm: String,
    pub hash_algorithm: String,
}

#[derive(Debug, Clone, Copy)]
struct TrustedSigner {
    name: &'static str,
    fingerprint: &'static str,
}

// These fingerprints are copied from kernel.org's published "Important fingerprints"
// list. The list is deliberately compiled into the application: a key downloaded from
// the same endpoint as the tarball is never trusted merely because GnuPG imported it.
const TRUSTED_SIGNERS: &[TrustedSigner] = &[
    TrustedSigner {
        name: "Linus Torvalds",
        fingerprint: "ABAF11C65A2970B130ABE3C479BE3E4300411886",
    },
    TrustedSigner {
        name: "Greg Kroah-Hartman",
        fingerprint: "647F28654894E3BD457199BE38DBBDC86092693E",
    },
    TrustedSigner {
        name: "Sasha Levin",
        fingerprint: "E27E5D8A3403A2EF66873BBCDEA66FF797772CDC",
    },
    TrustedSigner {
        name: "Ben Hutchings",
        fingerprint: "AC2B29BD34A6AFDDB3F68F35E7BFC8EC95861109",
    },
];

fn normalized_fingerprint(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .map(|character| character.to_ascii_uppercase())
        .collect()
}

fn signer_from_status_with_signers(status: &str, trusted_signers: &[TrustedSigner<'_>]) -> Result<SignatureVerification> {
    const FATAL_STATUS: &[&str] = &[
        "BADSIG",
        "EXPSIG",
        "ERRSIG",
        "NO_PUBKEY",
        "REVKEYSIG",
        "EXPKEYSIG",
        "KEYREVOKED",
        "SIGEXPIRED",
        "KEYEXPIRED",
        "NODATA",
    ];

    for line in status.lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() >= 2
            && fields[0] == "[GNUPG:]"
            && FATAL_STATUS.contains(&fields[1])
        {
            bail!("GnuPG reported {} while verifying the kernel source", fields[1]);
        }
    }

    let mut verified = Vec::new();
    for line in status.lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 3 || fields[0] != "[GNUPG:]" || fields[1] != "VALIDSIG" {
            continue;
        }

        // VALIDSIG contains the signing-key fingerprint and, for subkey signatures,
        // the primary-key fingerprint as its final field.
        let signing_fingerprint = normalized_fingerprint(fields[2]);
        if signing_fingerprint.len() != 40 && signing_fingerprint.len() != 64 {
            bail!("GnuPG VALIDSIG status contained an invalid signing-key fingerprint");
        }
        let primary_fingerprint = match fields.get(11) {
            Some(value) => {
                let fingerprint = normalized_fingerprint(value);
                if fingerprint.len() != 40 && fingerprint.len() != 64 {
                    bail!("GnuPG VALIDSIG status contained an invalid primary-key fingerprint");
                }
                fingerprint
            }
            None => signing_fingerprint,
        };

        if let Some(signer) = trusted_signers
            .iter()
            .find(|signer| signer.fingerprint == primary_fingerprint)
        {
            let signature_creation_date = fields
                .get(3)
                .context("GnuPG VALIDSIG status omitted the signature creation date")?
                .to_string();
            let signature_timestamp = fields
                .get(4)
                .context("GnuPG VALIDSIG status omitted the signature timestamp")?
                .to_string();
            let public_key_algorithm = fields
                .get(8)
                .context("GnuPG VALIDSIG status omitted the public-key algorithm")?
                .to_string();
            let hash_algorithm = fields
                .get(9)
                .context("GnuPG VALIDSIG status omitted the hash algorithm")?
                .to_string();
            verified.push(SignatureVerification {
                signer_name: signer.name.to_owned(),
                signer_primary_fingerprint: signer.fingerprint.to_owned(),
                signature_creation_date,
                signature_timestamp,
                public_key_algorithm,
                hash_algorithm,
            });
        }
    }

    if verified.len() != 1 {
        bail!(
            "expected exactly one valid signature from a pinned kernel.org release signer; found {}. Import and verify one of the documented signer keys, then retry",
            verified.len()
        );
    }
    Ok(verified.remove(0))
}

fn signer_from_status(status: &str) -> Result<SignatureVerification> {
    signer_from_status_with_signers(status, TRUSTED_SIGNERS)
}

fn verify_kernel_archive_with_signers(
    archive: &Path,
    signature: &Path,
    gpg_home: Option<&Path>,
    trusted_signers: &[TrustedSigner<'_>],
) -> Result<SignatureVerification> {
    if !archive.is_file() {
        bail!("kernel source archive does not exist: {}", archive.display());
    }
    if !signature.is_file() {
        bail!("kernel source signature does not exist: {}", signature.display());
    }

    // kernel.org signs the uncompressed tar stream, not the .tar.xz byte stream.
    let mut xz = Command::new("xz")
        .args(["--decompress", "--stdout", "--"])
        .arg(archive)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting xz to stream the uncompressed kernel tarball")?;

    let mut gpg_command = Command::new("gpg");
    gpg_command
        .args(["--no-options", "--batch", "--no-auto-key-import", "--no-auto-key-retrieve", "--status-fd=1"]);
    if let Some(home) = gpg_home {
        if !home.is_dir() {
            bail!("configured GPG home is not a directory: {}", home.display());
        }
        gpg_command.arg("--homedir").arg(home);
    }
    let mut gpg = gpg_command
        .arg("--verify")
        .arg(signature)
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting GnuPG; install gnupg and import a documented kernel.org signer key")?;

    let mut xz_stdout = xz.stdout.take().context("opening xz output stream")?;
    let mut gpg_stdin = gpg.stdin.take().context("opening GnuPG input stream")?;
    let pipe_result = std::io::copy(&mut xz_stdout, &mut gpg_stdin);
    drop(gpg_stdin);

    let xz_output = xz.wait_with_output().context("waiting for xz")?;
    let gpg_output = gpg.wait_with_output().context("waiting for GnuPG")?;

    if !xz_output.status.success() {
        bail!(
            "xz could not fully decompress the kernel archive: {}",
            String::from_utf8_lossy(&xz_output.stderr).trim()
        );
    }
    if !gpg_output.status.success() {
        let details = String::from_utf8_lossy(&gpg_output.stderr);
        bail!(
            "kernel source signature verification failed: {}",
            details.trim()
        );
    }
    pipe_result.context("streaming the decompressed tarball into GnuPG")?;

    let status = String::from_utf8_lossy(&gpg_output.stdout);
    signer_from_status_with_signers(&status, trusted_signers).context("kernel source was not signed by an allowed, valid release key")
}

pub fn verify_kernel_archive(
    archive: &Path,
    signature: &Path,
    gpg_home: Option<&Path>,
) -> Result<SignatureVerification> {
    verify_kernel_archive_with_signers(archive, signature, gpg_home, TRUSTED_SIGNERS)
}

pub fn trusted_key_available(gpg_home: Option<&Path>) -> Result<Option<String>> {
    let mut command = Command::new("gpg");
    command.args(["--no-options", "--batch"]);
    if let Some(home) = gpg_home {
        if !home.is_dir() {
            bail!("configured GPG home is not a directory: {}", home.display());
        }
        command.arg("--homedir").arg(home);
    }
    command.args(["--with-colons", "--fingerprint", "--list-keys"]);
    let output = command.output().context("listing keys in the configured GnuPG keyring")?;
    if !output.status.success() {
        bail!(
            "GnuPG could not list the configured keyring: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let fingerprints = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let fields = line.split(':').collect::<Vec<_>>();
            (fields.first() == Some(&"fpr")).then(|| fields.get(9).copied()).flatten()
        })
        .map(normalized_fingerprint)
        .collect::<Vec<_>>();
    Ok(TRUSTED_SIGNERS
        .iter()
        .find(|signer| fingerprints.iter().any(|fingerprint| fingerprint == signer.fingerprint))
        .map(|signer| signer.name.to_owned()))
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

// Hash a directory using a stable path/type/content encoding. Symlinks are hashed as
// link targets and are never followed, preventing loops or hashing outside the tree.
pub fn sha256_tree(root: &Path) -> Result<(String, u64, u64)> {
    if !root.is_dir() {
        bail!("tree hash target is not a directory: {}", root.display());
    }

    let mut entries = Vec::<PathBuf>::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let mut children = fs::read_dir(&directory)
            .with_context(|| format!("reading directory {}", directory.display()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        children.sort();
        for child in children {
            let file_type = fs::symlink_metadata(&child)?.file_type();
            if file_type.is_dir() {
                pending.push(child.clone());
            }
            entries.push(child);
        }
    }
    entries.sort_by(|left, right| {
        left.strip_prefix(root)
            .unwrap_or(left)
            .cmp(right.strip_prefix(root).unwrap_or(right))
    });

    let mut hasher = Sha256::new();
    hasher.update(b"kernel-optimizer-tree-sha256-v1\0");
    let mut file_count = 0_u64;
    let mut total_bytes = 0_u64;
    let mut buffer = [0_u8; 1024 * 1024];

    for path in entries {
        let relative = path.strip_prefix(root).context("normalizing tree entry path")?;
        let relative_bytes = relative.to_string_lossy();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            hasher.update(b"symlink\0");
            hasher.update(relative_bytes.as_bytes());
            hasher.update(b"\0");
            hasher.update(fs::read_link(&path)?.to_string_lossy().as_bytes());
            hasher.update(b"\0");
        } else if metadata.is_dir() {
            hasher.update(b"directory\0");
            hasher.update(relative_bytes.as_bytes());
            hasher.update(b"\0");
        } else if metadata.is_file() {
            hasher.update(b"file\0");
            hasher.update(relative_bytes.as_bytes());
            hasher.update(b"\0");
            hasher.update(metadata.len().to_le_bytes());
            hasher.update(b"\0");
            let mut file = File::open(&path)
                .with_context(|| format!("opening tree file {}", path.display()))?;
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                hasher.update(&buffer[..count]);
            }
            hasher.update(b"\0");
            file_count += 1;
            total_bytes = total_bytes.saturating_add(metadata.len());
        }
    }

    Ok((format!("{:x}", hasher.finalize()), file_count, total_bytes))
}

pub fn command_version(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = if output.stdout.is_empty() {
        String::from_utf8_lossy(&output.stderr)
    } else {
        String::from_utf8_lossy(&output.stdout)
    };
    text.lines().next().map(str::trim).filter(|line| !line.is_empty()).map(str::to_owned)
}

pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        stamp
    ));
    let result = (|| -> Result<()> {
        let bytes = serde_json::to_vec_pretty(value)?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("creating temporary manifest {}", temporary.display()))?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
            .with_context(|| format!("atomically writing {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_pinned_primary_fingerprint_from_validsig() {
        let status = "[GNUPG:] GOODSIG 6092693E Greg Kroah-Hartman\n[GNUPG:] VALIDSIG 0123456789012345678901234567890123456789 2026-11-14 1794620000 0 4 0 1 10 00 647F28654894E3BD457199BE38DBBDC86092693E\n";
        let signer = signer_from_status(status).unwrap();
        assert_eq!(signer.signer_name, "Greg Kroah-Hartman");
        assert_eq!(
            signer.signer_primary_fingerprint,
            "647F28654894E3BD457199BE38DBBDC86092693E"
        );
        assert_eq!(signer.signature_creation_date, "2026-11-14");
        assert_eq!(signer.signature_timestamp, "1794620000");
        assert_eq!(signer.public_key_algorithm, "1");
        assert_eq!(signer.hash_algorithm, "10");
    }

    #[test]
    fn rejects_unpinned_or_bad_signatures() {
        let unknown = "[GNUPG:] VALIDSIG 1111111111111111111111111111111111111111 2026-11-14 1794620000 0 4 0 1 10 00\n";
        assert!(signer_from_status(unknown).is_err());

        let revoked = "[GNUPG:] REVKEYSIG 6092693E Greg Kroah-Hartman\n[GNUPG:] VALIDSIG 647F28654894E3BD457199BE38DBBDC86092693E 2026-11-14 1794620000 0 4 0 1 10 00\n";
        assert!(signer_from_status(revoked).is_err());
    }


    #[cfg(unix)]
    #[test]
    fn verifies_decompressed_stream_and_rejects_tampered_archive() {
        use std::{fs::File, os::unix::fs::PermissionsExt};

        if Command::new("gpg").arg("--version").output().is_err()
            || Command::new("xz").arg("--version").output().is_err()
        {
            return;
        }

        let root = std::env::temp_dir().join(format!(
            "kernel-optimizer-gpg-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = root.join("gnupg");
        fs::create_dir_all(&home).unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();

        let generate = Command::new("gpg")
            .args(["--no-options", "--homedir"])
            .arg(&home)
            .args([
                "--batch",
                "--passphrase",
                "",
                "--quick-generate-key",
                "Kernel Optimizer Test Signer <kernel-optimizer-test@example.invalid>",
                "ed25519",
                "sign",
                "0",
            ])
            .output()
            .unwrap();
        assert!(
            generate.status.success(),
            "test key generation failed: {}",
            String::from_utf8_lossy(&generate.stderr)
        );

        let listing = Command::new("gpg")
            .args(["--no-options", "--homedir"])
            .arg(&home)
            .args(["--batch", "--with-colons", "--fingerprint", "--list-keys"])
            .output()
            .unwrap();
        assert!(listing.status.success());
        let fingerprint = String::from_utf8_lossy(&listing.stdout)
            .lines()
            .find_map(|line| {
                let fields = line.split(':').collect::<Vec<_>>();
                (fields.first() == Some(&"fpr"))
                    .then(|| fields.get(9).copied())
                    .flatten()
            })
            .unwrap()
            .to_owned();

        let trusted = [TrustedSigner {
            name: "Kernel Optimizer test signer",
            fingerprint: &fingerprint,
        }];
        let raw = root.join("source.tar");
        let archive = root.join("source.tar.xz");
        let signature = root.join("source.tar.sign");
        fs::create_dir_all(&root).unwrap();
        fs::write(&raw, b"test signed uncompressed tar stream").unwrap();

        let compress = |input: &Path, output: &Path| {
            let file = File::create(output).unwrap();
            let status = Command::new("xz")
                .arg("--compress")
                .arg("--stdout")
                .arg(input)
                .stdout(Stdio::from(file))
                .status()
                .unwrap();
            assert!(status.success(), "xz compression failed");
        };
        compress(&raw, &archive);

        let sign = Command::new("gpg")
            .args(["--no-options", "--homedir"])
            .arg(&home)
            .args(["--batch", "--yes", "--local-user", &fingerprint, "--detach-sign", "--output"])
            .arg(&signature)
            .arg(&raw)
            .output()
            .unwrap();
        assert!(
            sign.status.success(),
            "test signing failed: {}",
            String::from_utf8_lossy(&sign.stderr)
        );

        let valid = verify_kernel_archive_with_signers(&archive, &signature, Some(&home), &trusted)
            .unwrap();
        assert_eq!(valid.signer_primary_fingerprint, fingerprint);

        fs::write(&raw, b"tampered signed uncompressed tar stream").unwrap();
        compress(&raw, &archive);
        assert!(
            verify_kernel_archive_with_signers(&archive, &signature, Some(&home), &trusted).is_err(),
            "altered archive unexpectedly passed detached-signature verification"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tree_hash_is_stable_and_detects_content_changes() {
        let root = std::env::temp_dir().join(format!(
            "kernel-optimizer-tree-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("subdir")).unwrap();
        fs::write(root.join("b"), b"second").unwrap();
        fs::write(root.join("subdir/a"), b"first").unwrap();

        let first = sha256_tree(&root).unwrap();
        let second = sha256_tree(&root).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.1, 2);
        assert_eq!(first.2, 11);

        fs::write(root.join("b"), b"changed").unwrap();
        let changed = sha256_tree(&root).unwrap();
        assert_ne!(first.0, changed.0);
        fs::remove_dir_all(root).unwrap();
    }
}
