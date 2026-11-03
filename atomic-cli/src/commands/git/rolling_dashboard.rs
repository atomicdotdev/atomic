//! Live dashboard for the experimental rolling Git importer.

use crossterm::{
    cursor::{Hide, Show},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Gauge, Paragraph, Sparkline, Wrap},
    Frame, Terminal,
};
use std::collections::VecDeque;
use std::io::{IsTerminal, Write};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Clone)]
pub(super) struct Progress {
    pub phase: &'static str,
    pub total: usize,
    pub checked: usize,
    pub loaded: usize,
    pub published: usize,
    pub queued: usize,
    pub in_flight: usize,
    pub window_size: usize,
    pub reserved_bytes: usize,
    pub window_bytes: usize,
}

#[derive(Default)]
struct Resources {
    rss: Option<u64>,
    peak_rss: Option<u64>,
    cpu_seconds: Option<f64>,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn resources() -> Resources {
    let rss = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| text.trim().parse::<u64>().ok())
        .and_then(|kib| kib.checked_mul(1024));
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage initializes the output on success.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return Resources {
            rss,
            ..Resources::default()
        };
    }
    // SAFETY: getrusage succeeded above.
    let usage = unsafe { usage.assume_init() };
    #[cfg(target_os = "macos")]
    let peak_rss = usage.ru_maxrss as u64;
    #[cfg(target_os = "linux")]
    let peak_rss = (usage.ru_maxrss as u64).saturating_mul(1024);
    let seconds = |time: libc::timeval| time.tv_sec as f64 + time.tv_usec as f64 / 1_000_000.0;
    Resources {
        rss,
        peak_rss: Some(peak_rss),
        cpu_seconds: Some(seconds(usage.ru_utime) + seconds(usage.ru_stime)),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn resources() -> Resources {
    Resources::default()
}

fn memory(bytes: Option<u64>) -> String {
    bytes
        .map(|value| format!("{:.1} MiB", value as f64 / 1_048_576.0))
        .unwrap_or_else(|| "unavailable".into())
}

const HISTORY_SAMPLES: usize = 120;

#[derive(Default)]
struct Metrics {
    rss_history: VecDeque<u64>,
    cpu_history: VecDeque<u64>,
    max_cpu: f64,
}

impl Metrics {
    fn observe(&mut self, sample: &Resources, cpu: Option<f64>) {
        if let Some(rss) = sample.rss {
            push_history(&mut self.rss_history, rss / 1024);
        }
        if let Some(cpu) = cpu {
            self.max_cpu = self.max_cpu.max(cpu);
            push_history(&mut self.cpu_history, cpu.round() as u64);
        }
    }
}

fn push_history(history: &mut VecDeque<u64>, value: u64) {
    if history.len() == HISTORY_SAMPLES {
        history.pop_front();
    }
    history.push_back(value);
}

fn cpu_label(cpu: Option<f64>) -> String {
    cpu.map(|value| format!("{value:.1}% (100% = one core)"))
        .unwrap_or_else(|| "unavailable / sampling".into())
}

fn rate(progress: &Progress, elapsed: Duration) -> f64 {
    if elapsed.is_zero() {
        0.0
    } else {
        progress.published as f64 / elapsed.as_secs_f64()
    }
}

fn eta(progress: &Progress, elapsed: Duration) -> String {
    if progress.phase == "complete" {
        return "complete".into();
    }
    let remaining = progress.total.saturating_sub(progress.published);
    let rate = rate(progress, elapsed);
    if remaining == 0 {
        "final proof separate".into()
    } else if rate > 0.0 {
        format!("~{:.0}s + final proof", remaining as f64 / rate)
    } else {
        "estimating...".into()
    }
}

fn plain(progress: &Progress, sample: &Resources, cpu: Option<f64>, elapsed: Duration) -> String {
    format!("Rolling import | {} | {:.1}s | staged {}/{} | {:.1} commits/s | ETA {}\nwindow {}/{} | checked {} | loaded {} | in flight {} | blob reservation {} / {}\nprocess RSS {} | peak {} | CPU {}\n",
        progress.phase, elapsed.as_secs_f64(), progress.published, progress.total, rate(progress, elapsed), eta(progress, elapsed),
        progress.queued, progress.window_size, progress.checked, progress.loaded, progress.in_flight,
        memory(Some(progress.reserved_bytes as u64)), memory(Some(progress.window_bytes as u64)),
        memory(sample.rss), memory(sample.peak_rss), cpu_label(cpu))
}

fn ratio(value: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        (value as f64 / total as f64).clamp(0.0, 1.0)
    }
}

