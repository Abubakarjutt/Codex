//! Detects shell commands that write files to literal paths, so the transcript can summarize
//! them as `Update(path)` instead of echoing the whole script. Display-only: never used for
//! approvals. Returns `None` whenever a target can't be named with certainty.

use crate::exec_command::strip_bash_lc_and_escape;
use regex_lite::Regex;
use std::sync::LazyLock;

static PATH_BINDING: LazyLock<Regex> = LazyLock::new(|| {
    #[expect(clippy::unwrap_used)]
    Regex::new(r#"(?m)^\s*([A-Za-z_]\w*)\s*=\s*Path\(\s*['"]([^'"]+)['"]\s*\)\s*$"#).unwrap()
});
static VAR_WRITE: LazyLock<Regex> = LazyLock::new(|| {
    #[expect(clippy::unwrap_used)]
    Regex::new(r"\b([A-Za-z_]\w*)\.write_(?:text|bytes)\(").unwrap()
});
static INLINE_PATH_WRITE: LazyLock<Regex> = LazyLock::new(|| {
    #[expect(clippy::unwrap_used)]
    Regex::new(r#"Path\(\s*['"]([^'"]+)['"]\s*\)\.write_(?:text|bytes)\("#).unwrap()
});
static OPEN_WRITE: LazyLock<Regex> = LazyLock::new(|| {
    #[expect(clippy::unwrap_used)]
    Regex::new(r#"open\(\s*['"]([^'"]+)['"]\s*,\s*['"][wax]b?\+?['"]"#).unwrap()
});

/// File paths a command writes, in first-seen order without duplicates, or `None` when the
/// command is not a recognizable write to literal paths.
pub(crate) fn edited_paths(command: &[String]) -> Option<Vec<String>> {
    let script = strip_bash_lc_and_escape(command);
    let first_word = script.split_whitespace().next()?;
    let paths = if is_python(first_word) {
        python_written_paths(&script)?
    } else {
        shell_written_paths(&script)?
    };
    let mut unique: Vec<String> = Vec::new();
    for path in paths {
        if !is_literal_path(&path) {
            return None;
        }
        if !unique.contains(&path) {
            unique.push(path);
        }
    }
    (!unique.is_empty()).then_some(unique)
}

fn is_python(word: &str) -> bool {
    word == "python" || word == "python3" || word.starts_with("python3.")
}

fn python_written_paths(script: &str) -> Option<Vec<String>> {
    let bindings: Vec<(&str, &str)> = PATH_BINDING
        .captures_iter(script)
        .filter_map(|c| Some((c.get(1)?.as_str(), c.get(2)?.as_str())))
        .collect();
    // (byte offset, path) so files are reported in the order they are written.
    let mut writes: Vec<(usize, String)> = Vec::new();
    for c in VAR_WRITE.captures_iter(script) {
        let var = c.get(1)?;
        if var.as_str() == "Path" {
            continue;
        }
        // A write through a variable we can't resolve to a literal path makes the target unknown.
        let (_, path) = bindings
            .iter()
            .rev()
            .find(|(name, _)| *name == var.as_str())?;
        writes.push((var.start(), (*path).to_string()));
    }
    for re in [&*INLINE_PATH_WRITE, &*OPEN_WRITE] {
        for c in re.captures_iter(script) {
            let m = c.get(1)?;
            writes.push((m.start(), m.as_str().to_string()));
        }
    }
    writes.sort_by_key(|(offset, _)| *offset);
    Some(writes.into_iter().map(|(_, path)| path).collect())
}

/// Every `&&`/`;` step on the first line must be a write (`cat|echo|printf > f`, `| tee f`,
/// `sed -i … f`); a heredoc body after the first line is content, not commands.
fn shell_written_paths(script: &str) -> Option<Vec<String>> {
    let mut lines = script.lines();
    let first = lines.next()?;
    if lines.next().is_some() && !first.contains("<<") {
        return None;
    }
    let mut paths = Vec::new();
    for step in first.split("&&").flat_map(|s| s.split(';')) {
        let step = step.trim();
        if step.is_empty() {
            continue;
        }
        paths.push(step_written_path(step)?);
    }
    Some(paths)
}

fn step_written_path(step: &str) -> Option<String> {
    let tokens = shlex::split(step)?;
    let (head, rest) = tokens.split_first()?;
    match head.as_str() {
        "cat" | "echo" | "printf" => {
            if let Some(idx) = tokens.iter().position(|t| t == "|") {
                return tee_target(&tokens[idx + 1..]);
            }
            redirect_target(rest)
        }
        "sed" if rest.iter().any(|t| t.starts_with("-i")) => {
            rest.last().filter(|t| !t.starts_with('-')).cloned()
        }
        _ => None,
    }
}

fn redirect_target(tokens: &[String]) -> Option<String> {
    let mut iter = tokens.iter();
    while let Some(token) = iter.next() {
        let target = match token.as_str() {
            ">" | ">>" => iter.next()?.clone(),
            t if t.starts_with(">>") => t[2..].to_string(),
            t if t.starts_with('>') => t[1..].to_string(),
            _ => continue,
        };
        return Some(target);
    }
    None
}

fn tee_target(tokens: &[String]) -> Option<String> {
    match tokens {
        [tee, path] if tee == "tee" => Some(path.clone()),
        [tee, flag, path] if tee == "tee" && flag == "-a" => Some(path.clone()),
        _ => None,
    }
}

fn is_literal_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with("/dev/")
        && !path.contains(['$', '*', '?', '`', '(', ')', '{', '}', '<', '>'])
}

#[cfg(test)]
mod tests {
    use super::edited_paths;
    use pretty_assertions::assert_eq;

    fn sh(script: &str) -> Vec<String> {
        vec!["bash".to_string(), "-lc".to_string(), script.to_string()]
    }

    fn some(paths: &[&str]) -> Option<Vec<String>> {
        Some(paths.iter().map(ToString::to_string).collect())
    }

    #[test]
    fn python_path_var_write_text() {
        let script = "python3 - <<'PY'\nfrom pathlib import Path\np = Path('index.html')\ns = p.read_text()\np.write_text(s.replace('a','b',1))\nPY";
        assert_eq!(edited_paths(&sh(script)), some(&["index.html"]));
    }

    #[test]
    fn python_inline_path_write() {
        let script =
            "python3 - <<'PY'\nfrom pathlib import Path\nPath('/tmp/check.js').write_text('x')\nPY";
        assert_eq!(edited_paths(&sh(script)), some(&["/tmp/check.js"]));
    }

    #[test]
    fn python_open_w() {
        let script = "python3 -c \"open('notes.txt','w').write('x')\"";
        assert_eq!(edited_paths(&sh(script)), some(&["notes.txt"]));
    }

    #[test]
    fn python_two_files_dedup() {
        let script = "python3 - <<'PY'\nfrom pathlib import Path\nh = Path('index.html')\nc = Path(\"styles.css\")\nh.write_text('1')\nc.write_text('2')\nh.write_text('3')\nPY";
        assert_eq!(
            edited_paths(&sh(script)),
            some(&["index.html", "styles.css"])
        );
    }

    #[test]
    fn cat_heredoc() {
        assert_eq!(
            edited_paths(&sh("cat > AGENTS.md <<'EOF'\n# x\nEOF")),
            some(&["AGENTS.md"])
        );
    }

    #[test]
    fn sed_in_place_macos() {
        assert_eq!(
            edited_paths(&sh("sed -i '' 's/a/b/' styles.css")),
            some(&["styles.css"])
        );
    }

    #[test]
    fn echo_append() {
        assert_eq!(edited_paths(&sh("echo hi >> log.txt")), some(&["log.txt"]));
    }

    #[test]
    fn python_read_only() {
        assert_eq!(
            edited_paths(&sh("python3 -c \"print(open('a.txt').read())\"")),
            None
        );
    }

    #[test]
    fn python_non_literal_path() {
        let script =
            "python3 - <<'PY'\nfrom pathlib import Path\np = Path(name)\np.write_text('x')\nPY";
        assert_eq!(edited_paths(&sh(script)), None);
    }

    #[test]
    fn http_server() {
        assert_eq!(edited_paths(&sh("python3 -m http.server 8000")), None);
    }

    #[test]
    fn program_redirect_is_not_edit() {
        assert_eq!(edited_paths(&sh("npm test > out.log")), None);
    }

    #[test]
    fn dev_null_ignored() {
        assert_eq!(edited_paths(&sh("echo hi > /dev/null")), None);
    }
}
