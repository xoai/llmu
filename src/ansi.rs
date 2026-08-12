//! Minimal ANSI styling for the plain CLI (the TUI uses ratatui styles).
//! Honors NO_COLOR, CLICOLOR_FORCE=1, and only colors real terminals.

use std::io::IsTerminal;

pub fn on() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if std::env::var("CLICOLOR_FORCE").ok().as_deref() == Some("1") {
        return true;
    }
    std::io::stdout().is_terminal()
}

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const RED: &str = "\x1b[31m";
pub const GREEN: &str = "\x1b[32m";
pub const YELLOW: &str = "\x1b[33m";
pub const BLUE: &str = "\x1b[34m";
pub const MAGENTA: &str = "\x1b[35m";
pub const CYAN: &str = "\x1b[36m";

pub fn paint(s: &str, code: &str) -> String {
    paint_when(s, code, on())
}

/// Paint only when `enabled` is true — the caller decides, so color
/// decisions are deterministic and testable without touching the env.
pub(crate) fn paint_when(s: &str, code: &str, enabled: bool) -> String {
    if enabled {
        format!("{code}{s}{RESET}")
    } else {
        s.to_string()
    }
}

/// Threshold color for a percentage meter: calm below 60, warn to 85, hot above.
pub fn pct_color(p: f64) -> &'static str {
    if p < 60.0 {
        GREEN
    } else if p < 85.0 {
        YELLOW
    } else {
        RED
    }
}

/// Stable color per provider so the eye can track rows across views.
pub fn provider_color(id: &str) -> &'static str {
    match id {
        "anthropic" | "claude" => MAGENTA,
        "openai" | "codex" => GREEN,
        // BLUE is shared with gemini: 8-color terminals can't render LightBlue, so the
        // plain CLI collapses the two; tui::provider_color distinguishes them (Blue/LightBlue).
        "deepseek" => BLUE,
        "kimi" => CYAN,
        "glm" => YELLOW,
        "gemini" => BLUE,
        "qwen" => RED,
        _ => "",
    }
}

/// Line-level colorization of a rendered usage table: bold cyan header,
/// dim separators, bold TOTAL. Cell padding is untouched.
pub fn table(s: &str) -> String {
    if !on() {
        return s.to_string();
    }
    let mut out = String::new();
    for (i, line) in s.lines().enumerate() {
        if i == 0 {
            out.push_str(&format!("{BOLD}{CYAN}{line}{RESET}\n"));
        } else if line.starts_with('-') {
            out.push_str(&format!("{DIM}{line}{RESET}\n"));
        } else if line.starts_with("TOTAL") {
            out.push_str(&format!("{BOLD}{line}{RESET}\n"));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paint_when_off_is_identity() {
        assert_eq!(paint_when("abc", GREEN, false), "abc");
    }

    #[test]
    fn paint_when_on_wraps_exactly_like_paint() {
        let enabled = paint_when("abc", GREEN, true);
        assert_eq!(enabled, format!("{GREEN}abc{RESET}"));
    }

    /// FR-6: the plain CLI renders the qwen provider in RED.
    #[test]
    fn provider_color_maps_qwen_to_red() {
        assert_eq!(provider_color("qwen"), RED);
    }
}
