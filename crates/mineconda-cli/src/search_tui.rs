use std::io::IsTerminal;

use anyhow::{Context, Result};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode,
};
use mineconda_core::{LoaderKind, ModSource};
use mineconda_resolver::{InstallVersionsRequest, SearchResult, list_install_versions};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::i18n;

#[derive(Debug, Clone, Copy)]
pub struct SearchInteractiveInstallRequest {
    pub index: usize,
    pub choose_version: bool,
}

pub fn run_search_interactive(
    query: &str,
    results: &[SearchResult],
    no_color: bool,
    environment_label: Option<&str>,
) -> Result<Option<SearchInteractiveInstallRequest>> {
    let mut session = SearchInteractiveSession::enter()?;
    let backend = CrosstermBackend::new(&mut session.stdout);
    let mut terminal = Terminal::new(backend).context("failed to initialize terminal UI")?;
    let mut selected = 0usize;
    let mut offset = 0usize;
    let use_color = std::env::var_os("NO_COLOR").is_none() && !no_color;

    loop {
        let (_, terminal_height) = terminal::size().unwrap_or((120, 40));
        let list_height = interactive_list_height(terminal_height as usize);
        if selected < offset {
            offset = selected;
        } else if selected >= offset.saturating_add(list_height) {
            offset = selected.saturating_add(1).saturating_sub(list_height);
        }

        terminal.draw(|frame| {
            render_search_results(
                frame,
                query,
                results,
                selected,
                offset,
                use_color,
                environment_label,
            )
        })?;

        let event = event::read().context("failed to read interactive key event")?;
        if let Event::Key(key) = event {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    selected = selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = (selected + 1).min(results.len().saturating_sub(1));
                }
                KeyCode::Enter => {
                    return Ok(Some(SearchInteractiveInstallRequest {
                        index: selected,
                        choose_version: true,
                    }));
                }
                KeyCode::Char('v') | KeyCode::Char('V') => {
                    return Ok(Some(SearchInteractiveInstallRequest {
                        index: selected,
                        choose_version: true,
                    }));
                }
                KeyCode::Char('l') | KeyCode::Char('L') => {
                    return Ok(Some(SearchInteractiveInstallRequest {
                        index: selected,
                        choose_version: false,
                    }));
                }
                KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(None);
                }
                _ => {}
            }
        }
    }
}

pub fn choose_install_version_interactive(
    id: &str,
    source: ModSource,
    title: &str,
    no_color: bool,
    minecraft_version: Option<&str>,
    loader: Option<LoaderKind>,
) -> Result<Option<String>> {
    let request = InstallVersionsRequest {
        source,
        id: id.to_string(),
        limit: 40,
        minecraft_version: minecraft_version.map(std::string::ToString::to_string),
        loader,
    };
    let spinner_enabled = std::io::stderr().is_terminal()
        && std::env::var_os("CI").is_none()
        && std::env::var_os("MINECONDA_NO_SPINNER").is_none();
    let spinner_label = format!(
        "{} `{}` @{}",
        i18n::text("loading versions for", "正在加载版本"),
        id,
        source.as_str()
    );
    let spinner = crate::SearchSpinner::start(spinner_label, spinner_enabled);
    let versions = list_install_versions(&request)?;
    drop(spinner);

    if versions.is_empty() {
        eprintln!(
            "{} {} [{}]",
            i18n::text("no installable versions found for", "未找到可安装版本："),
            id,
            source.as_str()
        );
        return Ok(None);
    }

    let environment_label = match (minecraft_version, loader) {
        (Some(minecraft), Some(loader)) => {
            Some(format!("{minecraft}/{}", crate::loader_label(loader)))
        }
        _ => None,
    };

    let mut session = SearchInteractiveSession::enter()?;
    let backend = CrosstermBackend::new(&mut session.stdout);
    let mut terminal = Terminal::new(backend).context("failed to initialize terminal UI")?;
    let mut selected = 0usize;
    let mut offset = 0usize;
    let use_color = std::env::var_os("NO_COLOR").is_none() && !no_color;
    loop {
        let (_, terminal_height) = terminal::size().unwrap_or((120, 40));
        let list_height = version_list_height(terminal_height as usize);
        if selected < offset {
            offset = selected;
        } else if selected >= offset.saturating_add(list_height) {
            offset = selected.saturating_add(1).saturating_sub(list_height);
        }

        terminal.draw(|frame| {
            let view = VersionPickerView {
                title,
                id,
                source,
                versions: &versions,
                selected,
                offset,
                use_color,
                environment_label: environment_label.as_deref(),
            };
            render_version_picker(frame, &view)
        })?;

        let event = event::read().context("failed to read version selection key event")?;
        if let Event::Key(key) = event {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    selected = selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = (selected + 1).min(versions.len().saturating_sub(1));
                }
                KeyCode::Enter => return Ok(Some(versions[selected].value.clone())),
                KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(None);
                }
                _ => {}
            }
        }
    }
}

