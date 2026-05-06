//! Resources pane — per-host docker resource introspection. Three
//! sub-tabs for the three resource families lazydocker exposes:
//!
//! - **Images**  (`i`) — every cached image across every host, with
//!   tag list, size, and an "is this dangling?" hint. Actions: remove
//!   one image (`d`), prune dangling (`P`), prune all unused (`A`).
//! - **Volumes** (`v`) — named volumes with driver + mountpoint.
//!   Actions: remove one (`d`), prune unused (`P`).
//! - **Networks** (`n`) — user-defined networks with driver + scope.
//!   Built-ins (`bridge`, `host`, `none`) appear but reject removal.
//!   Actions: remove one (`d`), prune unused (`P`).
//!
//! Refreshes fan out across every configured host in parallel — same
//! shape `Hosts` uses — so the pane converges in roughly one round-trip
//! to the slowest daemon. Errors per host are surfaced as a footer line
//! rather than blanking the table; last-known good rows stay rendered.

use std::sync::Arc;

use futures_util::future::join_all;
use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Tabs};

use crate::config::Config;
use crate::docker_ops::{DockerOps, Host, ImageInfo, NetworkInfo, VolumeInfo};
use crate::output::{format_bytes, format_relative_time};

use super::ui::{
    FilterState, TABLE_HIGHLIGHT_SYMBOL, bold, clamp_selection, filter_footer, pane_layout,
    table_highlight_style,
};

/// Which sub-tab inside `View::Resources` is active. `Tab` / `BackTab`
/// (or direct letters `i` / `v` / `n`) cycles between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceTab {
    Images,
    Volumes,
    Networks,
}

impl ResourceTab {
    pub fn next(self) -> Self {
        match self {
            Self::Images => Self::Volumes,
            Self::Volumes => Self::Networks,
            Self::Networks => Self::Images,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::Images => Self::Networks,
            Self::Volumes => Self::Images,
            Self::Networks => Self::Volumes,
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Images => 0,
            Self::Volumes => 1,
            Self::Networks => 2,
        }
    }
}

/// One identifier the resources pane needs to know about. Used as the
/// "what was I selected on" anchor for the kill/prune flows in `app.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceTarget {
    pub host: Host,
    pub kind: ResourceTab,
    /// Image id / volume name / network name.
    pub id: String,
    /// Human display string for the confirmation modal.
    pub label: String,
}

pub struct ResourcesRefresh {
    pub images: Vec<(Host, Result<Vec<ImageInfo>, String>)>,
    pub volumes: Vec<(Host, Result<Vec<VolumeInfo>, String>)>,
    pub networks: Vec<(Host, Result<Vec<NetworkInfo>, String>)>,
}

#[derive(Default)]
pub struct ResourcesState {
    pub tab: Option<ResourceTab>,
    images: Vec<ImageInfo>,
    volumes: Vec<VolumeInfo>,
    networks: Vec<NetworkInfo>,
    /// Per-host fetch errors (one row per failing host), rendered as a
    /// footer line under the table.
    errors: Vec<String>,
    images_table: TableState,
    volumes_table: TableState,
    networks_table: TableState,
    loaded: bool,
    pub filter: FilterState,
}

impl ResourcesState {
    pub fn new() -> Self {
        Self {
            tab: Some(ResourceTab::Images),
            ..Self::default()
        }
    }

    pub fn current_tab(&self) -> ResourceTab {
        self.tab.unwrap_or(ResourceTab::Images)
    }

    pub fn set_tab(&mut self, tab: ResourceTab) {
        self.tab = Some(tab);
    }

    pub fn cycle_tab_forward(&mut self) {
        let next = self.current_tab().next();
        self.tab = Some(next);
    }

    pub fn cycle_tab_backward(&mut self) {
        let prev = self.current_tab().prev();
        self.tab = Some(prev);
    }

    pub fn select_next(&mut self) {
        let n = self.visible_indices().len();
        let table = self.active_table_mut();
        if n == 0 {
            return;
        }
        let i = table.selected().unwrap_or(0);
        table.select(Some((i + 1).min(n - 1)));
    }

