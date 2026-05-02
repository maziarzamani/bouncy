//! Live ratatui dashboard for `bouncy scrape --tui`.
//!
//! Subscribes to the `mpsc::UnboundedSender<ScrapeEvent>` that the
//! scrape task writes to, maintains per-URL state in an
//! insertion-order map, and re-renders on a 10 Hz tick.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use indexmap::IndexMap;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{BarChart, Block, Borders, Gauge, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::scrape::ScrapeEvent;

const TICK_MS: u64 = 100;
const THROUGHPUT_WINDOW_SECS: u64 = 10;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Queued,
    InFlight,
    BackingOff,
    Done,
    Failed,
}

#[derive(Clone, Debug)]
pub struct UrlState {
    pub phase: Phase,
    pub attempts: u32,
    pub status: Option<u16>,
    pub latency_ms: Option<u64>,
    pub title: Option<String>,
    pub last_error: Option<String>,
}

impl UrlState {
    fn new() -> Self {
        Self {
            phase: Phase::Queued,
            attempts: 0,
            status: None,
            latency_ms: None,
            title: None,
            last_error: None,
        }
    }
}

pub struct AppState {
    pub urls: IndexMap<String, UrlState>,
    pub completed: usize,
    pub active: usize,
    pub failed: usize,
    /// Final-status histogram (only counts completed URLs).
    pub status_hist: BTreeMap<u16, u32>,
    /// Total times for completed URLs — for p50/p95.
    pub latencies: Vec<u64>,
    /// (timestamp, completion-count) sliding window for throughput.
    pub throughput: VecDeque<Instant>,
    pub started_at: Instant,
    pub list_state: ListState,
    pub finished: bool,
    pub quit: bool,
    pub total_urls: usize,
}

impl AppState {
    pub fn new(total_urls: usize) -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self {
            urls: IndexMap::with_capacity(total_urls),
            completed: 0,
            active: 0,
            failed: 0,
            status_hist: BTreeMap::new(),
            latencies: Vec::with_capacity(total_urls),
            throughput: VecDeque::new(),
            started_at: Instant::now(),
            list_state,
            finished: false,
            quit: false,
            total_urls,
        }
    }

    pub fn apply(&mut self, ev: ScrapeEvent) {
        match ev {
            ScrapeEvent::Queued { url, .. } => {
                self.urls.entry(url).or_insert_with(UrlState::new);
            }
            ScrapeEvent::RequestStart { url, attempt } => {
                let entry = self.urls.entry(url).or_insert_with(UrlState::new);
                if entry.phase == Phase::Queued {
                    self.active += 1;
                }
                entry.phase = Phase::InFlight;
                entry.attempts = attempt + 1;
            }
            ScrapeEvent::Response { url, status, .. } => {
                if let Some(s) = self.urls.get_mut(&url) {
                    s.status = Some(status);
                }
            }
            ScrapeEvent::BackoffStart { url, attempt, .. } => {
                if let Some(s) = self.urls.get_mut(&url) {
                    s.phase = Phase::BackingOff;
                    s.attempts = attempt + 1;
                }
            }
            ScrapeEvent::Completed {
                url,
                final_status,
                title,
                total_time_ms,
                ..
            } => {
                let entry = self.urls.entry(url).or_insert_with(UrlState::new);
                if entry.phase != Phase::Done && entry.phase != Phase::Failed {
                    if entry.phase == Phase::InFlight || entry.phase == Phase::BackingOff {
                        self.active = self.active.saturating_sub(1);
                    }
                    self.completed += 1;
                }
                entry.phase = Phase::Done;
                entry.status = Some(final_status);
                entry.latency_ms = Some(total_time_ms);
                entry.title = Some(title);
                self.latencies.push(total_time_ms);
                *self.status_hist.entry(final_status).or_default() += 1;
                self.throughput.push_back(Instant::now());
            }
            ScrapeEvent::Failed {
                url,
                error,
                attempts,
            } => {
                let entry = self.urls.entry(url).or_insert_with(UrlState::new);
                if entry.phase != Phase::Done && entry.phase != Phase::Failed {
                    if entry.phase == Phase::InFlight || entry.phase == Phase::BackingOff {
                        self.active = self.active.saturating_sub(1);
                    }
                    self.failed += 1;
                }
                entry.phase = Phase::Failed;
                entry.last_error = Some(error);
                entry.attempts = attempts;
                self.throughput.push_back(Instant::now());
            }
        }
    }

    /// p50 / p95 / max latency in ms over completed URLs.
    pub fn latency_stats(&self) -> (Option<u64>, Option<u64>, Option<u64>) {
        if self.latencies.is_empty() {
            return (None, None, None);
        }
        let mut sorted = self.latencies.clone();
        sorted.sort_unstable();
        let p50 = percentile(&sorted, 0.50);
        let p95 = percentile(&sorted, 0.95);
        let max = sorted.last().copied();
        (p50, p95, max)
    }

    /// Completions per second over the trailing 10s window. Trims old
    /// entries from `throughput` as a side-effect.
    pub fn throughput_now(&mut self) -> f64 {
        let cutoff = Instant::now() - Duration::from_secs(THROUGHPUT_WINDOW_SECS);
        while self.throughput.front().is_some_and(|t| *t < cutoff) {
            self.throughput.pop_front();
        }
        let count = self.throughput.len() as f64;
        let elapsed = (Instant::now() - self.started_at)
            .as_secs_f64()
            .min(THROUGHPUT_WINDOW_SECS as f64);
        if elapsed <= 0.0 {
            0.0
        } else {
            count / elapsed
        }
    }
}

