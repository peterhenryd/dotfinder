//! Terminal output and prompts.

use std::io::{BufRead, IsTerminal, Write};

fn color_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

fn paint(code: &str, s: &str) -> String {
    if color_enabled() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
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
pub fn cyan(s: &str) -> String {
    paint("36", s)
}

pub fn heading(s: &str) {
    println!("\n{}", bold(s));
}

pub fn warn(s: &str) {
    eprintln!("{} {s}", yellow("warning:"));
}

/// Shortens a string to `max` characters for one-line display.
pub fn clip(s: &str, max: usize) -> String {
    let flat: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if flat.chars().count() <= max {
        flat
    } else {
        let mut out: String = flat.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Asks for one of the given single-letter choices. Returns the chosen
/// letter, or `None` on end of input.
pub fn choose(prompt: &str, choices: &[(char, &str)]) -> Option<char> {
    let menu: Vec<String> = choices
        .iter()
        .map(|(c, label)| format!("[{c}] {label}"))
        .collect();
    let stdin = std::io::stdin();
    loop {
        print!("{prompt} {} ", dim(&menu.join("  ")));
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).ok()? == 0 {
            println!();
            return None;
        }
        let answer = line.trim();
        if let Some(c) = answer.chars().next()
            && answer.chars().count() == 1
            && choices.iter().any(|(k, _)| *k == c)
        {
            return Some(c);
        }
        println!(
            "Type one of: {}",
            choices
                .iter()
                .map(|(c, _)| c.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

/// Reads a line without echoing it, for the vault's master password.
pub fn read_password(prompt: &str) -> std::io::Result<String> {
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let tty = std::fs::File::open("/dev/tty");
    let echo_off = match &tty {
        Ok(t) => std::process::Command::new("stty")
            .arg("-echo")
            .stdin(t.try_clone()?)
            .status()
            .map(|s| s.success())
            .unwrap_or(false),
        Err(_) => false,
    };
    let mut line = String::new();
    let result = match &tty {
        Ok(t) => std::io::BufReader::new(t.try_clone()?).read_line(&mut line),
        Err(_) => std::io::stdin().lock().read_line(&mut line),
    };
    if echo_off && let Ok(t) = &tty {
        let _ = std::process::Command::new("stty")
            .arg("echo")
            .stdin(t.try_clone()?)
            .status();
    }
    eprintln!();
    result?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}