    pub fn select_prev(&mut self) {
        if self.visible_indices().is_empty() {
            return;
        }
        let table = self.active_table_mut();
        let i = table.selected().unwrap_or(0);
        table.select(Some(i.saturating_sub(1)));
    }

    fn active_table_mut(&mut self) -> &mut TableState {
        match self.current_tab() {
            ResourceTab::Images => &mut self.images_table,
            ResourceTab::Volumes => &mut self.volumes_table,
            ResourceTab::Networks => &mut self.networks_table,
        }
    }

    fn active_table_selected(&self) -> Option<usize> {
        let t = match self.current_tab() {
            ResourceTab::Images => &self.images_table,
            ResourceTab::Volumes => &self.volumes_table,
            ResourceTab::Networks => &self.networks_table,
        };
        t.selected()
    }

    /// Indices into the active tab's source list passing the filter.
    fn visible_indices(&self) -> Vec<usize> {
        match self.current_tab() {
            ResourceTab::Images => self
                .images
                .iter()
                .enumerate()
                .filter_map(|(i, img)| {
                    let target = format!("{} {} {}", img.host, img.id, img.repo_tags.join(" "));
                    self.filter.matches(&target).then_some(i)
                })
                .collect(),
            ResourceTab::Volumes => self
                .volumes
                .iter()
                .enumerate()
                .filter_map(|(i, v)| {
                    let target = format!("{} {} {}", v.host, v.name, v.driver);
                    self.filter.matches(&target).then_some(i)
                })
                .collect(),
            ResourceTab::Networks => self
                .networks
                .iter()
                .enumerate()
                .filter_map(|(i, n)| {
                    let target = format!("{} {} {} {}", n.host, n.name, n.driver, n.scope);
                    self.filter.matches(&target).then_some(i)
                })
                .collect(),
        }
    }

    /// Currently-selected target on the active tab, if any. Returns
    /// `None` when the row points at the `local` sentinel host —
    /// pruning the operator's laptop docker from a remote-deploy
    /// TUI session is almost always an accident.
    pub fn selected_target(&self, config: &Config) -> Option<ResourceTarget> {
        let visible = self.visible_indices();
        let i = self.active_table_selected()?;
        let src = *visible.get(i)?;
        match self.current_tab() {
            ResourceTab::Images => {
                let img = self.images.get(src)?;
                let host = host_for_address(config, &img.host)?;
                let label = if let Some(tag) = img.repo_tags.first() {
                    format!("{tag} ({})", img.id)
                } else {
                    img.id.clone()
                };
                let id = img
                    .repo_tags
                    .first()
                    .filter(|t| t.as_str() != "<none>:<none>")
                    .cloned()
                    .unwrap_or_else(|| img.id.clone());
                Some(ResourceTarget {
                    host,
                    kind: ResourceTab::Images,
                    id,
                    label,
                })
            }
            ResourceTab::Volumes => {
                let v = self.volumes.get(src)?;
                let host = host_for_address(config, &v.host)?;
                Some(ResourceTarget {
                    host,
                    kind: ResourceTab::Volumes,
                    id: v.name.clone(),
                    label: v.name.clone(),
                })
            }
            ResourceTab::Networks => {
                let n = self.networks.get(src)?;
                let host = host_for_address(config, &n.host)?;
                Some(ResourceTarget {
                    host,
                    kind: ResourceTab::Networks,
                    id: n.name.clone(),
                    label: n.name.clone(),
                })
            }
        }
    }

