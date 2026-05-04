pub struct OutputMode {
    pub color: bool,
    pub ascii: bool,
}

pub enum Status {
    Ok,
    Warn,
    Fail,
    Unknown,
    Skipped,
}

impl Status {
    pub fn icon(&self, mode: &OutputMode) -> &'static str {
        if mode.ascii {
            match self {
                Status::Ok => "ok",
                Status::Warn => "warn",
                Status::Fail => "fail",
                Status::Unknown => "?",
                Status::Skipped => ".",
            }
        } else {
            match self {
                Status::Ok => "✓",
                Status::Warn => "!",
                Status::Fail => "✗",
                Status::Unknown => "?",
                Status::Skipped => "·",
            }
        }
    }

    pub fn style(&self, text: &str, mode: &OutputMode) -> String {
        use owo_colors::OwoColorize;
        if !mode.color {
            return text.to_string();
        }
        match self {
            Status::Ok => text.bright_green().to_string(),
            Status::Warn => text.bright_yellow().to_string(),
            Status::Fail => text.bright_red().to_string(),
            Status::Unknown | Status::Skipped => text.bright_black().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_icons() {
        let mode = OutputMode { color: false, ascii: true };
        assert_eq!(Status::Ok.icon(&mode), "ok");
        assert_eq!(Status::Warn.icon(&mode), "warn");
        assert_eq!(Status::Fail.icon(&mode), "fail");
        assert_eq!(Status::Unknown.icon(&mode), "?");
        assert_eq!(Status::Skipped.icon(&mode), ".");
    }

    #[test]
    fn unicode_icons() {
        let mode = OutputMode { color: false, ascii: false };
        assert_eq!(Status::Ok.icon(&mode), "✓");
        assert_eq!(Status::Warn.icon(&mode), "!");
        assert_eq!(Status::Fail.icon(&mode), "✗");
        assert_eq!(Status::Unknown.icon(&mode), "?");
        assert_eq!(Status::Skipped.icon(&mode), "·");
    }
}