/// Percentile (0.0..=1.0) over a pre-sorted ascending vec.
pub fn percentile(sorted: &[u64], p: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let p = p.clamp(0.0, 1.0);
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    Some(sorted[idx])
}

pub async fn run_tui(
    mut rx: UnboundedReceiver<ScrapeEvent>,
    total_urls: usize,
) -> anyhow::Result<()> {
    let mut terminal = init_terminal()?;
    let mut state = AppState::new(total_urls);
    let mut tick = tokio::time::interval(Duration::from_millis(TICK_MS));

    loop {
        tick.tick().await;

        // Drain pending scrape events.
        let mut closed = false;
        loop {
            match rx.try_recv() {
                Ok(ev) => state.apply(ev),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    closed = true;
                    break;
                }
            }
        }
        if closed {
            state.finished = true;
        }

        // Drain pending keyboard events.
        while event::poll(Duration::from_millis(0))? {
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                handle_key(k.code, &mut state);
            }
        }

        terminal.draw(|f| render(f, &mut state))?;

        if state.quit {
            break;
        }
    }
    restore_terminal()?;
    Ok(())
}

fn handle_key(code: KeyCode, state: &mut AppState) {
    match code {
        KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('Q') => state.quit = true,
        KeyCode::Down | KeyCode::Char('j') => {
            let i = state.list_state.selected().unwrap_or(0);
            let max = state.urls.len().saturating_sub(1);
            state.list_state.select(Some((i + 1).min(max)));
        }
        KeyCode::Up | KeyCode::Char('k') => {
            let i = state.list_state.selected().unwrap_or(0);
            state.list_state.select(Some(i.saturating_sub(1)));
        }
        KeyCode::PageDown => {
            let i = state.list_state.selected().unwrap_or(0);
            let max = state.urls.len().saturating_sub(1);
            state.list_state.select(Some((i + 10).min(max)));
        }
        KeyCode::PageUp => {
            let i = state.list_state.selected().unwrap_or(0);
            state.list_state.select(Some(i.saturating_sub(10)));
        }
        _ => {}
    }
}