    pub fn apply(&mut self, data: ResourcesRefresh) {
        self.loaded = true;
        let mut errors = Vec::new();
        let mut images = Vec::new();
        for (host, res) in data.images {
            match res {
                Ok(rows) => images.extend(rows),
                Err(e) => errors.push(format!("images@{}: {e}", host.address)),
            }
        }
        // Sort: dangling first (so prune candidates jump out), then by
        // size descending — operators usually want to know "what's
        // taking up space".
        images.sort_by(|a, b| {
            b.dangling
                .cmp(&a.dangling)
                .then_with(|| b.size_bytes.cmp(&a.size_bytes))
        });
        self.images = images;

        let mut volumes = Vec::new();
        for (host, res) in data.volumes {
            match res {
                Ok(rows) => volumes.extend(rows),
                Err(e) => errors.push(format!("volumes@{}: {e}", host.address)),
            }
        }
        volumes.sort_by(|a, b| a.host.cmp(&b.host).then_with(|| a.name.cmp(&b.name)));
        self.volumes = volumes;

        let mut networks = Vec::new();
        for (host, res) in data.networks {
            match res {
                Ok(rows) => networks.extend(rows),
                Err(e) => errors.push(format!("networks@{}: {e}", host.address)),
            }
        }
        networks.sort_by(|a, b| a.host.cmp(&b.host).then_with(|| a.name.cmp(&b.name)));
        self.networks = networks;

        self.errors = errors;
        // Clamp against per-tab *filtered* counts. `selected()` indexes
        // into each tab's `visible_indices()` slice, so clamping to the
        // unfiltered totals here would let `selected` point past the
        // filtered list for a frame.
        let images_visible = self
            .images
            .iter()
            .filter(|img| {
                let target = format!("{} {} {}", img.host, img.id, img.repo_tags.join(" "));
                self.filter.matches(&target)
            })
            .count();
        let volumes_visible = self
            .volumes
            .iter()
            .filter(|v| {
                let target = format!("{} {} {}", v.host, v.name, v.driver);
                self.filter.matches(&target)
            })
            .count();
        let networks_visible = self
            .networks
            .iter()
            .filter(|n| {
                let target = format!("{} {} {} {}", n.host, n.name, n.driver, n.scope);
                self.filter.matches(&target)
            })
            .count();
        clamp_selection(&mut self.images_table, images_visible);
        clamp_selection(&mut self.volumes_table, volumes_visible);
        clamp_selection(&mut self.networks_table, networks_visible);
    }

