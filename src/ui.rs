//! Rendering. Reads [`App`], never the filesystem. Palette follows the
//! "paper and ink" look: bone on black, with vermilion and gold only for load.

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Sparkline, Table, TableState,
    },
};

use crate::metrics::{human_bytes, human_kb, human_uptime};
use crate::state::{App, Mode, SortKey};

const INK: Color = Color::Rgb(0xcd, 0xc4, 0xba);
const DIM: Color = Color::Rgb(0x95, 0x8f, 0x87);
const FAINT: Color = Color::Rgb(0x5a, 0x56, 0x51);
const GOLD: Color = Color::Rgb(0xd9, 0xa4, 0x41);
const VERMILION: Color = Color::Rgb(0xe2, 0x34, 0x2a);
const PAPER: Color = Color::Rgb(0, 0, 0);

const MIN_W: u16 = 60;
const MIN_H: u16 = 16;

/// Bone while calm, gold when busy, vermilion when saturated.
pub fn load_color(pct: f32) -> Color {
    if pct >= 85.0 {
        VERMILION
    } else if pct >= 60.0 {
        GOLD
    } else {
        INK
    }
}

/// A `width`-cell bar for `pct` (0–100): `████░░░░`. Out-of-range values are
/// clamped, so a bad reading can never overflow the layout.
pub fn bar(pct: f32, width: usize) -> String {
    let pct = if pct.is_nan() {
        0.0
    } else {
        pct.clamp(0.0, 100.0)
    };
    let filled = ((pct / 100.0) * width as f32).round() as usize;
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

fn block(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(FAINT))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(INK).add_modifier(Modifier::BOLD),
        ))
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    f.render_widget(
        Block::default().style(Style::default().bg(PAPER).fg(INK)),
        area,
    );
    if area.width < MIN_W || area.height < MIN_H {
        let msg = format!(
            "Terminal demasiado pequeño ({}×{}); hace falta {MIN_W}×{MIN_H}.",
            area.width, area.height
        );
        f.render_widget(Paragraph::new(msg).style(Style::default().fg(GOLD)), area);
        return;
    }

    let top_h = (area.height / 3).clamp(8, 14);
    let [header, top, procs, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(top_h),
        Constraint::Min(5),
        Constraint::Length(1),
    ])
    .areas(area);

    draw_header(f, header, app);
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(top);
    draw_cpu(f, left, app);
    draw_resources(f, right, app);
    draw_procs(f, procs, app);
    draw_footer(f, footer, app);

    match app.mode.clone() {
        Mode::Kill { pid, name } => draw_kill(f, area, pid, &name),
        Mode::Help => draw_help(f, area),
        _ => {}
    }
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let m = &app.metrics;
    let mut spans = vec![
        Span::styled(
            " sysTUI ",
            Style::default()
                .fg(PAPER)
                .bg(INK)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "  {} · activo {} · carga {:.2} {:.2} {:.2} · {} núcleos",
                app.hostname,
                human_uptime(m.uptime_s),
                m.load.0,
                m.load.1,
                m.load.2,
                m.cores.len()
            ),
            Style::default().fg(DIM),
        ),
    ];
    if app.paused {
        spans.push(Span::styled(
            "  PAUSA",
            Style::default().fg(VERMILION).add_modifier(Modifier::BOLD),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_cpu(f: &mut Frame, area: Rect, app: &App) {
    let m = &app.metrics;
    let b = block(&format!("CPU {:>3.0}%", m.cpu));
    let inner = b.inner(area);
    f.render_widget(b, area);
    if inner.height < 2 {
        return;
    }
    let graph_h = if inner.height >= 6 { 3 } else { 1 };
    let [graph, cores] =
        Layout::vertical([Constraint::Length(graph_h), Constraint::Min(0)]).areas(inner);

    let data: Vec<u64> = app
        .cpu_hist
        .last(graph.width as usize)
        .iter()
        .map(|v| *v as u64)
        .collect();
    f.render_widget(
        Sparkline::default()
            .data(data)
            .max(100)
            .style(Style::default().fg(load_color(m.cpu))),
        graph,
    );

    // Two columns of per-core bars; whatever does not fit is summarised.
    let cols = 2usize;
    let col_w = (cores.width as usize / cols).max(12);
    let capacity = cores.height as usize * cols;
    let shown = if m.cores.len() > capacity && capacity > 0 {
        capacity - 1
    } else {
        m.cores.len().min(capacity)
    };
    let bar_w = col_w.saturating_sub(12).clamp(4, 24);
    let mut lines: Vec<Line> = Vec::new();
    for row in 0..cores.height as usize {
        let mut spans = Vec::new();
        for c in 0..cols {
            // Column-major so core numbers read down, then across.
            let i = c * cores.height as usize + row;
            if i < shown {
                let pct = m.cores[i];
                spans.push(Span::styled(format!("{i:>2} "), Style::default().fg(DIM)));
                spans.push(Span::styled(
                    bar(pct, bar_w),
                    Style::default().fg(load_color(pct)),
                ));
                spans.push(Span::styled(
                    format!(" {pct:>3.0}% "),
                    Style::default().fg(INK),
                ));
            } else if i == shown && m.cores.len() > shown {
                spans.push(Span::styled(
                    format!("+{} núcleos más", m.cores.len() - shown),
                    Style::default().fg(FAINT),
                ));
            }
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), cores);
}

fn meter_line(label: &str, used_kb: u64, total_kb: u64, width: u16) -> Line<'static> {
    let pct = if total_kb > 0 {
        used_kb as f32 * 100.0 / total_kb as f32
    } else {
        0.0
    };
    let text = format!(
        " {}/{} {:>3.0}%",
        human_kb(used_kb),
        human_kb(total_kb),
        pct
    );
    let bar_w = (width as usize)
        .saturating_sub(label.len() + 1 + text.chars().count())
        .clamp(4, 40);
    Line::from(vec![
        Span::styled(format!("{label} "), Style::default().fg(DIM)),
        Span::styled(bar(pct, bar_w), Style::default().fg(load_color(pct))),
        Span::styled(text, Style::default().fg(INK)),
    ])
}

/// Sum two histories point by point (rx+tx, read+write).
fn summed(a: &[f64], b: &[f64]) -> Vec<u64> {
    let n = a.len().min(b.len());
    let (a, b) = (&a[a.len() - n..], &b[b.len() - n..]);
    a.iter().zip(b).map(|(x, y)| (x + y) as u64).collect()
}

fn draw_resources(f: &mut Frame, area: Rect, app: &App) {
    let m = &app.metrics;
    let b = block("MEMORIA · RED · DISCO");
    let inner = b.inner(area);
    f.render_widget(b, area);
    if inner.height < 6 {
        return;
    }
    let [mem, swap, net_l, net_g, disk_l, disk_g] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .areas(inner);

    f.render_widget(
        Paragraph::new(meter_line(
            "MEM ",
            m.mem.used_kb(),
            m.mem.total_kb,
            inner.width,
        )),
        mem,
    );
    f.render_widget(
        Paragraph::new(meter_line(
            "SWAP",
            m.mem.swap_used_kb(),
            m.mem.swap_total_kb,
            inner.width,
        )),
        swap,
    );

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("RED   ", Style::default().fg(DIM)),
            Span::raw(format!(
                "↓ {}/s  ↑ {}/s",
                human_bytes(m.net_rx_s),
                human_bytes(m.net_tx_s)
            )),
        ])),
        net_l,
    );
    let n = inner.width as usize;
    let net = summed(&app.net_rx_hist.last(n), &app.net_tx_hist.last(n));
    f.render_widget(
        Sparkline::default()
            .data(net)
            .style(Style::default().fg(INK)),
        net_g,
    );

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("DISCO ", Style::default().fg(DIM)),
            Span::raw(format!(
                "L {}/s  E {}/s",
                human_bytes(m.disk_read_s),
                human_bytes(m.disk_write_s)
            )),
        ])),
        disk_l,
    );
    let disk = summed(&app.disk_r_hist.last(n), &app.disk_w_hist.last(n));
    f.render_widget(
        Sparkline::default()
            .data(disk)
            .style(Style::default().fg(GOLD)),
        disk_g,
    );
}