fn init_terminal() -> anyhow::Result<Terminal<CrosstermBackend<Stdout>>> {
    use crossterm::{execute, terminal};
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    // Panic hook so a panic during render still restores the terminal.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        original_hook(info);
    }));
    Ok(terminal)
}

fn restore_terminal() -> anyhow::Result<()> {
    use crossterm::{execute, terminal};
    let _ = terminal::disable_raw_mode();
    let _ = execute!(io::stdout(), terminal::LeaveAlternateScreen);
    Ok(())
}

fn render(f: &mut Frame, state: &mut AppState) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    render_header(f, chunks[0], state);
    render_body(f, chunks[1], state);
    render_footer(f, chunks[2], state);
}

fn render_header(f: &mut Frame, area: Rect, state: &AppState) {
    let throughput_done = state.completed + state.failed;
    let total = state.total_urls.max(state.urls.len());
    let title = format!(
        " bouncy scrape — {done}/{total} done · {active} active{suffix} ",
        done = throughput_done,
        total = total,
        active = state.active,
        suffix = if state.finished {
            " · ✔ finished"
        } else {
            ""
        },
    );
    let p = Paragraph::new(title).style(
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    );
    f.render_widget(p, area);
}

fn render_body(f: &mut Frame, area: Rect, state: &mut AppState) {
    let small = area.width < 100 || area.height < 20;
    if small {
        render_url_list(f, area, state);
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);
    render_url_list(f, chunks[0], state);
    render_right_column(f, chunks[1], state);
}

fn render_url_list(f: &mut Frame, area: Rect, state: &mut AppState) {
    let items: Vec<ListItem> = state
        .urls
        .iter()
        .map(|(url, s)| {
            let glyph = match s.phase {
                Phase::Queued => Span::styled("·", Style::default().fg(Color::DarkGray)),
                Phase::InFlight => Span::styled("⟳", Style::default().fg(Color::Yellow)),
                Phase::BackingOff => Span::styled("…", Style::default().fg(Color::Yellow)),
                Phase::Done => Span::styled("✓", Style::default().fg(Color::Green)),
                Phase::Failed => Span::styled("✗", Style::default().fg(Color::Red)),
            };
            let status_str = s
                .status
                .map(|c| format!("{c}"))
                .unwrap_or_else(|| "···".to_string());
            let lat_str = s
                .latency_ms
                .map(|ms| format!("{ms}ms"))
                .unwrap_or_else(|| "—".to_string());
            let attempt_marker = if s.attempts > 1 {
                format!(" (try {})", s.attempts)
            } else {
                String::new()
            };
            let title = s
                .title
                .as_deref()
                .filter(|t| !t.is_empty())
                .map(|t| format!("  \"{}\"", truncate(t, 40)))
                .unwrap_or_default();
            let line = Line::from(vec![
                glyph,
                Span::raw(format!(
                    " {:>4} {:>7}{}  ",
                    status_str, lat_str, attempt_marker
                )),
                Span::styled(truncate(url, 60), Style::default().fg(Color::Cyan)),
                Span::styled(title, Style::default().fg(Color::DarkGray)),
            ]);
            ListItem::new(line)
        })
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" URLs "))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, area, &mut state.list_state);
}

fn render_right_column(f: &mut Frame, area: Rect, state: &mut AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(5),
            Constraint::Min(0),
        ])
        .split(area);
    render_throughput(f, chunks[0], state);
    render_latency(f, chunks[1], state);
    render_status_hist(f, chunks[2], state);
}

fn render_throughput(f: &mut Frame, area: Rect, state: &mut AppState) {
    let rate = state.throughput_now();
    // Gauge ratio: scale rate against an arbitrary 50 req/s ceiling.
    let pct = ((rate / 50.0) * 100.0).clamp(0.0, 100.0) as u16;
    let label = format!("{rate:.1} req/s");
    let g = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title(" Throughput "))
        .gauge_style(Style::default().fg(Color::Green))
        .percent(pct)
        .label(label);
    f.render_widget(g, area);
}