fn panel(title: &str) -> Block<'_> {
    Block::bordered()
        .title(title)
        .border_style(Style::default().fg(Color::DarkGray))
}

fn draw(
    frame: &mut Frame,
    progress: &Progress,
    sample: &Resources,
    cpu: Option<f64>,
    elapsed: Duration,
    metrics: &Metrics,
) {
    let area = frame.area();
    if area.width < 76 || area.height < 22 {
        let content = format!("{}\nBlob budget is not total process memory.\nStaging is private; final proof is required.", plain(progress, sample, cpu, elapsed));
        frame.render_widget(
            Paragraph::new(content).wrap(Wrap { trim: true }).block(
                panel(" Atomic import ").title_style(Style::default().fg(Color::Cyan).bold()),
            ),
            area,
        );
        return;
    }
    let rows = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(3),
        Constraint::Length(6),
        Constraint::Min(7),
        Constraint::Length(2),
    ])
    .split(area);
    let status_color = match progress.phase {
        "complete" => Color::Green,
        "stopped / failed" => Color::Red,
        "final proof / promote" => Color::Yellow,
        _ => Color::Cyan,
    };
    let header = vec![
        Line::from(vec![
            Span::styled(
                format!(" {} ", progress.phase),
                Style::default().fg(status_color).bold(),
            ),
            Span::raw(format!("  {:.1}s elapsed", elapsed.as_secs_f64())),
        ]),
        Line::from(format!(
            " {:.1} commits/s average   Staging ETA {}",
            rate(progress, elapsed),
            eta(progress, elapsed)
        )),
    ];
    frame.render_widget(
        Paragraph::new(header).block(panel(" ATOMIC / ROLLING IMPORT ")),
        rows[0],
    );
    frame.render_widget(
        Gauge::default()
            .block(panel(" Privately staged history "))
            .gauge_style(Style::default().fg(Color::Cyan))
            .ratio(ratio(progress.published, progress.total))
            .label(format!(
                "{} / {} commits",
                progress.published, progress.total
            )),
        rows[1],
    );
    let pipeline = Layout::vertical([Constraint::Length(3), Constraint::Length(3)]).split(rows[2]);
    let gauges = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(pipeline[0]);
    frame.render_widget(
        Gauge::default()
            .block(panel(" Rolling queue "))
            .gauge_style(Style::default().fg(Color::Blue))
            .ratio(ratio(progress.queued, progress.window_size))
            .label(format!(
                "{} / {} commits",
                progress.queued, progress.window_size
            )),
        gauges[0],
    );
    frame.render_widget(
        Gauge::default()
            .block(panel(" Queued blob reservation (not RSS) "))
            .gauge_style(Style::default().fg(Color::Magenta))
            .ratio(ratio(progress.reserved_bytes, progress.window_bytes))
            .label(format!(
                "{} / {}",
                memory(Some(progress.reserved_bytes as u64)),
                memory(Some(progress.window_bytes as u64))
            )),
        gauges[1],
    );
    frame.render_widget(
        Paragraph::new(format!(
            "Checked {}   Loaded {}   In flight {}   Remaining to load {}",
            progress.checked,
            progress.loaded,
            progress.in_flight,
            progress.total.saturating_sub(progress.loaded)
        ))
        .block(panel(" Pipeline ")),
        pipeline[1],
    );
    let resources =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(rows[3]);
    for (index, title, lines, history, color) in [
        (
            0,
            " Process memory ",
            vec![
                format!("RSS    {}", memory(sample.rss)),
                format!("Peak   {} (process lifetime)", memory(sample.peak_rss)),
            ],
            &metrics.rss_history,
            Color::Magenta,
        ),
        (
            1,
            " Process CPU ",
            vec![
                cpu_label(cpu),
                if metrics.cpu_history.is_empty() {
                    "Observed peak unavailable".into()
                } else {
                    format!("Observed peak {:.1}%", metrics.max_cpu)
                },
            ],
            &metrics.cpu_history,
            Color::Green,
        ),
    ] {
        frame.render_widget(panel(title), resources[index]);
        let inner = panel(title).inner(resources[index]);
        let sections = Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).split(inner);
        frame.render_widget(
            Paragraph::new(lines.join("\n")).wrap(Wrap { trim: true }),
            sections[0],
        );
        let data: Vec<_> = history.iter().copied().collect();
        let visible = data.len().saturating_sub(usize::from(sections[1].width));
        frame.render_widget(
            Sparkline::default()
                .data(&data[visible..])
                .style(Style::default().fg(color)),
            sections[1],
        );
    }
    frame.render_widget(Paragraph::new("Blob reservation != total memory. CPU is process-wide, not machine-wide.\nHistory: up to 60s, width-limited. Staging ETA excludes final proof.").style(Style::default().fg(Color::DarkGray)), rows[4]);
}

