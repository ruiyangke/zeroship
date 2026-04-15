use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode};
use crossterm::ExecutableCommand;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, Paragraph};
use ratatui::Terminal;

use crate::config::Config;
use crate::stats::LiveStats;

/// Run the TUI dashboard on the current thread.
///
/// Called from main after spawning worker threads. Runs until `duration`
/// elapses or the user presses `q`.
pub fn run_tui(config: &Config, live: Arc<LiveStats>) {
    if let Err(e) = run_tui_inner(config, live) {
        // Ensure terminal is restored before printing the error.
        eprintln!("[tui] error: {e}");
    }
}

fn run_tui_inner(config: &Config, live: Arc<LiveStats>) -> io::Result<()> {
    let mut stdout = io::stdout();
    crossterm::terminal::enable_raw_mode()?;
    stdout.execute(EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let start = Instant::now();
    let total_duration = config.duration;

    loop {
        let elapsed = start.elapsed();
        let (requests, bytes, errors) = live.snapshot();

        let secs = elapsed.as_secs_f64().max(0.001);
        let rps = requests as f64 / secs;
        let bps = bytes as f64 / secs;
        let progress = (elapsed.as_secs_f64() / total_duration.as_secs_f64()).min(1.0);

        terminal.draw(|frame| {
            let area = frame.area();
            render(
                frame,
                area,
                config,
                elapsed,
                total_duration,
                requests,
                errors,
                rps,
                bps,
                progress,
            );
        })?;

        // Poll for 'q' keypress with 100ms timeout.
        if crossterm::event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = crossterm::event::read()? {
                if key.code == KeyCode::Char('q') {
                    break;
                }
            }
        }

        if elapsed >= total_duration {
            // Show the final frame briefly so the user sees 100%.
            std::thread::sleep(Duration::from_millis(300));
            break;
        }
    }

    // Restore terminal state.
    crossterm::terminal::disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn render(
    frame: &mut ratatui::Frame,
    area: Rect,
    config: &Config,
    elapsed: Duration,
    total: Duration,
    requests: u64,
    errors: u64,
    rps: f64,
    bps: f64,
    progress: f64,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header bar
            Constraint::Length(4), // stats block
            Constraint::Length(3), // progress bar
            Constraint::Min(0),    // padding
        ])
        .split(area);

    // --- Header bar ---
    let header_text = vec![Line::from(vec![
        Span::styled(" zerobench ", Style::default().fg(Color::Cyan)),
        Span::raw("│ "),
        Span::raw(config.url.as_str()),
        Span::raw("  "),
        Span::styled(
            format!("{}t {}c", config.threads, config.connections),
            Style::default().fg(Color::Yellow),
        ),
        Span::raw("  "),
        Span::styled(
            format!("Elapsed: {:.1}s / {:.0}s", elapsed.as_secs_f64(), total.as_secs_f64()),
            Style::default().fg(Color::Green),
        ),
        Span::raw(" "),
    ])];
    let header =
        Paragraph::new(header_text).block(Block::default().borders(Borders::ALL));
    frame.render_widget(header, chunks[0]);

    // --- Stats block ---
    let mb_sec = bps / 1_048_576.0;
    let stats_text = vec![
        Line::from(vec![
            Span::styled(
                format!(" Requests/sec: {:>14.0}", rps),
                Style::default().fg(Color::White),
            ),
            Span::raw("          "),
            Span::styled(
                format!("Errors: {errors}"),
                if errors > 0 {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default().fg(Color::Green)
                },
            ),
        ]),
        Line::from(vec![
            Span::styled(
                format!(" Transfer/sec: {:>11.2} MB/s", mb_sec),
                Style::default().fg(Color::White),
            ),
            Span::raw("          "),
            Span::styled(
                format!("Total:  {} req", format_count(requests)),
                Style::default().fg(Color::White),
            ),
        ]),
    ];
    let stats = Paragraph::new(stats_text)
        .block(Block::default().borders(Borders::ALL).title(" Stats "));
    frame.render_widget(stats, chunks[1]);

    // --- Progress bar ---
    let pct = (progress * 100.0) as u16;
    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL))
        .gauge_style(Style::default().fg(Color::Cyan).bg(Color::DarkGray))
        .percent(pct)
        .label(format!("{pct}% complete"));
    frame.render_widget(gauge, chunks[2]);
}

fn format_count(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.2}B", n as f64 / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{n}")
    }
}