fn render_search_results(
    frame: &mut Frame,
    query: &str,
    results: &[SearchResult],
    selected: usize,
    offset: usize,
    use_color: bool,
    environment_label: Option<&str>,
) {
    let selected = selected.min(results.len().saturating_sub(1));
    let area = frame.area();
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(8),
            Constraint::Length(1),
        ])
        .split(area);

    let env_suffix = environment_label
        .map(|value| format!("  env={value}"))
        .unwrap_or_default();
    let header = Paragraph::new(format!(
        "🔎 {}: `{}`  {} {}{}",
        i18n::text("Search", "搜索"),
        query,
        i18n::text("results", "结果"),
        results.len(),
        env_suffix
    ));
    frame.render_widget(header, vertical[0]);

    let body = if area.width >= 100 {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(vertical[1])
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(vertical[1])
    };

    let list_height = body[0].height.saturating_sub(2) as usize;
    let end = (offset + list_height).min(results.len());
    let list_items: Vec<ListItem> = results
        .iter()
        .enumerate()
        .skip(offset)
        .take(end.saturating_sub(offset))
        .map(|(_, item)| {
            let side = crate::format_supported_side(item.supported_side).unwrap_or("-");
            ListItem::new(format!(
                "{} [{}] {}={} {}={}",
                item.title,
                item.source.as_str(),
                i18n::text("side", "端"),
                side,
                i18n::text("deps", "依赖"),
                item.dependencies.len()
            ))
        })
        .collect();
    let list = List::new(list_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(i18n::text("Results", "搜索结果")),
        )
        .highlight_style(highlight_style(use_color))
        .highlight_symbol("❯ ");
    let mut list_state = ListState::default();
    if selected >= offset && selected < end {
        list_state.select(Some(selected - offset));
    }
    frame.render_stateful_widget(list, body[0], &mut list_state);

    let item = &results[selected];
    let mut details = vec![
        Line::from(vec![
            Span::styled(
                format!("{}: ", i18n::text("Selected", "选中")),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(item.title.clone()),
            Span::raw(format!(" ({})", item.source.as_str())),
        ]),
        Line::from(format!(
            "ID: {}",
            crate::optional_value(item.id.as_str()).unwrap_or("-")
        )),
        Line::from(format!(
            "Slug: {}",
            crate::optional_value(item.slug.as_str()).unwrap_or("-")
        )),
        Line::from(format!(
            "{}: {}",
            i18n::text("Homepage", "主页"),
            crate::optional_value(item.url.as_str()).unwrap_or("-")
        )),
        Line::from(format!(
            "{}: {}",
            i18n::text("Supported side", "适用端"),
            crate::format_supported_side(item.supported_side).unwrap_or("-")
        )),
        Line::from(""),
    ];
    if let Some(summary) = crate::optional_value(item.summary.as_str()) {
        details.push(Line::from(Span::styled(
            i18n::text("Summary:", "简介:"),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for line in crate::wrap_visual(summary, body[1].width.saturating_sub(4) as usize) {
            details.push(Line::from(format!("  {line}")));
        }
    }

    let detail = Paragraph::new(details)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(i18n::text("Details", "详情")),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(detail, body[1]);

    let footer = Paragraph::new(i18n::text(
        "↑/↓ select, Enter/V choose version, L quick install latest, q/Esc quit",
        "↑/↓ 选择, Enter/V 选版本安装, L 快速安装最新版, q/Esc 退出",
    ));
    frame.render_widget(footer, vertical[2]);
}

struct VersionPickerView<'a> {
    title: &'a str,
    id: &'a str,
    source: ModSource,
    versions: &'a [mineconda_resolver::InstallVersion],
    selected: usize,
    offset: usize,
    use_color: bool,
    environment_label: Option<&'a str>,
}

fn render_version_picker(frame: &mut Frame, view: &VersionPickerView<'_>) {
    let selected = view.selected.min(view.versions.len().saturating_sub(1));
    let area = frame.area();
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(2),
        ])
        .split(area);

    let env_suffix = view
        .environment_label
        .map(|value| format!(" env={value}"))
        .unwrap_or_default();
    let header = Paragraph::new(format!(
        "📦 {}: {} [{}]{}",
        i18n::text("Choose install version", "选择安装版本"),
        crate::optional_value(view.title).unwrap_or(view.id),
        view.source.as_str(),
        env_suffix
    ));
    frame.render_widget(header, vertical[0]);

    let list_height = vertical[1].height.saturating_sub(2) as usize;
    let end = (view.offset + list_height).min(view.versions.len());
    let list_items: Vec<ListItem> = view
        .versions
        .iter()
        .skip(view.offset)
        .take(end.saturating_sub(view.offset))
        .map(|item| ListItem::new(item.label.clone()))
        .collect();
    let list = List::new(list_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("ID: {}", view.id)),
        )
        .highlight_style(highlight_style(view.use_color))
        .highlight_symbol("❯ ");
    let mut list_state = ListState::default();
    if selected >= view.offset && selected < end {
        list_state.select(Some(selected - view.offset));
    }
    frame.render_stateful_widget(list, vertical[1], &mut list_state);

    let selected_item = &view.versions[selected];
    let mut footer_lines = vec![Line::from(format!(
        "{}: {}",
        i18n::text("value", "值"),
        selected_item.value
    ))];
    if let Some(published_at) = selected_item.published_at.as_deref() {
        footer_lines.push(Line::from(format!(
            "{}: {}",
            i18n::text("published", "发布时间"),
            published_at
        )));
    }
    footer_lines.push(Line::from(i18n::text(
        "Enter install, q/Esc cancel",
        "Enter 安装, q/Esc 取消",
    )));
    let footer = Paragraph::new(footer_lines);
    frame.render_widget(footer, vertical[2]);
}

