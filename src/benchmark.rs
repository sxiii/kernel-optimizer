use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    fs::OpenOptions,
    hint::black_box,
    path::{Path, PathBuf},
    process::Command,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkResult {
    pub name: String,
    pub workload: String,
    pub unit: String,
    pub higher_is_better: bool,
    pub iterations: usize,
    pub samples: Vec<f64>,
    pub median: f64,
    pub min: f64,
    pub max: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkReport {
    pub schema_version: u32,
    pub created_unix: u64,
    pub kernel: String,
    pub architecture: String,
    pub cpu: String,
    pub cpu_microarchitecture: String,
    pub logical_cpus: usize,
    pub memory_kib: u64,
    pub cpu_governors: Vec<String>,
    pub profile: String,
    pub results: Vec<BenchmarkResult>,
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    if values.is_empty() {
        return 0.0;
    }
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    }
}

fn summarize(
    name: &str,
    workload: &str,
    unit: &str,
    higher_is_better: bool,
    samples: Vec<f64>,
) -> BenchmarkResult {
    let mut sorted = samples.clone();
    let median = median(&mut sorted);
    let min = samples.iter().copied().fold(f64::INFINITY, f64::min);
    let max = samples.iter().copied().fold(0.0, f64::max);
    BenchmarkResult {
        name: name.into(),
        workload: workload.into(),
        unit: unit.into(),
        higher_is_better,
        iterations: samples.len(),
        samples,
        median,
        min,
        max,
    }
}

fn cpu_sample(rounds: u64) -> f64 {
    let start = Instant::now();
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    for index in 0..rounds {
        state = state
            .wrapping_mul(0xbf58_476d_1ce4_e5b9)
            .wrapping_add(index ^ 0x94d0_49bb_1331_11eb);
        state ^= state >> 29;
        state = state.rotate_left(17);
    }
    black_box(state);
    let elapsed = start.elapsed().as_secs_f64().max(f64::MIN_POSITIVE);
    rounds as f64 / elapsed
}

fn memory_sample(memory_mib: usize) -> Result<f64> {
    let bytes = memory_mib
        .checked_mul(1024 * 1024)
        .context("memory benchmark size overflow")?;
    let words = bytes / std::mem::size_of::<u64>();
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(words)
        .context("unable to reserve memory benchmark buffer")?;
    buffer.resize(words, 0_u64);

    let start = Instant::now();
    for (index, value) in buffer.iter_mut().enumerate() {
        *value = (index as u64).wrapping_mul(0x9e37_79b9);
    }
    let checksum = buffer.iter().fold(0_u64, |acc, value| acc.wrapping_add(*value));
    black_box(checksum);
    let elapsed = start.elapsed().as_secs_f64().max(f64::MIN_POSITIVE);
    // One full write and one full read pass are included in the byte count.
    Ok((bytes as f64 * 2.0 / (1024.0 * 1024.0)) / elapsed)
}

