//! `ccflip ui` — live terminal dashboard: the guarded credentials (the same
//! rows as `ccflip status`) and the Claude accounts cswap knows.
//! ↑/↓ select · l log in · r refresh · q quit. Refreshes every minute.
//!
//! Loading runs on a thread (a gcloud probe takes seconds), so the screen
//! never freezes; `draw` and `App::key` are pure and tested headless.

use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table, TableState};
use ratatui::{DefaultTerminal, Frame};

use crate::guard::{self, Account, State};
use crate::{clock, cswap, epoch};

const REFRESH: Duration = Duration::from_secs(60);

type Data = (Vec<Account>, Option<cswap::List>);

#[derive(Default)]
struct App {
    creds: Vec<Account>,
    claude: Option<cswap::List>,
    selected: usize,
    loading: bool,
    note: String,
}

#[derive(Debug, PartialEq)]
enum Action {
    Quit,
    Refresh,
    /// `<provider>.<account>`, as `ccflip login` takes it.
    Login(String),
}

impl App {
    fn key(&mut self, code: KeyCode) -> Option<Action> {
        let last = self.creds.len().saturating_sub(1);
        match code {
            KeyCode::Char('q') | KeyCode::Esc => Some(Action::Quit),
            KeyCode::Char('r') => Some(Action::Refresh),
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(last);
                None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                None
            }
            KeyCode::Char('l') | KeyCode::Enter => self
                .creds
                .get(self.selected)
                .map(|a| Action::Login(format!("{}.{}", a.provider, a.account))),
            _ => None,
        }
    }

    fn loaded(&mut self, (creds, claude): Data) {
        self.creds = creds;
        self.claude = claude;
        self.selected = self.selected.min(self.creds.len().saturating_sub(1));
        self.loading = false;
    }
}

fn load() -> Receiver<Data> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || tx.send((guard::accounts(), cswap::list())));
    rx
}

pub fn run() -> Result<()> {
    let mut terminal = ratatui::init(); // also restores the terminal on panic
    let result = event_loop(&mut terminal);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal) -> Result<()> {
    let mut app = App {
        loading: true,
        ..App::default()
    };
    let mut rx = load();
    let mut since = Instant::now();
    loop {
        if let Ok(data) = rx.try_recv() {
            app.loaded(data);
        }
        terminal.draw(|f| draw(f, &app))?;
        let action = if event::poll(Duration::from_millis(250))?
            && let Event::Key(k) = event::read()?
            && k.kind == KeyEventKind::Press
        {
            app.key(k.code)
        } else if since.elapsed() >= REFRESH && !app.loading {
            Some(Action::Refresh)
        } else {
            None
        };
        match action {
            None => continue,
            Some(Action::Quit) => return Ok(()),
            Some(Action::Refresh) => {}
            Some(Action::Login(key)) => {
                // The login is interactive (URL, browser, prompts): give it
                // the real terminal, then come back.
                ratatui::restore();
                let rc = guard::login(&key);
                *terminal = ratatui::init();
                app.note = format!(
                    "{key}: {}",
                    if rc == 0 { "logged in" } else { "login failed" }
                );
            }
        }
        rx = load();
        app.loading = true;
        since = Instant::now();
    }
}

fn state_style(state: &State) -> Style {
    Style::new().fg(match state {
        State::Ok => Color::Green,
        State::Expiring => Color::Yellow,
        State::Expired => Color::Red,
        State::Error => Color::Magenta,
    })
}

