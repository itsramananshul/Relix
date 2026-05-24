//! `relix setup` — guided interactive wizard.
//!
//! Five pages: welcome → provider → API key → channels → confirm.
//! crossterm-driven raw input so the same flow works under Windows
//! Terminal, PowerShell, macOS Terminal, GNOME Terminal, and any
//! curl|bash piped invocation that still has `/dev/tty`.
//!
//! Ctrl-C at any page prints a clean cancellation message and exits
//! 130 without trashing the terminal — every render path runs inside
//! a guard that disables raw mode on drop.

use std::io::{self, Write};

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute, queue,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{self, Clear, ClearType},
};

use crate::config::{ChannelsConfig, ProviderConfig, RelixConfig, mask_api_key};

/// Top-level entry from `main.rs`.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let _raw = RawGuard::new()?;

    welcome()?;
    let provider_name = pick_provider()?;
    let needs_key = !matches!(provider_name.as_str(), "mock" | "local");
    let api_key = if needs_key {
        prompt_api_key(&provider_name)?
    } else {
        String::new()
    };
    let channels = pick_channels()?;

    let cfg = RelixConfig {
        provider: ProviderConfig {
            name: provider_name,
            api_key,
        },
        channels,
        ..RelixConfig::default()
    };

    // `confirm` is a single Enter-or-cancel page; the only way out
    // other than confirmation is the cancel branch inside, which
    // diverges. So we don't need to branch on its return value.
    confirm(&cfg)?;

    // Final validation — empty key on a non-mock provider, missing
    // channel id, etc. The wizard already prevents most of these but
    // a paranoid double-check costs nothing and surfaces operator
    // edits to config.toml that arrived through a different path.
    let errs = cfg.validate();
    if !errs.is_empty() {
        leave_raw()?;
        eprintln!("Configuration invalid:");
        for e in &errs {
            eprintln!("  - {e}");
        }
        return Err("invalid setup state".into());
    }

    let path = RelixConfig::default_path();
    cfg.save_to(&path)?;

    leave_raw()?;
    println!();
    println!("Saved configuration to {}", path.display());
    println!();
    println!("Next steps:");
    println!("  relix boot     # start the mesh now");
    println!("  relix stop     # stop it");
    println!("  relix status   # check on it later");
    println!();
    Ok(())
}

// ---- pages ---------------------------------------------------------------

fn welcome() -> io::Result<()> {
    let mut out = io::stdout();
    clear_screen(&mut out)?;
    let lines = [
        "╔══════════════════════════════════════════╗",
        "║      RELIX — Relay Intelligence          ║",
        "║              Exchange  v0.1.0            ║",
        "║                                          ║",
        "║         The OS for AI Agents             ║",
        "║                                          ║",
        "║      Press Enter to begin setup          ║",
        "║      (Ctrl-C to cancel)                  ║",
        "╚══════════════════════════════════════════╝",
    ];
    for (i, line) in lines.iter().enumerate() {
        queue!(out, cursor::MoveTo(2, 2 + i as u16))?;
        queue!(out, SetForegroundColor(Color::Yellow))?;
        queue!(out, Print(line))?;
    }
    queue!(out, ResetColor)?;
    out.flush()?;
    wait_for_enter_or_cancel()?;
    Ok(())
}

const PROVIDER_CHOICES: &[(&str, &str)] = &[
    (
        "openrouter",
        "OpenRouter   (recommended — access to all models)",
    ),
    ("openai", "OpenAI"),
    ("anthropic", "Anthropic"),
    ("xai", "xAI (Grok)"),
    ("gemini", "Gemini"),
    (
        "local",
        "Local       (Ollama or any OpenAI-compatible endpoint)",
    ),
    ("mock", "Mock        (no API key — for testing)"),
];

fn pick_provider() -> io::Result<String> {
    let mut idx: usize = 0;
    let mut out = io::stdout();
    loop {
        clear_screen(&mut out)?;
        queue!(out, cursor::MoveTo(2, 1))?;
        queue!(out, SetForegroundColor(Color::Cyan))?;
        queue!(out, Print("Choose your AI provider"))?;
        queue!(out, ResetColor)?;
        queue!(out, cursor::MoveTo(2, 2))?;
        queue!(out, Print("(arrow keys, Enter to confirm)"))?;
        for (i, (_, label)) in PROVIDER_CHOICES.iter().enumerate() {
            queue!(out, cursor::MoveTo(2, 4 + i as u16))?;
            if i == idx {
                queue!(out, SetForegroundColor(Color::Yellow))?;
                queue!(out, Print(format!("> {label}")))?;
                queue!(out, ResetColor)?;
            } else {
                queue!(out, Print(format!("  {label}")))?;
            }
        }
        out.flush()?;

        match read_key()? {
            Key::Up => idx = idx.saturating_sub(1),
            Key::Down if idx + 1 < PROVIDER_CHOICES.len() => idx += 1,
            Key::Enter => return Ok(PROVIDER_CHOICES[idx].0.to_string()),
            Key::Cancel => cancel("Setup cancelled. Run `relix setup` to configure Relix."),
            _ => {}
        }
    }
}

