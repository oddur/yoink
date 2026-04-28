//! Secrets pane — view / add / edit / remove individual sealed
//! secrets without leaving the TUI. Reuses `crate::sealed` end-to-end
//! (`load_identity` → `unseal` → `parse_dotenv` → `render` → `seal` →
//! `write_atomically`) so the on-disk format and key resolution are
//! identical to `yoink secrets edit`.
//!
//! Per-environment story: the file path comes from
//! `sealed::resolve_sealed_path(config, file_override)` — same
//! resolution the CLI commands use. Operators run
//! `yoink -c yoink.prod.yaml tui` to edit prod's sealed file and
//! `yoink -c yoink.staging.yaml tui` for staging; the title bar
//! surfaces which file is active.
//!
//! Design discipline:
//!   - Per-key operations only. Bulk multi-line edits stay on the
//!     CLI (`yoink secrets edit`) — adding a multi-line text editor
//!     to ratatui isn't worth the surface for the granularity
//!     operators actually need mid-incident.
//!   - `provider: command` is read-only here. Rotation happens in
//!     the external tool; the panel renders a hint pointing back at
//!     the configured command.

#![allow(clippy::doc_markdown)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};

use crate::config::{Config, SecretsConfig};
use crate::sealed;

use super::ui::{FilterState, bold, clamp_selection, filter_footer, pane_layout};

const FLASH_TTL: Duration = Duration::from_secs(4);

/// Mask renderer — same shape as `mask_value` in the CLI: 2 visible
/// chars + dots. Bounded width so a 10 KB blob doesn't blow out the
/// row.
fn mask(value: &str) -> String {
    let visible = if value.chars().count() <= 4 { 0 } else { 2 };
    let prefix: String = value.chars().take(visible).collect();
    let dots: usize = value.chars().count().saturating_sub(visible).min(16);
    format!("{prefix}{}", "•".repeat(dots))
}

#[derive(Default)]
pub struct SecretsState {
    bundle: LoadStatus,
    table: TableState,
    pub filter: FilterState,
    reveal: bool,
    edit: EditState,
    /// Sticky one-line status surfaced in the footer (e.g.
    /// `saved 5 keys` or `decrypt failed: ...`). Cleared after
    /// `FLASH_TTL` or on the next load.
    flash: Option<(Instant, String)>,
}

#[derive(Default)]
enum LoadStatus {
    #[default]
    NotLoaded,
    Loaded(LoadedBundle),
    Failed(String),
}

struct LoadedBundle {
    /// In-memory plaintext map. All edits mutate this first; the
    /// re-seal-and-write step happens on commit. Kept sorted via
    /// BTreeMap (parse_dotenv already returns one).
    values: BTreeMap<String, String>,
    /// Stable list of keys (mirror of `values.keys()`) — table
    /// navigation indexes into this so the row order matches what
    /// the operator sees.
    keys: Vec<String>,
    /// Title-bar label, e.g. `age — secrets.prod.age (3 recipients)`.
    title: String,
    /// Where to write on save. `None` for read-only providers.
    write_target: Option<WriteTarget>,
}

#[derive(Clone)]
struct WriteTarget {
    path: PathBuf,
    recipients: Vec<String>,
}

#[derive(Default)]
enum EditState {
    #[default]
    None,
    /// Operator pressed `a` — typing the new key name. Enter commits
    /// to AddingValue, Esc cancels.
    AddingKey { buffer: String },
    /// Then typing the value for the new key. Enter commits the
    /// add (mutate map + re-seal + write); Esc cancels.
    AddingValue { key: String, buffer: String },
    /// Editing an existing key's value. Enter commits; Esc cancels.
    EditingValue { key: String, buffer: String },
    /// Modal confirmation before removing.
    Removing { key: String },
}

