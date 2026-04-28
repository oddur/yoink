//! User-confirmation prompts. One helper, three modes — picks the
//! right shape for the call site without each caller hand-rolling
//! TTY detection / `--yes` plumbing / non-TTY error messages.

use std::io::{self, IsTerminal, Write};

use anyhow::Result;

/// What kind of confirmation the caller wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmKind {
    /// Soft prompt with a yes-default (`[Y/n]`). Empty input or
    /// `y`/`yes` confirms; anything else rejects. Non-TTY without
    /// `yes_override` returns `Ok(true)` — the prompt was opt-in.
    DefaultYes,
    /// Soft prompt with a no-default (`[y/N]`). Same rules but the
    /// safe default is reject. Non-TTY without `yes_override` returns
    /// `Ok(false)`.
    DefaultNo,
    /// Hard prompt for destructive ops — requires the literal word
    /// `yes` (no shortcuts, no empty-input acceptance). Non-TTY
    /// without `yes_override` errors instead of guessing, with a
    /// pointer to `--yes`.
    Destructive,
}

/// Single entry point for every confirmation in the CLI. `yes_override`
/// is the value of the caller's `--yes` flag — when true the prompt
/// short-circuits to confirmed without touching the terminal, so
/// scripted callers can avoid the prompt entirely.
pub fn confirm(prompt: &str, kind: ConfirmKind, yes_override: bool) -> Result<bool> {
    if yes_override {
        return Ok(true);
    }
    if !io::stdin().is_terminal() {
        return match kind {
            ConfirmKind::DefaultYes => Ok(true),
            ConfirmKind::DefaultNo => Ok(false),
            ConfirmKind::Destructive => {
                anyhow::bail!(
                    "{prompt}\n\nstdin is not a TTY — pass --yes to confirm non-interactively."
                );
            }
        };
    }
    let suffix = match kind {
        ConfirmKind::DefaultYes => "[Y/n]",
        ConfirmKind::DefaultNo => "[y/N]",
        ConfirmKind::Destructive => "(type 'yes' to confirm)",
    };
    eprint!("{prompt} {suffix} ");
    io::stderr().flush().ok();
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;
    let answer = buf.trim();
    Ok(match kind {
        ConfirmKind::DefaultYes => {
            answer.is_empty()
                || answer.eq_ignore_ascii_case("y")
                || answer.eq_ignore_ascii_case("yes")
        }
        ConfirmKind::DefaultNo => {
            answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes")
        }
        ConfirmKind::Destructive => answer == "yes",
    })
}
