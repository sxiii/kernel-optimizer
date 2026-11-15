use crate::benchmark::{BenchmarkReport, BenchmarkResult};
use anyhow::{Context, Result, anyhow};
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, Vec2};
use rfd::FileDialog;
use std::{
    collections::VecDeque,
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};

const DEFAULT_PROFILE: &str = r#"source = "linux"
version = "stable"
local_version = "-ko"
jobs = 16
config_base = "current"
compiler = "gcc"
optimization = "native"
use_mold = false
ccache = true
build_dir = "/var/tmp/kernel-optimizer/build"
stage_dir = "/var/tmp/kernel-optimizer/stage"
enable_lto = false
enable_bpf = true
menuconfig = false
"#;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Overview,
    Hardware,
    Recommendations,
    Sources,
    Profile,
    Build,
    Finalize,
    Benchmarks,
    Compare,
    Tuning,
    Install,
    Activity,
}

impl Page {
    fn title(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Hardware => "Hardware",
            Self::Recommendations => "Recommendations",
            Self::Sources => "Kernel sources",
            Self::Profile => "Build profile",
            Self::Build => "Build kernel",
            Self::Finalize => "Finalize staged build",
            Self::Benchmarks => "Benchmarks",
            Self::Compare => "Compare reports",
            Self::Tuning => "System tuning",
            Self::Install => "Installation",
            Self::Activity => "Activity",
        }
    }

}

#[derive(Clone, Copy)]
enum OutputTarget {
    None,
    Hardware,
    Recommendations,
    Sources,
    Comparison,
    Package,
}

#[derive(Clone, Copy)]
enum ReportSlot {
    Baseline,
    Candidate,
}

enum RunnerMessage {
    Line { stderr: bool, text: String },
    Finished(i32),
}

#[derive(Clone)]
struct MetricComparison {
    baseline: BenchmarkResult,
    candidate: BenchmarkResult,
    change_percent: Option<f64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PrivilegedAction {
    Apply,
    Revert,
    InstallPackage,
}

struct KernelOptimizerApp {
    page: Page,
    current_kernel: String,
    profile_path: String,
    profile_text: String,
    profile_dirty: bool,
    selected_workload: String,
    selected_recommendation: String,
    finalize_version: String,
    iterations: usize,
    cpu_rounds: u64,
    memory_mib: usize,
    fio_enabled: bool,
    fio_path: String,
    fio_size_mib: usize,
    fio_runtime_seconds: usize,
    iperf_enabled: bool,
    iperf_server: String,
    baseline_path: String,
    candidate_path: String,
    hardware: Option<serde_json::Value>,
    recommendations: Option<serde_json::Value>,
    sources_text: String,
    baseline: Option<BenchmarkReport>,
    candidate: Option<BenchmarkReport>,
    comparison_output: String,
    output_capture: String,
    logs: VecDeque<String>,
    receiver: Option<Receiver<RunnerMessage>>,
    running: bool,
    current_command: String,
    output_target: OutputTarget,
    pending_report: Option<(PathBuf, ReportSlot)>,
    pending_privileged: Option<PrivilegedAction>,
    package_output_dir: String,
    package_path: String,
    status: String,
    last_exit_code: Option<i32>,
}

impl KernelOptimizerApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = Color32::from_rgb(15, 20, 31);
        visuals.window_fill = Color32::from_rgb(20, 27, 40);
        visuals.extreme_bg_color = Color32::from_rgb(10, 14, 23);
        visuals.faint_bg_color = Color32::from_rgb(27, 36, 51);
        visuals.selection.bg_fill = Color32::from_rgb(35, 123, 156);
        visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(24, 33, 47);
        visuals.widgets.inactive.bg_fill = Color32::from_rgb(31, 43, 60);
        visuals.widgets.hovered.bg_fill = Color32::from_rgb(43, 61, 82);
        visuals.widgets.active.bg_fill = Color32::from_rgb(35, 123, 156);
        cc.egui_ctx.set_visuals(visuals);
        cc.egui_ctx.global_style_mut(|style| {
            style.spacing.item_spacing = Vec2::new(10.0, 9.0);
            style.visuals.window_corner_radius = egui::CornerRadius::same(12);
            style.visuals.menu_corner_radius = egui::CornerRadius::same(10);
        });

        let profile_path = PathBuf::from("kernel-optimizer.toml");
        let profile_text = fs::read_to_string(&profile_path)
            .or_else(|_| fs::read_to_string("kernel-optimizer.example.toml"))
            .unwrap_or_else(|_| DEFAULT_PROFILE.to_owned());
        let current_kernel = Command::new("uname")
            .arg("-r")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .unwrap_or_else(|| "unknown".into());

