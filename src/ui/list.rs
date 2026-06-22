//! The main list screen: search box, host list with match highlighting, hint bar.

use std::collections::HashMap;

use nucleo_matcher::Matcher;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::app::App;
use crate::model::Host;
use crate::search;

pub fn render(frame: &mut Frame, app: &App) {
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(frame.area());

    render_search(frame, app, chunks[0]);
    render_list(frame, app, chunks[1]);
    render_hint(frame, app, chunks[2]);
}

fn render_search(frame: &mut Frame, app: &App, area: Rect) {
    let title = format!(" sshelf  {}/{} ", app.order.len(), app.hosts.len());
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    let text = Line::from(vec![
        Span::styled("> ", Style::default().fg(super::accent())),
        Span::raw(app.query.as_str()),
    ]);
    frame.render_widget(Paragraph::new(text).block(block), area);

    // Place the cursor right after the typed query.
    let cx = inner.x + 2 + app.query.chars().count() as u16;
    let cx = cx.min(inner.x + inner.width.saturating_sub(1));
    frame.set_cursor_position((cx, inner.y));
}

fn render_list(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL);

    if app.order.is_empty() {
        let msg = if app.hosts.is_empty() {
            "No hosts yet — press ^a to add one, or import with ^o."
        } else {
            "No matches."
        };
        frame.render_widget(
            Paragraph::new(msg)
                .style(Style::default().fg(Color::DarkGray))
                .block(block),
            area,
        );
        return;
    }

    let name_w = app
        .order
        .iter()
        .map(|&i| app.hosts[i].name.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(6, 24);

    let mut matcher = search::matcher();
    let base = Style::default();
    let hl = Style::default()
        .fg(super::accent())
        .add_modifier(Modifier::BOLD);
    // Highlight only the fuzzy part of the query (not any `tag:`/`site:` tokens).
    let (_, _, fuzzy) = search::parse_query(&app.query);

    // Idle → group hosts under site headers; filtering → a flat list with a site column.
    // `app.selected` indexes `order` (hosts only); headers shift the ListState index, so we
    // track the selected host's position among the rendered items.
    let grouped = app.query.is_empty();
    let mut items: Vec<ListItem> = Vec::new();
    let mut selected_listidx = 0usize;

    if grouped {
        let header_style = Style::default().fg(Color::DarkGray);
        let mut counts: HashMap<String, usize> = HashMap::new();
        for &i in &app.order {
            *counts.entry(section_key(&app.hosts[i])).or_default() += 1;
        }
        let mut prev: Option<String> = None;
        for (pos, &i) in app.order.iter().enumerate() {
            let h = &app.hosts[i];
            let key = section_key(h);
            if prev.as_deref() != Some(key.as_str()) {
                let n = counts.get(&key).copied().unwrap_or(0);
                items.push(ListItem::new(Line::from(Span::styled(
                    format!("── {} ({}) ──", section_display(h), n),
                    header_style,
                ))));
                prev = Some(key);
            }
            if pos == app.selected {
                selected_listidx = items.len();
            }
            items.push(host_row(h, name_w, &fuzzy, &mut matcher, base, hl, false));
        }
    } else {
        for (pos, &i) in app.order.iter().enumerate() {
            if pos == app.selected {
                selected_listidx = items.len();
            }
            items.push(host_row(
                &app.hosts[i],
                name_w,
                &fuzzy,
                &mut matcher,
                base,
                hl,
                true,
            ));
        }
    }

    let list = List::new(items)
        .block(block)
        .highlight_symbol("▸ ")
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    let mut state = ListState::default();
    state.select(Some(selected_listidx));
    frame.render_stateful_widget(list, area, &mut state);
}

/// The section a host belongs to, for display (its site name, or `(no site)`).
fn section_display(h: &Host) -> &str {
    h.site.as_deref().unwrap_or("(no site)")
}

/// The case-insensitive section key (matches `app::group_order`'s grouping).
fn section_key(h: &Host) -> String {
    section_display(h).to_lowercase()
}