fn prompt_api_key(provider: &str) -> io::Result<String> {
    let mut buf = String::new();
    let mut error: Option<String> = None;
    let mut out = io::stdout();
    loop {
        clear_screen(&mut out)?;
        queue!(out, cursor::MoveTo(2, 1))?;
        queue!(out, SetForegroundColor(Color::Cyan))?;
        queue!(out, Print(format!("Enter your {provider} API key")))?;
        queue!(out, ResetColor)?;
        queue!(out, cursor::MoveTo(2, 2))?;
        queue!(
            out,
            Print("(input is hidden; Enter to confirm, Ctrl-C to cancel)")
        )?;
        queue!(out, cursor::MoveTo(2, 4))?;
        queue!(out, Print(format!("> {}", "•".repeat(buf.chars().count()))))?;
        if let Some(e) = &error {
            queue!(out, cursor::MoveTo(2, 6))?;
            queue!(out, SetForegroundColor(Color::Red))?;
            queue!(out, Print(e))?;
            queue!(out, ResetColor)?;
        }
        out.flush()?;

        match read_key()? {
            Key::Char(c) => buf.push(c),
            Key::Backspace => {
                buf.pop();
                error = None;
            }
            Key::Enter => {
                if buf.trim().is_empty() {
                    error = Some(
                        "API key cannot be empty. Paste your key, or Ctrl-C to cancel.".into(),
                    );
                } else {
                    return Ok(buf);
                }
            }
            Key::Cancel => cancel("Setup cancelled. Run `relix setup` to configure Relix."),
            _ => {}
        }
    }
}

const CHANNEL_LABELS: &[(&str, &str)] = &[
    ("telegram", "Telegram"),
    ("discord", "Discord"),
    ("slack", "Slack"),
];

fn pick_channels() -> io::Result<ChannelsConfig> {
    let mut selected = [false; 3];
    let mut idx: usize = 0;
    let mut out = io::stdout();
    loop {
        clear_screen(&mut out)?;
        queue!(out, cursor::MoveTo(2, 1))?;
        queue!(out, SetForegroundColor(Color::Cyan))?;
        queue!(out, Print("Connect messaging channels (optional)"))?;
        queue!(out, ResetColor)?;
        queue!(out, cursor::MoveTo(2, 2))?;
        queue!(
            out,
            Print("Space to toggle, arrow keys to move, Enter to continue")
        )?;
        for (i, (_, label)) in CHANNEL_LABELS.iter().enumerate() {
            queue!(out, cursor::MoveTo(2, 4 + i as u16))?;
            let mark = if selected[i] { 'x' } else { ' ' };
            let cursor = if i == idx { "> " } else { "  " };
            if i == idx {
                queue!(out, SetForegroundColor(Color::Yellow))?;
                queue!(out, Print(format!("{cursor}[{mark}] {label}")))?;
                queue!(out, ResetColor)?;
            } else {
                queue!(out, Print(format!("{cursor}[{mark}] {label}")))?;
            }
        }
        queue!(out, cursor::MoveTo(2, 8))?;
        queue!(
            out,
            Print("(leave all unchecked to skip — channels can be added later via `relix setup`)")
        )?;
        out.flush()?;

        match read_key()? {
            Key::Up => idx = idx.saturating_sub(1),
            Key::Down if idx + 1 < CHANNEL_LABELS.len() => idx += 1,
            Key::Space => selected[idx] = !selected[idx],
            Key::Enter => break,
            Key::Cancel => cancel("Setup cancelled. Run `relix setup` to configure Relix."),
            _ => {}
        }
    }

    let mut ch = ChannelsConfig::default();
    if selected[0] {
        ch.telegram = true;
        ch.telegram_token = prompt_secret(
            "Enter your Telegram bot token",
            "(get one from @BotFather on Telegram; Ctrl-C to cancel)",
        )?;
    }
    if selected[1] {
        ch.discord = true;
        ch.discord_token = prompt_secret(
            "Enter your Discord bot token",
            "(Developer Portal → your application → Bot → Reset Token)",
        )?;
        ch.discord_channel = prompt_secret(
            "Enter your Discord channel ID",
            "(right-click the channel in Discord → Copy Channel ID; enable Developer Mode if missing)",
        )?;
    }
    if selected[2] {
        ch.slack = true;
        ch.slack_token = prompt_secret(
            "Enter your Slack bot token",
            "(starts with xoxb-...; OAuth & Permissions → Bot User OAuth Token)",
        )?;
        ch.slack_channel = prompt_secret(
            "Enter your Slack channel ID",
            "(right-click the channel → View channel details → bottom of the popout)",
        )?;
    }
    Ok(ch)
}

