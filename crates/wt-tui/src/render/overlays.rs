//! Modal overlays: help, copy, performance, pickers, confirmations, logs.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::Line,
    widgets::{Clear, Paragraph, Wrap},
};

use super::{centered, panel, panel_owned};
use crate::{Interaction, Model};

pub(crate) fn render(frame: &mut Frame<'_>, model: &mut Model, area: Rect) {
    if let Some(selected) = model.yank {
        let choices = model.yank_choices();
        let overlay = centered(area, 60, (choices.len() + 3) as u16);
        frame.render_widget(Clear, overlay);
        let lines = choices
            .iter()
            .enumerate()
            .map(|(index, (key, label, value))| {
                Line::from(format!(
                    "{} {key}  {label}: {value}",
                    if index == selected { "›" } else { " " }
                ))
                .style(if index == selected {
                    Style::new().fg(Color::Cyan)
                } else {
                    Style::new()
                })
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(lines).block(panel("Copy · Enter/y confirms · Esc cancels")),
            overlay,
        );
    }
    if model.show_perf {
        let overlay = centered(
            area,
            area.width.saturating_sub(4),
            area.height.saturating_sub(4),
        );
        frame.render_widget(Clear, overlay);
        let mut lines = vec![Line::from(format!(
            "Frames: {} · last draw: {} µs · continuous: {}",
            model.frame_count,
            model.last_frame_micros,
            if model.perf_continuous { "on" } else { "off" }
        ))];
        if model.board.perf.is_empty() {
            lines.push(Line::from("Sampling…"));
        }
        lines.extend(
            model
                .board
                .perf
                .iter()
                .skip(model.perf_scroll)
                .take(overlay.height.saturating_sub(3) as usize)
                .map(|line| Line::from(line.as_str())),
        );
        frame.render_widget(
            Paragraph::new(lines).block(panel(
                "Performance · r sample · i continuous · j/k scroll · P closes",
            )),
            overlay,
        );
    }
    if model.help {
        let overlay = centered(area, 74, area.height.saturating_sub(2));
        frame.render_widget(Clear, overlay);
        let query = model.help_query.text();
        let filtered = crate::help::filtered_lines(&query);
        let visible_lines = overlay.height.saturating_sub(3) as usize;
        model.help_scroll = model
            .help_scroll
            .min(filtered.len().saturating_sub(visible_lines));
        let mut lines = filtered
            .iter()
            .skip(model.help_scroll)
            .take(visible_lines)
            .map(|line| Line::from(*line))
            .collect::<Vec<_>>();
        if filtered.is_empty() {
            lines.push(Line::from("No matching help entries"));
        }
        let title = if model.help_searching {
            "wt keymap · / filter · Enter done · Esc clear"
        } else if query.is_empty() {
            "wt keymap · / search · j/k scroll · Esc closes"
        } else {
            "wt keymap · / search · Esc clear · q closes"
        };
        frame.render_widget(Paragraph::new(lines).block(panel(title)), overlay);
        if model.help_searching && overlay.width > 4 {
            let label = "/";
            let (text, cursor) = model
                .help_query
                .viewport(overlay.width.saturating_sub(4) as usize);
            frame.set_cursor_position((overlay.x + 2 + cursor, overlay.y + overlay.height - 2));
            frame.render_widget(
                Paragraph::new(format!("{label}{text}")),
                Rect::new(
                    overlay.x + 1,
                    overlay.y + overlay.height - 2,
                    overlay.width - 2,
                    1,
                ),
            );
        }
    }
    match &model.interaction {
        Interaction::Reviewers(picker) => {
            let height = (picker.candidates.len() as u16 + 2).min(area.height.saturating_sub(2));
            let visible = height.saturating_sub(2) as usize;
            let offset = picker
                .selected
                .saturating_sub(visible / 2)
                .min(picker.candidates.len().saturating_sub(visible));
            let overlay = centered(area, area.width.saturating_sub(4).min(80), height);
            frame.render_widget(Clear, overlay);
            let lines = picker
                .candidates
                .iter()
                .enumerate()
                .skip(offset)
                .take(visible)
                .map(|(index, option)| {
                    Line::from(format!(
                        "{} [{}] {}",
                        if index == picker.selected { "›" } else { " " },
                        if option.selected { "x" } else { " " },
                        option.label
                    ))
                    .style(if index == picker.selected {
                        Style::new().fg(Color::Cyan)
                    } else {
                        Style::new()
                    })
                })
                .collect::<Vec<_>>();
            frame.render_widget(
                Paragraph::new(lines).block(panel(
                    "Reviewers · Space toggles · v/Enter submits · Esc cancels",
                )),
                overlay,
            );
        }
        Interaction::Log {
            title,
            lines,
            scroll,
        } => {
            let overlay = centered(
                area,
                area.width.saturating_sub(4),
                area.height.saturating_sub(4),
            );
            frame.render_widget(Clear, overlay);
            let lines = lines
                .iter()
                .skip(*scroll)
                .take(overlay.height.saturating_sub(2) as usize)
                .map(|line| Line::from(line.as_str()))
                .collect::<Vec<_>>();
            frame.render_widget(
                Paragraph::new(lines)
                    .block(panel_owned(format!("{title} · j/k scroll · Esc closes"))),
                overlay,
            );
        }
        Interaction::Confirm(confirm) => {
            let max_lines = area.height.saturating_sub(4) as usize;
            let visible = confirm
                .lines
                .iter()
                .enumerate()
                .skip(confirm.selected.saturating_sub(max_lines.saturating_sub(1)))
                .take(max_lines);
            let lines = visible
                .map(|(index, line)| {
                    Line::from(format!(
                        "{} {line}",
                        if index == confirm.selected {
                            "›"
                        } else {
                            " "
                        }
                    ))
                    .style(if index == confirm.selected {
                        Style::new().fg(Color::Cyan)
                    } else {
                        Style::new()
                    })
                })
                .collect::<Vec<_>>();
            let height = (lines.len() as u16 + 2).min(area.height);
            let overlay = centered(area, area.width.saturating_sub(4).min(72), height);
            frame.render_widget(Clear, overlay);
            frame.render_widget(
                Paragraph::new(lines)
                    .block(panel_owned(format!(
                        "{} · Enter/y confirms · Esc cancels",
                        confirm.title
                    )))
                    .wrap(Wrap { trim: false }),
                overlay,
            );
        }
        Interaction::Picker(picker) => {
            let height = (picker.options.len() as u16 + 2).min(area.height.saturating_sub(2));
            let visible = height.saturating_sub(2) as usize;
            let offset = picker
                .selected
                .saturating_sub(visible / 2)
                .min(picker.options.len().saturating_sub(visible));
            let overlay = centered(area, area.width.saturating_sub(4).min(72), height);
            frame.render_widget(Clear, overlay);
            let lines = picker
                .options
                .iter()
                .enumerate()
                .skip(offset)
                .take(height.saturating_sub(2) as usize)
                .map(|(index, option)| {
                    let chord = option.chord.map(|c| format!("{c} ")).unwrap_or_default();
                    Line::from(format!(
                        "{} {chord}{}",
                        if index == picker.selected { "›" } else { " " },
                        option.label
                    ))
                    .style(if index == picker.selected {
                        Style::new().fg(Color::Cyan)
                    } else {
                        Style::new()
                    })
                })
                .collect::<Vec<_>>();
            frame.render_widget(
                Paragraph::new(lines).block(panel_owned(format!(
                    "{} · Enter confirms · Esc cancels",
                    picker.title
                ))),
                overlay,
            );
        }
        Interaction::Text(_) | Interaction::None => {}
    }
}
