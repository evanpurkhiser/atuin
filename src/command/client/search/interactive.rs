use std::{io::stdout, sync::Arc};

use eyre::Result;
use semver::Version;
use skim::{prelude::ExactOrFuzzyEngineFactory, MatchEngineFactory, SkimItem};
use termion::{
    event::Event as TermEvent, event::Key, event::MouseButton, event::MouseEvent,
    input::MouseTerminal, raw::IntoRawMode, screen::AlternateScreen,
};
use tui::{
    backend::{Backend, TermionBackend},
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Span, Spans, Text},
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame, Terminal,
};
use unicode_width::UnicodeWidthStr;

use atuin_client::{
    database::Database,
    database::{current_context, Context},
    history::History,
    settings::{ExitMode, FilterMode, Settings},
};

use super::{
    cursor::Cursor,
    event::{Event, Events},
    history_list::{HistoryList, ListState, PREFIX_LENGTH},
};
use crate::VERSION;

const RETURN_ORIGINAL: usize = usize::MAX;
const RETURN_QUERY: usize = usize::MAX - 1;

struct State {
    history_count: i64,
    input: Cursor,
    filter_mode: FilterMode,
    results_state: ListState,
    context: Context,
    update_needed: Option<Version>,
}

impl State {
    fn query_results(&mut self, db: &[Arc<SkimHistory>]) -> Vec<Arc<SkimHistory>> {
        let mut set = Vec::with_capacity(200);
        if self.input.as_str().is_empty() {
            for item in db {
                match self.filter_mode {
                    FilterMode::Global => set.push(item.clone()),
                    FilterMode::Host if item.0.hostname == self.context.hostname => {
                        set.push(item.clone());
                    }
                    FilterMode::Session if item.0.session == self.context.session => {
                        set.push(item.clone());
                    }
                    FilterMode::Directory if item.0.cwd == self.context.cwd => {
                        set.push(item.clone());
                    }
                    _ => {}
                }
                if set.len() == 200 {
                    break;
                }
            }
        } else {
            let mut ranks = Vec::with_capacity(200);
            let engine =
                ExactOrFuzzyEngineFactory::builder().fuzzy_algorithm(skim::FuzzyAlgorithm::SkimV2);
            let engine = engine.create_engine(self.input.as_str());
            for item in db {
                if let Some(res) = engine.match_item(item.clone()) {
                    let (Ok(i) | Err(i)) = ranks.binary_search(&res.rank);
                    if i >= 200 {
                        continue;
                    }
                    if set.len() == 200 {
                        set.pop();
                        ranks.pop();
                    }
                    ranks.insert(i, res.rank);
                    set.insert(i, item.clone());
                }
            }
        }
        set
    }

    fn handle_input(
        &mut self,
        settings: &Settings,
        input: &TermEvent,
        len: usize,
    ) -> Option<usize> {
        match input {
            TermEvent::Key(Key::Char('\t')) => {}
            TermEvent::Key(Key::Ctrl('c' | 'd' | 'g')) => return Some(RETURN_ORIGINAL),
            TermEvent::Key(Key::Esc) => {
                return Some(match settings.exit_mode {
                    ExitMode::ReturnOriginal => RETURN_ORIGINAL,
                    ExitMode::ReturnQuery => RETURN_QUERY,
                })
            }
            TermEvent::Key(Key::Char('\n')) => {
                return Some(self.results_state.selected());
            }
            TermEvent::Key(Key::Alt(c @ '1'..='9')) => {
                let c = c.to_digit(10)? as usize;
                return Some(self.results_state.selected() + c);
            }
            TermEvent::Key(Key::Left | Key::Ctrl('h')) => {
                self.input.left();
            }
            TermEvent::Key(Key::Right | Key::Ctrl('l')) => self.input.right(),
            TermEvent::Key(Key::Ctrl('a') | Key::Home) => self.input.start(),
            TermEvent::Key(Key::Ctrl('e') | Key::End) => self.input.end(),
            TermEvent::Key(Key::Char(c)) => self.input.insert(*c),
            TermEvent::Key(Key::Backspace) => {
                self.input.back();
            }
            TermEvent::Key(Key::Delete) => {
                self.input.remove();
            }
            TermEvent::Key(Key::Ctrl('w')) => {
                // remove the first batch of whitespace
                while matches!(self.input.back(), Some(c) if c.is_whitespace()) {}
                while self.input.left() {
                    if self.input.char().unwrap().is_whitespace() {
                        self.input.right(); // found whitespace, go back right
                        break;
                    }
                    self.input.remove();
                }
            }
            TermEvent::Key(Key::Ctrl('u')) => self.input.clear(),
            TermEvent::Key(Key::Ctrl('r')) => {
                pub static FILTER_MODES: [FilterMode; 4] = [
                    FilterMode::Global,
                    FilterMode::Host,
                    FilterMode::Session,
                    FilterMode::Directory,
                ];
                let i = self.filter_mode as usize;
                let i = (i + 1) % FILTER_MODES.len();
                self.filter_mode = FILTER_MODES[i];
            }
            TermEvent::Key(Key::Down | Key::Ctrl('n' | 'j'))
            | TermEvent::Mouse(MouseEvent::Press(MouseButton::WheelDown, _, _)) => {
                if self.results_state.selected() == 0 && input.eq(&TermEvent::Key(Key::Down)) {
                    return Some(RETURN_ORIGINAL);
                }
                let i = self.results_state.selected().saturating_sub(1);
                self.results_state.select(i);
            }
            TermEvent::Key(Key::Up | Key::Ctrl('p' | 'k'))
            | TermEvent::Mouse(MouseEvent::Press(MouseButton::WheelUp, _, _)) => {
                let i = self.results_state.selected() + 1;
                self.results_state.select(i.min(len - 1));
            }
            _ => {}
        };

        None
    }