    pub fn render(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        _config: &Config,
        throbber: &throbber_widgets_tui::ThrobberState,
    ) {
        let layout = pane_layout(area);
        let header = layout[0];
        let body = layout[1];
        let footer = layout[2];

        // Tab bar rendered inline at the top of the pane (the global
        // header tabs already mark "Resources" as the active section;
        // this row shows which sub-tab is selected).
        let tab_titles: Vec<Line<'_>> = ["Images", "Volumes", "Networks"]
            .iter()
            .map(|t| Line::from(*t))
            .collect();
        let tabs = Tabs::new(tab_titles)
            .style(Style::default().fg(Color::White))
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED),
            )
            .divider(Span::styled(" │ ", Style::default().fg(Color::Gray)))
            .select(self.current_tab().index());
        frame.render_widget(tabs, header);

        match self.current_tab() {
            ResourceTab::Images => self.render_images(frame, body, throbber),
            ResourceTab::Volumes => self.render_volumes(frame, body, throbber),
            ResourceTab::Networks => self.render_networks(frame, body, throbber),
        }

        // Footer: filter line + per-tab help on the right.
        let help = match self.current_tab() {
            ResourceTab::Images => {
                "Tab cycle · x remove · P prune dangling · A prune all · r refresh · ?"
            }
            ResourceTab::Volumes | ResourceTab::Networks => {
                "Tab cycle · x remove · P prune unused · r refresh · ?"
            }
        };
        frame.render_widget(filter_footer(&self.filter, help), footer);

        // Per-host errors render *above* the footer as a faint red
        // ribbon when present — same shape the dashboard uses for
        // partial fetch failures.
        if !self.errors.is_empty() && body.height > 2 {
            let line = self.errors.join(" · ");
            let err_area = Rect {
                x: body.x,
                y: body.y + body.height - 1,
                width: body.width,
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(line).style(Style::default().fg(Color::Red)),
                err_area,
            );
        }
    }

    fn render_images(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        throbber: &throbber_widgets_tui::ThrobberState,
    ) {
        let widths = [
            Constraint::Length(16), // host
            Constraint::Length(14), // id
            Constraint::Length(10), // size
            Constraint::Length(10), // age
            Constraint::Length(10), // dangling
            Constraint::Min(20),    // tags
        ];
        let visible = self.visible_indices();
        clamp_selection(&mut self.images_table, visible.len());
        let total = self.images.iter().map(|i| i.size_bytes).sum::<i64>();
        let dangling_count = self.images.iter().filter(|i| i.dangling).count();

        let rows: Vec<Row<'_>> = if !self.loaded {
            vec![Row::new(vec![Cell::from(super::ui::loading_line(
                throbber,
            ))])]
        } else if self.images.is_empty() {
            vec![Row::new(vec![Cell::from("(no images cached on any host)")])]
        } else if visible.is_empty() {
            vec![Row::new(vec![Cell::from("(no rows match the filter)")])]
        } else {
            visible
                .iter()
                .filter_map(|i| self.images.get(*i))
                .map(|img| {
                    let host = img.host.clone();
                    let id = img.id.clone();
                    let size = format_bytes(img.size_bytes);
                    let age = format_relative_time(img.created_unix);
                    let dangling = if img.dangling { "yes" } else { "" };
                    let dang_cell = if img.dangling {
                        Cell::from(dangling).style(Style::default().fg(Color::Yellow))
                    } else {
                        Cell::from(dangling)
                    };
                    let tags = if img.repo_tags.is_empty() {
                        "<none>".to_string()
                    } else {
                        img.repo_tags.join(", ")
                    };
                    Row::new(vec![
                        Cell::from(host),
                        Cell::from(id),
                        Cell::from(size),
                        Cell::from(age),
                        dang_cell,
                        Cell::from(tags),
                    ])
                })
                .collect()
        };

        let title = format!(
            " images · {} total · {} dangling · {} on disk ",
            self.images.len(),
            dangling_count,
            format_bytes(total)
        );
        let block = Block::default().borders(Borders::ALL).title(title);
        let table = Table::new(rows, widths)
            .header(Row::new(vec![
                Cell::from("host").style(bold()),
                Cell::from("id").style(bold()),
                Cell::from("size").style(bold()),
                Cell::from("age").style(bold()),
                Cell::from("dangling").style(bold()),
                Cell::from("tags").style(bold()),
            ]))
            .row_highlight_style(table_highlight_style())
            .highlight_symbol(TABLE_HIGHLIGHT_SYMBOL)
            .block(block);
        frame.render_stateful_widget(table, area, &mut self.images_table);
    }

    fn render_volumes(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        throbber: &throbber_widgets_tui::ThrobberState,
    ) {
        let widths = [
            Constraint::Length(16), // host
            Constraint::Length(28), // name
            Constraint::Length(10), // driver
            Constraint::Min(30),    // mountpoint
        ];
        let visible = self.visible_indices();
        clamp_selection(&mut self.volumes_table, visible.len());

        let rows: Vec<Row<'_>> = if !self.loaded {
            vec![Row::new(vec![Cell::from(super::ui::loading_line(
                throbber,
            ))])]
        } else if self.volumes.is_empty() {
            vec![Row::new(vec![Cell::from("(no volumes on any host)")])]
        } else if visible.is_empty() {
            vec![Row::new(vec![Cell::from("(no rows match the filter)")])]
        } else {
            visible
                .iter()
                .filter_map(|i| self.volumes.get(*i))
                .map(|v| {
                    Row::new(vec![
                        Cell::from(v.host.clone()),
                        Cell::from(v.name.clone()),
                        Cell::from(v.driver.clone()),
                        Cell::from(v.mountpoint.clone()),
                    ])
                })
                .collect()
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" volumes · {} total ", self.volumes.len()));
        let table = Table::new(rows, widths)
            .header(Row::new(vec![
                Cell::from("host").style(bold()),
                Cell::from("name").style(bold()),
                Cell::from("driver").style(bold()),
                Cell::from("mountpoint").style(bold()),
            ]))
            .row_highlight_style(table_highlight_style())
            .highlight_symbol(TABLE_HIGHLIGHT_SYMBOL)
            .block(block);
        frame.render_stateful_widget(table, area, &mut self.volumes_table);
    }

    fn render_networks(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        throbber: &throbber_widgets_tui::ThrobberState,
    ) {
        let widths = [
            Constraint::Length(16), // host
            Constraint::Length(28), // name
            Constraint::Length(10), // driver
            Constraint::Length(10), // scope
            Constraint::Length(10), // internal
        ];
        let visible = self.visible_indices();
        clamp_selection(&mut self.networks_table, visible.len());

        let rows: Vec<Row<'_>> = if !self.loaded {
            vec![Row::new(vec![Cell::from(super::ui::loading_line(
                throbber,
            ))])]
        } else if self.networks.is_empty() {
            vec![Row::new(vec![Cell::from("(no networks)")])]
        } else if visible.is_empty() {
            vec![Row::new(vec![Cell::from("(no rows match the filter)")])]
        } else {
            visible
                .iter()
                .filter_map(|i| self.networks.get(*i))
                .map(|n| {
                    Row::new(vec![
                        Cell::from(n.host.clone()),
                        Cell::from(n.name.clone()),
                        Cell::from(n.driver.clone()),
                        Cell::from(n.scope.clone()),
                        Cell::from(if n.internal { "yes" } else { "" }),
                    ])
                })
                .collect()
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" networks · {} total ", self.networks.len()));
        let table = Table::new(rows, widths)
            .header(Row::new(vec![
                Cell::from("host").style(bold()),
                Cell::from("name").style(bold()),
                Cell::from("driver").style(bold()),
                Cell::from("scope").style(bold()),
                Cell::from("internal").style(bold()),
            ]))
            .row_highlight_style(table_highlight_style())
            .highlight_symbol(TABLE_HIGHLIGHT_SYMBOL)
            .block(block);
        frame.render_stateful_widget(table, area, &mut self.networks_table);
    }
}

