use anyhow::{bail, Context, Result};
use regex::Regex;
use serde_json::Value;
use std::{fs, path::Path, process::Command, time::{Duration, SystemTime, UNIX_EPOCH}};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat { KernelXz, GzipTar }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchCompression { Zstd, Xz, Plain }

#[derive(Debug, Clone)]
pub struct PatchPlan {
    pub name: String,
    pub url: String,
    pub signature_url: Option<String>,
    pub expected_sha256: Option<String>,
    pub signer_name: Option<&'static str>,
    pub signer_fingerprint: Option<&'static str>,
    pub compression: PatchCompression,
    pub signature_covers_compressed_file: bool,
}

#[derive(Debug, Clone)]
pub struct SourcePlan {
    pub project: String,
    pub version: String,
    pub archive_url: String,
    pub archive_name: String,
    pub archive_format: ArchiveFormat,
    pub signature_url: Option<String>,
    pub signature_name: Option<String>,
    pub expected_archive_sha256: Option<String>,
    pub trust_reference: String,
    pub patches: Vec<PatchPlan>,
}

#[derive(Debug, serde::Serialize)]
pub struct PatchProvenance {
    pub name: String,
    pub url: String,
    pub sha256: String,
    pub signature_url: String,
    pub signature_sha256: String,
    pub signature_verified: bool,
    pub signer_name: String,
    pub signer_primary_fingerprint: String,
    pub source_revision: String,
}

fn client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder().timeout(Duration::from_secs(90))
        .user_agent("kernel-optimizer/0.3").build()?)
}

fn get_json(url: &str) -> Result<Value> {
    let response = client()?.get(url).header("Accept", "application/vnd.github+json")
        .send().with_context(|| format!("requesting {url}"))?
        .error_for_status().with_context(|| format!("HTTP error from {url}"))?;
    serde_json::from_str(&response.text()?).with_context(|| format!("parsing JSON from {url}"))
}

fn latest_stable() -> Result<String> {
    let value = get_json("https://www.kernel.org/releases.json")?;
    value["releases"].as_array().context("kernel.org releases response omitted releases")?
        .iter().find(|r| r["moniker"].as_str() == Some("stable"))
        .and_then(|r| r["version"].as_str()).map(str::to_owned)
        .context("kernel.org did not advertise a stable release")
}

fn kernel_archive(version: &str) -> (String, String) {
    let major = version.split('.').next().unwrap_or(version);
    let base = format!("https://cdn.kernel.org/pub/linux/kernel/v{major}.x");
    (format!("{base}/linux-{version}.tar.xz"), format!("{base}/linux-{version}.tar.sign"))
}

fn github_asset(release: &Value, suffix: &str) -> Result<(String, Option<String>, String)> {
    let assets = release["assets"].as_array().context("GitHub release omitted assets")?;
    let asset = assets.iter().find(|a| a["name"].as_str().is_some_and(|n| n.ends_with(suffix)))
        .with_context(|| format!("release does not contain an asset ending in {suffix}"))?;
    let url = asset["browser_download_url"].as_str().context("release asset omitted download URL")?.to_owned();
    let name = asset["name"].as_str().context("release asset omitted name")?.to_owned();
    let digest = asset["digest"].as_str().and_then(|v| v.strip_prefix("sha256:")).map(str::to_owned);
    Ok((url, digest, name))
}

fn validate_requested_version(requested: &str, resolved: &str, source: &str) -> Result<()> {
    if requested != "stable" && requested != resolved {
        bail!("{source} release is based on Linux {resolved}, but profile requests {requested}; choose version = \"{resolved}\" or version = \"stable\"");
    }
    Ok(())
}