    #[allow(clippy::cast_possible_truncation)]
    fn draw<T: Backend>(&mut self, f: &mut Frame<'_, T>, results: &[Arc<SkimHistory>]) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(0)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(3),
            ])
            .split(f.size());

        let top_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50); 2])
            .split(chunks[0]);

        let top_left_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1); 3])
            .split(top_chunks[0]);

        let top_right_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1); 3])
            .split(top_chunks[1]);

        let title = if self.update_needed.is_some() {
            let version = self.update_needed.clone().unwrap();

            Paragraph::new(Text::from(Span::styled(
                format!(" Atuin v{VERSION} - UPDATE AVAILABLE {version}"),
                Style::default().add_modifier(Modifier::BOLD).fg(Color::Red),
            )))
        } else {
            Paragraph::new(Text::from(Span::styled(
                format!(" Atuin v{VERSION}"),
                Style::default().add_modifier(Modifier::BOLD),
            )))
        };

        let help = vec![
            Span::raw(" Press "),
            Span::styled("Esc", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" to exit."),
        ];

        let help = Paragraph::new(Text::from(Spans::from(help)));
        let stats = Paragraph::new(Text::from(Span::raw(format!(
            "history count: {} ",
            self.history_count
        ))));

        f.render_widget(title, top_left_chunks[1]);
        f.render_widget(help, top_left_chunks[2]);
        f.render_widget(stats.alignment(Alignment::Right), top_right_chunks[1]);

        let results = HistoryList::new(results).block(
            Block::default()
                .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
                .border_type(BorderType::Rounded),
        );

        f.render_stateful_widget(results, chunks[1], &mut self.results_state);

        let input = format!(
            "[{:^14}] {}",
            self.filter_mode.as_str(),
            self.input.as_str(),
        );
        let input = Paragraph::new(input).block(
            Block::default()
                .borders(Borders::BOTTOM | Borders::LEFT | Borders::RIGHT)
                .border_type(BorderType::Rounded)
                .title(format!(
                    "{:─>width$}",
                    "",
                    width = chunks[2].width as usize - 2
                )),
        );
        f.render_widget(input, chunks[2]);

        let width = UnicodeWidthStr::width(self.input.substring());
        f.set_cursor(
            // Put cursor past the end of the input text
            chunks[2].x + width as u16 + PREFIX_LENGTH + 2,
            // Move one line down, from the border to the input line
            chunks[2].y + 1,
        );
    }

    #[allow(clippy::cast_possible_truncation)]
    fn draw_compact<T: Backend>(&mut self, f: &mut Frame<'_, T>, results: &[Arc<SkimHistory>]) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(0)
            .horizontal_margin(1)
            .constraints(
                [
                    Constraint::Length(1),
                    Constraint::Min(1),
                    Constraint::Length(1),
                ]
                .as_ref(),
            )
            .split(f.size());

        let header_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(
                [
                    Constraint::Ratio(1, 3),
                    Constraint::Ratio(1, 3),
                    Constraint::Ratio(1, 3),
                ]
                .as_ref(),
            )
            .split(chunks[0]);

        let title = Paragraph::new(Text::from(Span::styled(
            format!("Atuin v{VERSION}"),
            Style::default().fg(Color::DarkGray),
        )));

        let help = Paragraph::new(Text::from(Spans::from(vec![
            Span::styled("Esc", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" to exit"),
        ])))
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Center);

        let stats = Paragraph::new(Text::from(Span::raw(format!(
            "history count: {}",
            self.history_count,
        ))))
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Right);

        f.render_widget(title, header_chunks[0]);
        f.render_widget(help, header_chunks[1]);
        f.render_widget(stats, header_chunks[2]);

        let results = HistoryList::new(results);
        f.render_stateful_widget(results, chunks[1], &mut self.results_state);

        let input = format!(
            "[{:^14}] {}",
            self.filter_mode.as_str(),
            self.input.as_str(),
        );
        let input = Paragraph::new(input);
        f.render_widget(input, chunks[2]);

        let extra_width = UnicodeWidthStr::width(self.input.substring());

        f.set_cursor(
            // Put cursor past the end of the input text
            chunks[2].x + extra_width as u16 + PREFIX_LENGTH + 1,
            // Move one line down, from the border to the input line
            chunks[2].y + 1,
        );
    }
}

