//! A small inline, arrow-key selector drawn on /dev/tty (no extra crates).
//! Up/down or ctrl-p/ctrl-n move, typing filters, enter picks, esc/ctrl-c cancels.

use anyhow::{Context, Result, bail};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;

pub struct Item {
    /// The rendered line (may contain ANSI colors).
    pub text: String,
    /// What picking this line returns; `None` for decoration that can't be selected.
    pub key: Option<String>,
}

/// Restores the terminal however the picker exits.
struct Raw {
    tty: File,
    saved: libc::termios,
}

impl Raw {
    fn enter() -> Result<Raw> {
        let tty = OpenOptions::new().read(true).write(true).open("/dev/tty").context("no terminal to pick from")?;
        let fd = tty.as_raw_fd();
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
            bail!("not a terminal");
        }
        let mut raw = saved;
        unsafe { libc::cfmakeraw(&mut raw) };
        unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &raw) };
        let mut r = Raw { tty, saved };
        // Hide the cursor and disable line wrap so every item is exactly one row.
        r.tty.write_all(b"\x1b[?25l\x1b[?7l")?;
        Ok(r)
    }

    fn height(&self) -> usize {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        let ok = unsafe { libc::ioctl(self.tty.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) } == 0;
        if ok && ws.ws_row > 0 { ws.ws_row as usize } else { 24 }
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        let _ = self.tty.write_all(b"\x1b[?25h\x1b[?7h");
        unsafe { libc::tcsetattr(self.tty.as_raw_fd(), libc::TCSAFLUSH, &self.saved) };
    }
}

enum Key {
    Up,
    Down,
    Enter,
    Cancel,
    Backspace,
    /// Typed or pasted text (several characters can arrive in one read).
    Text(String),
    Other,
}

fn read_key(tty: &mut File) -> Result<Key> {
    let mut buf = [0u8; 32];
    let n = tty.read(&mut buf)?;
    Ok(match &buf[..n] {
        [] | [3] | [27] | [4] => Key::Cancel,
        [13] | [10] => Key::Enter,
        [127] | [8] => Key::Backspace,
        [16] | [27, b'[', b'A', ..] | [27, b'O', b'A', ..] => Key::Up,
        [14] | [27, b'[', b'B', ..] | [27, b'O', b'B', ..] => Key::Down,
        bytes if bytes.iter().all(|c| c.is_ascii_graphic() || *c == b' ') => {
            Key::Text(String::from_utf8_lossy(bytes).into_owned())
        }
        _ => Key::Other,
    })
}

/// Returns the chosen key, or `None` if the user cancelled.
pub fn pick(prompt: &str, items: &[Item], initial: Option<&str>) -> Result<Option<String>> {
    if !items.iter().any(|i| i.key.is_some()) {
        bail!("nothing to pick from");
    }
    let mut term = Raw::enter()?;
    let mut query = String::new();
    let mut selected = initial.and_then(|k| items.iter().position(|i| i.key.as_deref() == Some(k)));
    let mut top = 0usize;
    let mut drawn = 0usize;

    let result = loop {
        // With a filter, show only matching selectable lines; otherwise the whole graph.
        let q = query.to_lowercase();
        let visible: Vec<usize> = (0..items.len())
            .filter(|&i| q.is_empty() || items[i].key.as_ref().is_some_and(|k| k.to_lowercase().contains(&q)))
            .collect();
        let selectable: Vec<usize> = visible.iter().copied().filter(|&i| items[i].key.is_some()).collect();
        if selected.is_none_or(|s| !selectable.contains(&s)) {
            selected = selectable.first().copied();
        }
        let sel_pos = selected.and_then(|s| visible.iter().position(|&i| i == s)).unwrap_or(0);

        let rows = term.height().saturating_sub(2).max(3);
        if sel_pos < top {
            top = sel_pos;
        } else if sel_pos >= top + rows {
            top = sel_pos + 1 - rows;
        }
        top = top.min(visible.len().saturating_sub(rows));

        let mut frame = String::new();
        if drawn > 1 {
            frame += &format!("\x1b[{}A", drawn - 1);
        }
        frame += "\r\x1b[J";
        let hint = if query.is_empty() { crate::ui::dim("(type to filter, ↑↓ move, enter select, esc cancel)") } else { String::new() };
        frame += &format!("{} {}{query} {hint}", crate::ui::cyan("?"), crate::ui::bold(prompt));
        let window = &visible[top..visible.len().min(top + rows)];
        for &i in window {
            let mark = if Some(i) == selected { crate::ui::cyan("❯ ") } else { "  ".to_string() };
            frame += &format!("\r\n{mark}{}", items[i].text);
        }
        if visible.is_empty() {
            frame += &format!("\r\n  {}", crate::ui::dim("no matches"));
        }
        drawn = 1 + window.len().max(visible.is_empty() as usize);
        term.tty.write_all(frame.as_bytes())?;
        term.tty.flush()?;

        let pos = selected.and_then(|s| selectable.iter().position(|&i| i == s));
        match read_key(&mut term.tty)? {
            Key::Up => {
                if let Some(p) = pos {
                    selected = Some(selectable[p.saturating_sub(1)]);
                }
            }
            Key::Down => {
                if let Some(p) = pos {
                    selected = Some(selectable[(p + 1).min(selectable.len() - 1)]);
                }
            }
            Key::Enter => {
                if let Some(s) = selected {
                    break items[s].key.clone();
                }
            }
            Key::Cancel => break None,
            Key::Backspace => {
                query.pop();
            }
            Key::Text(t) => {
                query.push_str(&t);
                top = 0;
            }
            Key::Other => {}
        }
    };
    // Erase the picker so only the command's own output remains.
    let mut clear = String::new();
    if drawn > 1 {
        clear += &format!("\x1b[{}A", drawn - 1);
    }
    clear += "\r\x1b[J";
    term.tty.write_all(clear.as_bytes())?;
    Ok(result)
}

/// Asks a yes/no question on the terminal; anything but y/yes is no.
pub fn confirm(question: &str) -> Result<bool> {
    let mut tty = OpenOptions::new().read(true).write(true).open("/dev/tty").context("no terminal to confirm on (use --force)")?;
    write!(tty, "{} {question} [y/N] ", crate::ui::yellow("?"))?;
    tty.flush()?;
    let mut line = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(&mut tty), &mut line)?;
    Ok(matches!(line.trim().to_lowercase().as_str(), "y" | "yes"))
}
