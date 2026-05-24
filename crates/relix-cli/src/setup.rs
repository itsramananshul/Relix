//! `relix setup` — guided interactive wizard. Also reachable as
//! `relix reconfigure` (same flow, alias-only).
//!
//! Five pages: welcome → provider → API key → channels → confirm.
//! Each page after the welcome supports left-arrow / `b` back
//! navigation; the prior page re-renders with the user's last
//! selection pre-filled so going back never costs the user any
//! input they'd already given.
//!
//! When `~/.relix/config.toml` already exists the wizard loads it
//! and pre-fills every field — provider selection, masked current
//! API key (Enter to keep, type to replace), channel toggles,
//! per-channel secrets — so an operator who just wants to flip
//! one switch doesn't have to re-enter the rest.
//!
//! crossterm-driven raw input so the same flow works under Windows
//! Terminal, PowerShell, macOS Terminal, GNOME Terminal, and any
//! curl|bash piped invocation that still has `/dev/tty`. Ctrl-C at
//! any page exits 130 with the terminal restored — every render
//! path runs inside a RAII guard that disables raw mode on drop.

use std::io::{self, Write};

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute, queue,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{self, Clear, ClearType},
};

use crate::config::{ChannelsConfig, MeshConfig, ProviderConfig, RelixConfig, mask_api_key};

/// Top-level entry from `main.rs` for both `relix setup` and
/// `relix reconfigure`.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    // Load the existing config before we touch the terminal so we can
    // pre-fill the wizard. A missing file is the install-time case
    // and is fine; a real I/O / parse error is also non-fatal here —
    // we just start from defaults and the operator overwrites
    // whatever was broken.
    let prior = RelixConfig::load_default().ok().flatten();

    let _raw = RawGuard::new()?;
    let final_cfg = run_wizard(prior.as_ref())?;

    let errs = final_cfg.validate();
    if !errs.is_empty() {
        leave_raw()?;
        eprintln!("Configuration invalid:");
        for e in &errs {
            eprintln!("  - {e}");
        }
        return Err("invalid setup state".into());
    }

    let path = RelixConfig::default_path();
    final_cfg.save_to(&path)?;

    leave_raw()?;
    let verb = if prior.is_some() { "Updated" } else { "Saved" };
    println!();
    println!("{verb} configuration at {}", path.display());
    println!();
    println!("Next steps:");
    println!("  relix boot        # start the mesh now");
    println!("  relix stop        # stop it");
    println!("  relix status      # check on it later");
    println!("  relix reconfigure # re-run this wizard");
    println!();
    Ok(())
}

// ---- page state machine --------------------------------------------------

/// What a page returns to the run loop.
enum PageResult<T> {
    Next(T),
    Back,
}

#[derive(Copy, Clone)]
enum Page {
    Welcome,
    Provider,
    ApiKey,
    Channels,
    Confirm,
}

/// Mutable state threaded across pages so back-navigation always
/// re-renders the prior page with the operator's last confirmed
/// selection still in place.
struct WizardState {
    /// Index into `PROVIDER_CHOICES` — drives the provider page's
    /// pre-selected row.
    provider_idx: usize,
    /// The current API key. Starts as the prior key on a reconfigure
    /// (so "Enter to keep" works), or empty on a fresh install.
    api_key: String,
    /// Per-channel toggles for the multi-select page.
    channels_sel: [bool; 3],
    /// Full channels block including tokens — kept across toggles so
    /// disabling and re-enabling a channel doesn't drop the operator's
    /// existing token.
    channels: ChannelsConfig,
    /// Mesh block carried straight through from the prior config (or
    /// defaults) — the wizard doesn't expose these knobs.
    mesh: MeshConfig,
    /// True when we were initialised from an existing `config.toml`.
    /// Drives diff hints on the confirm page and the "Updated" /
    /// "Saved" verb at the end.
    is_reconfigure: bool,
    /// Snapshot of the prior config, only set on a reconfigure. Used
    /// to diff the confirm page.
    prior: Option<RelixConfig>,
}

