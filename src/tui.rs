use crate::{
    hardware::HardwareProfile,
    recommendations::{self, RecommendationReport},
    tuning::Workload,
};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Hardware,
    Recommendations,
}

fn hardware_lines(hw: &HardwareProfile) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Kernel  ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw(hw.kernel_release.clone()),
        ]),
        Line::from(vec![
            Span::styled("Architecture  ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw(hw.architecture.clone()),
        ]),
        Line::from(vec![
            Span::styled("CPU  ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw(format!("{} — {}", hw.cpu.model, hw.cpu.microarchitecture)),
        ]),
        Line::from(format!(
            "Topology  {} logical CPUs | {} physical cores | {} sockets",
            hw.cpu.logical_cpus,
            hw.cpu.physical_cores.map_or_else(|| "unknown".into(), |value| value.to_string()),
            hw.cpu.sockets.map_or_else(|| "unknown".into(), |value| value.to_string()),
        )),
        Line::from(format!("Memory  {:.1} GiB", hw.memory_kib as f64 / 1024.0 / 1024.0)),
        Line::from(format!("Virtualization  {}", hw.virtualization)),
        Line::from(format!("CPU flags  {}", hw.cpu.flags.join(" "))),
        Line::from(""),
        Line::from(Span::styled("Storage", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))),
    ];

    if hw.block_devices.is_empty() {
        lines.push(Line::from("  No block devices detected (lsblk may be unavailable)."));
    } else {
        for device in &hw.block_devices {
            lines.push(Line::from(format!(
                "  {}  {}  {}  {}  {}",
                device.name, device.media_type, device.size, device.model, device.scheduler
            )));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("Graphics controllers", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))));
    if hw.gpus.is_empty() {
        lines.push(Line::from("  No PCI graphics controllers detected."));
    } else {
        lines.extend(hw.gpus.iter().map(|item| Line::from(format!("  {item}"))));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("Network controllers", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))));
    if hw.network_controllers.is_empty() {
        lines.push(Line::from("  No PCI network controllers detected."));
    } else {
        lines.extend(hw.network_controllers.iter().map(|item| Line::from(format!("  {item}"))));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("CPU frequency policies", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))));
    if hw.cpu_frequency_policies.is_empty() {
        lines.push(Line::from("  cpufreq policy interface not exposed."));
    } else {
        for policy in &hw.cpu_frequency_policies {
            lines.push(Line::from(format!(
                "  {} CPUs [{}] governor={} EPP={} range={:?}-{:?} MHz current={:?} MHz",
                policy.name,
                policy.affected_cpus,
                policy.governor,
                policy.energy_performance_preference.as_deref().unwrap_or("n/a"),
                policy.min_frequency_mhz,
                policy.max_frequency_mhz,
                policy.current_frequency_mhz
            )));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(format!("Kernel command line: {}", hw.current_cmdline)));
    lines
}

fn recommendation_lines(report: &RecommendationReport) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(format!("Workload profile: {}", report.profile)),
        Line::from("These are read-only suggestions. Nothing is applied from this screen."),
        Line::from(""),
    ];
    for item in &report.recommendations {
        lines.push(Line::from(vec![
            Span::styled(
                format!("[{}] ", item.category.to_uppercase()),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::styled(item.component.clone(), Style::default().add_modifier(Modifier::BOLD)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("Current: ", Style::default().fg(Color::Cyan)),
            Span::raw(item.current.clone()),
        ]));
        lines.push(Line::from(vec![
            Span::styled("Suggestion: ", Style::default().fg(Color::Green)),
            Span::raw(item.suggestion.clone()),
        ]));
        lines.push(Line::from(format!("Why: {}", item.rationale)));
        lines.push(Line::from(format!("Trade-off: {}", item.tradeoff)));
        lines.push(Line::from(format!("Action: {}", item.action)));
        lines.push(Line::from(""));
    }
    lines
}

fn draw(frame: &mut Frame<'_>, page: Page, scroll: u16, hw: &HardwareProfile, report: &RecommendationReport) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .split(frame.area());

    let hardware_style = if page == Page::Hardware {
        Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    let recommendation_style = if page == Page::Recommendations {
        Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    let header = Paragraph::new(Line::from(vec![
        Span::styled(" KERNEL OPTIMIZER ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(" [1] Hardware ", hardware_style),
        Span::styled(" [2] Recommendations ", recommendation_style),
    ]))
    .block(Block::default().borders(Borders::ALL).title(" System overview "));
    frame.render_widget(header, areas[0]);

    let title = match page {
        Page::Hardware => " Detected hardware ",
        Page::Recommendations => " Recommendations (read-only) ",
    };
    let lines = match page {
        Page::Hardware => hardware_lines(hw),
        Page::Recommendations => recommendation_lines(report),
    };
    let body = Paragraph::new(Text::from(lines))
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(body, areas[1]);

    let footer = Paragraph::new(
        " Tab / 1 / 2: switch page    ↑ / ↓: scroll    PgUp / PgDn: page    Home: top    q / Esc: quit ",
    )
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, areas[2]);
}

fn event_loop(
    mut terminal: DefaultTerminal,
    hw: HardwareProfile,
    report: RecommendationReport,
) -> Result<()> {
    let mut page = Page::Hardware;
    let mut scroll = 0_u16;
    loop {
        terminal.draw(|frame| draw(frame, page, scroll, &hw, &report))?;
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => break,
            KeyCode::Char('1') => {
                page = Page::Hardware;
                scroll = 0;
            }
            KeyCode::Char('2') | KeyCode::Tab => {
                page = Page::Recommendations;
                scroll = 0;
            }
            KeyCode::Up => scroll = scroll.saturating_sub(1),
            KeyCode::Down => scroll = scroll.saturating_add(1),
            KeyCode::PageUp => scroll = scroll.saturating_sub(10),
            KeyCode::PageDown => scroll = scroll.saturating_add(10),
            KeyCode::Home => scroll = 0,
            _ => {}
        }
    }
    Ok(())
}

pub fn run(profile: Workload) -> Result<()> {
    let hw = crate::hardware::detect();
    let report = recommendations::generate_from_hardware(hw.clone(), profile);
    let terminal = ratatui::init();
    let result = event_loop(terminal, hw, report);
    ratatui::restore();
    result
}
