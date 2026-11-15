//! Owned-transcript command previews with full command/output retained for local disclosure.

use super::concise::MARKER;
use super::concise::OUTPUT_FIRST_PREFIX;
use super::concise::OUTPUT_PREFIX_WIDTH;
use super::concise::OUTPUT_REST_PREFIX;
use super::concise::command_label;
use super::concise::exit_suffix;
use super::concise::first_line;
use super::concise::summary_line;
use super::model::ExecCell;
use crate::exec_command::strip_bash_lc_and_escape;
use crate::history_cell::ActivityDisclosure;
use crate::history_cell::HistoryRenderMode;
use crate::history_cell::activity_preview::DETAIL_PREVIEW_LINES;
use crate::history_cell::activity_preview::clipped_line;
use crate::motion::MotionMode;
use crate::motion::ReducedMotionIndicator;
use crate::motion::activity_indicator;
use crate::terminal_hyperlinks::HyperlinkLine;
use codex_ansi_escape::ansi_escape_line;
use ratatui::style::Modifier;
use ratatui::style::Stylize;
use ratatui::text::Line;

impl ExecCell {
    pub(super) fn command_disclosure(&self, width: u16) -> Option<ActivityDisclosure> {
        let [call] = self.group.calls.as_slice() else {
            return None;
        };
        // Interaction output is intentionally absent from the expanded transcript.
        if !call.is_unified_exec_interaction()
            && let Some(output) = &call.output
        {
            let retained = output.line_counts().1;
            let clipped = output
                .lines()
                .rev()
                .take(DETAIL_PREVIEW_LINES)
                .filter(|raw| {
                    ansi_escape_line(raw.as_ref()).width() + OUTPUT_PREFIX_WIDTH
                        > usize::from(width)
                })
                .count();
            let hidden = retained.saturating_sub(DETAIL_PREVIEW_LINES) + clipped;
            if hidden > 0 {
                return Some(ActivityDisclosure::OutputLines(hidden));
            }
        }
        if !self
            .group
            .details
            .lines_after(/*after_calls*/ 1, width, HistoryRenderMode::Rich)
            .is_empty()
        {
            return Some(ActivityDisclosure::Generic);
        }
        let script = strip_bash_lc_and_escape(&call.command);
        if script.lines().nth(/*n*/ 1).is_some()
            || self
                .compact_command_lines(u16::MAX)
                .first()
                .is_some_and(|header| header.width() > usize::from(width))
        {
            return Some(ActivityDisclosure::Generic);
        }
        call.output
            .as_ref()
            .is_some_and(|output| {
                output.line_counts().1 > DETAIL_PREVIEW_LINES
                    || output.transcript_lines().any(|line| {
                        ansi_escape_line(line.as_ref()).width() + OUTPUT_PREFIX_WIDTH
                            > usize::from(width)
                    })
            })
            .then_some(ActivityDisclosure::Generic)
    }

    pub(super) fn compact_command_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        let [call] = self.group.calls.as_slice() else {
            return Vec::new();
        };
        if call.is_unified_exec_interaction()
            && !self.is_active()
            && call.output.as_ref().is_some_and(|output| {
                output.exit_code == 0 && output.lines().all(|line| line.trim().is_empty())
            })
        {
            return Vec::new();
        }
        let nonzero_exit = call.output.as_ref().filter(|output| output.exit_code != 0);
        let marker = if nonzero_exit.is_some() {
            MARKER.red().bold()
        } else if self.is_active() {
            activity_indicator(
                call.start_time,
                MotionMode::from_animations_enabled(self.animations_enabled()),
                ReducedMotionIndicator::Hidden,
            )
            .unwrap_or_else(|| MARKER.dim())
        } else {
            MARKER.green().bold()
        };
        let mut header = Line::from(vec![marker, " ".into()]);
        let mut is_update = false;
        if call.is_unified_exec_interaction() {
            // Background polls have no command of their own to label.
            let script = strip_bash_lc_and_escape(&call.command);
            header.push_span("Background output ".bold());
            header.push_span(first_line(&script));
        } else {
            let (label, update) = command_label(&call.command);
            is_update = update;
            header.spans.extend(label);
        }
        let suffix =
            nonzero_exit.map(|output| exit_suffix(output.exit_code, /*benign*/ false));
        let mut lines = vec![summary_line(header, suffix, width)];
        let show_output = !is_update || nonzero_exit.is_some();
        if let Some(output) = call.output.as_ref().filter(|_| show_output) {
            let tail: Vec<_> = output.lines().rev().take(DETAIL_PREVIEW_LINES).collect();
            for (index, raw) in tail.into_iter().rev().enumerate() {
                let mut line = ansi_escape_line(raw.as_ref());
                line.spans.insert(
                    /*index*/ 0,
                    if index == 0 {
                        OUTPUT_FIRST_PREFIX
                    } else {
                        OUTPUT_REST_PREFIX
                    }
                    .dim(),
                );
                line.spans.iter_mut().for_each(|span| {
                    span.style = span.style.add_modifier(Modifier::DIM);
                });
                lines.push(clipped_line(line, width));
            }
            if lines.len() == 1 && !call.is_unified_exec_interaction() {
                lines.push(clipped_line(
                    Line::from(format!("{OUTPUT_FIRST_PREFIX}(no output)").dim()),
                    width,
                ));
            }
        }
        lines
    }
}