fn highlight_style(use_color: bool) -> Style {
    if use_color {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    }
}

fn interactive_list_height(total_rows: usize) -> usize {
    total_rows.saturating_sub(12).max(4)
}

fn version_list_height(total_rows: usize) -> usize {
    total_rows.saturating_sub(8).max(4)
}

struct SearchInteractiveSession {
    stdout: std::io::Stdout,
    use_alt_screen: bool,
}

impl SearchInteractiveSession {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("failed to enter raw mode")?;
        let mut stdout = std::io::stdout();
        let use_alt_screen = std::env::var_os("MINECONDA_ALT_SCREEN")
            .map(|value| value != "0")
            .unwrap_or_else(|| std::env::var_os("TMUX").is_none());
        if use_alt_screen {
            execute!(stdout, EnterAlternateScreen, Hide)
                .context("failed to enter interactive screen")?;
        } else {
            execute!(stdout, Hide).context("failed to hide cursor for interactive screen")?;
        }
        Ok(Self {
            stdout,
            use_alt_screen,
        })
    }
}

impl Drop for SearchInteractiveSession {
    fn drop(&mut self) {
        if self.use_alt_screen {
            let _ = execute!(self.stdout, Show, LeaveAlternateScreen);
        } else {
            let _ = execute!(self.stdout, Show, MoveTo(0, 0), Clear(ClearType::All));
        }
        let _ = disable_raw_mode();
    }
}
