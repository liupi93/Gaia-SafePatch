//! Presentation only: two footer lines, no state synthesis or safety decisions.
use std::io::{self, IsTerminal, Write};

pub struct Terminal {
    origin: u64,
    tty: bool,
    color: bool,
    drawn: bool,
    status: String,
    status_key: String,
    prompt: String,
    saved_termios: Option<libc::termios>,
}
impl Terminal {
    pub fn new(origin: u64) -> Self {
        let tty = io::stdout().is_terminal() && std::env::var("TERM").as_deref() != Ok("dumb");
        let mut saved_termios = None;
        if tty && io::stdin().is_terminal() {
            let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
            if unsafe { libc::tcgetattr(0, &mut original) } == 0 {
                let mut quiet = original;
                quiet.c_lflag &= !(libc::ECHO | libc::ECHONL);
                if unsafe { libc::tcsetattr(0, libc::TCSANOW, &quiet) } == 0 {
                    saved_termios = Some(original);
                }
            }
        }
        Self {
            origin,
            tty,
            color: tty && std::env::var_os("NO_COLOR").is_none(),
            drawn: false,
            status: String::new(),
            status_key: String::new(),
            prompt: String::new(),
            saved_termios,
        }
    }
    fn stamp(&self, at: u64, text: &str) -> String {
        format!(
            "+{:.3}s  {text}",
            at.saturating_sub(self.origin) as f64 / 1e9
        )
    }
    fn paint(&self, text: &str, code: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.into()
        }
    }
    fn fit(&self, text: &str) -> String {
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        let width =
            if unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut size) } == 0 && size.ws_col > 0 {
                size.ws_col as usize
            } else {
                100
            };
        let mut used = 0;
        text.chars()
            .take_while(|c| {
                used += if c.is_ascii() { 1 } else { 2 };
                used < width
            })
            .collect()
    }
    fn clear(&mut self) -> io::Result<()> {
        if self.tty && self.drawn {
            // Cursor is at the end of the prompt, directly below the status.
            print!("\r\x1b[2K\x1b[1A\r\x1b[2K");
            self.drawn = false;
        }
        Ok(())
    }
    fn footer(&mut self) -> io::Result<()> {
        if self.tty && (!self.status.is_empty() || !self.prompt.is_empty()) {
            print!(
                "{}\r\n{}",
                self.paint(&self.fit(&self.status), "33"),
                self.fit(&self.prompt)
            );
            self.drawn = true;
        }
        io::stdout().flush()
    }
    pub fn event(&mut self, at: u64, text: &str, kind: &str) -> io::Result<()> {
        self.clear()?;
        let color = match kind {
            "patch_success" => "32",
            "patch_failed" | "patch_refused" | "demo_stopped" => "31",
            _ => "0",
        };
        println!("{}", self.paint(&self.stamp(at, text), color));
        self.footer()
    }
    pub fn status(&mut self, at: u64, key: &str, text: &str) -> io::Result<()> {
        let changed = self.status_key != key;
        self.status_key = key.into();
        if self.tty {
            self.clear()?;
            self.status = self.stamp(at, text);
            self.footer()
        } else {
            // Pipes record transitions, not hundreds of busy-count snapshots.
            if changed {
                println!("{}", self.stamp(at, text));
            }
            io::stdout().flush()
        }
    }
    pub fn clear_status(&mut self) -> io::Result<()> {
        self.clear()?;
        self.status.clear();
        self.status_key.clear();
        self.footer()
    }
    pub fn prompt(&mut self, text: Option<&str>) -> io::Result<()> {
        self.clear()?;
        self.prompt = text.map(|t| format!("[Enter] {t}")).unwrap_or_default();
        if !self.tty && !self.prompt.is_empty() {
            println!("{}", self.prompt);
        }
        self.footer()
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.clear();
        if let Some(saved) = &self.saved_termios {
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, saved);
            }
        }
        let _ = io::stdout().flush();
    }
}