struct Screen {
    terminal: Terminal<CrosstermBackend<std::io::Stderr>>,
}

impl Screen {
    fn enter() -> std::io::Result<Self> {
        let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stderr()))?;
        // No raw mode: normal Ctrl-C behavior remains owned by the CLI.
        if let Err(error) = execute!(terminal.backend_mut(), EnterAlternateScreen, Hide) {
            let _ = execute!(terminal.backend_mut(), Show, LeaveAlternateScreen);
            return Err(error);
        }
        Ok(Self { terminal })
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = execute!(self.terminal.backend_mut(), Show, LeaveAlternateScreen);
    }
}

pub(super) struct Dashboard {
    state: Arc<Mutex<Progress>>,
    stop: Option<mpsc::Sender<()>>,
    worker: Option<JoinHandle<()>>,
    complete: bool,
}

impl Dashboard {
    pub fn start(window_size: usize, window_bytes: usize, enabled: bool) -> Self {
        let enabled = enabled && !crate::output::live::is_active();
        let state = Arc::new(Mutex::new(Progress {
            phase: "preflight",
            total: 0,
            checked: 0,
            loaded: 0,
            published: 0,
            queued: 0,
            in_flight: 0,
            window_size,
            reserved_bytes: 0,
            window_bytes,
        }));
        let interactive = std::io::stderr().is_terminal()
            && std::env::var_os("TERM").is_some_and(|term| term != "dumb");
        let (stop, worker) = if enabled {
            let (tx, rx) = mpsc::channel();
            let shared = Arc::clone(&state);
            let handle = thread::spawn(move || {
                let mut screen = interactive.then(Screen::enter).and_then(Result::ok);
                let mut metrics = Metrics::default();
                let start = Instant::now();
                let mut previous: Option<(Instant, f64)> = None;
                let mut stopping = false;
                loop {
                    let now = Instant::now();
                    let sample = resources();
                    let cpu =
                        previous
                            .zip(sample.cpu_seconds)
                            .and_then(|((time, prior), current)| {
                                let delta = now.duration_since(time).as_secs_f64();
                                (delta > 0.0)
                                    .then_some(((current - prior).max(0.0) / delta) * 100.0)
                            });
                    if let Some(current) = sample.cpu_seconds {
                        previous = Some((now, current));
                    }
                    let progress = shared.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    metrics.observe(&sample, cpu);
                    if let Some(active) = &mut screen {
                        if active
                            .terminal
                            .draw(|frame| {
                                draw(frame, &progress, &sample, cpu, start.elapsed(), &metrics)
                            })
                            .is_err()
                        {
                            // A broken terminal must not fail the import.
                            screen = None;
                        }
                    } else {
                        let text = plain(&progress, &sample, cpu, start.elapsed());
                        let mut stderr = std::io::stderr().lock();
                        let _ = stderr.write_all(text.as_bytes());
                        let _ = stderr.flush();
                    }
                    if stopping {
                        break;
                    }
                    stopping = match rx.recv_timeout(Duration::from_millis(500)) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => true,
                        Err(mpsc::RecvTimeoutError::Timeout) => false,
                    };
                }
                drop(screen);
                if interactive {
                    let progress = shared.lock().unwrap_or_else(|e| e.into_inner());
                    eprintln!(
                        "Rolling import {}: {}/{} staged privately",
                        progress.phase, progress.published, progress.total
                    );
                }
            });
            (Some(tx), Some(handle))
        } else {
            (None, None)
        };
        Self {
            state,
            stop,
            worker,
            complete: false,
        }
    }

    pub fn enabled(&self) -> bool {
        self.stop.is_some()
    }

    pub fn update(&self, f: impl FnOnce(&mut Progress)) {
        if self.enabled() {
            f(&mut self.state.lock().unwrap_or_else(|e| e.into_inner()));
        }
    }

    pub fn finish(&mut self) {
        self.complete = true;
        self.update(|state| state.phase = "complete");
    }
}