fn resolve_zen(requested: &str) -> Result<SourcePlan> {
    const SIGNER: &str = "Jan Alexander Steffens (heftig)";
    const FINGERPRINT: &str = "83BC8889351B5DEBBB68416EB8AC08600F108CDF";
    let release = get_json("https://api.github.com/repos/zen-kernel/zen-kernel/releases/latest")?;
    let base_re = Regex::new(r"stable version[^0-9]*v?([0-9]+\.[0-9]+\.[0-9]+)")?;
    let version = base_re.captures(release["body"].as_str().unwrap_or_default())
        .and_then(|c| c.get(1)).map(|c| c.as_str().to_owned())
        .context("Zen release notes do not declare the upstream stable base version")?;
    validate_requested_version(requested, &version, "Linux Zen")?;
    let tag = release["tag_name"].as_str().context("Zen release omitted its tag")?;
    let (patch_url, digest, patch_name) = github_asset(&release, ".patch.zst")?;
    let (signature_url, _, _) = github_asset(&release, ".patch.zst.sig")?;
    let (archive_url, signature) = kernel_archive(&version);
    Ok(SourcePlan {
        project: "Linux Zen".into(), version: version.clone(), archive_url,
        archive_name: format!("linux-{version}.tar.xz"), archive_format: ArchiveFormat::KernelXz,
        signature_url: Some(signature), signature_name: Some(format!("linux-{version}.tar.sign")),
        expected_archive_sha256: None,
        trust_reference: format!("https://github.com/zen-kernel/zen-kernel/releases/tag/{tag}"),
        patches: vec![PatchPlan { name: patch_name, url: patch_url, signature_url: Some(signature_url),
            expected_sha256: digest, signer_name: Some(SIGNER), signer_fingerprint: Some(FINGERPRINT),
            compression: PatchCompression::Zstd, signature_covers_compressed_file: true }],
    })
}