        Self {
            page: Page::Overview,
            current_kernel,
            profile_path: profile_path.display().to_string(),
            profile_text,
            profile_dirty: false,
            selected_workload: "desktop".into(),
            selected_recommendation: "desktop".into(),
            finalize_version: String::new(),
            iterations: 5,
            cpu_rounds: 5_000_000,
            memory_mib: 64,
            fio_enabled: false,
            fio_path: String::new(),
            fio_size_mib: 256,
            fio_runtime_seconds: 10,
            iperf_enabled: false,
            iperf_server: String::new(),
            baseline_path: "baseline.json".into(),
            candidate_path: "candidate.json".into(),
            hardware: None,
            recommendations: None,
            sources_text: String::new(),
            baseline: None,
            candidate: None,
            comparison_output: String::new(),
            output_capture: String::new(),
            logs: VecDeque::new(),
            receiver: None,
            running: false,
            current_command: String::new(),
            output_target: OutputTarget::None,
            pending_report: None,
            pending_privileged: None,
            package_output_dir: "/var/tmp/kernel-optimizer/stage/packages".into(),
            package_path: String::new(),
            status: "Ready".into(),
            last_exit_code: None,
        }
    }

    fn log(&mut self, line: impl Into<String>) {
        self.logs.push_back(line.into());
        while self.logs.len() > 3000 {
            self.logs.pop_front();
        }
    }

    fn launch(&mut self, args: Vec<String>, elevated: bool, target: OutputTarget) {
        if self.running {
            self.log("Another operation is already running.");
            return;
        }
        let executable = match std::env::current_exe() {
            Ok(path) => path,
            Err(error) => {
                self.log(format!("Unable to resolve application executable: {error}"));
                return;
            }
        };
        let (sender, receiver) = mpsc::channel();
        let display_command = args.join(" ");
        self.current_command = if elevated {
            format!("pkexec {} {display_command}", executable.display())
        } else {
            format!("{} {display_command}", executable.display())
        };
        self.output_capture.clear();
        self.output_target = target;
        self.last_exit_code = None;
        self.status = if elevated {
            "Waiting for administrator authorization...".into()
        } else {
            "Running...".into()
        };
        self.log(format!("$ {}", self.current_command));
        self.running = true;
        self.receiver = Some(receiver);

        thread::spawn(move || {
            let mut command = if elevated {
                let mut command = Command::new("pkexec");
                command.arg(&executable);
                command
            } else {
                Command::new(executable)
            };
            command
                .args(&args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(error) => {
                    let _ = sender.send(RunnerMessage::Line {
                        stderr: true,
                        text: format!("Could not start command: {error}"),
                    });
                    let _ = sender.send(RunnerMessage::Finished(-1));
                    return;
                }
            };

            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            let stdout_sender = sender.clone();
            let stdout_thread = stdout.map(|stream| {
                thread::spawn(move || forward_lines(stream, false, stdout_sender))
            });
            let stderr_sender = sender.clone();
            let stderr_thread = stderr.map(|stream| {
                thread::spawn(move || forward_lines(stream, true, stderr_sender))
            });

            let code = child.wait().ok().and_then(|status| status.code()).unwrap_or(-1);
            if let Some(reader) = stdout_thread {
                let _ = reader.join();
            }
            if let Some(reader) = stderr_thread {
                let _ = reader.join();
            }
            let _ = sender.send(RunnerMessage::Finished(code));
        });
    }

    fn poll_runner(&mut self, ctx: &egui::Context) {
        let mut finished = None;
        let mut messages = Vec::new();
        if let Some(receiver) = self.receiver.as_ref() {
            while let Ok(message) = receiver.try_recv() {
                messages.push(message);
            }
        }

        for message in messages {
            match message {
                RunnerMessage::Line { stderr, text } => {
                    if !stderr {
                        self.output_capture.push_str(&text);
                        self.output_capture.push('\n');
                    }
                    self.log(if stderr { format!("stderr: {text}") } else { text });
                }
                RunnerMessage::Finished(code) => finished = Some(code),
            }
        }

        if let Some(code) = finished {
            self.running = false;
            self.receiver = None;
            self.last_exit_code = Some(code);
            self.status = if code == 0 {
                "Completed".into()
            } else {
                format!("Exited with status {code}")
            };
            self.log(format!("Process finished with exit code {code}."));
            if code == 0 {
                match self.output_target {
                    OutputTarget::Hardware => match serde_json::from_str(&self.output_capture) {
                        Ok(report) => self.hardware = Some(report),
                        Err(error) => self.log(format!("Could not parse hardware JSON: {error}")),
                    },
                    OutputTarget::Recommendations => {
                        match serde_json::from_str(&self.output_capture) {
                            Ok(report) => self.recommendations = Some(report),
                            Err(error) => self.log(format!("Could not parse recommendation JSON: {error}")),
                        }
                    }
                    OutputTarget::Sources => self.sources_text = self.output_capture.clone(),
                    OutputTarget::Comparison => self.comparison_output = self.output_capture.clone(),
                    OutputTarget::Package => {
                        if let Some(path) = self.output_capture.lines().find_map(|line| line.strip_prefix("KERNEL_OPTIMIZER_PACKAGE=")) {
                            self.package_path = path.trim().to_owned();
                            self.log(format!("Package ready: {}", self.package_path));
                        } else {
                            self.log("The package build completed, but no package path was reported.");
                        }
                    },
                    OutputTarget::None => {}
                }
                if let Some((path, slot)) = self.pending_report.take() {
                    match load_benchmark_report(&path) {
                        Ok(report) => {
                            match slot {
                                ReportSlot::Baseline => self.baseline = Some(report),
                                ReportSlot::Candidate => self.candidate = Some(report),
                            }
                            self.log(format!("Loaded benchmark report from {}", path.display()));
                        }
                        Err(error) => self.log(format!("Benchmark completed, but its JSON could not be loaded: {error:#}")),
                    }
                }
                if self.page == Page::Profile
                    && let Ok(contents) = fs::read_to_string(&self.profile_path)
                {
                    self.profile_text = contents;
                    self.profile_dirty = false;
                }
            }
            self.output_target = OutputTarget::None;
            self.pending_report = None;
        }
        if self.running {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
    }

    fn save_profile(&mut self) -> Result<()> {
        let _: toml::Value = toml::from_str(&self.profile_text)
            .context("profile text is not valid TOML")?;
        let path = PathBuf::from(self.profile_path.trim());
        if path.as_os_str().is_empty() {
            anyhow::bail!("choose a profile path first");
        }
        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, &self.profile_text)
            .with_context(|| format!("writing profile {}", path.display()))?;
        self.profile_dirty = false;
        self.log(format!("Saved profile to {}", path.display()));
        Ok(())
    }

    fn load_profile(&mut self) {
        match fs::read_to_string(self.profile_path.trim()) {
            Ok(contents) => {
                self.profile_text = contents;
                self.profile_dirty = false;
                self.log(format!("Loaded profile from {}", self.profile_path));
            }
            Err(error) => self.log(format!("Could not load profile: {error}")),
        }
    }

    fn load_report_into(&mut self, path: &str, slot: ReportSlot) {
        match load_benchmark_report(Path::new(path)) {
            Ok(report) => {
                let kernel = report.kernel.clone();
                match slot {
                    ReportSlot::Baseline => self.baseline = Some(report),
                    ReportSlot::Candidate => self.candidate = Some(report),
                }
                self.log(format!("Imported benchmark report for {kernel} from {path}"));
            }
            Err(error) => self.log(format!("Report import failed: {error:#}")),
        }
    }

    fn run_hardware(&mut self) {
        self.launch(vec!["hardware".into()], false, OutputTarget::Hardware);
    }

    fn run_recommendations(&mut self) {
        self.launch(
            vec!["recommend".into(), "--profile".into(), self.selected_recommendation.clone()],
            false,
            OutputTarget::Recommendations,
        );
    }

    fn run_doctor(&mut self) {
        if let Err(error) = self.save_profile() {
            self.log(format!("Cannot run doctor: {error:#}"));
            return;
        }
        self.launch(
            vec!["doctor".into(), "--profile".into(), self.profile_path.clone()],
            false,
            OutputTarget::None,
        );
    }

    fn run_benchmark(&mut self, slot: ReportSlot) {
        let output = match slot {
            ReportSlot::Baseline => self.baseline_path.clone(),
            ReportSlot::Candidate => self.candidate_path.clone(),
        };
        if output.trim().is_empty() {
            self.log("Choose an output JSON path first.");
            return;
        }
        let profile = match slot {
            ReportSlot::Baseline => "baseline",
            ReportSlot::Candidate => "candidate",
        };
        let mut args = vec![
            "benchmark".into(),
            "--output".into(), output.clone(),
            "--iterations".into(), self.iterations.to_string(),
            "--profile".into(), profile.into(),
            "--cpu-rounds".into(), self.cpu_rounds.to_string(),
            "--memory-mib".into(), self.memory_mib.to_string(),
        ];
        if self.fio_enabled {
            if self.fio_path.trim().is_empty() {
                self.log("Disk benchmark is enabled but no existing test directory was selected.");
                return;
            }
            args.extend([
                "--fio-path".into(), self.fio_path.clone(),
                "--fio-size-mib".into(), self.fio_size_mib.to_string(),
                "--fio-runtime-seconds".into(), self.fio_runtime_seconds.to_string(),
            ]);
        }
        if self.iperf_enabled {
            if self.iperf_server.trim().is_empty() {
                self.log("Network benchmark is enabled but no iperf3 server was provided.");
                return;
            }
            args.extend(["--iperf-server".into(), self.iperf_server.clone()]);
        }
        self.pending_report = Some((PathBuf::from(output), slot));
        self.launch(args, false, OutputTarget::None);
    }

    fn run_compare(&mut self) {
        let baseline = match load_benchmark_report(Path::new(self.baseline_path.trim())) {
            Ok(report) => report,
            Err(error) => {
                self.log(format!("Could not load baseline report: {error:#}"));
                return;
            }
        };
        let candidate = match load_benchmark_report(Path::new(self.candidate_path.trim())) {
            Ok(report) => report,
            Err(error) => {
                self.log(format!("Could not load candidate report: {error:#}"));
                return;
            }
        };
        self.baseline = Some(baseline);
        self.candidate = Some(candidate);
        self.comparison_output.clear();
        self.launch(
            vec![
                "compare".into(),
                "--baseline".into(), self.baseline_path.clone(),
                "--candidate".into(), self.candidate_path.clone(),
            ],
            false,
            OutputTarget::Comparison,
        );
    }

    fn request_privileged(&mut self, action: PrivilegedAction) {
        self.pending_privileged = Some(action);
    }

    fn confirm_privileged(&mut self) {
        let Some(action) = self.pending_privileged.take() else {
            return;
        };
        match action {
            PrivilegedAction::Apply => self.launch(
                vec![
                    "tune".into(),
                    "--profile".into(), self.selected_workload.clone(),
                ],
                true,
                OutputTarget::None,
            ),
            PrivilegedAction::Revert => self.launch(
                vec!["revert".into()],
                true,
                OutputTarget::None,
            ),
            PrivilegedAction::InstallPackage => self.launch(
                vec![
                    "install".into(),
                    "--package".into(), self.package_path.clone(),
                    "--yes".into(),
                ],
                true,
                OutputTarget::None,
            ),
        }
    }

    fn open_tui(&mut self) {
        if self.running {
            self.log("Wait for the active operation to finish before opening the terminal dashboard.");
            return;
        }
        let executable = match std::env::current_exe() {
            Ok(path) => path,
            Err(error) => {
                self.log(format!("Could not locate executable: {error}"));
                return;
            }
        };
        let mut last_error = None;
        for terminal in ["x-terminal-emulator", "konsole", "xfce4-terminal", "kitty", "alacritty", "foot", "gnome-terminal"] {
            let mut command = Command::new(terminal);
            if terminal == "gnome-terminal" || terminal == "wezterm" {
                command.arg("--");
            } else {
                command.arg("-e");
            }
            command
                .arg(&executable)
                .arg("tui")
                .arg("--profile")
                .arg(&self.selected_recommendation);
            match command.spawn() {
                Ok(_) => {
                    self.log(format!("Opened terminal dashboard using {terminal}."));
                    return;
                }
                Err(error) => last_error = Some(error),
            }
        }
        self.log(format!(
            "No supported terminal emulator could be started. Install x-terminal-emulator or another terminal emulator. Last error: {}",
            last_error.map_or_else(|| "not found".into(), |error| error.to_string())
        ));
    }

    fn nav_button(&mut self, ui: &mut egui::Ui, page: Page) {
        let selected = self.page == page;
        let label = page.title().to_owned();
        let button = egui::Button::new(
            RichText::new(label)
                .size(14.0)
                .color(if selected { Color32::WHITE } else { Color32::from_rgb(180, 192, 209) }),
        )
        .fill(if selected {
            Color32::from_rgb(37, 93, 126)
        } else {
            Color32::TRANSPARENT
        })
        .corner_radius(egui::CornerRadius::same(8))
        .min_size(Vec2::new(188.0, 36.0));
        if ui.add(button).clicked() {
            self.page = page;
        }
    }

    fn page_header(ui: &mut egui::Ui, title: &str, subtitle: &str) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.heading(RichText::new(title).size(25.0).strong());
                ui.label(RichText::new(subtitle).color(Color32::from_rgb(148, 164, 185)));
            });
        });
        ui.add_space(10.0);
    }

    fn card<R>(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
        egui::Frame::group(ui.style())
            .fill(Color32::from_rgb(23, 31, 46))
            .stroke(Stroke::new(1.0, Color32::from_rgb(43, 57, 76)))
            .corner_radius(egui::CornerRadius::same(10))
            .inner_margin(12)
            .show(ui, add_contents)
            .inner
    }

    fn stat_card(ui: &mut egui::Ui, title: &str, value: &str, detail: &str, accent: Color32) {
        Self::card(ui, |ui| {
            ui.label(RichText::new(title.to_uppercase()).size(11.0).color(Color32::from_rgb(147, 163, 184)).strong());
            ui.add_space(5.0);
            ui.label(RichText::new(value).size(21.0).strong().color(accent));
            ui.label(RichText::new(detail).size(11.0).color(Color32::from_rgb(147, 163, 184)));
        });
    }

    fn overview_page(&mut self, ui: &mut egui::Ui) {
        Self::page_header(
            ui,
            "Kernel Optimizer",
            "A safer, measurable workflow for building and tuning Linux kernels.",
        );

        Self::card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new("CURRENTLY RUNNING").size(11.0).color(Color32::from_rgb(145, 164, 188)).strong());
                    ui.heading(RichText::new(&self.current_kernel).size(24.0).color(Color32::from_rgb(93, 204, 224)));
                    ui.label("Kernel version reported by uname");
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_enabled(!self.running, egui::Button::new("  Refresh hardware").fill(Color32::from_rgb(35, 105, 135))).clicked() {
                        self.run_hardware();
                    }
                });
            });
        });

        ui.add_space(10.0);
        let cpu = self.hardware.as_ref()
            .and_then(|value| value.get("cpu"))
            .and_then(|value| value.get("model"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Refresh hardware to detect CPU");
        let memory = self.hardware.as_ref()
            .and_then(|value| value.get("memory_kib"))
            .and_then(serde_json::Value::as_u64)
            .map(|kib| format!("{:.1} GiB", kib as f64 / 1024.0 / 1024.0))
            .unwrap_or_else(|| "-".into());
        let devices = self.hardware.as_ref()
            .and_then(|value| value.get("block_devices"))
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len);
        ui.columns(3, |columns| {
            Self::stat_card(&mut columns[0], "Processor", cpu, "Detected CPU model", Color32::from_rgb(93, 204, 224));
            Self::stat_card(&mut columns[1], "Memory", &memory, "Installed system memory", Color32::from_rgb(151, 190, 255));
            Self::stat_card(&mut columns[2], "Storage devices", &devices.to_string(), "Block devices and volumes", Color32::from_rgb(125, 208, 161));
        });

        ui.add_space(12.0);
        ui.columns(2, |columns| {
            Self::card(&mut columns[0], |ui| {
                ui.heading("Kernel workflow");
                ui.label("Preflight, configure, compile and verify staged kernel builds.");
                ui.add_space(8.0);
                if ui.button("Open build workspace").clicked() {
                    self.page = Page::Build;
                }
                if ui.button("Run environment doctor").clicked() {
                    self.run_doctor();
                }
            });
            Self::card(&mut columns[1], |ui| {
                ui.heading("Performance lab");
                ui.label("Run repeatable CPU, memory, storage and network tests.");
                ui.add_space(8.0);
                if ui.button("Run benchmark").clicked() {
                    self.page = Page::Benchmarks;
                }
                if ui.button("Compare JSON reports").clicked() {
                    self.page = Page::Compare;
                }
            });
        });

        ui.add_space(12.0);
        Self::card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("System tuning");
                ui.label(RichText::new("Preview first").color(Color32::from_rgb(235, 191, 104)));
            });
            ui.label("Review workload-aware sysctl and I/O scheduler changes. Applying or reverting requires an administrator authorization prompt.");
            if ui.button("Open tuning controls").clicked() {
                self.page = Page::Tuning;
            }
        });

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui.add_enabled(!self.running, egui::Button::new("Open terminal dashboard")).clicked() {
                self.open_tui();
            }
            if ui.add_enabled(!self.running, egui::Button::new("View supported sources")).clicked() {
                self.launch(vec!["sources".into()], false, OutputTarget::Sources);
                self.page = Page::Sources;
            }
            if ui.add_enabled(!self.running, egui::Button::new("Inspect hardware")).clicked() {
                self.run_hardware();
                self.page = Page::Hardware;
            }
        });
    }

    fn hardware_page(&mut self, ui: &mut egui::Ui) {
        Self::page_header(ui, "Hardware inventory", "Live system information collected by the existing hardware detector.");
        ui.horizontal(|ui| {
            if ui.add_enabled(!self.running, egui::Button::new("Refresh hardware").fill(Color32::from_rgb(35, 105, 135))).clicked() {
                self.run_hardware();
            }
            if ui.button("Open raw JSON").clicked() {
                self.log(self.hardware.as_ref().map_or_else(
                    || "Hardware report has not been loaded yet.".into(),
                    |value| serde_json::to_string_pretty(value).unwrap_or_default(),
                ));
                self.page = Page::Activity;
            }
        });
        ui.add_space(8.0);
        let Some(hw) = self.hardware.clone() else {
            Self::card(ui, |ui| {
                ui.label("No hardware report loaded yet.");
                ui.label("Select Refresh hardware to run the same hardware inspection used by the CLI.");
            });
            return;
        };

        let cpu = &hw["cpu"];
        ui.columns(2, |columns| {
            Self::card(&mut columns[0], |ui| {
                ui.heading("Processor");
                ui.label(RichText::new(cpu["model"].as_str().unwrap_or("Unknown")).strong().size(16.0));
                ui.label(format!("Vendor: {}", cpu["vendor"].as_str().unwrap_or("unknown")));
                ui.label(format!("Microarchitecture: {}", cpu["microarchitecture"].as_str().unwrap_or("unknown")));
                ui.label(format!("Logical CPUs: {}", cpu["logical_cpus"].as_u64().unwrap_or(0)));
                ui.label(format!("Physical cores: {}", cpu["physical_cores"].as_u64().map_or_else(|| "unknown".into(), |v| v.to_string())));
                ui.label(format!("Sockets: {}", cpu["sockets"].as_u64().map_or_else(|| "unknown".into(), |v| v.to_string())));
            });
            Self::card(&mut columns[1], |ui| {
                ui.heading("System");
                ui.label(format!("Kernel: {}", hw["kernel_release"].as_str().unwrap_or("unknown")));
                ui.label(format!("Architecture: {}", hw["architecture"].as_str().unwrap_or("unknown")));
                let memory = hw["memory_kib"].as_u64().unwrap_or(0) as f64 / 1024.0 / 1024.0;
                ui.label(format!("Memory: {memory:.1} GiB"));
                ui.label(format!("Virtualization: {}", hw["virtualization"].as_str().unwrap_or("unknown")));
                ui.label("Kernel command line:");
                ui.label(RichText::new(hw["current_cmdline"].as_str().unwrap_or("")).small().monospace());
            });
        });

        ui.add_space(10.0);
        Self::card(ui, |ui| {
            ui.heading("Storage devices");
            if let Some(devices) = hw["block_devices"].as_array() {
                egui::Grid::new("hardware_block_devices").striped(true).show(ui, |ui| {
                    for title in ["Device", "Media", "Size", "Model", "Scheduler"] {
                        ui.label(RichText::new(title).strong());
                    }
                    ui.end_row();
                    for item in devices {
                        ui.label(item["name"].as_str().unwrap_or("-"));
                        ui.label(item["media_type"].as_str().unwrap_or("-"));
                        ui.label(item["size"].as_str().unwrap_or("-"));
                        ui.label(item["model"].as_str().unwrap_or("-"));
                        ui.label(item["scheduler"].as_str().unwrap_or("-"));
                        ui.end_row();
                    }
                });
            }
        });
        ui.add_space(10.0);
        ui.columns(2, |columns| {
            Self::card(&mut columns[0], |ui| {
                ui.heading("Graphics controllers");
                if let Some(gpus) = hw["gpus"].as_array() {
                    for gpu in gpus {
                        ui.label(gpu.as_str().unwrap_or("-"));
                    }
                    if gpus.is_empty() { ui.label("No controllers reported."); }
                }
            });
            Self::card(&mut columns[1], |ui| {
                ui.heading("Network controllers");
                if let Some(network) = hw["network_controllers"].as_array() {
                    for controller in network {
                        ui.label(controller.as_str().unwrap_or("-"));
                    }
                    if network.is_empty() { ui.label("No controllers reported."); }
                }
            });
        });
        ui.add_space(10.0);
        Self::card(ui, |ui| {
            ui.heading("CPU frequency policies");
            if let Some(policies) = hw["cpu_frequency_policies"].as_array() {
                egui::Grid::new("hardware_cpu_policies").striped(true).show(ui, |ui| {
                    for title in ["Policy", "CPUs", "Governor", "EPP", "Current MHz", "Limits MHz"] {
                        ui.label(RichText::new(title).strong());
                    }
                    ui.end_row();
                    for policy in policies {
                        ui.label(policy["name"].as_str().unwrap_or("-"));
                        ui.label(policy["affected_cpus"].as_str().unwrap_or("-"));
                        ui.label(policy["governor"].as_str().unwrap_or("-"));
                        ui.label(policy["energy_performance_preference"].as_str().unwrap_or("-"));
                        ui.label(policy["current_frequency_mhz"].as_f64().map_or_else(|| "-".into(), |v| format!("{v:.0}")));
                        let min = policy["min_frequency_mhz"].as_f64().map_or_else(|| "-".into(), |v| format!("{v:.0}"));
                        let max = policy["max_frequency_mhz"].as_f64().map_or_else(|| "-".into(), |v| format!("{v:.0}"));
                        ui.label(format!("{min}-{max}"));
                        ui.end_row();
                    }
                });
            }
        });
    }

    fn recommendations_page(&mut self, ui: &mut egui::Ui) {
        Self::page_header(ui, "Recommendations", "Read-only, workload-aware suggestions based on detected hardware.");
        ui.horizontal(|ui| {
            egui::ComboBox::from_label("Workload")
                .selected_text(&self.selected_recommendation)
                .show_ui(ui, |ui| {
                    for profile in ["desktop", "laptop", "server", "gaming"] {
                        ui.selectable_value(&mut self.selected_recommendation, profile.into(), profile);
                    }
                });
            if ui.add_enabled(!self.running, egui::Button::new("Generate recommendations").fill(Color32::from_rgb(35, 105, 135))).clicked() {
                self.run_recommendations();
            }
        });
        ui.add_space(8.0);
        let Some(report) = self.recommendations.clone() else {
            Self::card(ui, |ui| {
                ui.label("No recommendation report loaded.");
                ui.label("Generate a report to see current settings, suggestions, rationale and trade-offs.");
            });
            return;
        };
        ui.label(format!("Workload profile: {}", report["profile"].as_str().unwrap_or("unknown")));
        ui.label(RichText::new("Nothing is applied from this page.").color(Color32::from_rgb(235, 191, 104)));
        if let Some(items) = report["recommendations"].as_array() {
            for item in items {
                ui.add_space(7.0);
                Self::card(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(item["category"].as_str().unwrap_or("GENERAL").to_uppercase()).color(Color32::from_rgb(96, 194, 220)).strong());
                        ui.heading(item["component"].as_str().unwrap_or("Recommendation"));
                    });
                    ui.columns(2, |columns| {
                        columns[0].label(RichText::new("CURRENT").size(10.0).color(Color32::GRAY).strong());
                        columns[0].label(item["current"].as_str().unwrap_or("-"));
                        columns[1].label(RichText::new("SUGGESTION").size(10.0).color(Color32::GRAY).strong());
                        columns[1].label(RichText::new(item["suggestion"].as_str().unwrap_or("-")).color(Color32::from_rgb(125, 208, 161)));
                    });
                    ui.add_space(4.0);
                    ui.label(RichText::new("Why").strong());
                    ui.label(item["rationale"].as_str().unwrap_or("-"));
                    ui.label(RichText::new("Trade-off").strong());
                    ui.label(item["tradeoff"].as_str().unwrap_or("-"));
                    ui.label(RichText::new("Action").strong());
                    ui.label(item["action"].as_str().unwrap_or("-"));
                });
            }
        }
    }

    fn sources_page(&mut self, ui: &mut egui::Ui) {
        Self::page_header(ui, "Kernel sources", "Source tracks currently advertised by the CLI.");
        if ui.add_enabled(!self.running, egui::Button::new("Refresh source list")).clicked() {
            self.launch(vec!["sources".into()], false, OutputTarget::Sources);
        }
        ui.add_space(8.0);
        if self.sources_text.trim().is_empty() {
            self.sources_text = String::from(
                "SOURCE            TRACK / PURPOSE\nlinux             Latest stable upstream kernel (kernel.org)\nlinux-lts          Long-term support upstream kernel series\nlinux-zen          Desktop responsiveness / low-latency patchset (Arch)\nlinux-cachyos      CachyOS patches and scheduler/config profiles\nlinux-tkg          Community kernel patch/config framework (TKG)\nlinux-rt           PREEMPT_RT real-time patchset (specialized workloads)\n\nOnly upstream linux is fetched automatically. Other tracks are planned adapters; no untrusted patch URL is executed.",
            );
        }
        Self::card(ui, |ui| {
            ui.label(RichText::new(&self.sources_text).monospace());
        });
        ui.add_space(8.0);
        ui.label(RichText::new("Current implementation: upstream linux only. Other listed sources are informational and cannot be selected for builds yet.").color(Color32::from_rgb(235, 191, 104)));
    }

    fn profile_page(&mut self, ui: &mut egui::Ui) {
        Self::page_header(ui, "Build profile", "Edit and validate the TOML profile used by doctor, build and finalize.");
        ui.horizontal(|ui| {
            ui.label("Profile file");
            ui.add(egui::TextEdit::singleline(&mut self.profile_path).desired_width(360.0));
            if ui.button("Open...").clicked()
                && let Some(path) = FileDialog::new().add_filter("TOML profile", &["toml"]).pick_file() {
                    self.profile_path = path.display().to_string();
                    self.load_profile();
            }
            if ui.button("Reload").clicked() {
                self.load_profile();
            }
        });
        ui.horizontal(|ui| {
            if ui.button("Save profile").clicked() {
                match self.save_profile() {
                    Ok(()) => self.status = "Profile saved".into(),
                    Err(error) => self.log(format!("Profile save failed: {error:#}")),
                }
            }
            if ui.add_enabled(!self.running, egui::Button::new("Create default profile")).clicked() {
                if Path::new(&self.profile_path).exists() {
                    self.log("Profile already exists. Choose another path or edit the current file; it was not overwritten.");
                } else {
                    self.launch(
                        vec!["profile".into(), "--file".into(), self.profile_path.clone(), "--create".into()],
                        false,
                        OutputTarget::None,
                    );
                }
            }
            if ui.add_enabled(!self.running, egui::Button::new("Run doctor")).clicked() {
                self.run_doctor();
            }
        });
        ui.add_space(4.0);
        if self.profile_dirty {
            ui.label(RichText::new("Unsaved changes").color(Color32::from_rgb(237, 187, 94)).strong());
        } else {
            ui.label(RichText::new("Profile editor").color(Color32::GRAY));
        }
        let response = ui.add(
            egui::TextEdit::multiline(&mut self.profile_text)
                .font(egui::TextStyle::Monospace)
                .desired_rows(24)
                .desired_width(f32::INFINITY)
                .code_editor(),
        );
        if response.changed() {
            self.profile_dirty = true;
        }
        ui.label(RichText::new("The application validates the profile before a build. Keep build_dir and stage_dir separate.").small().color(Color32::GRAY));
    }

    fn build_page(&mut self, ui: &mut egui::Ui) {
        Self::page_header(ui, "Build kernel", "Preflight and compile a signed upstream kernel into the staging directory.");
        ui.horizontal(|ui| {
            ui.label("Profile");
            ui.add(egui::TextEdit::singleline(&mut self.profile_path).desired_width(340.0));
            if ui.button("Browse...").clicked()
                && let Some(path) = FileDialog::new().add_filter("TOML profile", &["toml"]).pick_file() {
                    self.profile_path = path.display().to_string();
                    self.load_profile();
            }
            if ui.button("Edit profile").clicked() {
                self.page = Page::Profile;
            }
        });
        ui.add_space(8.0);
        Self::card(ui, |ui| {
            ui.heading("Before you build");
            ui.label("Kernel builds can take hours and require substantial disk space. The builder verifies kernel.org signatures and stages output separately from the live boot path.");
            ui.horizontal(|ui| {
                if ui.add_enabled(!self.running, egui::Button::new("Run doctor").fill(Color32::from_rgb(35, 105, 135))).clicked() {
                    self.run_doctor();
                }
                if ui.add_enabled(!self.running, egui::Button::new("Dry run")).clicked() {
                    if let Err(error) = self.save_profile() {
                        self.log(format!("Cannot start dry run: {error:#}"));
                    } else {
                        self.launch(
                            vec!["build".into(), "--profile".into(), self.profile_path.clone(), "--dry-run".into()],
                            false,
                            OutputTarget::None,
                        );
                    }
                }
            });
        });
        ui.add_space(8.0);
        Self::card(ui, |ui| {
            ui.heading("Compile and stage");
            ui.label("This runs the full build command. It does not install modules into the host or modify GRUB.");
            let menuconfig = toml::from_str::<toml::Value>(&self.profile_text)
                .ok()
                .and_then(|value| value.get("menuconfig").and_then(toml::Value::as_bool))
                .unwrap_or(false);
            if menuconfig {
                ui.label(RichText::new("menuconfig is enabled. The build must run in an external terminal so the interactive Kconfig screen has a real TTY.").color(Color32::from_rgb(235, 191, 104)));
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(!self.running, egui::Button::new(if menuconfig { "Build in terminal" } else { "Start kernel build" }).fill(Color32::from_rgb(35, 105, 135))).clicked() {
                    if let Err(error) = self.save_profile() {
                        self.log(format!("Cannot start build: {error:#}"));
                    } else if menuconfig {
                        self.launch_terminal_command(vec![
                            "build".into(), "--profile".into(), self.profile_path.clone()
                        ]);
                    } else {
                        self.launch(
                            vec!["build".into(), "--profile".into(), self.profile_path.clone()],
                            false,
                            OutputTarget::None,
                        );
                    }
                }
                if ui.add_enabled(!self.running, egui::Button::new("Open activity log")).clicked() {
                    self.page = Page::Activity;
                }
            });
        });
    }

    fn finalize_page(&mut self, ui: &mut egui::Ui) {
        Self::page_header(ui, "Finalize staged build", "Recover and verify metadata for an existing build without recompiling.");
        ui.horizontal(|ui| {
            ui.label("Profile");
            ui.add(egui::TextEdit::singleline(&mut self.profile_path).desired_width(330.0));
            if ui.button("Browse...").clicked()
                && let Some(path) = FileDialog::new().add_filter("TOML profile", &["toml"]).pick_file() {
                    self.profile_path = path.display().to_string();
                    self.load_profile();
            }
        });
        ui.horizontal(|ui| {
            ui.label("Kernel version");
            ui.add(egui::TextEdit::singleline(&mut self.finalize_version).hint_text("e.g. 7.3.4 (optional)").desired_width(220.0));
        });
        Self::card(ui, |ui| {
            ui.label("Finalize validates the staged kernel image, configuration, System.map, module indexes and upstream detached signature, then writes provenance.json and BUILD-METADATA.txt.");
            ui.label(RichText::new("No kernel compilation, modules_install, initramfs generation or bootloader changes are performed.").color(Color32::from_rgb(125, 208, 161)));
            if ui.add_enabled(!self.running, egui::Button::new("Finalize staged build").fill(Color32::from_rgb(35, 105, 135))).clicked() {
                if let Err(error) = self.save_profile() {
                    self.log(format!("Cannot finalize: {error:#}"));
                } else {
                    let mut args = vec![
                        "finalize".into(),
                        "--profile".into(), self.profile_path.clone(),
                    ];
                    if !self.finalize_version.trim().is_empty() {
                        args.extend(["--version".into(), self.finalize_version.trim().to_owned()]);
                    }
                    self.launch(args, false, OutputTarget::None);
                }
            }
        });
    }

    fn benchmarks_page(&mut self, ui: &mut egui::Ui) {
        Self::page_header(ui, "Performance lab", "Run repeatable CPU, memory, optional disk and network benchmarks.");
        ui.columns(2, |columns| {
            Self::card(&mut columns[0], |ui| {
                ui.heading("Benchmark settings");
                ui.horizontal(|ui| {
                    ui.label("Iterations");
                    ui.add(egui::DragValue::new(&mut self.iterations).range(1..=20));
                });
                ui.horizontal(|ui| {
                    ui.label("CPU rounds");
                    ui.add(egui::DragValue::new(&mut self.cpu_rounds).range(100_000..=100_000_000).speed(100_000.0));
                });
                ui.horizontal(|ui| {
                    ui.label("Memory MiB");
                    ui.add(egui::DragValue::new(&mut self.memory_mib).range(8..=1024));
                });
                ui.separator();
                ui.checkbox(&mut self.fio_enabled, "Include fio disk test");
                ui.add_enabled_ui(self.fio_enabled, |ui| {
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.fio_path).hint_text("Existing test directory").desired_width(190.0));
                        if ui.button("...").clicked()
                            && let Some(path) = FileDialog::new().pick_folder() {
                                self.fio_path = path.display().to_string();
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("File size MiB");
                        ui.add(egui::DragValue::new(&mut self.fio_size_mib).range(32..=4096));
                    });
                    ui.horizontal(|ui| {
                        ui.label("Runtime seconds");
                        ui.add(egui::DragValue::new(&mut self.fio_runtime_seconds).range(3..=60));
                    });
                });
                ui.separator();
                ui.checkbox(&mut self.iperf_enabled, "Include iperf3 network test");
                ui.add_enabled_ui(self.iperf_enabled, |ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.iperf_server).hint_text("iperf3 server hostname/IP"));
                });
            });
            Self::card(&mut columns[1], |ui| {
                ui.heading("Report destinations");
                ui.label("Baseline report");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.baseline_path).desired_width(220.0));
                    if ui.button("...").clicked()
                        && let Some(path) = FileDialog::new().add_filter("Benchmark JSON", &["json"]).set_file_name("baseline.json").save_file() {
                            self.baseline_path = path.display().to_string();
                    }
                });
                ui.label("Candidate report");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.candidate_path).desired_width(220.0));
                    if ui.button("...").clicked()
                        && let Some(path) = FileDialog::new().add_filter("Benchmark JSON", &["json"]).set_file_name("candidate.json").save_file() {
                            self.candidate_path = path.display().to_string();
                    }
                });
                ui.add_space(8.0);
                ui.label("Run each kernel after booting it. Use matching settings and comparable thermal/background conditions.");
                ui.add_space(4.0);
                if ui.add_enabled(!self.running, egui::Button::new("Run baseline benchmark").fill(Color32::from_rgb(55, 91, 137))).clicked() {
                    self.run_benchmark(ReportSlot::Baseline);
                }
                if ui.add_enabled(!self.running, egui::Button::new("Run candidate benchmark").fill(Color32::from_rgb(35, 105, 135))).clicked() {
                    self.run_benchmark(ReportSlot::Candidate);
                }
                if ui.button("Open comparison charts").clicked() {
                    self.page = Page::Compare;
                }
            });
        });
        ui.add_space(10.0);
        ui.columns(2, |columns| {
            Self::report_summary(&mut columns[0], "Baseline", self.baseline.as_ref());
            Self::report_summary(&mut columns[1], "Candidate", self.candidate.as_ref());
        });
    }

    fn report_summary(ui: &mut egui::Ui, title: &str, report: Option<&BenchmarkReport>) {
        Self::card(ui, |ui| {
            ui.heading(title);
            if let Some(report) = report {
                ui.label(RichText::new(&report.kernel).strong().color(Color32::from_rgb(93, 204, 224)));
                ui.label(&report.cpu);
                ui.label(format!("{} metrics  |  {} logical CPUs", report.results.len(), report.logical_cpus));
                for result in &report.results {
                    ui.label(format!("{}: {} {}", result.name, format_value(result.median), result.unit));
                }
            } else {
                ui.label("No report imported yet.");
            }
        });
    }

    fn compare_page(&mut self, ui: &mut egui::Ui) {
        Self::page_header(ui, "Compare benchmark reports", "Import schema-v2 JSON reports and inspect median changes and per-iteration charts.");
        ui.columns(2, |columns| {
            Self::card(&mut columns[0], |ui| {
                ui.heading("Baseline JSON");
                ui.add(egui::TextEdit::singleline(&mut self.baseline_path).desired_width(f32::INFINITY));
                ui.horizontal(|ui| {
                    if ui.button("Browse...").clicked()
                        && let Some(path) = FileDialog::new().add_filter("Benchmark JSON", &["json"]).pick_file() {
                            self.baseline_path = path.display().to_string();
                    }
                    if ui.button("Import baseline").clicked() {
                        self.load_report_into(&self.baseline_path.clone(), ReportSlot::Baseline);
                    }
                });
                if let Some(report) = &self.baseline {
                    ui.label(RichText::new(&report.kernel).color(Color32::from_rgb(93, 204, 224)).strong());
                }
            });
            Self::card(&mut columns[1], |ui| {
                ui.heading("Candidate JSON");
                ui.add(egui::TextEdit::singleline(&mut self.candidate_path).desired_width(f32::INFINITY));
                ui.horizontal(|ui| {
                    if ui.button("Browse...").clicked()
                        && let Some(path) = FileDialog::new().add_filter("Benchmark JSON", &["json"]).pick_file() {
                            self.candidate_path = path.display().to_string();
                    }
                    if ui.button("Import candidate").clicked() {
                        self.load_report_into(&self.candidate_path.clone(), ReportSlot::Candidate);
                    }
                });
                if let Some(report) = &self.candidate {
                    ui.label(RichText::new(&report.kernel).color(Color32::from_rgb(93, 204, 224)).strong());
                }
            });
        });
        ui.horizontal(|ui| {
            if ui.add_enabled(!self.running, egui::Button::new("Run official comparison").fill(Color32::from_rgb(35, 105, 135))).clicked() {
                self.run_compare();
            }
            if ui.button("Clear imported reports").clicked() {
                self.baseline = None;
                self.candidate = None;
                self.comparison_output.clear();
            }
        });
        ui.add_space(8.0);

        if let (Some(baseline), Some(candidate)) = (&self.baseline, &self.candidate) {
            self.render_metadata_comparison(ui, baseline, candidate);
            let comparisons = comparable_metrics(baseline, candidate);
            if comparisons.is_empty() {
                Self::card(ui, |ui| {
                    ui.label("No metrics share compatible names, units, workload parameters and direction between these reports.");
                });
            }
            for comparison in &comparisons {
                ui.add_space(8.0);
                Self::render_metric_chart(ui, comparison);
            }
            ui.add_space(8.0);
            Self::card(ui, |ui| {
                ui.heading("Raw comparison output");
                if self.comparison_output.trim().is_empty() {
                    ui.label("Run the official comparison to show the CLI's exact validation and summary output.");
                } else {
                    ui.label(RichText::new(&self.comparison_output).monospace());
                }
            });
        } else {
            Self::card(ui, |ui| {
                ui.label("Import both JSON reports to render charts.");
                ui.label("Reports are validated for schema version, finite values, sample counts, and metric consistency by the comparison command.");
            });
        }
    }

    fn render_metadata_comparison(&self, ui: &mut egui::Ui, baseline: &BenchmarkReport, candidate: &BenchmarkReport) {
        Self::card(ui, |ui| {
            ui.heading("Comparison context");
            ui.columns(2, |columns| {
                columns[0].label(RichText::new(format!("Baseline  |  {}", baseline.kernel)).strong());
                columns[0].label(format!("{}  |  {} logical CPUs", baseline.architecture, baseline.logical_cpus));
                columns[0].label(&baseline.cpu);
                columns[1].label(RichText::new(format!("Candidate  |  {}", candidate.kernel)).strong());
                columns[1].label(format!("{}  |  {} logical CPUs", candidate.architecture, candidate.logical_cpus));
                columns[1].label(&candidate.cpu);
            });
            let memory_delta = if baseline.memory_kib > 0 {
                (candidate.memory_kib as f64 / baseline.memory_kib as f64 - 1.0) * 100.0
            } else { 0.0 };
            ui.label(format!("Reported memory delta: {memory_delta:+.4}%"));
            let other_mismatch = baseline.cpu != candidate.cpu
                || baseline.cpu_microarchitecture != candidate.cpu_microarchitecture
                || baseline.architecture != candidate.architecture
                || baseline.logical_cpus != candidate.logical_cpus
                || baseline.cpu_governors != candidate.cpu_governors;
            if other_mismatch {
                ui.label(RichText::new("Hardware/governor metadata differs. Treat this comparison as uncontrolled.").color(Color32::from_rgb(235, 191, 104)));
            } else if memory_delta.abs() < 0.1 {
                ui.label(RichText::new("CPU, architecture, logical CPU count and governor metadata match. The reported RAM difference is negligible.").color(Color32::from_rgb(125, 208, 161)));
            } else {
                ui.label(RichText::new("System memory metadata differs. Review whether the reports were collected under comparable conditions.").color(Color32::from_rgb(235, 191, 104)));
            }
        });
    }

    fn render_metric_chart(ui: &mut egui::Ui, comparison: &MetricComparison) {
        let old = &comparison.baseline;
        let new = &comparison.candidate;
        Self::card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading(&old.name);
                if let Some(change) = comparison.change_percent {
                    let color = if (old.higher_is_better && change >= 0.0) || (!old.higher_is_better && change <= 0.0) {
                        Color32::from_rgb(125, 208, 161)
                    } else {
                        Color32::from_rgb(236, 133, 127)
                    };
                    ui.label(RichText::new(format!("{change:+.2}%")).size(18.0).strong().color(color));
                } else {
                    ui.label(RichText::new("n/a").color(Color32::GRAY));
                }
            });
            ui.label(format!("{}  |  {}  |  higher is {}",
                old.workload,
                old.unit,
                if old.higher_is_better { "better" } else { "worse" }
            ));
            draw_median_bars(ui, old, new);
            ui.add_space(6.0);
            ui.label(RichText::new("Iteration samples").strong());
            draw_sample_lines(ui, old, new);
            ui.horizontal(|ui| {
                ui.colored_label(Color32::from_rgb(93, 164, 231), format!("Baseline: median {}  |  range {}-{}", format_value(old.median), format_value(old.min), format_value(old.max)));
                ui.colored_label(Color32::from_rgb(91, 202, 173), format!("Candidate: median {}  |  range {}-{}", format_value(new.median), format_value(new.min), format_value(new.max)));
            });
        });
    }

    fn tuning_page(&mut self, ui: &mut egui::Ui) {
        Self::page_header(ui, "System tuning", "Preview and apply workload-aware sysctl and I/O scheduler settings.");
        ui.horizontal(|ui| {
            egui::ComboBox::from_label("Workload profile")
                .selected_text(&self.selected_workload)
                .show_ui(ui, |ui| {
                    for profile in ["desktop", "laptop", "server", "gaming"] {
                        ui.selectable_value(&mut self.selected_workload, profile.into(), profile);
                    }
                });
            if ui.add_enabled(!self.running, egui::Button::new("Preview tuning").fill(Color32::from_rgb(35, 105, 135))).clicked() {
                self.launch(
                    vec!["tune".into(), "--profile".into(), self.selected_workload.clone(), "--dry-run".into()],
                    false,
                    OutputTarget::None,
                );
            }
            if ui.add_enabled(!self.running, egui::Button::new("Preview revert")).clicked() {
                self.launch(vec!["revert".into(), "--dry-run".into()], false, OutputTarget::None);
            }
        });
        ui.add_space(8.0);
        Self::card(ui, |ui| {
            ui.heading("Apply settings");
            ui.label("Applying writes the two optimizer-managed drop-in files under /etc, reloads sysctl and may reload udev rules.");
            ui.label(RichText::new("Administrator authorization is required. Review the preview before applying.").color(Color32::from_rgb(235, 191, 104)));
            if ui.add_enabled(!self.running, egui::Button::new("Apply tuning...").fill(Color32::from_rgb(35, 105, 135))).clicked() {
                self.request_privileged(PrivilegedAction::Apply);
            }
        });
        ui.add_space(8.0);
        Self::card(ui, |ui| {
            ui.heading("Revert optimizer settings");
            ui.label("Removes only the two configuration files owned by Kernel Optimizer and reloads the remaining system configuration. It does not guarantee restoration of runtime values changed by other services.");
            if ui.add_enabled(!self.running, egui::Button::new("Revert tuning...").fill(Color32::from_rgb(126, 67, 66))).clicked() {
                self.request_privileged(PrivilegedAction::Revert);
            }
        });
        if let Some(action) = self.pending_privileged {
            egui::Window::new("Confirm privileged operation")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ui.ctx(), |ui| {
                    match action {
                        PrivilegedAction::Apply => {
                            ui.label(format!("Apply the '{}' tuning profile?", self.selected_workload));
                            ui.label("This modifies system configuration files and reloads kernel settings.");
                        }
                        PrivilegedAction::Revert => {
                            ui.label("Remove Kernel Optimizer's managed tuning files?");
                            ui.label("This reloads sysctl and udev configuration.");
                        }
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            self.pending_privileged = None;
                        }
                        if ui.button(RichText::new("Authorize with polkit").strong()).clicked() {
                            self.confirm_privileged();
                        }
                    });
                });
        }
    }

    fn install_page(&mut self, ui: &mut egui::Ui) {
        Self::page_header(
            ui,
            "Arch package installation",
            "Build a pacman package from verified staging artifacts, then install it as a separate administrator-approved step.",
        );

        Self::card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("SUPPORTED").color(Color32::from_rgb(125, 208, 161)).strong());
                ui.heading("Arch Linux, Garuda and Arch-derived systems - x86_64");
            });
            ui.label("Uses makepkg to create a normal pacman package. The package includes the kernel image under /usr/lib/modules/<release>/vmlinuz and pkgbase so the detected pacman hook can generate initramfs images.");
            ui.label(RichText::new("The package builder detects either mkinitcpio with its active pacman hook or dracut with its active pacman hook (including Garuda's dracut hook). It refuses ambiguous, masked or incomplete hook setups. Build as your normal user; installation uses PolicyKit.").color(Color32::from_rgb(145, 164, 185)));
        });

        ui.add_space(8.0);
        Self::card(ui, |ui| {
            ui.heading("1. Build package");
            ui.label("Build profile");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.profile_path).desired_width(360.0));
                if ui.button("Browse...").clicked()
                    && let Some(path) = FileDialog::new().add_filter("TOML profile", &["toml"]).pick_file()
                {
                    self.profile_path = path.display().to_string();
                    self.load_profile();
                }
            });
            ui.horizontal(|ui| {
                ui.label("Kernel version");
                ui.add(egui::TextEdit::singleline(&mut self.finalize_version)
                    .hint_text("Optional; infer from staged modules")
                    .desired_width(260.0));
            });
            ui.label("Package output directory");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.package_output_dir).desired_width(360.0));
                if ui.button("Choose folder...").clicked()
                    && let Some(path) = FileDialog::new().pick_folder()
                {
                    self.package_output_dir = path.display().to_string();
                }
            });
            ui.add_space(4.0);
            if ui.add_enabled(!self.running, egui::Button::new("Build Arch package").fill(Color32::from_rgb(35, 105, 135))).clicked() {
                if let Err(error) = self.save_profile() {
                    self.log(format!("Cannot build package: {error:#}"));
                } else if self.package_output_dir.trim().is_empty() {
                    self.log("Choose a package output directory first.");
                } else {
                    let mut args = vec![
                        "arch-package".into(),
                        "--profile".into(), self.profile_path.clone(),
                        "--output-dir".into(), self.package_output_dir.clone(),
                    ];
                    if !self.finalize_version.trim().is_empty() {
                        args.extend(["--version".into(), self.finalize_version.trim().to_owned()]);
                    }
                    self.package_path.clear();
                    self.launch(args, false, OutputTarget::Package);
                    self.page = Page::Activity;
                }
            }
        });

        ui.add_space(8.0);
        Self::card(ui, |ui| {
            ui.heading("2. Install package");
            ui.label("Select a package built above, or choose an existing linux-optimizer package.");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.package_path).desired_width(500.0));
                if ui.button("Browse package...").clicked()
                    && let Some(path) = FileDialog::new()
                        .add_filter("Arch package", &["zst"])
                        .pick_file()
                {
                    self.package_path = path.display().to_string();
                }
            });
            let package_exists = !self.package_path.trim().is_empty() && Path::new(self.package_path.trim()).is_file();
            if !package_exists {
                ui.label(RichText::new("No existing package selected. Build a package first.").color(Color32::from_rgb(235, 191, 104)));
            }
            if ui.add_enabled(!self.running && package_exists, egui::Button::new("Install with administrator authorization").fill(Color32::from_rgb(35, 105, 135))).clicked() {
                self.request_privileged(PrivilegedAction::InstallPackage);
            }
        });

        ui.add_space(8.0);
        Self::card(ui, |ui| {
            ui.heading("Important boot notes");
            ui.label("The package installs /boot/vmlinuz-linux-optimizer. The selected pacman hook generates initramfs images using the host's active initramfs backend; mkinitcpio also receives a dedicated default/fallback preset.");
            ui.label(RichText::new("It does NOT edit GRUB, systemd-boot, EFI entries or Secure Boot keys. After installation, inspect /boot and add a bootloader entry manually. Keep your known-good kernel entry until you have successfully booted the new one.").color(Color32::from_rgb(235, 191, 104)));
            ui.label("The installer refuses forced module-signature configurations, enabled Secure Boot, an unmounted separate /boot filesystem and an existing unowned module directory.");
        });

        if self.pending_privileged == Some(PrivilegedAction::InstallPackage) {
            egui::Window::new("Confirm kernel package installation")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ui.ctx(), |ui| {
                    ui.label(format!("Install this package with pacman?\n{}", self.package_path));
                    ui.label("This modifies /boot and /usr/lib/modules and runs the detected initramfs pacman hook.");
                    ui.label("No bootloader configuration will be changed.");
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            self.pending_privileged = None;
                        }
                        if ui.button("Authorize with PolicyKit").clicked() {
                            self.confirm_privileged();
                        }
                    });
                });
        }
    }

    fn activity_page(&mut self, ui: &mut egui::Ui) {
        Self::page_header(ui, "Activity", "Command output, build progress and diagnostic messages.");
        ui.horizontal(|ui| {
            ui.label(if self.running { "Operation running" } else { "Idle" });
            ui.label(RichText::new(&self.status).color(if self.running { Color32::from_rgb(235, 191, 104) } else { Color32::from_rgb(125, 208, 161) }));
            if ui.button("Clear log").clicked() {
                self.logs.clear();
            }
            if self.last_exit_code == Some(0) {
                ui.label(RichText::new("Last command succeeded").color(Color32::from_rgb(125, 208, 161)));
            } else if self.last_exit_code.is_some() {
                ui.label(RichText::new("Last command failed").color(Color32::from_rgb(236, 133, 127)));
            }
        });
        ui.label(RichText::new(&self.current_command).small().monospace().color(Color32::GRAY));
        ui.add_space(6.0);
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for line in &self.logs {
                    ui.label(RichText::new(line).monospace().size(11.5));
                }
            });
    }

    fn launch_terminal_command(&mut self, args: Vec<String>) {
        let executable = match std::env::current_exe() {
            Ok(path) => path,
            Err(error) => {
                self.log(format!("Could not locate executable: {error}"));
                return;
            }
        };
        for terminal in ["x-terminal-emulator", "konsole", "xfce4-terminal", "kitty", "alacritty", "foot", "gnome-terminal"] {
            let mut command = Command::new(terminal);
            if terminal == "gnome-terminal" {
                command.arg("--");
            } else {
                command.arg("-e");
            }
            command.arg(&executable).args(&args);
            match command.spawn() {
                Ok(_) => {
                    self.log(format!("Started interactive command in {terminal}."));
                    return;
                }
                Err(_) => continue,
            }
        }
        self.log("No terminal emulator found. Install x-terminal-emulator or run the command from a terminal.");
    }

    fn draw_sidebar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("navigation")
            .resizable(false)
            .exact_size(218.0)
            .frame(egui::Frame::new().fill(Color32::from_rgb(12, 17, 27)).inner_margin(12.0))
            .show(ui, |ui| {
                ui.add_space(7.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("KO").size(19.0).color(Color32::from_rgb(93, 204, 224)).strong());
                    ui.vertical(|ui| {
                        ui.label(RichText::new("KERNEL").size(15.0).strong());
                        ui.label(RichText::new("OPTIMIZER").size(11.0).color(Color32::from_rgb(93, 204, 224)).strong());
                    });
                });
                ui.add_space(18.0);
                ui.label(RichText::new("WORKSPACE").size(10.0).color(Color32::GRAY).strong());
                for page in [Page::Overview, Page::Hardware, Page::Recommendations, Page::Sources] {
                    self.nav_button(ui, page);
                }
                ui.add_space(13.0);
                ui.label(RichText::new("KERNEL BUILD").size(10.0).color(Color32::GRAY).strong());
                for page in [Page::Profile, Page::Build, Page::Finalize, Page::Install] {
                    self.nav_button(ui, page);
                }
                ui.add_space(13.0);
                ui.label(RichText::new("PERFORMANCE").size(10.0).color(Color32::GRAY).strong());
                for page in [Page::Benchmarks, Page::Compare] {
                    self.nav_button(ui, page);
                }
                ui.add_space(13.0);
                ui.label(RichText::new("SYSTEM").size(10.0).color(Color32::GRAY).strong());
                for page in [Page::Tuning, Page::Activity] {
                    self.nav_button(ui, page);
                }
                ui.add_space(12.0);
                ui.separator();
                ui.label(RichText::new(format!("Running: {}", self.current_kernel)).small().color(Color32::from_rgb(145, 164, 188)));
                if self.running {
                    ui.label(RichText::new("Busy").color(Color32::from_rgb(235, 191, 104)));
                } else {
                    ui.label(RichText::new("Ready").color(Color32::from_rgb(125, 208, 161)));
                }
            });
    }
}