impl Drop for Dashboard {
    fn drop(&mut self) {
        if !self.complete {
            self.update(|state| state.phase = "stopped / failed");
        }
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_labels_distinct_resources_and_bounds_bars() {
        let progress = Progress {
            phase: "publishing",
            total: 100,
            checked: 100,
            loaded: 50,
            published: 25,
            queued: 20,
            in_flight: 5,
            window_size: 40,
            reserved_bytes: 2 * 1_048_576,
            window_bytes: 32 * 1_048_576,
        };
        let frame = plain(
            &progress,
            &Resources {
                rss: Some(100 * 1_048_576),
                peak_rss: Some(200 * 1_048_576),
                cpu_seconds: Some(2.0),
            },
            Some(150.0),
            Duration::from_secs(5),
        );
        assert!(frame.contains("process RSS"));
        assert!(frame.contains("100.0 MiB"));
        assert!(frame.contains("200.0 MiB"));
        assert!(frame.contains("150.0% (100% = one core)"));
        assert!(frame.contains("blob reservation"));
        assert_eq!(ratio(999, 1), 1.0);
        assert_eq!(ratio(0, 0), 0.0);

        for (width, height) in [(120, 30), (80, 24), (76, 22), (60, 20), (30, 12), (1, 1)] {
            let backend = ratatui::backend::TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| {
                    draw(
                        frame,
                        &progress,
                        &Resources::default(),
                        None,
                        Duration::from_secs(5),
                        &Metrics::default(),
                    )
                })
                .unwrap();
            let content: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            if width >= 60 {
                assert!(content.contains("import") || content.contains("IMPORT"));
                assert!(content.contains("process") || content.contains("Process"));
            }
        }
    }

    #[test]
    fn missing_resources_are_explicit() {
        assert_eq!(memory(None), "unavailable");
    }

    #[test]
    fn staging_eta_is_not_a_final_verification_promise() {
        let mut progress = Progress {
            phase: "publishing batch",
            total: 100,
            checked: 100,
            loaded: 80,
            published: 25,
            queued: 50,
            in_flight: 5,
            window_size: 500,
            reserved_bytes: 0,
            window_bytes: 32 * 1_048_576,
        };
        assert_eq!(rate(&progress, Duration::from_secs(5)), 5.0);
        assert_eq!(eta(&progress, Duration::from_secs(5)), "~15s + final proof");
        progress.published = 0;
        assert_eq!(eta(&progress, Duration::ZERO), "estimating...");
        progress.published = 100;
        assert_eq!(
            eta(&progress, Duration::from_secs(5)),
            "final proof separate"
        );
        progress.phase = "complete";
        assert_eq!(eta(&progress, Duration::from_secs(5)), "complete");
    }

    #[test]
    fn histories_are_bounded_and_missing_samples_are_not_zeroes() {
        let mut metrics = Metrics::default();
        for _ in 0..200 {
            metrics.observe(
                &Resources {
                    rss: Some(1024),
                    ..Resources::default()
                },
                Some(150.0),
            );
        }
        metrics.observe(&Resources::default(), None);
        assert_eq!(metrics.rss_history.len(), HISTORY_SAMPLES);
        assert_eq!(metrics.cpu_history.len(), HISTORY_SAMPLES);
        assert_eq!(metrics.max_cpu, 150.0);
    }
}