fn run_fio(directory: &Path, size_mib: usize, runtime_seconds: usize) -> Result<f64> {
    if !directory.is_dir() {
        bail!("fio path must be an existing directory");
    }
    if !(32..=4096).contains(&size_mib) {
        bail!("fio size must be between 32 and 4096 MiB");
    }
    if !(3..=60).contains(&runtime_seconds) {
        bail!("fio runtime must be between 3 and 60 seconds");
    }

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let test_file = directory.join(format!(
        ".kernel-optimizer-fio-{}-{stamp}.tmp",
        std::process::id()
    ));
    let placeholder = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&test_file)
        .context("creating temporary fio test file")?;
    drop(placeholder);

    let result = (|| -> Result<f64> {
        let output = Command::new("fio")
            .arg("--name=kernel-optimizer")
            .arg("--filename")
            .arg(&test_file)
            .arg(format!("--size={size_mib}m"))
            .args([
                "--rw=readwrite",
                "--bs=1m",
                "--direct=1",
                "--iodepth=1",
                "--numjobs=1",
                "--group_reporting",
                "--output-format=json",
            ])
            .arg(format!("--runtime={runtime_seconds}"))
            .arg("--time_based")
            .output()
            .context("starting fio; install fio to use --fio-path")?;
        if !output.status.success() {
            bail!(
                "fio failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let json: serde_json::Value = serde_json::from_slice(&output.stdout)
            .context("parsing fio JSON output")?;
        let job = json["jobs"]
            .as_array()
            .and_then(|jobs| jobs.first())
            .context("fio JSON output did not contain a job")?;
        let read_bytes = job["read"]["bw_bytes"].as_f64().unwrap_or(0.0);
        let write_bytes = job["write"]["bw_bytes"].as_f64().unwrap_or(0.0);
        let throughput = (read_bytes + write_bytes) / (1024.0 * 1024.0);
        if throughput <= 0.0 {
            bail!("fio returned no read/write throughput");
        }
        Ok(throughput)
    })();

    let cleanup = fs::remove_file(&test_file);
    if let Err(error) = cleanup {
        eprintln!(
            "Warning: could not remove fio temporary file {}: {error}",
            test_file.display()
        );
    }
    result
}

fn run_iperf(server: &str) -> Result<f64> {
    let output = Command::new("iperf3")
        .args(["--client", server, "--json", "--time", "5", "--parallel", "1"])
        .output()
        .context("starting iperf3; install iperf3 or omit --iperf-server")?;
    if !output.status.success() {
        bail!(
            "iperf3 failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("parsing iperf3 JSON output")?;
    let bits_per_second = json
        .pointer("/end/sum_received/bits_per_second")
        .or_else(|| json.pointer("/end/sum/bits_per_second"))
        .and_then(serde_json::Value::as_f64)
        .context("iperf3 output did not contain a received throughput value")?;
    Ok(bits_per_second / 1_000_000.0)
}

pub fn run(
    report_path: &Path,
    iterations: usize,
    profile: &str,
    cpu_rounds: u64,
    memory_mib: usize,
    fio_path: Option<&Path>,
    fio_size_mib: usize,
    fio_runtime_seconds: usize,
    iperf_server: Option<&str>,
) -> Result<BenchmarkReport> {
    if !(1..=20).contains(&iterations) {
        bail!("iterations must be between 1 and 20");
    }
    if !(100_000..=100_000_000).contains(&cpu_rounds) {
        bail!("CPU rounds must be between 100,000 and 100,000,000");
    }
    if !(8..=1024).contains(&memory_mib) {
        bail!("memory-mib must be between 8 and 1024");
    }

    let cpu_samples = (0..iterations)
        .map(|_| cpu_sample(cpu_rounds))
        .collect::<Vec<_>>();
    let memory_samples = (0..iterations)
        .map(|_| memory_sample(memory_mib))
        .collect::<Result<Vec<_>>>()?;

    let mut results = vec![
        summarize("integer-throughput", &format!("{cpu_rounds} rounds"), "iterations/s", true, cpu_samples),
        summarize("memory-copy-bandwidth", &format!("{memory_mib} MiB"), "MiB/s", true, memory_samples),
    ];
    if let Some(directory) = fio_path {
        let samples = (0..iterations)
            .map(|_| run_fio(directory, fio_size_mib, fio_runtime_seconds))
            .collect::<Result<Vec<_>>>()?;
        results.push(summarize(
            "disk-throughput",
            &format!("fio {fio_size_mib} MiB, {fio_runtime_seconds}s, direct readwrite"),
            "MiB/s",
            true,
            samples,
        ));
    }
    if let Some(server) = iperf_server {
        let samples = (0..iterations)
            .map(|_| run_iperf(server))
            .collect::<Result<Vec<_>>>()?;
        results.push(summarize("network-throughput", "iperf3 5s / 1 stream", "Mbit/s", true, samples));
    }

    let hardware = crate::hardware::detect();
    let report = BenchmarkReport {
        schema_version: 2,
        created_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        kernel: hardware.kernel_release,
        architecture: hardware.architecture,
        cpu: hardware.cpu.model,
        cpu_microarchitecture: hardware.cpu.microarchitecture,
        logical_cpus: hardware.cpu.logical_cpus,
        memory_kib: hardware.memory_kib,
        cpu_governors: hardware.cpu_frequency_policies.iter()
            .map(|policy| format!("{}:{}", policy.name, policy.governor))
            .collect(),
        profile: profile.into(),
        results,
    };
    if let Some(parent) = report_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let temporary = PathBuf::from(format!(
        "{}.{}.tmp",
        report_path.display(),
        std::process::id()
    ));
    fs::write(&temporary, serde_json::to_vec_pretty(&report)?)?;
    fs::rename(&temporary, report_path)
        .with_context(|| format!("writing report to {}", report_path.display()))?;
    Ok(report)
}

pub fn compare(baseline_path: &Path, candidate_path: &Path) -> Result<String> {
    let baseline: BenchmarkReport =
        serde_json::from_slice(&fs::read(baseline_path).with_context(|| {
            format!("reading baseline report {}", baseline_path.display())
        })?)?;
    let candidate: BenchmarkReport =
        serde_json::from_slice(&fs::read(candidate_path).with_context(|| {
            format!("reading candidate report {}", candidate_path.display())
        })?)?;

    let mut output = format!(
        "Baseline kernel: {}\nCandidate kernel: {}\nCPU: {}\n\n",
        baseline.kernel, candidate.kernel, candidate.cpu
    );
    if baseline.cpu != candidate.cpu
        || baseline.cpu_microarchitecture != candidate.cpu_microarchitecture
        || baseline.architecture != candidate.architecture
        || baseline.logical_cpus != candidate.logical_cpus
        || baseline.memory_kib != candidate.memory_kib
        || baseline.cpu_governors != candidate.cpu_governors
    {
        output.push_str(
            "WARNING: hardware metadata differs; these results are not a controlled comparison.\n\n",
        );
    }

    output.push_str(&format!(
        "{:<28} {:>14} {:>14} {:>12} {:>12}\n",
        "METRIC", "BASELINE", "CANDIDATE", "CHANGE", "RESULT"
    ));
    for old in &baseline.results {
        let Some(new) = candidate.results.iter().find(|item| item.name == old.name) else {
            continue;
        };
        if old.unit != new.unit
            || old.workload != new.workload
            || old.higher_is_better != new.higher_is_better
        {
            output.push_str(&format!("{:<28} incompatible report metadata\n", old.name));
            continue;
        }
        let change = if old.median.abs() < f64::EPSILON {
            0.0
        } else {
            (new.median / old.median - 1.0) * 100.0
        };
        let improved = if old.higher_is_better {
            change > 0.0
        } else {
            change < 0.0
        };
        let result = if change.abs() < 0.5 {
            "similar"
        } else if improved {
            "improved"
        } else {
            "regressed"
        };
        output.push_str(&format!(
            "{:<28} {:>11.2} {:>11.2} {:>+10.2}% {:>12}\n",
            old.name, old.median, new.median, change, result
        ));
    }
    output.push_str(
        "\nHigher throughput is better. Repeat tests under comparable thermal, power and background-load conditions before drawing conclusions.\n",
    );
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_handles_odd_and_even_samples() {
        assert_eq!(median(&mut [1.0, 3.0, 2.0]), 2.0);
        assert_eq!(median(&mut [1.0, 3.0, 2.0, 4.0]), 2.5);
    }

    #[test]
    fn summary_tracks_direction_and_statistics() {
        let result = summarize("test", "5 rounds", "unit/s", true, vec![1.0, 2.0, 3.0]);
        assert_eq!(result.median, 2.0);
        assert_eq!(result.workload, "5 rounds");
        assert_eq!(result.min, 1.0);
        assert_eq!(result.max, 3.0);
        assert!(result.higher_is_better);
    }
}
