// SPDX-License-Identifier: Apache-2.0

use std::io::{self, Stdout};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};

type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

pub fn run_tui() -> io::Result<()> {
    let mut terminal = TerminalSession::enter()?;
    run_app(terminal.terminal_mut())
}

fn run_app(terminal: &mut TuiTerminal) -> io::Result<()> {
    loop {
        terminal.draw(render)?;

        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(key) if should_quit(key) => return Ok(()),
                _ => {}
            }
        }
    }
}

fn render(frame: &mut Frame<'_>) {
    let area = frame.area();
    let [_, content, _] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(7),
            Constraint::Fill(1),
        ])
        .areas(area);

    let title = Line::from(vec![
        Span::styled(
            "Hotpath",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" local codebase intelligence"),
    ]);
    let body = Paragraph::new(vec![
        title,
        Line::raw(""),
        Line::raw("TUI runtime skeleton is active."),
        Line::raw("Press q or Esc to exit."),
    ])
    .alignment(Alignment::Center)
    .block(
        Block::default()
            .title(" Hotpath TUI ")
            .borders(Borders::ALL),
    );

    frame.render_widget(body, content);
}

fn should_quit(key: KeyEvent) -> bool {
    if key.kind != KeyEventKind::Press {
        return false;
    }

    matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
}

struct TerminalSession {
    terminal: TuiTerminal,
    raw_mode_enabled: bool,
    alternate_screen_enabled: bool,
}

impl TerminalSession {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;

        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error);
        }

        let backend = CrosstermBackend::new(stdout);
        let terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => {
                let mut stdout = io::stdout();
                let _ = execute!(stdout, LeaveAlternateScreen);
                let _ = disable_raw_mode();
                return Err(error);
            }
        };

        Ok(Self {
            terminal,
            raw_mode_enabled: true,
            alternate_screen_enabled: true,
        })
    }

    fn terminal_mut(&mut self) -> &mut TuiTerminal {
        &mut self.terminal
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if self.alternate_screen_enabled {
            let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
            self.alternate_screen_enabled = false;
        }

        if self.raw_mode_enabled {
            let _ = disable_raw_mode();
            self.raw_mode_enabled = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_keys_are_q_and_escape_key_presses() {
        assert!(should_quit(KeyEvent::from(KeyCode::Char('q'))));
        assert!(should_quit(KeyEvent::from(KeyCode::Esc)));
        assert!(!should_quit(KeyEvent::from(KeyCode::Char('Q'))));
        assert!(!should_quit(KeyEvent::from(KeyCode::Enter)));
    }
}