impl eframe::App for KernelOptimizerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_runner(&ctx);

        egui::Panel::top("topbar")
            .frame(egui::Frame::new().fill(Color32::from_rgb(17, 24, 37)).inner_margin(egui::Margin::symmetric(16, 10)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(self.page.title()).size(14.0).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.running {
                            ui.spinner();
                            ui.label(RichText::new("Working").color(Color32::from_rgb(235, 191, 104)));
                        } else {
                            ui.label(RichText::new("System ready").color(Color32::from_rgb(125, 208, 161)));
                        }
                        ui.separator();
                        ui.label(RichText::new(format!("Kernel: {}", self.current_kernel)).small().color(Color32::from_rgb(145, 164, 188)));
                    });
                });
            });

        self.draw_sidebar(ui);

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(Color32::from_rgb(15, 20, 31)).inner_margin(egui::Margin::same(18)))
            .show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    match self.page {
                        Page::Overview => self.overview_page(ui),
                        Page::Hardware => self.hardware_page(ui),
                        Page::Recommendations => self.recommendations_page(ui),
                        Page::Sources => self.sources_page(ui),
                        Page::Profile => self.profile_page(ui),
                        Page::Build => self.build_page(ui),
                        Page::Finalize => self.finalize_page(ui),
                        Page::Benchmarks => self.benchmarks_page(ui),
                        Page::Compare => self.compare_page(ui),
                        Page::Tuning => self.tuning_page(ui),
                        Page::Install => self.install_page(ui),
                        Page::Activity => self.activity_page(ui),
                    }
                });
            });
    }
}