impl SecretsState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// True while an inline-input edit (add-key / add-value /
    /// edit-value) is active. Removing is a confirmation modal, not
    /// inline input — handled separately so it doesn't capture
    /// printable keys.
    pub fn input_mode(&self) -> bool {
        matches!(
            self.edit,
            EditState::AddingKey { .. }
                | EditState::AddingValue { .. }
                | EditState::EditingValue { .. }
        )
    }

    pub fn confirming_remove(&self) -> bool {
        matches!(self.edit, EditState::Removing { .. })
    }

    /// Lazily load the bundle on first entry into the pane. `force`
    /// re-loads even if already loaded — used after a save to pick
    /// up the freshly-written file (defensive: the in-memory map is
    /// already correct, but re-reading catches any out-of-band edit).
    pub fn ensure_loaded(&mut self, config: &Config, force: bool) {
        if !force && !matches!(self.bundle, LoadStatus::NotLoaded) {
            return;
        }
        self.bundle = load(config);
        if let LoadStatus::Loaded(bundle) = &self.bundle {
            clamp_selection(&mut self.table, bundle.keys.len());
        }
    }

    /// Cancel any in-progress edit. Called on Esc and on view exit.
    pub fn cancel_edit(&mut self) {
        self.edit = EditState::None;
    }

    /// Start an `add` flow. No-op when read-only (the key handler
    /// gates this, but we re-check defensively).
    pub fn begin_add(&mut self) {
        if self.is_writable() {
            self.edit = EditState::AddingKey {
                buffer: String::new(),
            };
        }
    }

    /// Start an `edit` flow on the currently-selected key.
    pub fn begin_edit_selected(&mut self) {
        if !self.is_writable() {
            return;
        }
        if let Some(key) = self.selected_key() {
            let current = self.value_for(&key).unwrap_or_default().to_string();
            self.edit = EditState::EditingValue {
                key,
                buffer: current,
            };
        }
    }

    /// Start a `remove` confirmation modal on the selected key.
    pub fn begin_remove_selected(&mut self) {
        if !self.is_writable() {
            return;
        }
        if let Some(key) = self.selected_key() {
            self.edit = EditState::Removing { key };
        }
    }

    /// Toggle reveal/mask. No-op while editing.
    pub fn toggle_reveal(&mut self) {
        if !self.input_mode() {
            self.reveal = !self.reveal;
        }
    }

    /// Push a char into the active edit buffer.
    pub fn push_char(&mut self, c: char) {
        match &mut self.edit {
            EditState::AddingKey { buffer }
            | EditState::AddingValue { buffer, .. }
            | EditState::EditingValue { buffer, .. } => buffer.push(c),
            _ => {}
        }
    }

    pub fn backspace(&mut self) {
        match &mut self.edit {
            EditState::AddingKey { buffer }
            | EditState::AddingValue { buffer, .. }
            | EditState::EditingValue { buffer, .. } => {
                buffer.pop();
            }
            _ => {}
        }
    }

    /// Confirm the current Removing state. Returns `true` when the
    /// modal consumed the key (caller should not interpret it as
    /// anything else). Caller is expected to call `commit_remove`
    /// on the same tick when this returns true and the key was
    /// confirmed (y/Enter).
    pub fn confirm_remove(&mut self) -> Option<String> {
        match std::mem::replace(&mut self.edit, EditState::None) {
            EditState::Removing { key } => Some(key),
            other => {
                self.edit = other;
                None
            }
        }
    }

    /// Apply the current input buffer. Returns the action requested
    /// so the caller can persist it (separates "what does the user
    /// want" from "perform IO and update flash"). On AddingKey, the
    /// state advances to AddingValue and `None` is returned so the
    /// caller doesn't write yet.
    pub fn commit_input(&mut self) -> Option<EditCommit> {
        match std::mem::replace(&mut self.edit, EditState::None) {
            EditState::AddingKey { buffer } => {
                let key = buffer.trim().to_string();
                if key.is_empty() {
                    self.flash("key cannot be empty");
                    return None;
                }
                if !is_valid_key(&key) {
                    self.flash(format!(
                        "invalid key {key:?} (must match [A-Za-z_][A-Za-z0-9_]*)"
                    ));
                    return None;
                }
                if self.value_for(&key).is_some() {
                    self.flash(format!("{key:?} already exists — `e` to edit instead"));
                    return None;
                }
                self.edit = EditState::AddingValue {
                    key,
                    buffer: String::new(),
                };
                None
            }
            EditState::AddingValue { key, buffer } | EditState::EditingValue { key, buffer } => {
                Some(EditCommit::Set { key, value: buffer })
            }
            other => {
                self.edit = other;
                None
            }
        }
    }

    /// Apply a confirmed `EditCommit` to the in-memory map and
    /// re-seal to disk. Updates the flash with the result. Returns
    /// `true` when the new sealed file made it to disk — the caller
    /// uses the signal to refresh the app's drift-detection bundle
    /// cache so the dashboard drift cell picks up the edit on the
    /// very next tick instead of waiting for a TUI restart.
    pub fn apply_commit(&mut self, commit: EditCommit) -> bool {
        let LoadStatus::Loaded(bundle) = &mut self.bundle else {
            self.flash("internal: bundle not loaded");
            return false;
        };
        let saved = match commit {
            EditCommit::Set { key, value } => {
                bundle.values.insert(key.clone(), value);
                bundle.keys = bundle.values.keys().cloned().collect();
                if let Err(e) = persist(bundle) {
                    self.flash(format!("save failed: {e}"));
                    return false;
                }
                self.flash(format!("set {key}"));
                true
            }
            EditCommit::Remove { key } => {
                bundle.values.remove(&key);
                bundle.keys = bundle.values.keys().cloned().collect();
                if let Err(e) = persist(bundle) {
                    self.flash(format!("save failed: {e}"));
                    return false;
                }
                self.flash(format!("removed {key}"));
                true
            }
        };
        let n = self.bundle_len();
        clamp_selection(&mut self.table, n);
        saved
    }

    /// Convenience for the on_key handler: confirmed remove path.
    /// Returns the same `did-we-persist` bool as `apply_commit` so the
    /// caller can refresh the drift cache on success.
    pub fn apply_remove(&mut self, key: String) -> bool {
        self.apply_commit(EditCommit::Remove { key })
    }

    pub fn select_next(&mut self) {
        let n = self.visible_indices().len();
        if n == 0 {
            return;
        }
        let i = self.table.selected().unwrap_or(0);
        self.table.select(Some((i + 1).min(n - 1)));
    }

    pub fn select_prev(&mut self) {
        if self.visible_indices().is_empty() {
            return;
        }
        let i = self.table.selected().unwrap_or(0);
        self.table.select(Some(i.saturating_sub(1)));
    }

    fn selected_key(&self) -> Option<String> {
        let visible = self.visible_indices();
        let LoadStatus::Loaded(bundle) = &self.bundle else {
            return None;
        };
        self.table
            .selected()
            .and_then(|i| visible.get(i).copied())
            .and_then(|src| bundle.keys.get(src))
            .cloned()
    }

    fn value_for(&self, key: &str) -> Option<&str> {
        let LoadStatus::Loaded(bundle) = &self.bundle else {
            return None;
        };
        bundle.values.get(key).map(String::as_str)
    }

    fn visible_indices(&self) -> Vec<usize> {
        let LoadStatus::Loaded(bundle) = &self.bundle else {
            return Vec::new();
        };
        bundle
            .keys
            .iter()
            .enumerate()
            .filter_map(|(i, k)| self.filter.matches(k).then_some(i))
            .collect()
    }

    fn bundle_len(&self) -> usize {
        match &self.bundle {
            LoadStatus::Loaded(b) => b.keys.len(),
            _ => 0,
        }
    }

    fn is_writable(&self) -> bool {
        matches!(
            &self.bundle,
            LoadStatus::Loaded(b) if b.write_target.is_some()
        )
    }

    fn flash(&mut self, msg: impl Into<String>) {
        self.flash = Some((Instant::now(), msg.into()));
    }

    pub fn render(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        throbber: &throbber_widgets_tui::ThrobberState,
    ) {
        let layout = pane_layout(area);

        let header = Paragraph::new(self.header_text(throbber)).style(bold());
        frame.render_widget(header, layout[0]);

        match &self.bundle {
            LoadStatus::NotLoaded => {
                let p = Paragraph::new(super::ui::loading_line(throbber))
                    .block(Block::default().borders(Borders::ALL).title("secrets"));
                frame.render_widget(p, layout[1]);
            }
            LoadStatus::Failed(msg) => {
                let p = Paragraph::new(msg.as_str())
                    .style(Style::default().fg(Color::Red))
                    .block(Block::default().borders(Borders::ALL).title("secrets"));
                frame.render_widget(p, layout[1]);
            }
            LoadStatus::Loaded(_) => self.render_table(frame, layout[1]),
        }

        let footer_help = self.footer_help();
        let footer = match self.flash_text() {
            Some(text) => Paragraph::new(text).style(Style::default().fg(Color::Yellow)),
            None => filter_footer(&self.filter, &footer_help),
        };
        frame.render_widget(footer, layout[2]);
    }

    fn flash_text(&self) -> Option<String> {
        let (when, msg) = self.flash.as_ref()?;
        if when.elapsed() >= FLASH_TTL {
            return None;
        }
        Some(format!("· {msg}"))
    }

    fn footer_help(&self) -> String {
        let writable = self.is_writable();
        let editing = self.input_mode();
        if editing {
            return "(typing) · enter apply · esc cancel".into();
        }
        if let EditState::Removing { key } = &self.edit {
            return format!("remove {key}? · y/enter confirm · any other key cancels");
        }
        let mut parts = vec!["q quit", "↑↓ select", "/ filter", "r reveal"];
        if writable {
            parts.extend_from_slice(&["a add", "e edit", "d delete"]);
        }
        parts.push("esc back");
        parts.join(" · ")
    }

    fn header_text(&self, _throbber: &throbber_widgets_tui::ThrobberState) -> String {
        match &self.bundle {
            LoadStatus::NotLoaded => "yoink secrets · loading…".into(),
            LoadStatus::Failed(_) => "yoink secrets · (error)".into(),
            LoadStatus::Loaded(b) => {
                let n = b.values.len();
                let mode = if b.write_target.is_some() {
                    "writable"
                } else {
                    "read-only"
                };
                format!(
                    "yoink secrets · {} · {} key{} · {mode}",
                    b.title,
                    n,
                    if n == 1 { "" } else { "s" }
                )
            }
        }
    }

    fn render_table(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let LoadStatus::Loaded(bundle) = &self.bundle else {
            return;
        };
        let visible = self.visible_indices();
        clamp_selection(&mut self.table, visible.len());

        let widths = [
            Constraint::Length(28), // key
            Constraint::Min(20),    // value
        ];

        let mut rows: Vec<Row<'_>> = Vec::new();
        if bundle.keys.is_empty() {
            rows.push(Row::new(vec![Cell::from(
                "(no secrets yet — press `a` to add)",
            )]));
        } else if visible.is_empty() {
            rows.push(Row::new(vec![Cell::from("(no secrets match filter)")]));
        } else {
            for src in &visible {
                let k = &bundle.keys[*src];
                let v = bundle.values.get(k).map_or("", String::as_str);
                let display = if self.reveal { v.to_string() } else { mask(v) };
                rows.push(Row::new(vec![Cell::from(k.clone()), Cell::from(display)]));
            }
        }

        // Append an "input row" while editing — gives the operator a
        // visual anchor for what they're typing without a separate
        // overlay.
        if let Some((label, buffer)) = self.input_buffer_view() {
            rows.push(Row::new(vec![
                Cell::from(label).style(Style::default().fg(Color::Yellow)),
                Cell::from(format!("{buffer}_")).style(Style::default().fg(Color::Yellow)),
            ]));
        }

        let table = Table::new(rows, widths)
            .header(Row::new(vec![
                Cell::from("KEY").style(bold()),
                Cell::from("VALUE").style(bold()),
            ]))
            .row_highlight_style(super::ui::table_highlight_style())
            .highlight_symbol(super::ui::TABLE_HIGHLIGHT_SYMBOL)
            .block(Block::default().borders(Borders::ALL).title("secrets"));
        frame.render_stateful_widget(table, area, &mut self.table);
    }

    fn input_buffer_view(&self) -> Option<(String, &str)> {
        match &self.edit {
            EditState::AddingKey { buffer } => Some(("(new key)".into(), buffer.as_str())),
            EditState::AddingValue { key, buffer } => {
                Some((format!("(new value for {key})"), buffer.as_str()))
            }
            EditState::EditingValue { key, buffer } => {
                Some((format!("(editing {key})"), buffer.as_str()))
            }
            _ => None,
        }
    }

    /// Lines for the confirmation-modal renderer (`render_modal`).
    /// Returns `None` when no confirmation is active.
    pub fn confirm_modal_lines(&self) -> Option<(&'static str, Vec<String>)> {
        let EditState::Removing { key } = &self.edit else {
            return None;
        };
        let body = vec![
            "About to remove this secret from the sealed file.".into(),
            String::new(),
            format!("key:  {key}"),
            String::new(),
            "The change is committed locally — re-encrypted on disk.".into(),
            "Don't forget to commit + push secrets.age afterward.".into(),
            String::new(),
            "[y] / Enter   confirm".into(),
            "[any]         cancel".into(),
        ];
        Some(("remove secret?", body))
    }
}

