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

pub struct BenchmarkOptions<'a> {
    pub iterations: usize,
    pub profile: &'a str,
    pub cpu_rounds: u64,
    pub memory_mib: usize,
    pub fio_path: Option<&'a Path>,
    pub fio_size_mib: usize,
    pub fio_runtime_seconds: usize,
    pub iperf_server: Option<&'a str>,
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

pub fn run(report_path: &Path, options: BenchmarkOptions<'_>) -> Result<BenchmarkReport> {
    let BenchmarkOptions {
        iterations,
        profile,
        cpu_rounds,
        memory_mib,
        fio_path,
        fio_size_mib,
        fio_runtime_seconds,
        iperf_server,
    } = options;
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

fn validate_report(report: &BenchmarkReport, label: &str) -> Result<()> {
    if report.schema_version != 2 {
        bail!("{label} report uses unsupported schema version {}; expected 2", report.schema_version);
    }
    if report.results.is_empty() {
        bail!("{label} report contains no benchmark results");
    }
    let mut names = std::collections::HashSet::new();
    for result in &report.results {
        if result.name.trim().is_empty() || !names.insert(result.name.as_str()) {
            bail!("{label} report contains an empty or duplicate metric name");
        }
        if result.samples.is_empty() || result.iterations != result.samples.len() {
            bail!("{label} metric '{}' has inconsistent iteration/sample counts", result.name);
        }
        if result.samples.iter().any(|sample| !sample.is_finite() || *sample < 0.0)
            || !result.median.is_finite()
            || !result.min.is_finite()
            || !result.max.is_finite()
            || result.median < 0.0
            || result.min < 0.0
            || result.max < 0.0
        {
            bail!("{label} metric '{}' contains invalid numeric values", result.name);
        }

        let mut ordered_samples = result.samples.clone();
        let expected_median = median(&mut ordered_samples);
        let expected_min = result.samples.iter().copied().fold(f64::INFINITY, f64::min);
        let expected_max = result.samples.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let close_enough = |actual: f64, expected: f64| {
            (actual - expected).abs() <= 1e-9 * actual.abs().max(expected.abs()).max(1.0)
        };
        if !close_enough(result.min, expected_min)
            || !close_enough(result.median, expected_median)
            || !close_enough(result.max, expected_max)
        {
            bail!(
                "{label} metric '{}' summary values do not match its samples",
                result.name
            );
        }
    }
    Ok(())
}

fn compare_reports(baseline: &BenchmarkReport, candidate: &BenchmarkReport) -> Result<String> {
    validate_report(baseline, "baseline")?;
    validate_report(candidate, "candidate")?;

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
        "{:<28} {:>14} {:>14} {:>12} {:>20}\n",
        "METRIC", "BASELINE", "CANDIDATE", "CHANGE", "RESULT"
    ));
    let baseline_names = baseline.results.iter().map(|item| item.name.as_str()).collect::<std::collections::HashSet<_>>();
    for old in &baseline.results {
        let Some(new) = candidate.results.iter().find(|item| item.name == old.name) else {
            output.push_str(&format!(
                "{:<28} {:>14.2} {:>14} {:>12} {:>20}\n",
                old.name, old.median, "--", "--", "missing from candidate"
            ));
            continue;
        };
        if old.unit != new.unit
            || old.workload != new.workload
            || old.higher_is_better != new.higher_is_better
        {
            output.push_str(&format!("{:<28} incompatible report metadata\n", old.name));
            continue;
        }
        if old.median == 0.0 {
            if new.median == 0.0 {
                output.push_str(&format!(
                    "{:<28} {:>11.2} {:>11.2} {:>+10.2}% {:>20}\n",
                    old.name, old.median, new.median, 0.0, "similar"
                ));
            } else {
                output.push_str(&format!(
                    "{:<28} {:>11.2} {:>11.2} {:>10} {:>20}\n",
                    old.name, old.median, new.median, "n/a", "zero baseline"
                ));
            }
            continue;
        }
        let change = (new.median / old.median - 1.0) * 100.0;
        if !change.is_finite() {
            output.push_str(&format!(
                "{:<28} {:>11.2} {:>11.2} {:>10} {:>20}\n",
                old.name, old.median, new.median, "n/a", "extreme ratio"
            ));
            continue;
        }
        let improved = if old.higher_is_better { change > 0.0 } else { change < 0.0 };
        let result = if change.abs() < 0.5 {
            "similar"
        } else if improved {
            "improved"
        } else {
            "regressed"
        };
        output.push_str(&format!(
            "{:<28} {:>11.2} {:>11.2} {:>+10.2}% {:>20}\n",
            old.name, old.median, new.median, change, result
        ));
    }
    for new in &candidate.results {
        if !baseline_names.contains(new.name.as_str()) {
            output.push_str(&format!(
                "{:<28} {:>14} {:>14.2} {:>12} {:>20}\n",
                new.name, "--", new.median, "--", "candidate-only metric"
            ));
        }
    }
    output.push_str(
        "\nHigher throughput is better. Repeat tests under comparable thermal, power and background-load conditions before drawing conclusions.\n",
    );
    Ok(output)
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
    compare_reports(&baseline, &candidate)
}

