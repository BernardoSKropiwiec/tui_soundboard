use std::io::{self, Stdout, stdout};

use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

pub type AppTerminal = Terminal<CrosstermBackend<Stdout>>;

pub struct TerminalSession {
    terminal: AppTerminal,
    restored: bool,
}

impl TerminalSession {
    pub fn new() -> io::Result<Self> {
        enable_raw_mode()?;

        let mut output = stdout();
        if let Err(error) = execute!(output, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error);
        }

        let terminal = match Terminal::new(CrosstermBackend::new(output)) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = disable_raw_mode();
                let mut output = stdout();
                let _ = execute!(output, LeaveAlternateScreen);
                return Err(error);
            }
        };

        Ok(Self {
            terminal,
            restored: false,
        })
    }

    pub fn terminal_mut(&mut self) -> &mut AppTerminal {
        &mut self.terminal
    }

    pub fn restore(&mut self) -> io::Result<()> {
        if self.restored {
            return Ok(());
        }
        let mut first_error = disable_raw_mode().err();
        if let Err(error) = execute!(self.terminal.backend_mut(), LeaveAlternateScreen)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        if let Err(error) = self.terminal.show_cursor()
            && first_error.is_none()
        {
            first_error = Some(error);
        }

        if let Some(error) = first_error {
            Err(error)
        } else {
            self.restored = true;
            Ok(())
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}