fn render_latency(f: &mut Frame, area: Rect, state: &AppState) {
    let (p50, p95, max) = state.latency_stats();
    let fmt = |v: Option<u64>| v.map(|n| format!("{n} ms")).unwrap_or_else(|| "—".into());
    let text = vec![
        Line::from(format!(" p50  {}", fmt(p50))),
        Line::from(format!(" p95  {}", fmt(p95))),
        Line::from(format!(" max  {}", fmt(max))),
    ];
    let p = Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(" Latency "));
    f.render_widget(p, area);
}

fn render_status_hist(f: &mut Frame, area: Rect, state: &AppState) {
    let mut bars: Vec<(String, u64)> = state
        .status_hist
        .iter()
        .map(|(code, count)| (format!("{code}"), *count as u64))
        .collect();
    if state.failed > 0 {
        bars.push(("err".to_string(), state.failed as u64));
    }
    if bars.is_empty() {
        let p = Paragraph::new(" (no responses yet)")
            .block(Block::default().borders(Borders::ALL).title(" Status "));
        f.render_widget(p, area);
        return;
    }
    let data: Vec<(&str, u64)> = bars.iter().map(|(s, n)| (s.as_str(), *n)).collect();
    let chart = BarChart::default()
        .block(Block::default().borders(Borders::ALL).title(" Status "))
        .data(&data)
        .bar_width(4)
        .bar_gap(1)
        .bar_style(Style::default().fg(Color::Blue));
    f.render_widget(chart, area);
}