#[cfg(test)]
mod tests {
    use super::*;


    fn test_report(results: Vec<BenchmarkResult>) -> BenchmarkReport {
        BenchmarkReport {
            schema_version: 2,
            created_unix: 0,
            kernel: "test-kernel".into(),
            architecture: "x86_64".into(),
            cpu: "test-cpu".into(),
            cpu_microarchitecture: "test-arch".into(),
            logical_cpus: 8,
            memory_kib: 1024,
            cpu_governors: vec!["policy0:performance".into()],
            profile: "test".into(),
            results,
        }
    }

    fn test_result(name: &str, median: f64, workload: &str) -> BenchmarkResult {
        BenchmarkResult {
            name: name.into(),
            workload: workload.into(),
            unit: "MiB/s".into(),
            higher_is_better: true,
            iterations: 3,
            samples: vec![median, median, median],
            median,
            min: median,
            max: median,
        }
    }

    #[test]
    fn compare_reports_lists_metrics_missing_from_candidate() {
        let baseline = test_report(vec![test_result("disk", 100.0, "fio")]);
        let candidate = test_report(vec![test_result("memory", 200.0, "copy")]);
        let output = compare_reports(&baseline, &candidate).unwrap();
        assert!(output.contains("missing from candidate"));
        assert!(output.contains("candidate-only metric"));
    }

    #[test]
    fn compare_reports_does_not_divide_by_zero_baseline() {
        let baseline = test_report(vec![test_result("disk", 0.0, "fio")]);
        let candidate = test_report(vec![test_result("disk", 10.0, "fio")]);
        let output = compare_reports(&baseline, &candidate).unwrap();
        assert!(output.contains("zero baseline"));
        assert!(output.contains("n/a"));
    }

    #[test]
    fn compare_reports_rejects_unknown_schema_and_invalid_samples() {
        let mut baseline = test_report(vec![test_result("disk", 10.0, "fio")]);
        baseline.schema_version = 1;
        let candidate = test_report(vec![test_result("disk", 11.0, "fio")]);
        assert!(compare_reports(&baseline, &candidate).is_err());

        let mut invalid = test_result("disk", 10.0, "fio");
        invalid.samples.clear();
        let bad = test_report(vec![invalid]);
        assert!(compare_reports(&candidate, &bad).is_err());
    }

    #[test]
    fn report_validation_rejects_duplicate_metrics() {
        let duplicate = test_report(vec![
            test_result("cpu", 10.0, "rounds"),
            test_result("cpu", 11.0, "rounds"),
        ]);
        assert!(validate_report(&duplicate, "test").is_err());
    }

    #[test]
    fn report_validation_rejects_summary_values_that_do_not_match_samples() {
        let mut result = test_result("cpu", 10.0, "rounds");
        result.samples = vec![9.0, 10.0, 11.0];
        result.min = 8.0;
        let report = test_report(vec![result]);
        assert!(validate_report(&report, "test").is_err());
    }

    #[test]
    fn report_validation_rejects_negative_and_non_finite_samples() {
        let mut negative = test_result("cpu", 1.0, "rounds");
        negative.samples = vec![-1.0, 1.0, 3.0];
        negative.min = -1.0;
        let negative_report = test_report(vec![negative]);
        assert!(validate_report(&negative_report, "test").is_err());

        let mut non_finite = test_result("cpu", 1.0, "rounds");
        non_finite.samples = vec![1.0, f64::NAN, 1.0];
        let non_finite_report = test_report(vec![non_finite]);
        assert!(validate_report(&non_finite_report, "test").is_err());
    }

    #[test]
    fn comparison_respects_lower_is_better_metrics() {
        let mut baseline_result = test_result("latency", 10.0, "request");
        baseline_result.higher_is_better = false;
        let mut candidate_result = test_result("latency", 8.0, "request");
        candidate_result.higher_is_better = false;
        let baseline = test_report(vec![baseline_result]);
        let candidate = test_report(vec![candidate_result]);
        let output = compare_reports(&baseline, &candidate).unwrap();
        assert!(output.contains("improved"));
    }

    #[test]
    fn comparison_handles_extreme_finite_ratios_without_infinity_output() {
        let baseline = test_report(vec![test_result("throughput", 1e-300, "tiny")]);
        let candidate = test_report(vec![test_result("throughput", 1e300, "tiny")]);
        let output = compare_reports(&baseline, &candidate).unwrap();
        assert!(output.contains("extreme ratio"));
        assert!(!output.contains("inf%"));
    }

    #[test]
    fn comparison_handles_two_zero_baselines_as_similar() {
        let baseline = test_report(vec![test_result("metric", 0.0, "test")]);
        let candidate = test_report(vec![test_result("metric", 0.0, "test")]);
        let output = compare_reports(&baseline, &candidate).unwrap();
        assert!(output.contains("similar"));
        assert!(output.contains("+0.00%"));
    }

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