/// Generic single-line hidden-input prompt, used for channel tokens.
fn prompt_secret(title: &str, hint: &str) -> io::Result<String> {
    let mut buf = String::new();
    let mut error: Option<String> = None;
    let mut out = io::stdout();
    loop {
        clear_screen(&mut out)?;
        queue!(out, cursor::MoveTo(2, 1))?;
        queue!(out, SetForegroundColor(Color::Cyan))?;
        queue!(out, Print(title))?;
        queue!(out, ResetColor)?;
        queue!(out, cursor::MoveTo(2, 2))?;
        queue!(out, Print(hint))?;
        queue!(out, cursor::MoveTo(2, 4))?;
        queue!(out, Print(format!("> {}", "•".repeat(buf.chars().count()))))?;
        if let Some(e) = &error {
            queue!(out, cursor::MoveTo(2, 6))?;
            queue!(out, SetForegroundColor(Color::Red))?;
            queue!(out, Print(e))?;
            queue!(out, ResetColor)?;
        }
        out.flush()?;
        match read_key()? {
            Key::Char(c) => buf.push(c),
            Key::Backspace => {
                buf.pop();
                error = None;
            }
            Key::Enter => {
                if buf.trim().is_empty() {
                    error = Some("Required. Paste the value or Ctrl-C to cancel.".into());
                } else {
                    return Ok(buf);
                }
            }
            Key::Cancel => cancel("Setup cancelled. Run `relix setup` to configure Relix."),
            _ => {}
        }
    }
}

fn confirm(cfg: &RelixConfig) -> io::Result<bool> {
    let mut out = io::stdout();
    clear_screen(&mut out)?;
    let mut row = 1u16;
    queue!(out, cursor::MoveTo(2, row))?;
    queue!(out, SetForegroundColor(Color::Cyan))?;
    queue!(out, Print("Ready to save configuration"))?;
    queue!(out, ResetColor)?;
    row += 2;

    queue!(out, cursor::MoveTo(2, row))?;
    queue!(out, Print(format!("Provider:  {}", cfg.provider.name)))?;
    row += 1;

    if !cfg.provider.api_key.is_empty() {
        queue!(out, cursor::MoveTo(2, row))?;
        queue!(
            out,
            Print(format!(
                "API key:   {}",
                mask_api_key(&cfg.provider.api_key)
            ))
        )?;
        row += 1;
    }

    let mut channel_summary: Vec<&str> = Vec::new();
    if cfg.channels.telegram {
        channel_summary.push("Telegram");
    }
    if cfg.channels.discord {
        channel_summary.push("Discord");
    }
    if cfg.channels.slack {
        channel_summary.push("Slack");
    }
    let channel_str = if channel_summary.is_empty() {
        "(none)".to_string()
    } else {
        channel_summary.join(", ")
    };
    queue!(out, cursor::MoveTo(2, row))?;
    queue!(out, Print(format!("Channels:  {channel_str}")))?;
    row += 2;

    queue!(out, cursor::MoveTo(2, row))?;
    queue!(out, Print("Press Enter to save, Ctrl-C to cancel."))?;
    out.flush()?;
    wait_for_enter_or_cancel()?;
    Ok(true)
}

// ---- key + terminal helpers ---------------------------------------------

enum Key {
    Char(char),
    Up,
    Down,
    Enter,
    Backspace,
    Space,
    Cancel,
    Other,
}

fn read_key() -> io::Result<Key> {
    loop {
        match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press || k.kind == KeyEventKind::Repeat => {
                if k.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(k.code, KeyCode::Char('c') | KeyCode::Char('C'))
                {
                    return Ok(Key::Cancel);
                }
                return Ok(match k.code {
                    KeyCode::Up => Key::Up,
                    KeyCode::Down => Key::Down,
                    KeyCode::Enter => Key::Enter,
                    KeyCode::Backspace => Key::Backspace,
                    KeyCode::Esc => Key::Cancel,
                    KeyCode::Char(' ') => Key::Space,
                    KeyCode::Char(c) => Key::Char(c),
                    _ => Key::Other,
                });
            }
            _ => continue,
        }
    }
}

fn wait_for_enter_or_cancel() -> io::Result<()> {
    loop {
        match read_key()? {
            Key::Enter => return Ok(()),
            Key::Cancel => cancel("Setup cancelled. Run `relix setup` to configure Relix."),
            _ => {}
        }
    }
}

fn clear_screen(out: &mut io::Stdout) -> io::Result<()> {
    execute!(out, Clear(ClearType::All), cursor::MoveTo(0, 0))
}

fn leave_raw() -> io::Result<()> {
    let _ = terminal::disable_raw_mode();
    let mut out = io::stdout();
    execute!(out, cursor::Show, ResetColor)?;
    Ok(())
}

fn cancel(msg: &str) -> ! {
    let _ = leave_raw();
    eprintln!();
    eprintln!("{msg}");
    std::process::exit(130);
}

/// RAII guard that flips the terminal into raw mode on construction
/// and unconditionally restores it on drop — so a panic mid-wizard
/// doesn't leave the operator's shell in a broken state.
struct RawGuard;

impl RawGuard {
    fn new() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut out = io::stdout();
        execute!(out, cursor::Hide)?;
        Ok(Self)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let mut out = io::stdout();
        let _ = execute!(out, cursor::Show, ResetColor);
    }
}
