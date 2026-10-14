use std::io::IsTerminal;
use std::sync::OnceLock;

fn color_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal())
}

fn paint(code: &str, s: &str) -> String {
    if color_enabled() { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
}

pub fn bold(s: &str) -> String {
    paint("1", s)
}
pub fn dim(s: &str) -> String {
    paint("2", s)
}
pub fn red(s: &str) -> String {
    paint("31", s)
}
pub fn green(s: &str) -> String {
    paint("32", s)
}
pub fn yellow(s: &str) -> String {
    paint("33", s)
}
pub fn blue(s: &str) -> String {
    paint("34", s)
}
pub fn magenta(s: &str) -> String {
    paint("35", s)
}
pub fn cyan(s: &str) -> String {
    paint("36", s)
}

/// OSC 8 hyperlink so PR numbers are clickable in terminals that support it.
pub fn link(url: &str, text: &str) -> String {
    if color_enabled() { format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\") } else { text.to_string() }
}

pub fn info(msg: &str) {
    eprintln!("{} {msg}", cyan("›"));
}

pub fn ok(msg: &str) {
    eprintln!("{} {msg}", green("✓"));
}

pub fn warn(msg: &str) {
    eprintln!("{} {msg}", yellow("!"));
}

pub fn short(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}