impl WizardState {
    fn from_prior(prior: Option<&RelixConfig>) -> Self {
        let p = prior.cloned().unwrap_or_default();
        let provider_idx = PROVIDER_CHOICES
            .iter()
            .position(|(slug, _)| *slug == p.provider.name.as_str())
            .unwrap_or(0);
        Self {
            provider_idx,
            api_key: p.provider.api_key.clone(),
            channels_sel: [p.channels.telegram, p.channels.discord, p.channels.slack],
            channels: p.channels.clone(),
            mesh: p.mesh.clone(),
            is_reconfigure: prior.is_some(),
            prior: prior.cloned(),
        }
    }

    fn provider_name(&self) -> &'static str {
        PROVIDER_CHOICES[self.provider_idx].0
    }

    fn needs_key(&self) -> bool {
        !matches!(self.provider_name(), "mock" | "local")
    }

    fn to_config(&self) -> RelixConfig {
        let mut ch = self.channels.clone();
        ch.telegram = self.channels_sel[0];
        ch.discord = self.channels_sel[1];
        ch.slack = self.channels_sel[2];
        RelixConfig {
            provider: ProviderConfig {
                name: self.provider_name().to_string(),
                api_key: self.api_key.clone(),
            },
            channels: ch,
            mesh: self.mesh.clone(),
        }
    }
}

fn run_wizard(prior: Option<&RelixConfig>) -> io::Result<RelixConfig> {
    let mut state = WizardState::from_prior(prior);
    let mut page = Page::Welcome;

    loop {
        match page {
            Page::Welcome => match welcome()? {
                PageResult::Next(()) => page = Page::Provider,
                PageResult::Back => { /* welcome has no back; stay */ }
            },
            Page::Provider => match pick_provider(state.provider_idx)? {
                PageResult::Next(idx) => {
                    state.provider_idx = idx;
                    page = if state.needs_key() {
                        Page::ApiKey
                    } else {
                        Page::Channels
                    };
                }
                PageResult::Back => page = Page::Welcome,
            },
            Page::ApiKey => match prompt_api_key(state.provider_name(), &state.api_key)? {
                PageResult::Next(key) => {
                    state.api_key = key;
                    page = Page::Channels;
                }
                PageResult::Back => page = Page::Provider,
            },
            Page::Channels => match run_channels_stage(&mut state)? {
                PageResult::Next(()) => page = Page::Confirm,
                PageResult::Back => {
                    page = if state.needs_key() {
                        Page::ApiKey
                    } else {
                        Page::Provider
                    };
                }
            },
            Page::Confirm => match confirm(&state)? {
                PageResult::Next(()) => break,
                PageResult::Back => page = Page::Channels,
            },
        }
    }

    Ok(state.to_config())
}

// ---- pages ---------------------------------------------------------------