/// One host row: `name  user@host:port  [tags]`, fuzzy-highlighted, with an optional dim
/// `·site·` column (shown only in the flat/filtered view, where there are no section headers).
fn host_row(
    h: &Host,
    name_w: usize,
    fuzzy: &str,
    matcher: &mut Matcher,
    base: Style,
    hl: Style,
    show_site: bool,
) -> ListItem<'static> {
    let mut text = format!("{:<width$}  {}", h.name, h.endpoint(), width = name_w);
    if !h.tags.is_empty() {
        text.push_str(&format!("  [{}]", h.tags.join(",")));
    }
    let indices = search::match_indices(&text, fuzzy, matcher);
    let mut spans = super::highlight(&text, &indices, base, hl);
    if show_site && let Some(site) = &h.site {
        spans.push(Span::styled(
            format!("  ·{site}·"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    ListItem::new(Line::from(spans))
}

fn render_hint(frame: &mut Frame, app: &App, area: Rect) {
    if let Some(status) = &app.status {
        frame.render_widget(
            Paragraph::new(status.as_str()).style(Style::default().fg(super::accent())),
            area,
        );
        return;
    }
    let hint = "↵ connect  ^a add  ^e edit  ^d del  ^y yank  ^t transfer  ^f forward  ^o import  F1 help  F2 settings  F3 sites  F4 forwards  esc quit";
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::config::Config;
    use crate::model::Host;
    use crate::paths::Paths;
    use crate::state::FrecencyState;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
        buf.content().iter().map(|c| c.symbol()).collect()
    }

    fn app_with(query: &str) -> App {
        let hosts = vec![
            Host::new("prod-web", "10.0.0.1"),
            Host::new("prod-db", "10.0.0.2"),
            Host::new("bastion", "h.example.com"),
        ];
        let paths = Paths {
            config_dir: std::env::temp_dir(),
            data_dir: std::env::temp_dir(),
            config_file_override: None,
        };
        let mut app = App::new(
            hosts,
            Vec::new(),
            FrecencyState::default(),
            Config::default(),
            paths,
        );
        app.query.push_str(query);
        app.recompute();
        app
    }

    fn draw(app: &App) -> String {
        let mut term = Terminal::new(TestBackend::new(80, 12)).unwrap();
        term.draw(|f| render(f, app)).unwrap();
        buffer_text(term.backend().buffer())
    }

    #[test]
    fn renders_all_hosts_when_idle() {
        let text = draw(&app_with(""));
        assert!(text.contains("prod-web"));
        assert!(text.contains("bastion"));
        assert!(text.contains("3/3")); // hint count
    }

    #[test]
    fn filtering_hides_nonmatches() {
        let text = draw(&app_with("prod"));
        assert!(text.contains("prod-web"));
        assert!(text.contains("prod-db"));
        assert!(!text.contains("bastion"));
    }

    #[test]
    fn idle_groups_under_site_headers() {
        let mut app = app_with("");
        app.hosts[0].site = Some("prod-dc".into());
        app.hosts[1].site = Some("prod-dc".into());
        app.recompute(); // bastion (idx 2) stays site-less
        let text = draw(&app);
        assert!(
            text.contains("prod-dc"),
            "expected a prod-dc section header"
        );
        assert!(text.contains("(no site)"), "expected a (no site) group");
    }

    #[test]
    fn filtering_shows_the_site_column() {
        let mut app = app_with("");
        app.hosts[0].site = Some("prod-dc".into());
        app.query.push_str("prod-web");
        app.recompute();
        let text = draw(&app);
        assert!(
            text.contains("·prod-dc·"),
            "expected a ·site· column when filtering"
        );
    }

    fn buffer_lines(buf: &ratatui::buffer::Buffer) -> Vec<String> {
        let w = buf.area.width as usize;
        buf.content()
            .chunks(w)
            .map(|row| row.iter().map(|c| c.symbol()).collect())
            .collect()
    }

    /// Renders a representative screen and writes an ASCII snapshot to
    /// `target/tui-snapshot.txt` for eyeballing the layout (no TTY needed).
    #[test]
    fn writes_snapshot_artifact() {
        let mut prod_web = Host::new("prod-web", "10.25.25.10");
        prod_web.user = Some("deploy".into());
        prod_web.tags = vec!["prod".into(), "web".into()];
        let mut prod_db = Host::new("prod-db", "10.25.25.25");
        prod_db.user = Some("mike".into());
        prod_db.port = Some(5432);
        prod_db.tags = vec!["prod".into(), "db".into()];
        let mut bastion = Host::new("bastion", "bastion.example.com");
        bastion.user = Some("ops".into());
        bastion.tags = vec!["infra".into()];

        let paths = Paths {
            config_dir: std::env::temp_dir(),
            data_dir: std::env::temp_dir(),
            config_file_override: None,
        };
        let app = App::new(
            vec![prod_web, prod_db, bastion],
            Vec::new(),
            FrecencyState::default(),
            Config::default(),
            paths,
        );

        let mut term = Terminal::new(TestBackend::new(72, 10)).unwrap();
        term.draw(|f| render(f, &app)).unwrap();
        let lines = buffer_lines(term.backend().buffer());
        let snapshot: String = lines.join("\n");

        assert!(snapshot.contains("prod-web"));
        assert!(snapshot.contains("connect"));

        if let Ok(dir) = std::env::var("CARGO_MANIFEST_DIR") {
            let path = std::path::Path::new(&dir).join("target/tui-snapshot.txt");
            let _ = std::fs::write(path, &snapshot);
        }
    }

    #[test]
    fn highlight_groups_matched_runs() {
        let base = Style::default();
        let hl = Style::default().fg(Color::Cyan);
        let spans = crate::ui::highlight("prod-web", &[0, 1, 2, 3], base, hl);
        assert_eq!(spans[0].content, "prod");
        assert_eq!(spans[0].style.fg, Some(Color::Cyan));
        assert_eq!(spans[1].content, "-web");
        assert_eq!(spans[1].style.fg, None);
    }
}