fn render_footer(f: &mut Frame, area: Rect, _state: &AppState) {
    let line = Line::from(vec![
        Span::styled(" q", Style::default().fg(Color::Yellow)),
        Span::raw(" quit · "),
        Span::styled("↑↓/jk", Style::default().fg(Color::Yellow)),
        Span::raw(" scroll · "),
        Span::styled("PgUp/PgDn", Style::default().fg(Color::Yellow)),
        Span::raw(" page "),
    ]);
    let p = Paragraph::new(line).style(Style::default().fg(Color::DarkGray));
    f.render_widget(p, area);
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev_queued(url: &str, idx: usize) -> ScrapeEvent {
        ScrapeEvent::Queued {
            url: url.to_string(),
            index: idx,
        }
    }
    fn ev_request(url: &str, attempt: u32) -> ScrapeEvent {
        ScrapeEvent::RequestStart {
            url: url.to_string(),
            attempt,
        }
    }
    fn ev_completed(url: &str, status: u16, ms: u64) -> ScrapeEvent {
        ScrapeEvent::Completed {
            url: url.to_string(),
            final_status: status,
            title: "ok".into(),
            total_time_ms: ms,
            retries: 0,
            eval: None,
        }
    }
    fn ev_failed(url: &str) -> ScrapeEvent {
        ScrapeEvent::Failed {
            url: url.to_string(),
            error: "boom".into(),
            attempts: 1,
        }
    }

    #[test]
    fn queued_event_inserts_url_in_phase_queued() {
        let mut s = AppState::new(2);
        s.apply(ev_queued("https://a", 0));
        s.apply(ev_queued("https://b", 1));
        assert_eq!(s.urls.len(), 2);
        assert_eq!(s.urls["https://a"].phase, Phase::Queued);
        assert_eq!(s.urls["https://b"].phase, Phase::Queued);
        assert_eq!(s.active, 0);
        assert_eq!(s.completed, 0);
    }

    #[test]
    fn request_start_increments_active_and_attempts() {
        let mut s = AppState::new(1);
        s.apply(ev_queued("https://a", 0));
        s.apply(ev_request("https://a", 0));
        assert_eq!(s.urls["https://a"].phase, Phase::InFlight);
        assert_eq!(s.urls["https://a"].attempts, 1);
        assert_eq!(s.active, 1);
        // A second RequestStart (= retry) shouldn't double-count active.
        s.apply(ev_request("https://a", 1));
        assert_eq!(s.urls["https://a"].attempts, 2);
        assert_eq!(s.active, 1);
    }

    #[test]
    fn completed_decrements_active_and_records_stats() {
        let mut s = AppState::new(1);
        s.apply(ev_queued("https://a", 0));
        s.apply(ev_request("https://a", 0));
        s.apply(ev_completed("https://a", 200, 142));
        assert_eq!(s.urls["https://a"].phase, Phase::Done);
        assert_eq!(s.urls["https://a"].status, Some(200));
        assert_eq!(s.urls["https://a"].latency_ms, Some(142));
        assert_eq!(s.active, 0);
        assert_eq!(s.completed, 1);
        assert_eq!(s.status_hist.get(&200), Some(&1));
        assert_eq!(s.latencies, vec![142]);
    }

    #[test]
    fn failed_decrements_active_and_increments_failed_count() {
        let mut s = AppState::new(1);
        s.apply(ev_queued("https://a", 0));
        s.apply(ev_request("https://a", 0));
        s.apply(ev_failed("https://a"));
        assert_eq!(s.urls["https://a"].phase, Phase::Failed);
        assert_eq!(s.failed, 1);
        assert_eq!(s.active, 0);
    }

    #[test]
    fn completed_after_failed_doesnt_double_count() {
        let mut s = AppState::new(1);
        s.apply(ev_queued("https://a", 0));
        s.apply(ev_request("https://a", 0));
        s.apply(ev_completed("https://a", 200, 100));
        s.apply(ev_completed("https://a", 200, 100)); // duplicate
        assert_eq!(s.completed, 1);
    }

    #[test]
    fn status_histogram_aggregates_by_code() {
        let mut s = AppState::new(3);
        for url in ["https://a", "https://b", "https://c"] {
            s.apply(ev_queued(url, 0));
            s.apply(ev_request(url, 0));
        }
        s.apply(ev_completed("https://a", 200, 50));
        s.apply(ev_completed("https://b", 200, 60));
        s.apply(ev_completed("https://c", 503, 70));
        assert_eq!(s.status_hist.get(&200), Some(&2));
        assert_eq!(s.status_hist.get(&503), Some(&1));
    }

    #[test]
    fn percentile_handles_known_distributions() {
        let v: Vec<u64> = (1..=100).collect();
        // Nearest-rank percentile, 0-indexed, half-rounded up — for
        // len=100 p=0.5 lands on idx 50 (value 51), not the textbook
        // statistician's 50.5. Good enough for a dashboard.
        assert_eq!(percentile(&v, 0.50), Some(51));
        assert_eq!(percentile(&v, 0.95), Some(95));
        assert_eq!(percentile(&v, 1.0), Some(100));
        assert_eq!(percentile(&v, 0.0), Some(1));
    }

    #[test]
    fn percentile_empty_returns_none() {
        let v: Vec<u64> = vec![];
        assert_eq!(percentile(&v, 0.5), None);
    }

    #[test]
    fn percentile_single_value() {
        let v = vec![42u64];
        assert_eq!(percentile(&v, 0.5), Some(42));
        assert_eq!(percentile(&v, 0.95), Some(42));
    }

    #[test]
    fn latency_stats_after_three_completions() {
        let mut s = AppState::new(3);
        s.apply(ev_completed("https://a", 200, 100));
        s.apply(ev_completed("https://b", 200, 200));
        s.apply(ev_completed("https://c", 200, 300));
        let (p50, p95, max) = s.latency_stats();
        assert_eq!(p50, Some(200));
        assert_eq!(p95, Some(300));
        assert_eq!(max, Some(300));
    }

    #[test]
    fn truncate_handles_short_and_long_strings() {
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(truncate("abcdefghij", 5), "abcd…");
    }
}
