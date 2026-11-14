use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::{fs, path::{Path, PathBuf}, process::{Command, Stdio}, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};

#[derive(Debug, Serialize)]
pub struct BenchmarkResult {
    pub name: String, pub command: Vec<String>, pub iterations: usize,
    pub samples: Vec<f64>, pub median_seconds: f64, pub min_seconds: f64,
    pub max_seconds: f64, pub status: String,
}
#[derive(Debug, Serialize)]
pub struct BenchmarkReport {
    pub created_unix: u64, pub kernel: String, pub cpu: String,
    pub profile: String, pub results: Vec<BenchmarkResult>,
}
fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a,b|a.total_cmp(b));
    if values.is_empty() { return 0.0; }
    let mid=values.len()/2;
    if values.len()%2==0 {(values[mid-1]+values[mid])/2.0} else {values[mid]}
}
fn run_one(name:&str, command:&[String], iterations:usize)->Result<BenchmarkResult> {
    if command.is_empty() { bail!("benchmark command cannot be empty"); }
    let mut samples=Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start=Instant::now();
        let status=Command::new(&command[0]).args(&command[1..]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).status()
            .with_context(|| format!("starting benchmark {}",command[0]))?;
        let elapsed=start.elapsed().as_secs_f64();
        if !status.success() { bail!("benchmark '{}' exited with {status}", name); }
        samples.push(elapsed);
    }
    let mut sorted=samples.clone();
    let med=median(&mut sorted);
    let min=samples.iter().copied().fold(f64::INFINITY,f64::min);
    let max=samples.iter().copied().fold(0.0,f64::max);
    Ok(BenchmarkResult {name:name.into(),command:command.to_vec(),iterations,samples,median_seconds:med,min_seconds:min,max_seconds:max,status:"ok".into()})
}
pub fn run(report_path:&Path, iterations:usize, profile:&str)->Result<BenchmarkReport> {
    if !(1..=20).contains(&iterations) { bail!("iterations must be between 1 and 20"); }
    let mut results=Vec::new();
    // These are lightweight, repeatable baseline probes rather than synthetic claims of system performance.
    results.push(run_one("procfs-metadata", &["sh".into(),"-c".into(),"cat /proc/cpuinfo /proc/meminfo >/dev/null".into()], iterations)?);
    results.push(run_one("filesystem-metadata", &["sh".into(),"-c".into(),"find /sys/block -maxdepth 2 -type f -name scheduler -print >/dev/null".into()], iterations)?);
    let report=BenchmarkReport {
        created_unix:SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
        kernel:Command::new("uname").arg("-r").output().map(|o|String::from_utf8_lossy(&o.stdout).trim().into()).unwrap_or_default(),
        cpu:Command::new("sh").args(["-c","awk -F: '/model name/ {gsub(/^ +/, \"\", $2); print $2; exit}' /proc/cpuinfo"]).output().map(|o|String::from_utf8_lossy(&o.stdout).trim().into()).unwrap_or_default(),
        profile:profile.into(),results,
    };
    if let Some(parent)=report_path.parent() { if !parent.as_os_str().is_empty(){fs::create_dir_all(parent)?;} }
    let tmp=PathBuf::from(format!("{}.tmp",report_path.display()));
    fs::write(&tmp,serde_json::to_vec_pretty(&report)?)?;
    fs::rename(tmp,report_path)?;
    Ok(report)
}
