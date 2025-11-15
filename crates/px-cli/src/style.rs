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
        match status {
            CommandStatus::Ok => self.paint(text, (45, 204, 159), true),
            CommandStatus::UserError => self.paint(text, (240, 180, 41), true),
            CommandStatus::Failure => self.paint(text, (224, 69, 95), true),
        }
    }

    pub fn info(&self, text: &str) -> String {
        self.paint(text, (92, 106, 196), false)
    }

    pub fn table_header(&self, text: &str) -> String {
        if !self.enabled {
            return text.to_string();
        }
        text.truecolor(74, 85, 104).bold().to_string()
    }

    fn paint(&self, text: &str, rgb: (u8, u8, u8), bold: bool) -> String {
        if !self.enabled {
            return text.to_string();
        }
        let colored = text.truecolor(rgb.0, rgb.1, rgb.2);
        if bold {
            colored.bold().to_string()
        } else {
            colored.to_string()
        }
    }
}