fn forward_lines<R: Read + Send + 'static>(reader: R, stderr: bool, sender: Sender<RunnerMessage>) {
    for line in BufReader::new(reader).lines() {
        match line {
            Ok(text) => {
                if sender.send(RunnerMessage::Line { stderr, text }).is_err() {
                    break;
                }
            }
            Err(error) => {
                let _ = sender.send(RunnerMessage::Line {
                    stderr: true,
                    text: format!("Error reading command output: {error}"),
                });
                break;
            }
        }
    }
}

fn load_benchmark_report(path: &Path) -> Result<BenchmarkReport> {
    let bytes = fs::read(path).with_context(|| format!("reading benchmark report {}", path.display()))?;
    let report: BenchmarkReport = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing benchmark report {}", path.display()))?;
    validate_gui_report(&report)?;
    Ok(report)
}

fn validate_gui_report(report: &BenchmarkReport) -> Result<()> {
    if report.schema_version != 2 {
        anyhow::bail!("unsupported schema version {}; expected 2", report.schema_version);
    }
    if report.results.is_empty() {
        anyhow::bail!("benchmark report has no metrics");
    }
    for metric in &report.results {
        if metric.name.trim().is_empty()
            || metric.samples.is_empty()
            || metric.iterations != metric.samples.len()
            || !metric.median.is_finite()
            || !metric.min.is_finite()
            || !metric.max.is_finite()
            || metric.samples.iter().any(|value| !value.is_finite())
            || metric.min > metric.median
            || metric.median > metric.max
        {
            anyhow::bail!("invalid sample data for metric '{}'", metric.name);
        }
    }
    Ok(())
}

