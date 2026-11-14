//! One-line action labels shared by the scrollback and transcript renderers:
//! `Read(x)`, `Read 3 files`, `Search("q" in p)`, `List(p)`, `Update(f)`, `Bash(cmd)`.

use super::edit_detect::edited_paths;
use crate::exec_command::strip_bash_lc_and_escape;
use codex_protocol::parse_command::ParsedCommand;
use ratatui::style::Stylize;
use ratatui::text::Span;

pub(super) const MARKER: &str = "⏺";
pub(super) const OUTPUT_FIRST_PREFIX: &str = "  ⎿  ";
pub(super) const OUTPUT_REST_PREFIX: &str = "     ";
/// Display columns taken by either output prefix.
pub(super) const OUTPUT_PREFIX_WIDTH: usize = 5;

/// Label for a command call that is not an exploring read/search/list. The flag is true when
/// the command was recognized as a file edit, whose output is noise unless it failed.
pub(super) fn command_label(command: &[String]) -> (Vec<Span<'static>>, bool) {
    if let Some(paths) = edited_paths(command) {
        let label = match paths.as_slice() {
            [path] => tool("Update", path.clone()),
            _ => vec![format!("Update {} files", paths.len()).bold()],
        };
        return (label, true);
    }
    (
        tool("Bash", first_line(&strip_bash_lc_and_escape(command))),
        false,
    )
}

/// Label for one exploring call's parsed actions, joined with ", " for compound commands.
pub(super) fn parsed_label(parsed: &[ParsedCommand]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (index, action) in parsed.iter().enumerate() {
        if index > 0 {
            spans.push(", ".dim());
        }
        spans.extend(match action {
            ParsedCommand::Read { name, .. } => tool("Read", name.clone()),
            ParsedCommand::ListFiles { cmd, path } => {
                tool("List", path.clone().unwrap_or_else(|| cmd.clone()))
            }
            ParsedCommand::Search { cmd, query, path } => {
                let target = match (query, path) {
                    (Some(q), Some(p)) => format!("\"{q}\" in {p}"),
                    (Some(q), None) => format!("\"{q}\""),
                    _ => cmd.clone(),
                };
                tool("Search", target)
            }
            ParsedCommand::Unknown { cmd } => tool("Bash", first_line(cmd)),
        });
    }
    spans
}

/// `Read(name)` for one file, `Read N files` for several.
pub(super) fn reads_label(names: &[String]) -> Vec<Span<'static>> {
    match names {
        [name] => tool("Read", name.clone()),
        _ => vec![format!("Read {} files", names.len()).bold()],
    }
}

pub(super) fn exit_suffix(code: i32, benign: bool) -> Span<'static> {
    let text = format!(" · exit {code}");
    if benign { text.dim() } else { text.red() }
}

fn tool(name: &'static str, arg: String) -> Vec<Span<'static>> {
    vec![name.bold(), "(".into(), arg.into(), ")".into()]
}

/// First line of a script, with " …" when more lines were dropped.
pub(super) fn first_line(script: &str) -> String {
    let mut lines = script.lines();
    let first = lines.next().unwrap_or_default().to_string();
    if lines.next().is_some() {
        format!("{first} …")
    } else {
        first
    }
}