pub struct SkimHistory(pub History);
impl SkimItem for SkimHistory {
    fn text(&self) -> std::borrow::Cow<str> {
        std::borrow::Cow::Borrowed(self.0.command.as_str())
    }
}

// this is a big blob of horrible! clean it up!
// for now, it works. But it'd be great if it were more easily readable, and
// modular. I'd like to add some more stats and stuff at some point
#[allow(clippy::cast_possible_truncation)]
pub async fn history(
    query: &[String],
    settings: &Settings,
    db: &mut impl Database,
) -> Result<String> {
    let stdout = stdout().into_raw_mode()?;
    let stdout = MouseTerminal::from(stdout);
    let stdout = AlternateScreen::from(stdout);
    let backend = TermionBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Setup event handlers
    let events = Events::new();

    let mut input = Cursor::from(query.join(" "));
    // Put the cursor at the end of the query by default
    input.end();

    let update_needed = settings.needs_update().await;

    let context = current_context();

    let all_entries = db
        .list(FilterMode::Global, &context, None, true)
        .await
        .unwrap()
        .into_iter()
        .map(SkimHistory)
        .map(Arc::new)
        .collect::<Vec<_>>();

    let mut app = State {
        history_count: db.history_count().await?,
        input,
        results_state: ListState::default(),
        context,
        filter_mode: if settings.shell_up_key_binding {
            settings.filter_mode_shell_up_key_binding
        } else {
            settings.filter_mode
        },
        update_needed,
    };

    let mut results = app.query_results(&all_entries);

    let index = 'render: loop {
        let initial_input = app.input.as_str().to_owned();
        let initial_filter_mode = app.filter_mode;

        // Handle input
        if let Event::Input(input) = events.next()? {
            if let Some(i) = app.handle_input(settings, &input, results.len()) {
                break 'render i;
            }
        }

        // After we receive input process the whole event channel before query/render.
        while let Ok(Event::Input(input)) = events.try_next() {
            if let Some(i) = app.handle_input(settings, &input, results.len()) {
                break 'render i;
            }
        }

        if initial_input != app.input.as_str() || initial_filter_mode != app.filter_mode {
            results = app.query_results(&all_entries);
        }

        let compact = match settings.style {
            atuin_client::settings::Style::Auto => {
                terminal.size().map(|size| size.height < 14).unwrap_or(true)
            }
            atuin_client::settings::Style::Compact => true,
            atuin_client::settings::Style::Full => false,
        };
        if compact {
            terminal.draw(|f| app.draw_compact(f, &results))?;
        } else {
            terminal.draw(|f| app.draw(f, &results))?;
        }
    };

    if index < results.len() {
        // index is in bounds so we return that entry
        Ok(results.swap_remove(index).0.command.clone())
    } else if index == RETURN_ORIGINAL {
        Ok(String::new())
    } else {
        // Either:
        // * index == RETURN_QUERY, in which case we should return the input
        // * out of bounds -> usually implies no selected entry so we return the input
        Ok(app.input.into_inner())
    }
}
