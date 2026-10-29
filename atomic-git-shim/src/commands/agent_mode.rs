//! Command-scoped dancing ASCII visual. This never observes or mutates repository state.

use std::io::IsTerminal;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossterm::{
    cursor::{Hide, Show},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    widgets::{Block, Paragraph, Wrap},
    Frame, Terminal,
};

use crate::output::live::{self, LiveOutput};

const ART: &str = include_str!("ascii_dancer.txt");

fn draw(frame: &mut Frame, elapsed: Duration, status: &str) {
    let area = frame.area();
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(3),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(format!(
            "Agent mode / importing   {:.1}s",
            elapsed.as_secs_f64()
        ))
        .centered()
        .block(
            Block::bordered()
                .title(" ATOMIC / AGENT MODE ")
                .border_style(Style::default().fg(Color::Cyan)),
        ),
        rows[0],
    );
    dance(frame, rows[1], elapsed);
    frame.render_widget(
        Paragraph::new(if status.is_empty() {
            "Import is running. Keep dancing..."
        } else {
            status
        })
        .wrap(Wrap { trim: true })
        .style(Style::default().fg(Color::Gray))
        .block(
            Block::bordered()
                .title(" Import status ")
                .border_style(Style::default().fg(Color::DarkGray)),
        ),
        rows[2],
    );
}

fn dance(frame: &mut Frame, area: Rect, elapsed: Duration) {
    if area.width < 4 || area.height < 3 {
        return;
    }
    let lines: Vec<_> = ART.lines().collect();
    let source_width = lines.iter().map(|line| line.len()).max().unwrap_or(1);
    let scale = ((area.width.saturating_sub(6)) as f64 / source_width as f64)
        .min(area.height.saturating_sub(4) as f64 / lines.len() as f64)
        .clamp(0.01, 1.0);
    let width = (source_width as f64 * scale).max(1.0) as usize;
    let height = (lines.len() as f64 * scale).max(1.0) as usize;
    let t = elapsed.as_secs_f64() * std::f64::consts::TAU / 2.4;
    let origin_x = i32::from(area.x) + (i32::from(area.width) - width as i32) / 2;
    let origin_y = i32::from(area.y) + (i32::from(area.height) - height as i32) / 2;
    for y in 0..height {
        let source_y = ((y as f64 / scale) as usize).min(lines.len() - 1);
        let line = lines[source_y].as_bytes();
        for x in 0..width {
            let source_x = (x as f64 / scale) as usize;
            let ch = char::from(*line.get(source_x).unwrap_or(&b' '));
            if ch == ' ' {
                continue;
            }
            let nx = x as f64 / width as f64;
            let ny = y as f64 / height as f64;
            let leg = if nx < 0.5 { -1.0 } else { 1.0 };
            let feet = ((ny - 0.68) / 0.25).clamp(0.0, 1.0);
            let hand = (-((nx - 0.16) / 0.18).powi(2) - ((ny - 0.62) / 0.15).powi(2)).exp()
                - (-((nx - 0.85) / 0.18).powi(2) - ((ny - 0.62) / 0.15).powi(2)).exp();
            let dx = (t.sin() * 2.0 * (1.0 - ny) + feet * leg * t.sin()).round() as i32;
            let dy = ((2.0 * t).cos() * 0.7 + hand * t.sin() * 2.0 + feet * leg * t.sin()).round()
                as i32;
            let px = origin_x + x as i32 + dx;
            let py = origin_y + y as i32 + dy;
            if px < i32::from(area.x)
                || px >= i32::from(area.right())
                || py < i32::from(area.y)
                || py >= i32::from(area.bottom())
            {
                continue;
            }
            let color = if ny > 0.78 {
                Color::LightMagenta
            } else if ny > 0.48 && !(0.28..=0.73).contains(&nx) {
                Color::White
            } else {
                Color::Yellow
            };
            frame.buffer_mut()[(px as u16, py as u16)]
                .set_char(ch)
                .set_fg(color);
        }
    }
}

struct Screen(Terminal<CrosstermBackend<std::io::Stderr>>);

impl Screen {
    fn enter() -> std::io::Result<Self> {
        let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stderr()))?;
        if let Err(error) = execute!(terminal.backend_mut(), EnterAlternateScreen, Hide) {
            let _ = execute!(terminal.backend_mut(), Show, LeaveAlternateScreen);
            return Err(error);
        }
        Ok(Self(terminal))
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = execute!(self.0.backend_mut(), Show, LeaveAlternateScreen);
    }
}

#[derive(Default)]
pub(super) struct AgentAnimation {
    stop: Option<mpsc::Sender<()>>,
    worker: Option<JoinHandle<()>>,
    output: Option<LiveOutput>,
    result: Option<bool>,
}

impl AgentAnimation {
    pub fn start(requested: bool) -> Self {
        if !requested
            || !std::io::stderr().is_terminal()
            || !std::io::stdout().is_terminal()
            || std::env::var_os("TERM").is_none_or(|term| term == "dumb")
        {
            return Self::default();
        }
        let Some(output) = LiveOutput::start() else {
            return Self::default();
        };
        let Ok(mut screen) = Screen::enter() else {
            drop(output);
            return Self::default();
        };
        let (stop, receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("atomic-agent-dance".into())
            .spawn(move || {
                let started = Instant::now();
                loop {
                    let status = live::latest();
                    // Fullscreen draw handles erasure and resizes without a cursor
                    // position query (which may require a terminal response).
                    if screen
                        .0
                        .draw(|frame| draw(frame, started.elapsed(), &status))
                        .is_err()
                    {
                        break;
                    }
                    match receiver.recv_timeout(Duration::from_millis(100)) {
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        _ => break,
                    }
                }
            });
        match worker {
            Ok(worker) => Self {
                stop: Some(stop),
                worker: Some(worker),
                output: Some(output),
                result: None,
            },
            Err(_) => {
                drop(output);
                Self::default()
            }
        }
    }

    pub fn finish(&mut self, success: bool) {
        self.result = Some(success);
    }
}

impl Drop for AgentAnimation {
    fn drop(&mut self) {
        let enabled = self.stop.is_some();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        // Restore the screen before replaying diagnostics and the final success.
        drop(self.output.take());
        if enabled {
            eprintln!(
                "Agent-mode import {}.",
                if self.result == Some(true) {
                    "finished"
                } else {
                    "stopped / failed"
                }
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dance_animates_and_adapts_to_terminal_size() {
        for (width, height) in [(120, 60), (80, 24), (40, 16), (1, 1)] {
            let mut terminal =
                Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| draw(frame, Duration::ZERO, "Importing..."))
                .unwrap();
            let before = terminal.backend().buffer().clone();
            terminal
                .draw(|frame| draw(frame, Duration::from_millis(600), "Importing..."))
                .unwrap();
            if width >= 40 {
                assert_ne!(&before, terminal.backend().buffer());
            }
            terminal
                .draw(|frame| dance(frame, frame.area(), Duration::ZERO))
                .unwrap();
            let pose = terminal.backend().buffer().clone();
            terminal
                .draw(|frame| dance(frame, frame.area(), Duration::from_millis(600)))
                .unwrap();
            if width >= 40 {
                assert_ne!(
                    &pose,
                    terminal.backend().buffer(),
                    "the character itself must move"
                );
            }
        }
    }
}