fn comparable_metrics(baseline: &BenchmarkReport, candidate: &BenchmarkReport) -> Vec<MetricComparison> {
    baseline.results.iter().filter_map(|old| {
        let new = candidate.results.iter().find(|new| {
            new.name == old.name
                && new.unit == old.unit
                && new.workload == old.workload
                && new.higher_is_better == old.higher_is_better
        })?;
        let change_percent = if old.median.abs() < f64::EPSILON {
            None
        } else {
            Some((new.median / old.median - 1.0) * 100.0)
        };
        Some(MetricComparison {
            baseline: old.clone(),
            candidate: new.clone(),
            change_percent,
        })
    }).collect()
}

fn draw_median_bars(ui: &mut egui::Ui, baseline: &BenchmarkResult, candidate: &BenchmarkResult) {
    let desired = Vec2::new(ui.available_width().max(280.0), 168.0);
    let (response, painter) = ui.allocate_painter(desired, Sense::hover());
    let rect = response.rect;
    let plot = rect.shrink2(Vec2::new(28.0, 22.0));
    let max_value = baseline.median.max(candidate.median).max(f64::EPSILON) * 1.18;
    let base_x = plot.left() + plot.width() * 0.31;
    let candidate_x = plot.left() + plot.width() * 0.69;
    let bar_width = (plot.width() * 0.19).min(112.0);
    let bottom = plot.bottom() - 19.0;
    let height = plot.height() - 34.0;
    let baseline_height = (baseline.median / max_value * height as f64) as f32;
    let candidate_height = (candidate.median / max_value * height as f64) as f32;
    painter.line_segment(
        [Pos2::new(plot.left(), bottom), Pos2::new(plot.right(), bottom)],
        Stroke::new(1.0, Color32::from_rgb(75, 89, 108)),
    );
    let old_bar = Rect::from_min_max(
        Pos2::new(base_x - bar_width * 0.5, bottom - baseline_height),
        Pos2::new(base_x + bar_width * 0.5, bottom),
    );
    let new_bar = Rect::from_min_max(
        Pos2::new(candidate_x - bar_width * 0.5, bottom - candidate_height),
        Pos2::new(candidate_x + bar_width * 0.5, bottom),
    );
    painter.rect_filled(old_bar, 5.0, Color32::from_rgb(76, 139, 201));
    painter.rect_filled(new_bar, 5.0, Color32::from_rgb(62, 177, 145));
    painter.text(
        Pos2::new(base_x, (bottom - baseline_height - 9.0).max(plot.top())),
        Align2::CENTER_BOTTOM,
        format_value(baseline.median),
        FontId::proportional(11.0),
        Color32::WHITE,
    );
    painter.text(
        Pos2::new(candidate_x, (bottom - candidate_height - 9.0).max(plot.top())),
        Align2::CENTER_BOTTOM,
        format_value(candidate.median),
        FontId::proportional(11.0),
        Color32::WHITE,
    );
    painter.text(
        Pos2::new(base_x, bottom + 6.0),
        Align2::CENTER_TOP,
        "Baseline",
        FontId::proportional(11.0),
        Color32::from_rgb(166, 185, 207),
    );
    painter.text(
        Pos2::new(candidate_x, bottom + 6.0),
        Align2::CENTER_TOP,
        "Candidate",
        FontId::proportional(11.0),
        Color32::from_rgb(166, 185, 207),
    );
}

