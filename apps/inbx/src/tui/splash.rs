use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream};
use futures_util::StreamExt;
use hjkl_splash::{CellKind, Layout, Splash, default_trail_color};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::Clear;

use super::Term;

const ART: &str = include_str!("../art.txt");

pub async fn play(term: &mut Term) -> Result<()> {
    // Strip trailing blank lines.
    let art_lines: Vec<&str> = ART
        .lines()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .skip_while(|l| l.trim().is_empty())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let art = art_lines.join("\n");

    let art_rows = art_lines.len() as u16;
    let art_cols = art_lines
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0) as u16;

    // Reading-order path: collect every non-space char (row, col, ch).
    let path: Vec<(u8, u8, char)> = art_lines
        .iter()
        .enumerate()
        .flat_map(|(row, line)| {
            line.chars().enumerate().filter_map(move |(col, ch)| {
                if ch != ' ' {
                    Some((row as u8, col as u8, ch))
                } else {
                    None
                }
            })
        })
        .collect();

    let trail_len = hjkl_splash::DEFAULT_TRAIL_LEN as u64;
    let done_tick = path.len() as u64 + trail_len + 15;

    let splash = Splash::new(&art, &path).with_period(Duration::from_millis(33));
    let mut events = EventStream::new();

    loop {
        let size = term.size()?;
        let layout = Layout::centered(size.width, size.height, art_rows, art_cols);

        term.draw(|f| {
            let area = f.area();
            f.render_widget(Clear, area);

            for cell in splash.cells(layout) {
                if cell.x >= area.width || cell.y >= area.height {
                    continue;
                }
                let style = match cell.kind {
                    CellKind::Art => Style::default().fg(Color::DarkGray),
                    CellKind::Trail { age } => {
                        let rgb = default_trail_color(age);
                        Style::default().fg(Color::Rgb(rgb.0, rgb.1, rgb.2))
                    }
                    CellKind::Cursor => Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                };
                let span = Span::styled(cell.ch.to_string(), style);
                f.buffer_mut().set_span(cell.x, cell.y, &span, 1);
            }
        })?;

        let done = tokio::select! {
            ev = events.next() => {
                matches!(ev, Some(Ok(Event::Key(_))) | None)
            }
            _ = tokio::time::sleep(Duration::from_millis(33)) => false,
        };
        if done || splash.tick() >= done_tick {
            break;
        }
    }

    Ok(())
}