fn host_for_address(config: &Config, address: &str) -> Option<Host> {
    config
        .hosts
        .iter()
        .find(|h| h.address == address)
        .map(Host::from)
}

/// Background-friendly fetch — fans out per-host across all three
/// resource families in parallel. Errors per host land in the result
/// vec as `Err(String)` so the pane can render partial successes.
pub async fn fetch_owned(ops: Arc<dyn DockerOps>, config: Arc<Config>) -> ResourcesRefresh {
    let hosts: Vec<Host> = config.hosts.iter().map(Host::from).collect();

    // Three concurrent fan-outs, one per resource family.
    let images_fut = {
        let ops = ops.clone();
        let hosts = hosts.clone();
        async move {
            let futs = hosts.into_iter().map(|host| {
                let ops = ops.clone();
                async move {
                    let res = ops.list_images(&host).await.map_err(|e| e.to_string());
                    (host, res)
                }
            });
            join_all(futs).await
        }
    };
    let volumes_fut = {
        let ops = ops.clone();
        let hosts = hosts.clone();
        async move {
            let futs = hosts.into_iter().map(|host| {
                let ops = ops.clone();
                async move {
                    let res = ops.list_volumes(&host).await.map_err(|e| e.to_string());
                    (host, res)
                }
            });
            join_all(futs).await
        }
    };
    let networks_fut = {
        let ops = ops.clone();
        let hosts = hosts.clone();
        async move {
            let futs = hosts.into_iter().map(|host| {
                let ops = ops.clone();
                async move {
                    let res = ops.list_networks(&host).await.map_err(|e| e.to_string());
                    (host, res)
                }
            });
            join_all(futs).await
        }
    };

    let (images, volumes, networks) =
        futures_util::future::join3(images_fut, volumes_fut, networks_fut).await;
    ResourcesRefresh {
        images,
        volumes,
        networks,
    }
}