fn header(cells: &[&'static str]) -> Row<'static> {
    Row::new(cells.iter().copied()).style(Style::new().add_modifier(Modifier::BOLD))
}

fn pct(p: Option<f64>) -> String {
    p.map_or_else(|| "—".to_string(), |p| format!("{p:.0}%"))
}

fn draw(f: &mut Frame, app: &App) {
    let [creds_area, claude_area, help_area] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(f.area());

    let creds = app.creds.iter().map(|a| {
        Row::new([
            Cell::from(a.provider),
            Cell::from(a.account.clone()),
            Cell::from(format!("{:?}", a.state).to_lowercase()).style(state_style(&a.state)),
            Cell::from(
                a.expires_at
                    .as_deref()
                    .and_then(epoch)
                    .map_or_else(|| "—".to_string(), clock),
            ),
            Cell::from(if a.needs_you { "login needed" } else { "" }),
        ])
    });
    let title = if app.loading {
        " Credentials (refreshing…) "
    } else {
        " Credentials "
    };
    let table = Table::new(
        creds,
        [
            Constraint::Length(8),
            Constraint::Fill(1),
            Constraint::Length(9),
            Constraint::Length(8),
            Constraint::Length(13),
        ],
    )
    .header(header(&[
        "PROVIDER",
        "ACCOUNT",
        "STATE",
        "EXPIRES",
        "NEEDS YOU",
    ]))
    .block(Block::bordered().title(title))
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    let mut state =
        TableState::default().with_selected((!app.creds.is_empty()).then_some(app.selected));
    f.render_stateful_widget(table, creds_area, &mut state);

    let claude = app.claude.iter().flat_map(|l| &l.accounts).map(|a| {
        let usage = a.usage.as_ref();
        let health = if a.limit_flagged {
            "limit hit".to_string()
        } else if a.disabled {
            "disabled".to_string()
        } else {
            format!("{:?}", a.usage_status).to_lowercase()
        };
        Row::new([
            a.number.to_string(),
            a.email.clone(),
            if a.active { "●" } else { "" }.to_string(),
            pct(usage.and_then(|u| u.five_hour).map(|w| w.pct)),
            pct(usage.and_then(|u| u.seven_day).map(|w| w.pct)),
            pct(a.spend_pct()),
            health,
        ])
    });
    let title = if app.claude.is_some() {
        " Claude accounts "
    } else {
        " Claude accounts (cswap unavailable) "
    };
    f.render_widget(
        Table::new(
            claude,
            [
                Constraint::Length(3),
                Constraint::Fill(1),
                Constraint::Length(6),
                Constraint::Length(5),
                Constraint::Length(5),
                Constraint::Length(6),
                Constraint::Length(11),
            ],
        )
        .header(header(&[
            "#", "EMAIL", "ACTIVE", "5H", "7D", "SPEND", "HEALTH",
        ]))
        .block(Block::bordered().title(title)),
        claude_area,
    );

    f.render_widget(
        Paragraph::new(format!(
            "↑↓ select · l log in · r refresh · q quit   {}",
            app.note
        )),
        help_area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn app() -> App {
        let account =
            |provider, account: &str, state, expires_at: Option<&str>, needs_you| Account {
                provider,
                account: account.to_string(),
                state,
                expires_at: expires_at.map(String::from),
                needs_you,
            };
        App {
            creds: vec![
                account(
                    "aws",
                    "heka",
                    State::Ok,
                    Some("2026-11-11T18:00:00Z"),
                    false,
                ),
                account("gcloud", "me@x.com", State::Expired, None, true),
            ],
            claude: serde_json::from_str(
                r#"{"activeAccountNumber":1,"accounts":[
 {"number":1,"email":"sia@x.com","active":true,"usageStatus":"ok",
  "usage":{"fiveHour":{"pct":97.0},"sevenDay":{"pct":40.0}}},
 {"number":2,"email":"qm@x.com","active":false,"usageStatus":"ok","disabled":true,
  "usage":{"spend":{"used":14.0,"limit":100.0,"pct":14.0}}}]}"#,
            )
            .unwrap(),
            ..App::default()
        }
    }

    fn screen(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn shows_credentials_and_claude_accounts() {
        let s = screen(&app());
        for wanted in [
            "heka",
            "ok",
            "me@x.com",
            "expired",
            "login needed",
            "sia@x.com",
            "97%",
            "40%",
            "qm@x.com",
            "14%",
            "disabled",
        ] {
            assert!(s.contains(wanted), "missing {wanted:?} in:\n{s}");
        }
        assert!(
            s.contains(&clock(epoch("2026-11-11T18:00:00Z").unwrap())),
            "{s}"
        );
    }

    #[test]
    fn says_when_cswap_is_missing_and_when_refreshing() {
        let s = screen(&App {
            loading: true,
            ..App::default()
        });
        assert!(s.contains("Credentials (refreshing…)"), "{s}");
        assert!(s.contains("Claude accounts (cswap unavailable)"), "{s}");
    }

    #[test]
    fn keys_select_and_log_in_the_selected_account() {
        let mut a = app();
        assert_eq!(
            a.key(KeyCode::Char('l')),
            Some(Action::Login("aws.heka".into()))
        );
        a.key(KeyCode::Down);
        a.key(KeyCode::Down); // clamped at the last row
        assert_eq!(
            a.key(KeyCode::Enter),
            Some(Action::Login("gcloud.me@x.com".into()))
        );
        a.key(KeyCode::Up);
        a.key(KeyCode::Up);
        assert_eq!(a.selected, 0);
        assert_eq!(a.key(KeyCode::Char('q')), Some(Action::Quit));
        assert_eq!(a.key(KeyCode::Char('r')), Some(Action::Refresh));
        assert_eq!(
            App::default().key(KeyCode::Char('l')),
            None,
            "nothing to log in"
        );
    }

    #[test]
    fn reload_keeps_the_selection_in_range() {
        let mut a = app();
        a.selected = 1;
        a.loaded((Vec::new(), None));
        assert_eq!((a.selected, a.loading), (0, false));
    }
}