fn welcome() -> io::Result<PageResult<()>> {
    let mut out = io::stdout();
    clear_screen(&mut out)?;
    // Build the centred version line into the boxed panel.
    let version_line = pad_box_line(&format!("Exchange  v{}", env!("CARGO_PKG_VERSION")));
    let lines: [&str; 9] = [
        "╔══════════════════════════════════════════╗",
        "║      RELIX — Relay Intelligence          ║",
        version_line.as_str(),
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
    loop {
        match read_key()? {
            Key::Enter => return Ok(PageResult::Next(())),
            Key::Cancel => cancel("Setup cancelled. Run `relix setup` to configure Relix."),
            _ => {}
        }
    }
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

fn pick_provider(initial_idx: usize) -> io::Result<PageResult<usize>> {
    let mut idx = initial_idx.min(PROVIDER_CHOICES.len() - 1);
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
        draw_nav_hint(&mut out, 4 + PROVIDER_CHOICES.len() as u16 + 1)?;
        out.flush()?;

        match read_key()? {
            Key::Up => idx = idx.saturating_sub(1),
            Key::Down if idx + 1 < PROVIDER_CHOICES.len() => idx += 1,
            Key::Enter => return Ok(PageResult::Next(idx)),
            Key::Left | Key::Char('b') | Key::Char('B') => return Ok(PageResult::Back),
            Key::Cancel => cancel("Setup cancelled. Run `relix setup` to configure Relix."),
            _ => {}
        }
    }
}

fn prompt_api_key(provider: &str, current: &str) -> io::Result<PageResult<String>> {
    let mut buf = String::new();
    let mut error: Option<String> = None;
    let mut out = io::stdout();
    let have_current = !current.trim().is_empty();
    loop {
        clear_screen(&mut out)?;
        queue!(out, cursor::MoveTo(2, 1))?;
        queue!(out, SetForegroundColor(Color::Cyan))?;
        queue!(out, Print(format!("Enter your {provider} API key")))?;
        queue!(out, ResetColor)?;
        queue!(out, cursor::MoveTo(2, 2))?;
        let hint = if have_current {
            "(Enter to keep current key, or type to replace; ← back)"
        } else {
            "(input is hidden; Enter to confirm; ← back)"
        };
        queue!(out, Print(hint))?;
        let input_row: u16 = if have_current {
            queue!(out, cursor::MoveTo(2, 4))?;
            queue!(out, Print(format!("Current:  {}", mask_api_key(current))))?;
            6
        } else {
            4
        };
        queue!(out, cursor::MoveTo(2, input_row))?;
        queue!(out, Print(format!("> {}", "•".repeat(buf.chars().count()))))?;
        if let Some(e) = &error {
            queue!(out, cursor::MoveTo(2, input_row + 2))?;
            queue!(out, SetForegroundColor(Color::Red))?;
            queue!(out, Print(e))?;
            queue!(out, ResetColor)?;
        }
        draw_nav_hint(&mut out, input_row + 4)?;
        out.flush()?;

        match read_key()? {
            // Input pages treat every printable character — including
            // 'b'/'B' — as literal text. API keys regularly contain
            // both. Back-nav on this page is left-arrow only.
            Key::Char(c) => {
                buf.push(c);
                error = None;
            }
            Key::Space => {
                buf.push(' ');
                error = None;
            }
            Key::Backspace => {
                buf.pop();
                error = None;
            }
            Key::Enter => {
                if buf.is_empty() {
                    if have_current {
                        return Ok(PageResult::Next(current.to_string()));
                    }
                    error = Some(
                        "API key cannot be empty. Paste your key, or Ctrl-C to cancel.".into(),
                    );
                } else {
                    return Ok(PageResult::Next(buf));
                }
            }
            Key::Left => return Ok(PageResult::Back),
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

/// Channels stage: multi-select, then per-channel secret follow-ups
/// for whatever the operator ticked. Treated as a single unit so back
/// from any sub-prompt lands on the multi-select with toggles intact,
/// and back from the multi-select itself lands on the prior wizard
/// page.
fn run_channels_stage(state: &mut WizardState) -> io::Result<PageResult<()>> {
    'stage: loop {
        match pick_channels(state.channels_sel)? {
            PageResult::Back => return Ok(PageResult::Back),
            PageResult::Next(sel) => state.channels_sel = sel,
        }

        // Telegram
        if state.channels_sel[0] {
            match prompt_keep_or_replace(
                "Enter your Telegram bot token",
                "(get one from @BotFather on Telegram)",
                &state.channels.telegram_token,
                /* sensitive */ true,
            )? {
                PageResult::Back => continue 'stage,
                PageResult::Next(v) => state.channels.telegram_token = v,
            }
        }
        // Discord
        if state.channels_sel[1] {
            match prompt_keep_or_replace(
                "Enter your Discord bot token",
                "(Developer Portal → Application → Bot → Reset Token)",
                &state.channels.discord_token,
                true,
            )? {
                PageResult::Back => continue 'stage,
                PageResult::Next(v) => state.channels.discord_token = v,
            }
            match prompt_keep_or_replace(
                "Enter your Discord channel ID",
                "(right-click the channel → Copy Channel ID; enable Developer Mode if missing)",
                &state.channels.discord_channel,
                false,
            )? {
                PageResult::Back => continue 'stage,
                PageResult::Next(v) => state.channels.discord_channel = v,
            }
        }
        // Slack
        if state.channels_sel[2] {
            match prompt_keep_or_replace(
                "Enter your Slack bot token",
                "(starts with xoxb-...; OAuth & Permissions → Bot User OAuth Token)",
                &state.channels.slack_token,
                true,
            )? {
                PageResult::Back => continue 'stage,
                PageResult::Next(v) => state.channels.slack_token = v,
            }
            match prompt_keep_or_replace(
                "Enter your Slack channel ID",
                "(right-click the channel → View channel details → bottom of the popout)",
                &state.channels.slack_channel,
                false,
            )? {
                PageResult::Back => continue 'stage,
                PageResult::Next(v) => state.channels.slack_channel = v,
            }
        }

        return Ok(PageResult::Next(()));
    }
}

fn pick_channels(initial: [bool; 3]) -> io::Result<PageResult<[bool; 3]>> {
    let mut selected = initial;
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
            let lead = if i == idx { "> " } else { "  " };
            if i == idx {
                queue!(out, SetForegroundColor(Color::Yellow))?;
                queue!(out, Print(format!("{lead}[{mark}] {label}")))?;
                queue!(out, ResetColor)?;
            } else {
                queue!(out, Print(format!("{lead}[{mark}] {label}")))?;
            }
        }
        queue!(out, cursor::MoveTo(2, 8))?;
        queue!(
            out,
            Print("(leave all unchecked to skip — channels can be added later)")
        )?;
        draw_nav_hint(&mut out, 10)?;
        out.flush()?;

        match read_key()? {
            Key::Up => idx = idx.saturating_sub(1),
            Key::Down if idx + 1 < CHANNEL_LABELS.len() => idx += 1,
            Key::Space => selected[idx] = !selected[idx],
            Key::Enter => return Ok(PageResult::Next(selected)),
            Key::Left | Key::Char('b') | Key::Char('B') => return Ok(PageResult::Back),
            Key::Cancel => cancel("Setup cancelled. Run `relix setup` to configure Relix."),
            _ => {}
        }
    }
}

/// Single-line secret/value prompt with keep-or-replace semantics
/// when `current` is non-empty. `sensitive` controls whether the
/// current value is shown masked (API key / bot token) or in full
/// (channel ID, which is non-secret and useful to verify visually).
fn prompt_keep_or_replace(
    title: &str,
    hint: &str,
    current: &str,
    sensitive: bool,
) -> io::Result<PageResult<String>> {
    let mut buf = String::new();
    let mut error: Option<String> = None;
    let mut out = io::stdout();
    let have_current = !current.trim().is_empty();
    let render_current = if sensitive {
        mask_api_key(current)
    } else {
        current.to_string()
    };
    loop {
        clear_screen(&mut out)?;
        queue!(out, cursor::MoveTo(2, 1))?;
        queue!(out, SetForegroundColor(Color::Cyan))?;
        queue!(out, Print(title))?;
        queue!(out, ResetColor)?;
        queue!(out, cursor::MoveTo(2, 2))?;
        let input_row: u16 = if have_current {
            queue!(
                out,
                Print(format!("{hint}  (Enter to keep current; ← back)"))
            )?;
            queue!(out, cursor::MoveTo(2, 4))?;
            queue!(out, Print(format!("Current:  {render_current}")))?;
            6
        } else {
            queue!(out, Print(format!("{hint}  (← back)")))?;
            4
        };
        queue!(out, cursor::MoveTo(2, input_row))?;
        let echo = if sensitive {
            "•".repeat(buf.chars().count())
        } else {
            buf.clone()
        };
        queue!(out, Print(format!("> {echo}")))?;
        if let Some(e) = &error {
            queue!(out, cursor::MoveTo(2, input_row + 2))?;
            queue!(out, SetForegroundColor(Color::Red))?;
            queue!(out, Print(e))?;
            queue!(out, ResetColor)?;
        }
        draw_nav_hint(&mut out, input_row + 4)?;
        out.flush()?;

        match read_key()? {
            // Input pages treat every printable character — including
            // 'b'/'B' — as literal text; back-nav is left-arrow only.
            Key::Char(c) => {
                buf.push(c);
                error = None;
            }
            Key::Space => {
                buf.push(' ');
                error = None;
            }
            Key::Backspace => {
                buf.pop();
                error = None;
            }
            Key::Enter => {
                if buf.is_empty() {
                    if have_current {
                        return Ok(PageResult::Next(current.to_string()));
                    }
                    error = Some("Required. Paste the value or Ctrl-C to cancel.".into());
                } else {
                    return Ok(PageResult::Next(buf));
                }
            }
            Key::Left => return Ok(PageResult::Back),
            Key::Cancel => cancel("Setup cancelled. Run `relix setup` to configure Relix."),
            _ => {}
        }
    }
}

fn confirm(state: &WizardState) -> io::Result<PageResult<()>> {
    let new_cfg = state.to_config();
    let mut out = io::stdout();
    clear_screen(&mut out)?;
    let mut row = 1u16;
    queue!(out, cursor::MoveTo(2, row))?;
    queue!(out, SetForegroundColor(Color::Cyan))?;
    queue!(
        out,
        Print(if state.is_reconfigure {
            "Ready to save updated configuration"
        } else {
            "Ready to save configuration"
        })
    )?;
    queue!(out, ResetColor)?;
    row += 2;

    let prior = state.prior.as_ref();

    // Provider
    let provider_changed = prior.is_some_and(|p| p.provider.name != new_cfg.provider.name);
    queue!(out, cursor::MoveTo(2, row))?;
    let provider_line = if let (true, Some(p)) = (provider_changed, prior) {
        format!(
            "Provider:  {} (was: {})",
            new_cfg.provider.name, p.provider.name
        )
    } else {
        format!("Provider:  {}", new_cfg.provider.name)
    };
    queue!(out, Print(provider_line))?;
    row += 1;

    // API key
    if !new_cfg.provider.api_key.is_empty() {
        let api_changed = prior.is_some_and(|p| p.provider.api_key != new_cfg.provider.api_key);
        queue!(out, cursor::MoveTo(2, row))?;
        let suffix = if api_changed { "  (updated)" } else { "" };
        queue!(
            out,
            Print(format!(
                "API key:   {}{suffix}",
                mask_api_key(&new_cfg.provider.api_key)
            ))
        )?;
        row += 1;
    }

    // Channels
    let mut channel_summary: Vec<&str> = Vec::new();
    if new_cfg.channels.telegram {
        channel_summary.push("Telegram");
    }
    if new_cfg.channels.discord {
        channel_summary.push("Discord");
    }
    if new_cfg.channels.slack {
        channel_summary.push("Slack");
    }
    let channel_str = if channel_summary.is_empty() {
        "(none)".to_string()
    } else {
        channel_summary.join(", ")
    };
    queue!(out, cursor::MoveTo(2, row))?;
    queue!(out, Print(format!("Channels:  {channel_str}")))?;
    row += 1;
    if let Some(p) = prior {
        let diffs = channel_diff(&p.channels, &new_cfg.channels);
        if !diffs.is_empty() {
            queue!(out, cursor::MoveTo(2, row))?;
            queue!(out, SetForegroundColor(Color::Yellow))?;
            queue!(out, Print(format!("           ({})", diffs.join(", "))))?;
            queue!(out, ResetColor)?;
            row += 1;
        }
    }
    row += 1;

    queue!(out, cursor::MoveTo(2, row))?;
    queue!(out, Print("Press Enter to save, ← back to edit."))?;
    row += 1;
    draw_nav_hint(&mut out, row + 1)?;
    out.flush()?;

    loop {
        match read_key()? {
            Key::Enter => return Ok(PageResult::Next(())),
            Key::Left | Key::Char('b') | Key::Char('B') => return Ok(PageResult::Back),
            Key::Cancel => cancel("Setup cancelled. Run `relix setup` to configure Relix."),
            _ => {}
        }
    }
}

fn channel_diff(prior: &ChannelsConfig, now: &ChannelsConfig) -> Vec<String> {
    let mut out = Vec::new();
    for (name, was, is_now) in [
        ("Telegram", prior.telegram, now.telegram),
        ("Discord", prior.discord, now.discord),
        ("Slack", prior.slack, now.slack),
    ] {
        match (was, is_now) {
            (false, true) => out.push(format!("added: {name}")),
            (true, false) => out.push(format!("removed: {name}")),
            _ => {}
        }
    }
    // Token-only changes on still-enabled channels
    if prior.telegram && now.telegram && prior.telegram_token != now.telegram_token {
        out.push("Telegram token updated".to_string());
    }
    if prior.discord
        && now.discord
        && (prior.discord_token != now.discord_token
            || prior.discord_channel != now.discord_channel)
    {
        out.push("Discord credentials updated".to_string());
    }
    if prior.slack
        && now.slack
        && (prior.slack_token != now.slack_token || prior.slack_channel != now.slack_channel)
    {
        out.push("Slack credentials updated".to_string());
    }
    out
}

// ---- key + terminal helpers ---------------------------------------------

enum Key {
    Char(char),
    Up,
    Down,
    Left,
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
                    KeyCode::Left => Key::Left,
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

fn clear_screen(out: &mut io::Stdout) -> io::Result<()> {
    execute!(out, Clear(ClearType::All), cursor::MoveTo(0, 0))
}

fn draw_nav_hint(out: &mut io::Stdout, row: u16) -> io::Result<()> {
    queue!(out, cursor::MoveTo(2, row))?;
    queue!(out, SetForegroundColor(Color::DarkGrey))?;
    queue!(out, Print("(← back  |  Ctrl-C cancel)"))?;
    queue!(out, ResetColor)?;
    Ok(())
}

/// Centre `content` inside the 42-char-wide welcome panel and wrap
/// with the box-drawing border characters.
fn pad_box_line(content: &str) -> String {
    let inner = 42usize;
    let len = content.chars().count();
    if len >= inner {
        return format!("║{content}║");
    }
    let total_pad = inner - len;
    let left = total_pad / 2;
    let right = total_pad - left;
    format!("║{}{content}{}║", " ".repeat(left), " ".repeat(right))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_from_prior_pre_fills_every_field() {
        let mut prior = RelixConfig::default();
        prior.provider.name = "openai".into();
        prior.provider.api_key = "sk-test-1234567890abcdef".into();
        prior.channels.telegram = true;
        prior.channels.telegram_token = "tg-token".into();
        prior.channels.discord = true;
        prior.channels.discord_token = "dc-token".into();
        prior.channels.discord_channel = "12345".into();

        let s = WizardState::from_prior(Some(&prior));
        assert_eq!(s.provider_idx, 1, "openai is row 1 in PROVIDER_CHOICES");
        assert_eq!(s.api_key, "sk-test-1234567890abcdef");
        assert_eq!(s.channels_sel, [true, true, false]);
        assert_eq!(s.channels.telegram_token, "tg-token");
        assert_eq!(s.channels.discord_token, "dc-token");
        assert_eq!(s.channels.discord_channel, "12345");
        assert!(s.is_reconfigure);
        assert!(s.needs_key(), "openai needs a key");
    }

    #[test]
    fn state_from_no_prior_uses_defaults() {
        let s = WizardState::from_prior(None);
        // Default config is `mock` provider; PROVIDER_CHOICES indexes
        // mock at row 6.
        assert_eq!(s.provider_idx, 6);
        assert!(s.api_key.is_empty());
        assert_eq!(s.channels_sel, [false; 3]);
        assert!(!s.is_reconfigure);
        assert!(!s.needs_key(), "mock skips the API-key page");
    }

    #[test]
    fn state_round_trips_back_to_config_through_to_config() {
        let mut prior = RelixConfig::default();
        prior.provider.name = "openrouter".into();
        prior.provider.api_key = "sk-or-abc".into();
        prior.channels.slack = true;
        prior.channels.slack_token = "xoxb-...".into();
        prior.channels.slack_channel = "C123".into();
        let s = WizardState::from_prior(Some(&prior));
        let back = s.to_config();
        assert_eq!(back, prior);
    }

    #[test]
    fn channel_diff_flags_added_removed_and_token_changes() {
        let prior = ChannelsConfig {
            telegram: true,
            telegram_token: "old".into(),
            ..Default::default()
        };
        let now = ChannelsConfig {
            telegram: true,
            telegram_token: "new".into(),
            discord: true,
            discord_token: "x".into(),
            discord_channel: "c".into(),
            ..Default::default()
        };

        let diffs = channel_diff(&prior, &now);
        assert!(diffs.iter().any(|d| d.contains("added: Discord")));
        assert!(diffs.iter().any(|d| d.contains("Telegram token updated")));
    }

    #[test]
    fn provider_index_unknown_falls_back_to_zero() {
        let mut prior = RelixConfig::default();
        prior.provider.name = "made-up-provider".into();
        let s = WizardState::from_prior(Some(&prior));
        assert_eq!(s.provider_idx, 0, "unknown provider lands on openrouter");
    }

    #[test]
    fn pick_provider_initial_index_clamp_arithmetic() {
        // `pick_provider` clamps an absurdly-high initial_idx. We
        // can't drive the interactive loop in a unit test but we
        // can at least confirm the clamp arithmetic that protects
        // it from a corrupted on-disk index.
        let init = 9999usize;
        let clamped = init.min(PROVIDER_CHOICES.len() - 1);
        assert_eq!(clamped, PROVIDER_CHOICES.len() - 1);
    }
}