fn draw_procs(f: &mut Frame, area: Rect, app: &mut App) {
    // Rows that fit under the header line, so PgUp/PgDn move one screenful.
    app.page = (area.height as usize).saturating_sub(3).max(1);
    let rows = app.visible();
    let total = app.metrics.procs.len();
    let title = if app.filter.is_empty() {
        format!("PROCESOS {total}")
    } else {
        format!(
            "PROCESOS {} de {total} · filtro «{}»",
            rows.len(),
            app.filter
        )
    };
    let b = block(&title);
    let inner = b.inner(area);
    f.render_widget(b, area);

    let arrow = |key: SortKey| {
        if app.sort != key {
            return "";
        }
        let descending = matches!(key, SortKey::Cpu | SortKey::Mem) != app.reverse;
        if descending { "▼" } else { "▲" }
    };
    let head = |label: &str, key: SortKey| {
        let style = if app.sort == key {
            Style::default().fg(INK).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(DIM)
        };
        Cell::from(format!("{label}{}", arrow(key))).style(style)
    };
    let header = Row::new(vec![
        head("PID", SortKey::Pid),
        Cell::from("USUARIO").style(Style::default().fg(DIM)),
        Cell::from("E").style(Style::default().fg(DIM)),
        head("CPU%", SortKey::Cpu),
        head("MEM", SortKey::Mem),
        head("PROCESO", SortKey::Name),
        Cell::from("COMANDO").style(Style::default().fg(DIM)),
    ]);

    let table_rows: Vec<Row> = rows
        .iter()
        .map(|p| {
            Row::new(vec![
                Cell::from(p.pid.to_string()),
                Cell::from(truncate(&p.user, 10)),
                Cell::from(p.state.to_string()),
                Cell::from(format!("{:.1}", p.cpu)).style(Style::default().fg(load_color(p.cpu))),
                Cell::from(human_kb(p.rss_kb)),
                Cell::from(truncate(&p.name, 18)),
                Cell::from(p.cmd.clone()).style(Style::default().fg(DIM)),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(8),
        Constraint::Length(10),
        Constraint::Length(2),
        Constraint::Length(6),
        Constraint::Length(8),
        Constraint::Length(19),
        Constraint::Min(10),
    ];
    let table = Table::new(table_rows, widths)
        .header(header)
        .column_spacing(1)
        .row_highlight_style(
            Style::default()
                .fg(PAPER)
                .bg(INK)
                .add_modifier(Modifier::BOLD),
        );
    let mut state = TableState::default().with_selected(app.selected_index());
    f.render_stateful_widget(table, inner, &mut state);
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let line = match &app.mode {
        Mode::Filter => Line::from(vec![
            Span::styled(" / ", Style::default().fg(PAPER).bg(INK)),
            Span::raw(format!(" {}", app.filter)),
            Span::styled("▏", Style::default().fg(VERMILION)),
            Span::styled("   Enter aplicar · Esc borrar", Style::default().fg(FAINT)),
        ]),
        _ => {
            let hint = format!(
                " ↑↓ mover · c m p n orden · / filtro · x terminar · espacio pausa · +/- {:.2}s · ? ayuda · q salir",
                app.interval_ms as f64 / 1000.0
            );
            match &app.status {
                Some(s) => Line::from(vec![
                    Span::styled(format!(" {s} "), Style::default().fg(PAPER).bg(GOLD)),
                    Span::styled(hint, Style::default().fg(FAINT)),
                ]),
                None => Line::from(Span::styled(hint, Style::default().fg(FAINT))),
            }
        }
    };
    f.render_widget(Paragraph::new(line), area);
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

fn popup(f: &mut Frame, area: Rect, title: &str, lines: Vec<Line<'static>>) {
    f.render_widget(Clear, area);
    let b = block(title).border_style(Style::default().fg(INK));
    f.render_widget(
        Paragraph::new(lines)
            .block(b)
            .style(Style::default().bg(PAPER)),
        area,
    );
}

fn draw_kill(f: &mut Frame, area: Rect, pid: u32, name: &str) {
    let r = centered(area, 50, 7);
    popup(
        f,
        r,
        "TERMINAR PROCESO",
        vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("{} (PID {pid})", truncate(name, 30)),
                Style::default().fg(INK).add_modifier(Modifier::BOLD),
            ))
            .alignment(Alignment::Center),
            Line::from(""),
            Line::from("Enter: pedir que termine (TERM)").alignment(Alignment::Center),
            Line::from(Span::styled(
                "9: matar a la fuerza (KILL) · Esc: cancelar",
                Style::default().fg(DIM),
            ))
            .alignment(Alignment::Center),
        ],
    );
}

fn draw_help(f: &mut Frame, area: Rect) {
    let keys = [
        ("↑ ↓  j k", "mover la selección"),
        ("AvPág RePág", "saltar una página"),
        ("g  G", "primero / último"),
        ("c  m  p  n", "ordenar por CPU, memoria, PID, nombre"),
        ("(misma tecla)", "invertir el orden"),
        ("/", "filtrar por nombre, comando, usuario o PID"),
        ("x", "terminar el proceso (pide confirmación)"),
        ("espacio", "pausar / reanudar"),
        ("+  -", "muestrear más rápido / más lento"),
        ("q", "salir"),
    ];
    let mut lines = vec![Line::from("")];
    for (k, d) in keys {
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {k:<14}"),
                Style::default().fg(INK).add_modifier(Modifier::BOLD),
            ),
            Span::styled(d, Style::default().fg(DIM)),
        ]));
    }
    let r = centered(area, 62, lines.len() as u16 + 2);
    popup(f, r, "AYUDA", lines);
}