/// What an inline-edit commit actually wants the caller to do.
/// Splits "user pressed Enter on the input buffer" from the IO step
/// so tests can drive the state machine without a real filesystem.
pub enum EditCommit {
    Set { key: String, value: String },
    Remove { key: String },
}

fn is_valid_key(key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    let mut chars = key.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn load(config: &Config) -> LoadStatus {
    let Some(secrets_cfg) = &config.secrets else {
        return LoadStatus::Failed(
            "no `secrets:` block in yoink.yaml — run `yoink secrets key generate` to bootstrap"
                .into(),
        );
    };

    match secrets_cfg {
        SecretsConfig::Command { command, .. } => {
            // Read-only path. The bundle comes from a third-party CLI
            // we don't manage; rotation/edits happen in that tool's
            // own UI/CLI, not here.
            let pretty = command
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(" ");
            LoadStatus::Loaded(LoadedBundle {
                values: BTreeMap::new(),
                keys: Vec::new(),
                title: format!("command (read-only — edit via {pretty})"),
                write_target: None,
            })
        }
        SecretsConfig::Age { file, recipients } => {
            let path = match sealed::resolve_sealed_path(config, file.as_deref()) {
                Ok(p) => p,
                Err(e) => return LoadStatus::Failed(e.to_string()),
            };
            let identity = match sealed::load_identity(recipients) {
                Ok(id) => id,
                Err(e) => {
                    return LoadStatus::Failed(format!(
                        "cannot read secrets: {e}\n\nFix locally with: yoink secrets key generate"
                    ));
                }
            };
            if recipients.is_empty() {
                return LoadStatus::Failed(
                    "no `secrets.recipients:` configured — add at least one age public key (age1...) to yoink.yaml".into(),
                );
            }
            let basename = path
                .file_name()
                .map_or_else(|| "secrets.age".into(), |s| s.to_string_lossy().to_string());
            let title_prefix = format!(
                "age — {} ({} recipient{})",
                basename,
                recipients.len(),
                if recipients.len() == 1 { "" } else { "s" }
            );
            let values = if path.exists() {
                let bytes = match std::fs::read(&path) {
                    Ok(b) => b,
                    Err(e) => {
                        return LoadStatus::Failed(format!("read {}: {e}", path.display()));
                    }
                };
                let plaintext = match sealed::unseal(&bytes, &identity) {
                    Ok(p) => p,
                    Err(e) => {
                        return LoadStatus::Failed(format!(
                            "decrypt failed: {e}\n\nThe age identity yoink found doesn't match any recipient in {}.\nCheck YOINK_AGE_KEY or run `yoink secrets key generate` to inspect.",
                            path.display()
                        ));
                    }
                };
                match sealed::parse_dotenv(&plaintext) {
                    Ok(v) => v,
                    Err(e) => return LoadStatus::Failed(format!("parse: {e}")),
                }
            } else {
                BTreeMap::new()
            };
            let title = if path.exists() {
                title_prefix
            } else {
                format!("{title_prefix} · empty (will be created on first save)")
            };
            let keys: Vec<String> = values.keys().cloned().collect();
            LoadStatus::Loaded(LoadedBundle {
                values,
                keys,
                title,
                write_target: Some(WriteTarget {
                    path,
                    recipients: recipients.clone(),
                }),
            })
        }
    }
}

fn persist(bundle: &LoadedBundle) -> Result<(), String> {
    let target = bundle
        .write_target
        .as_ref()
        .ok_or_else(|| "this provider is read-only".to_string())?;
    let canonical = sealed::render_dotenv(&bundle.values);
    let bytes =
        sealed::seal(canonical.as_bytes(), &target.recipients).map_err(|e| format!("seal: {e}"))?;
    sealed::write_atomically(&target.path, &bytes)
        .map_err(|e| format!("write {}: {e}", target.path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn fixture_loaded(values: &[(&str, &str)]) -> SecretsState {
        let mut s = SecretsState::new();
        let map: BTreeMap<String, String> = values
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        let keys: Vec<String> = map.keys().cloned().collect();
        s.bundle = LoadStatus::Loaded(LoadedBundle {
            values: map,
            keys,
            title: "test".into(),
            write_target: Some(WriteTarget {
                path: PathBuf::from("/tmp/test.age"),
                recipients: vec!["age1...".into()],
            }),
        });
        s
    }

    #[test]
    fn mask_short_value_dots_only() {
        assert_eq!(mask(""), "");
        assert_eq!(mask("ab"), "••");
        assert_eq!(mask("abcd"), "••••");
        assert_eq!(mask("abcde"), "ab•••");
    }

    #[test]
    fn is_valid_key_matches_dotenv_rules() {
        assert!(is_valid_key("FOO"));
        assert!(is_valid_key("_X"));
        assert!(is_valid_key("DATABASE_URL_2"));
        assert!(!is_valid_key(""));
        assert!(!is_valid_key("1FOO"));
        assert!(!is_valid_key("FOO-BAR"));
        assert!(!is_valid_key("FOO BAR"));
    }

    #[test]
    fn add_flow_advances_through_key_then_value() {
        let mut s = fixture_loaded(&[]);
        s.begin_add();
        assert!(matches!(s.edit, EditState::AddingKey { .. }));
        for c in "FOO".chars() {
            s.push_char(c);
        }
        // Pressing Enter on AddingKey advances state without IO.
        let early = s.commit_input();
        assert!(early.is_none());
        assert!(matches!(s.edit, EditState::AddingValue { .. }));
        for c in "bar".chars() {
            s.push_char(c);
        }
        let commit = s.commit_input().expect("Set commit returned");
        match commit {
            EditCommit::Set { key, value } => {
                assert_eq!(key, "FOO");
                assert_eq!(value, "bar");
            }
            EditCommit::Remove { .. } => panic!("expected Set"),
        }
    }

    #[test]
    fn add_rejects_empty_key() {
        let mut s = fixture_loaded(&[]);
        s.begin_add();
        // Operator hits Enter with an empty buffer.
        let res = s.commit_input();
        assert!(res.is_none());
        assert!(matches!(s.edit, EditState::None));
        let flash = s.flash.as_ref().expect("flash set");
        assert!(flash.1.contains("empty"));
    }

    #[test]
    fn add_rejects_invalid_key_chars() {
        let mut s = fixture_loaded(&[]);
        s.begin_add();
        for c in "1BAD".chars() {
            s.push_char(c);
        }
        let res = s.commit_input();
        assert!(res.is_none());
        let flash = s.flash.as_ref().expect("flash set");
        assert!(flash.1.contains("invalid"));
    }

    #[test]
    fn add_rejects_existing_key() {
        let mut s = fixture_loaded(&[("FOO", "1")]);
        s.begin_add();
        for c in "FOO".chars() {
            s.push_char(c);
        }
        let res = s.commit_input();
        assert!(res.is_none());
        let flash = s.flash.as_ref().expect("flash set");
        assert!(flash.1.contains("already exists"));
    }

    #[test]
    fn edit_flow_yields_set_commit_with_existing_key() {
        let mut s = fixture_loaded(&[("FOO", "old")]);
        // Select the only row.
        s.table.select(Some(0));
        s.begin_edit_selected();
        match &s.edit {
            EditState::EditingValue { key, buffer } => {
                assert_eq!(key, "FOO");
                assert_eq!(buffer, "old");
            }
            _ => panic!("expected EditingValue"),
        }
        // Backspace 3, type new value.
        for _ in 0..3 {
            s.backspace();
        }
        for c in "new".chars() {
            s.push_char(c);
        }
        let commit = s.commit_input().expect("Set commit");
        let EditCommit::Set { key, value } = commit else {
            panic!("expected Set");
        };
        assert_eq!(key, "FOO");
        assert_eq!(value, "new");
    }

    #[test]
    fn remove_flow_requires_confirmation() {
        let mut s = fixture_loaded(&[("FOO", "1")]);
        s.table.select(Some(0));
        s.begin_remove_selected();
        assert!(s.confirming_remove());
        // Cancel path: confirm_remove only fires on actual y/Enter via
        // the on_key handler; here we just verify the modal lines.
        let modal = s.confirm_modal_lines().expect("modal active");
        assert_eq!(modal.0, "remove secret?");
        // Now confirm.
        let key = s.confirm_remove().expect("confirmed");
        assert_eq!(key, "FOO");
        assert!(matches!(s.edit, EditState::None));
    }

    #[test]
    fn reveal_toggles_off_during_input_mode() {
        let mut s = fixture_loaded(&[("FOO", "1")]);
        s.toggle_reveal();
        assert!(s.reveal);
        s.begin_add();
        let was = s.reveal;
        s.toggle_reveal();
        assert_eq!(s.reveal, was, "toggle is no-op while editing");
    }

    #[test]
    fn end_to_end_seal_unseal_roundtrip_via_persist() {
        // Generate a fresh identity, seal an empty bundle, mutate
        // through apply_commit, then re-load via the same code path
        // and verify the value lands.
        let dir = tempdir();
        let path = dir.join("secrets.age");
        let id = age::x25519::Identity::generate();
        let recipient = id.to_public().to_string();

        let mut bundle = LoadedBundle {
            values: BTreeMap::new(),
            keys: Vec::new(),
            title: "test".into(),
            write_target: Some(WriteTarget {
                path: path.clone(),
                recipients: vec![recipient.clone()],
            }),
        };
        bundle.values.insert("FOO".into(), "bar baz".into());
        bundle.keys = bundle.values.keys().cloned().collect();
        persist(&bundle).expect("persist");

        let bytes = std::fs::read(&path).expect("read sealed");
        let plaintext = sealed::unseal(&bytes, &id).expect("unseal");
        let parsed = sealed::parse_dotenv(&plaintext).expect("parse");
        assert_eq!(parsed.get("FOO").unwrap(), "bar baz");

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn apply_commit_signals_persist_success() {
        // The bool return drives the app's spawn_secrets_loader call —
        // we want a refresh on persist success, not on read-only or
        // failure paths.
        let dir = tempdir();
        let path = dir.join("secrets.age");
        let id = age::x25519::Identity::generate();
        let recipient = id.to_public().to_string();
        let mut s = SecretsState::new();
        s.bundle = LoadStatus::Loaded(LoadedBundle {
            values: BTreeMap::new(),
            keys: Vec::new(),
            title: "test".into(),
            write_target: Some(WriteTarget {
                path,
                recipients: vec![recipient],
            }),
        });
        let saved = s.apply_commit(EditCommit::Set {
            key: "K".into(),
            value: "v".into(),
        });
        assert!(saved, "successful set must signal persist");
        let removed = s.apply_remove("K".into());
        assert!(removed, "successful remove must signal persist");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn apply_commit_returns_false_on_read_only_bundle() {
        // provider:command bundles arrive with write_target=None;
        // commits should fail loud and not signal a refresh.
        let mut s = SecretsState::new();
        s.bundle = LoadStatus::Loaded(LoadedBundle {
            values: BTreeMap::new(),
            keys: Vec::new(),
            title: "test".into(),
            write_target: None,
        });
        let saved = s.apply_commit(EditCommit::Set {
            key: "K".into(),
            value: "v".into(),
        });
        assert!(!saved);
    }

    fn tempdir() -> PathBuf {
        let pid = std::process::id();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let p = std::env::temp_dir().join(format!("yoink-tui-secrets-{pid}-{stamp}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
