use std::env;

use color_eyre::owo_colors::OwoColorize;
use px_core::CommandStatus;

pub struct Style {
    enabled: bool,
}

impl Style {
    pub fn new(force_no_color: bool, is_tty: bool) -> Self {
        let env_no_color = env::var_os("NO_COLOR").is_some();
        Self {
            enabled: !(force_no_color || env_no_color) && is_tty,
        }
    }

    pub fn status(&self, status: &CommandStatus, text: &str) -> String {
        let (symbol, tone) = match status {
            CommandStatus::Ok => ("✔", Tone::Green),
            CommandStatus::UserError => ("✗", Tone::Yellow),
            CommandStatus::Failure => ("✖", Tone::Red),
        };
        let line = format!("{symbol} {text}");
        self.paint(&line, tone, true)
    }

    pub fn info(&self, text: &str) -> String {
        self.paint(text, Tone::Blue, false)
    }

    pub fn traceback_header(&self, text: &str) -> String {
        if !self.enabled {
            return text.to_string();
        }
        text.cyan().bold().to_string()
    }

    pub fn traceback_location(&self, file: &str, line: u32, function: &str) -> String {
        if !self.enabled {
            return format!("  File \"{file}\", line {line}, in {function}");
        }
        let file_part = format!("\"{file}\"").cyan().underline().to_string();
        let func_part = function.bold().to_string();
        format!("  File {file_part}, line {line}, in {func_part}")
    }

    pub fn traceback_code(&self, code: &str) -> String {
        if !self.enabled {
            return format!("    {code}");
        }
        format!("    {}", code.dimmed())
    }

    pub fn traceback_error(&self, error_type: &str, message: &str) -> String {
        let line = if message.is_empty() {
            error_type.to_string()
        } else {
            format!("{error_type}: {message}")
        };
        self.paint(&line, Tone::Red, true)
    }

    pub fn traceback_hint(&self, hint: &str) -> String {
        if !self.enabled {
            return format!("px ▸ Hint: {hint}");
        }
        let prefix = "px ▸ Hint:".cyan().bold().to_string();
        format!("{prefix} {hint}")
    }

    pub fn table_header(&self, text: &str) -> String {
        if !self.enabled {
            return text.to_string();
        }
        text.bold().to_string()
    }

    fn paint(&self, text: &str, tone: Tone, bold: bool) -> String {
        if !self.enabled {
            return text.to_string();
        }
        match tone {
            Tone::Green => {
                if bold {
                    text.green().bold().to_string()
                } else {
                    text.green().to_string()
                }
            }
            Tone::Yellow => {
                if bold {
                    text.yellow().bold().to_string()
                } else {
                    text.yellow().to_string()
                }
            }
            Tone::Red => {
                if bold {
                    text.red().bold().to_string()
                } else {
                    text.red().to_string()
                }
            }
            Tone::Blue => {
                if bold {
                    text.cyan().bold().to_string()
                } else {
                    text.cyan().to_string()
                }
            }
        }
    }
}

enum Tone {
    Green,
    Yellow,
    Red,
    Blue,
}