fn resolve_rt(requested: &str) -> Result<SourcePlan> {
    const SIGNER: &str = "Sebastian Andrzej Siewior";
    const FINGERPRINT: &str = "64254695FFF0AA4466CC19E67B96E8162A8CF5D1";
    let latest = latest_stable()?;
    let series = latest.split('.').take(2).collect::<Vec<_>>().join(".");
    let version = if requested == "stable" { series.clone() } else { requested.to_owned() };
    if version != series {
        bail!("PREEMPT_RT patches target exact base release {series}; profile version must be \"{series}\", not a later point release");
    }
    let index_url = format!("https://www.kernel.org/pub/linux/kernel/projects/rt/{series}/");
    let html = client()?.get(&index_url).send()?.error_for_status()?.text()?;
    let pattern = Regex::new(&format!(r#"href="patch-{}-rt([0-9]+)\.patch\.xz""#, regex::escape(&series)))?;
    let revision = pattern.captures_iter(&html).filter_map(|c| c.get(1)?.as_str().parse::<u32>().ok()).max()
        .context("kernel.org RT index has no patch for this kernel series")?;
    let stem = format!("patch-{series}-rt{revision}.patch");
    let (archive_url, signature) = kernel_archive(&version);
    Ok(SourcePlan {
        project: "Linux PREEMPT_RT".into(), version: version.clone(), archive_url,
        archive_name: format!("linux-{version}.tar.xz"), archive_format: ArchiveFormat::KernelXz,
        signature_url: Some(signature), signature_name: Some(format!("linux-{version}.tar.sign")),
        expected_archive_sha256: None, trust_reference: index_url.clone(),
        patches: vec![PatchPlan { name: format!("{stem}.xz"), url: format!("{index_url}{stem}.xz"),
            signature_url: Some(format!("{index_url}{stem}.sign")), expected_sha256: None,
            signer_name: Some(SIGNER), signer_fingerprint: Some(FINGERPRINT),
            compression: PatchCompression::Xz, signature_covers_compressed_file: false }],
    })
}

fn resolve_cachyos(requested: &str, allow_unverified: bool) -> Result<SourcePlan> {
    if !allow_unverified {
        bail!("CachyOS archives do not have a currently trusted detached-signature key. Set allow_unverified_sources = true to accept the official GitHub release SHA-256 digest; these builds remain non-packageable.");
    }
    let release = get_json("https://api.github.com/repos/CachyOS/linux/releases/latest")?;
    let tag = release["tag_name"].as_str().context("CachyOS release omitted tag_name")?;
    let re = Regex::new(r"^cachyos-([0-9]+\.[0-9]+\.[0-9]+)-[0-9]+$")?;
    let version = re.captures(tag).and_then(|c| c.get(1)).map(|c| c.as_str().to_owned())
        .context("unrecognized CachyOS release tag")?;
    validate_requested_version(requested, &version, "CachyOS")?;
    let (archive_url, digest, archive_name) = github_asset(&release, ".tar.gz")?;
    let digest = digest.context("CachyOS release asset has no GitHub SHA-256 digest; refusing download")?;
    Ok(SourcePlan { project: "CachyOS Linux".into(), version, archive_url, archive_name,
        archive_format: ArchiveFormat::GzipTar, signature_url: None, signature_name: None,
        expected_archive_sha256: Some(digest), trust_reference: format!("https://github.com/CachyOS/linux/releases/tag/{tag}"),
        patches: Vec::new() })
}

fn resolve_tkg(requested: &str, allow_unverified: bool) -> Result<SourcePlan> {
    if !allow_unverified {
        bail!("Linux TKG patchsets are unsigned. Set allow_unverified_sources = true to accept the pinned GitHub patch revision; these builds remain non-packageable.");
    }
    let latest = latest_stable()?;
    let version = if requested == "stable" { latest } else { requested.to_owned() };
    let tag = format!("v{version}");
    let reference = get_json(&format!("https://api.github.com/repos/Frogging-Family/linux-tkg/git/ref/tags/{tag}"))
        .with_context(|| format!("resolving TKG release tag {tag}"))?;
    let mut commit = reference["object"]["sha"].as_str().context("TKG tag omitted object SHA")?.to_owned();
    if reference["object"]["type"].as_str() == Some("tag") {
        let annotated = get_json(&format!("https://api.github.com/repos/Frogging-Family/linux-tkg/git/tags/{commit}"))?;
        commit = annotated["object"]["sha"].as_str().context("annotated TKG tag omitted target commit")?.to_owned();
    }
    let series = version.split('.').take(2).collect::<Vec<_>>().join(".");
    let directory = get_json(&format!("https://api.github.com/repos/Frogging-Family/linux-tkg/contents/linux-tkg-patches/{series}?ref={commit}"))?;
    let names = directory.as_array().context("TKG release lacks patches for this kernel series")?
        .iter().filter_map(|v| v["name"].as_str()).collect::<Vec<_>>();
    let selected = ["0003-glitched-base.patch", "0003-glitched-eevdf-additions.patch"];
    for name in selected {
        if !names.contains(&name) { bail!("TKG release {tag} is missing curated patch {name}; refusing to substitute another patchset"); }
    }
    let patches = selected.iter().map(|name| PatchPlan {
        name: (*name).to_owned(),
        url: format!("https://raw.githubusercontent.com/Frogging-Family/linux-tkg/{commit}/linux-tkg-patches/{series}/{name}"),
        signature_url: None, expected_sha256: None, signer_name: None, signer_fingerprint: None,
        compression: PatchCompression::Plain, signature_covers_compressed_file: false,
    }).collect();
    let (archive_url, signature) = kernel_archive(&version);
    Ok(SourcePlan { project: "Linux TKG (curated EEVDF patch profile)".into(), version: version.clone(),
        archive_url, archive_name: format!("linux-{version}.tar.xz"), archive_format: ArchiveFormat::KernelXz,
        signature_url: Some(signature), signature_name: Some(format!("linux-{version}.tar.sign")),
        expected_archive_sha256: None,
        trust_reference: format!("https://github.com/Frogging-Family/linux-tkg/tree/{commit}/linux-tkg-patches/{series}"),
        patches })
}

pub fn resolve(source: &str, requested_version: &str, allow_unverified: bool) -> Result<Option<SourcePlan>> {
    let plan = match source {
        "linux" | "linux-lts" => return Ok(None),
        "linux-zen" => resolve_zen(requested_version)?,
        "linux-cachyos" => resolve_cachyos(requested_version, allow_unverified)?,
        "linux-tkg" => resolve_tkg(requested_version, allow_unverified)?,
        "linux-rt" => resolve_rt(requested_version)?,
        other => bail!("unknown kernel source adapter: {other}"),
    };
    Ok(Some(plan))
}

fn download(url: &str, destination: &Path) -> Result<()> {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let temporary = destination.with_extension(format!("download-{}-{stamp}", std::process::id()));
    let result = (|| -> Result<()> {
        let status = Command::new("curl").args(["--fail", "--location", "--retry", "3", "--output"])
            .arg(&temporary).arg(url).status().with_context(|| format!("starting download of {url}"))?;
        if !status.success() { bail!("download failed ({status}) from {url}"); }
        if fs::metadata(&temporary)?.len() == 0 { bail!("download from {url} was empty"); }
        fs::rename(&temporary, destination)?;
        Ok(())
    })();
    if result.is_err() { let _ = fs::remove_file(&temporary); }
    result
}

fn decompress_patch(input: &Path, output: &Path, compression: PatchCompression) -> Result<()> {
    if compression == PatchCompression::Plain { fs::copy(input, output)?; return Ok(()); }
    let (program, args): (&str, &[&str]) = match compression {
        PatchCompression::Zstd => ("zstd", &["--decompress", "--stdout", "--"]),
        PatchCompression::Xz => ("xz", &["--decompress", "--stdout", "--"]),
        PatchCompression::Plain => unreachable!(),
    };
    let result = Command::new(program).args(args).arg(input).output()
        .with_context(|| format!("decompressing patch with {program}"))?;
    if !result.status.success() { bail!("{program} could not decompress {}: {}", input.display(), String::from_utf8_lossy(&result.stderr).trim()); }
    fs::write(output, result.stdout)?;
    if fs::metadata(output)?.len() == 0 { bail!("decompressed patch is empty: {}", output.display()); }
    Ok(())
}

pub fn download_and_apply_patches(plan: &SourcePlan, source_dir: &Path, build_dir: &Path, gpg_home: Option<&Path>) -> Result<Vec<PatchProvenance>> {
    let mut manifests = Vec::new();
    for (index, patch) in plan.patches.iter().enumerate() {
        let filename = format!("source-{}-{:02}-{}", plan.version, index + 1, patch.name);
        let downloaded = build_dir.join(&filename);
        download(&patch.url, &downloaded)?;
        let digest = crate::provenance::sha256_file(&downloaded)?;
        if patch.expected_sha256.as_ref().is_some_and(|expected| expected != &digest) {
            bail!("SHA-256 mismatch for patch {}; refusing to apply it", patch.name);
        }
        let signature = if let Some(url) = patch.signature_url.as_deref() {
            let path = build_dir.join(format!("{filename}.sig"));
            download(url, &path)?;
            Some(path)
        } else { None };
        let expanded = build_dir.join(format!("{filename}.patch"));
        decompress_patch(&downloaded, &expanded, patch.compression)?;
        let mut signature_sha = String::new();
        let mut signer_name = String::from("not independently verified");
        let mut signer_fingerprint = String::new();
        let verified = if let (Some(sig), Some(name), Some(fingerprint)) = (signature.as_deref(), patch.signer_name, patch.signer_fingerprint) {
            signature_sha = crate::provenance::sha256_file(sig)?;
            let signed_path = if patch.signature_covers_compressed_file { &downloaded } else { &expanded };
            let verification = crate::provenance::verify_detached_file(signed_path, sig, gpg_home, name, fingerprint)
                .with_context(|| format!("verifying signature for patch {}", patch.name))?;
            signer_name = verification.signer_name;
            signer_fingerprint = verification.signer_primary_fingerprint;
            true
        } else { false };
        let status = Command::new("patch").args(["--batch", "--forward", "--fuzz=0", "-p1", "-i"])
            .arg(&expanded).current_dir(source_dir).status()
            .with_context(|| format!("applying patch {}", patch.name))?;
        if !status.success() { bail!("patch {} does not apply cleanly to Linux {}", patch.name, plan.version); }
        manifests.push(PatchProvenance {
            name: patch.name.clone(), url: patch.url.clone(), sha256: digest,
            signature_url: patch.signature_url.clone().unwrap_or_default(), signature_sha256: signature_sha,
            signature_verified: verified, signer_name, signer_primary_fingerprint: signer_fingerprint,
            source_revision: plan.trust_reference.clone(),
        });
    }
    Ok(manifests)
}

pub fn verify_archive_digest(path: &Path, expected: Option<&str>) -> Result<String> {
    let actual = crate::provenance::sha256_file(path)?;
    if expected.is_some_and(|value| value != actual) {
        bail!("source archive SHA-256 does not match the digest advertised by its upstream release");
    }
    Ok(actual)
}