fn draw_sample_lines(ui: &mut egui::Ui, baseline: &BenchmarkResult, candidate: &BenchmarkResult) {
    let desired = Vec2::new(ui.available_width().max(280.0), 176.0);
    let (response, painter) = ui.allocate_painter(desired, Sense::hover());
    let rect = response.rect;
    let plot = rect.shrink2(Vec2::new(34.0, 16.0));
    let low = baseline.samples.iter().chain(candidate.samples.iter()).copied().fold(f64::INFINITY, f64::min);
    let high = baseline.samples.iter().chain(candidate.samples.iter()).copied().fold(f64::NEG_INFINITY, f64::max);
    let span = (high - low).max(high.abs() * 0.001).max(1.0e-9);
    for grid in 0..=4 {
        let y = plot.bottom() - plot.height() * grid as f32 / 4.0;
        painter.line_segment(
            [Pos2::new(plot.left(), y), Pos2::new(plot.right(), y)],
            Stroke::new(1.0, Color32::from_rgb(43, 56, 75)),
        );
    }
    painter.text(
        Pos2::new(plot.left(), plot.top()),
        Align2::LEFT_TOP,
        format_value(high),
        FontId::proportional(10.0),
        Color32::GRAY,
    );
    painter.text(
        Pos2::new(plot.left(), plot.bottom()),
        Align2::LEFT_BOTTOM,
        format_value(low),
        FontId::proportional(10.0),
        Color32::GRAY,
    );

    let make_points = |samples: &[f64]| -> Vec<Pos2> {
        let denominator = samples.len().saturating_sub(1).max(1) as f32;
        samples.iter().enumerate().map(|(index, value)| {
            let x = plot.left() + plot.width() * index as f32 / denominator;
            let y = plot.bottom() - (((*value - low) / span) as f32 * plot.height());
            Pos2::new(x, y)
        }).collect()
    };
    for (samples, color) in [
        (&baseline.samples, Color32::from_rgb(76, 139, 201)),
        (&candidate.samples, Color32::from_rgb(62, 177, 145)),
    ] {
        let points = make_points(samples);
        if points.len() > 1 {
            painter.add(egui::Shape::line(points.clone(), Stroke::new(2.2, color)));
        }
        for point in points {
            painter.circle_filled(point, 3.1, color);
        }
    }
}