/// Render one frame of `app` into plain text. Used by `--snapshot`, which
/// lets the interface be checked (and shown in a README) without a TTY.
pub fn render_text(app: &mut App, width: u16, height: u16) -> String {
    use ratatui::{Terminal, backend::TestBackend};
    let mut term = Terminal::new(TestBackend::new(width, height)).expect("test backend");
    term.draw(|f| draw(f, app)).expect("draw");
    let buf = term.backend().buffer().clone();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{Metrics, ProcRow};
    use crate::procfs::MemInfo;

    fn sample_app() -> App {
        let mut a = App::new("ryoku".into());
        for i in 0..30 {
            a.update(Metrics {
                cpu: (i * 3 % 100) as f32,
                cores: vec![10.0, 95.0, 70.0, 5.0],
                mem: MemInfo {
                    total_kb: 16 * 1024 * 1024,
                    available_kb: 6 * 1024 * 1024,
                    swap_total_kb: 4 * 1024 * 1024,
                    swap_free_kb: 4 * 1024 * 1024,
                },
                load: (0.5, 0.4, 0.3),
                uptime_s: 90_000,
                net_rx_s: 2.0 * 1024.0 * 1024.0,
                net_tx_s: 100.0 * 1024.0,
                procs: vec![
                    ProcRow {
                        pid: 4242,
                        user: "adrian".into(),
                        state: 'R',
                        cpu: 87.5,
                        rss_kb: 512_000,
                        name: "cargo".into(),
                        cmd: "cargo build --release".into(),
                    },
                    ProcRow {
                        pid: 1,
                        user: "root".into(),
                        state: 'S',
                        cpu: 0.0,
                        rss_kb: 12_000,
                        name: "systemd".into(),
                        cmd: "/sbin/init".into(),
                    },
                ],
                ..Metrics::default()
            });
        }
        a
    }

    #[test]
    fn bar_is_proportional_and_clamped() {
        assert_eq!(bar(50.0, 8), "████░░░░");
        assert_eq!(bar(0.0, 4), "░░░░");
        assert_eq!(bar(250.0, 4), "████");
        assert_eq!(bar(-5.0, 4), "░░░░");
        assert_eq!(bar(f32::NAN, 4), "░░░░");
        assert_eq!(bar(100.0, 0), "");
    }

    #[test]
    fn load_color_thresholds() {
        assert_eq!(load_color(10.0), INK);
        assert_eq!(load_color(60.0), GOLD);
        assert_eq!(load_color(85.0), VERMILION);
    }

    #[test]
    fn truncate_counts_chars_not_bytes() {
        assert_eq!(truncate("canción", 4), "can…");
        assert_eq!(truncate("ab", 4), "ab");
    }

    #[test]
    fn full_frame_shows_every_section() {
        let mut a = sample_app();
        let t = render_text(&mut a, 100, 30);
        for needle in [
            "sysTUI",
            "ryoku",
            "CPU",
            "MEMORIA",
            "RED",
            "DISCO",
            "PROCESOS 2",
            "cargo build --release",
            "87.5",
            "10.0G/16.0G",
            "↓ 2.0M/s",
        ] {
            assert!(t.contains(needle), "missing {needle:?} in:\n{t}");
        }
    }

    #[test]
    fn tiny_terminal_gets_a_message_not_a_panic() {
        let mut a = sample_app();
        assert!(render_text(&mut a, 40, 10).contains("demasiado pequeño"));
    }

    #[test]
    fn every_size_from_minimum_up_renders() {
        let mut a = sample_app();
        for (w, h) in [(60, 16), (61, 17), (80, 24), (120, 40), (200, 60)] {
            render_text(&mut a, w, h);
        }
    }

    #[test]
    fn many_cores_are_summarised_instead_of_overflowing() {
        let mut a = sample_app();
        a.metrics.cores = vec![50.0; 64];
        let t = render_text(&mut a, 100, 24);
        assert!(t.contains("núcleos más"), "{t}");
    }

    #[test]
    fn popups_render() {
        let mut a = sample_app();
        a.mode = Mode::Kill {
            pid: 4242,
            name: "cargo".into(),
        };
        assert!(render_text(&mut a, 100, 30).contains("TERMINAR PROCESO"));
        a.mode = Mode::Help;
        assert!(render_text(&mut a, 100, 30).contains("AYUDA"));
    }

    #[test]
    fn filter_shows_the_count_and_the_text() {
        let mut a = sample_app();
        a.filter = "cargo".into();
        let t = render_text(&mut a, 100, 30);
        assert!(t.contains("PROCESOS 1 de 2"), "{t}");
    }
}