fn format_value(value: f64) -> String {
    if value.abs() >= 1_000_000.0 {
        format!("{:.2}M", value / 1_000_000.0)
    } else if value.abs() >= 10_000.0 {
        format!("{:.1}k", value / 1_000.0)
    } else {
        format!("{value:.2}")
    }
}

pub fn run() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Kernel Optimizer")
            .with_inner_size([1420.0, 940.0])
            .with_min_inner_size([1080.0, 720.0]),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "Kernel Optimizer",
        options,
        Box::new(|cc| Ok(Box::new(KernelOptimizerApp::new(cc)))),
    )
    .map_err(|error| anyhow!("launching desktop GUI: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metric(name: &str, median: f64, unit: &str, workload: &str) -> BenchmarkResult {
        BenchmarkResult {
            name: name.into(),
            workload: workload.into(),
            unit: unit.into(),
            higher_is_better: true,
            iterations: 3,
            samples: vec![median * 0.99, median, median * 1.01],
            median,
            min: median * 0.99,
            max: median * 1.01,
        }
    }

    fn report(kernel: &str, results: Vec<BenchmarkResult>) -> BenchmarkReport {
        BenchmarkReport {
            schema_version: 2,
            created_unix: 0,
            kernel: kernel.into(),
            architecture: "x86_64".into(),
            cpu: "test CPU".into(),
            cpu_microarchitecture: "test architecture".into(),
            logical_cpus: 8,
            memory_kib: 1024,
            cpu_governors: vec!["performance".into()],
            profile: "test".into(),
            results,
        }
    }

    #[test]
    fn comparison_uses_only_matching_metric_definitions() {
        let baseline = report("baseline", vec![
            metric("cpu", 100.0, "iterations/s", "5 rounds"),
            metric("memory", 100.0, "MiB/s", "64 MiB"),
        ]);
        let candidate = report("candidate", vec![
            metric("cpu", 103.0, "iterations/s", "5 rounds"),
            metric("memory", 110.0, "MiB/s", "128 MiB"),
        ]);
        let comparisons = comparable_metrics(&baseline, &candidate);
        assert_eq!(comparisons.len(), 1);
        assert_eq!(comparisons[0].baseline.name, "cpu");
        assert!((comparisons[0].change_percent.unwrap() - 3.0).abs() < 1.0e-9);
    }

    #[test]
    fn report_validation_rejects_bad_schema_and_non_finite_values() {
        let mut valid = report("test", vec![metric("cpu", 100.0, "iterations/s", "work")]);
        assert!(validate_gui_report(&valid).is_ok());
        valid.schema_version = 1;
        assert!(validate_gui_report(&valid).is_err());

        let mut bad = report("test", vec![metric("cpu", 100.0, "iterations/s", "work")]);
        bad.results[0].samples[0] = f64::NAN;
        assert!(validate_gui_report(&bad).is_err());
    }

    #[test]
    fn zero_baseline_has_no_infinite_percentage() {
        let baseline = report("old", vec![metric("cpu", 0.0, "unit", "work")]);
        let candidate = report("new", vec![metric("cpu", 10.0, "unit", "work")]);
        let comparison = comparable_metrics(&baseline, &candidate);
        assert_eq!(comparison.len(), 1);
        assert_eq!(comparison[0].change_percent, None);
    }
}
