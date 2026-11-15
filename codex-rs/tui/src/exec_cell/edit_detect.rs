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

/// Anything that can act beyond writing the named files: processes, the OS, the network,
/// dynamic code, or file-system operations other than a write.
static PY_SIDE_EFFECT: LazyLock<Regex> = LazyLock::new(|| {
    #[expect(clippy::unwrap_used)]
    Regex::new(
        r"\b(?:os|subprocess|shutil|socket|urllib|requests|http|ctypes|pty|multiprocessing|asyncio|importlib)\b|\b(?:exec|eval|compile|__import__|system|popen)\s*\(|\.(?:unlink|rename|replace_file|rmdir|chmod|symlink_to|hardlink_to)\s*\(",
    )
    .unwrap()
});
static ANY_PATH_WRITE: LazyLock<Regex> = LazyLock::new(|| {
    #[expect(clippy::unwrap_used)]
    Regex::new(r"\.write_(?:text|bytes)\(").unwrap()
});
static ANY_OPEN_WRITE: LazyLock<Regex> = LazyLock::new(|| {
    #[expect(clippy::unwrap_used)]
    Regex::new(r#"open\([^)]*['"][wax]b?\+?['"]"#).unwrap()
});

/// File paths a command writes, in first-seen order without duplicates, or `None` when the
/// command is not a recognizable write to literal paths.
pub(crate) fn edited_paths(command: &[String]) -> Option<Vec<String>> {
    // PowerShell quoting and subexpressions follow different rules than the POSIX checks below.
    let program = command
        .first()?
        .rsplit(['/', '\\'])
        .next()?
        .to_ascii_lowercase();
    if program.starts_with("pwsh") || program.starts_with("powershell") {
        return None;
    }
    let script = strip_bash_lc_and_escape(command);
    // `$'…'` and `$"…"` quote differently in the shell than in shlex, so the checks below
    // could disagree with what actually runs.
    if script.contains("$'") || script.contains("$\"") {
        return None;
    }
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
    // Only a lone `python3 -c "…"` or `python3 - <<'PY' … PY`; anything else may run more commands.
    // shlex keeps `"…";id` as one word, so shell operators are checked outside quotes too.
    let bare = unquoted(script);
    let lone_c = matches!(shlex::split(script).as_deref(), Some([_, flag, _]) if flag == "-c")
        && !bare.contains([';', '&', '|', '<', '>', '(', '\n'])
        && !script.contains("$(")
        && !script.contains('`');
    if !lone_c {
        let tokens = shlex::split(command_line(script)?)?;
        let heredoc_only = match tokens.as_slice() {
            [_, dash, op] => dash == "-" && op.len() > 2 && op.starts_with("<<"),
            [_, dash, op, _] => dash == "-" && (op == "<<" || op == "<<-"),
            _ => false,
        };
        if !heredoc_only || !script.contains('\n') {
            return None;
        }
    }
    if PY_SIDE_EFFECT.is_match(script) {
        return None;
    }
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
        // A variable assigned more than once may no longer hold the literal path.
        let assignment = Regex::new(&format!(r"(?m)^\s*{}\s*=[^=]", var.as_str())).ok()?;
        if assignment.find_iter(script).count() != 1 {
            return None;
        }
        writes.push((var.start(), (*path).to_string()));
    }
    for re in [&*INLINE_PATH_WRITE, &*OPEN_WRITE] {
        for c in re.captures_iter(script) {
            let m = c.get(1)?;
            writes.push((m.start(), m.as_str().to_string()));
        }
    }
    // Every write must have been resolved to a literal path above.
    let total = ANY_PATH_WRITE.find_iter(script).count() + ANY_OPEN_WRITE.find_iter(script).count();
    if writes.len() != total {
        return None;
    }
    writes.sort_by_key(|(offset, _)| *offset);
    Some(writes.into_iter().map(|(_, path)| path).collect())
}

/// The script's single command line. Later lines are accepted only as one heredoc body whose
/// terminator is the last line; anything after the terminator would be more commands.
fn command_line(script: &str) -> Option<&str> {
    let lines: Vec<&str> = script.lines().collect();
    let (first, rest) = lines.split_first()?;
    let Some((last, body)) = rest.split_last() else {
        return (!unquoted(first).contains("<<")).then_some(*first);
    };
    let bare = unquoted(first);
    let op = bare.find("<<")?;
    if bare[op..].starts_with("<<<") || bare[op + 2..].contains("<<") {
        return None;
    }
    let after = first[op + 2..].trim_start_matches('-').trim_start();
    let delimiter = shlex::split(after)?.into_iter().next()?;
    if delimiter.is_empty()
        || !delimiter
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    let quoted = after.starts_with(['\'', '"', '\\']);
    let terminated = last.trim() == delimiter && !body.iter().any(|l| l.trim() == delimiter);
    // An unquoted heredoc still runs `$(…)` and backticks inside its body.
    let inert = quoted || !body.iter().any(|l| l.contains("$(") || l.contains('`'));
    (terminated && inert).then_some(*first)
}

/// `line` with quoted and escaped text blanked to `_` (same byte length), so shell operators
/// can be located without being fooled by quoted characters.
fn unquoted(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in line.chars() {
        let blank = if escaped {
            escaped = false;
            true
        } else if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if c == '\\' && q == '"' {
                escaped = true;
            }
            true
        } else if c == '\'' || c == '"' {
            quote = Some(c);
            true
        } else if c == '\\' {
            escaped = true;
            true
        } else {
            false
        };
        if blank {
            out.extend(std::iter::repeat_n('_', c.len_utf8()));
        } else {
            out.push(c);
        }
    }
    out
}

/// Every `&&`/`;` step must be a write (`cat|echo|printf > f`, `| tee f`, `sed -i … f`).
fn shell_written_paths(script: &str) -> Option<Vec<String>> {
    let line = command_line(script)?;
    let bare = unquoted(line);
    let mut steps = Vec::new();
    let mut start = 0;
    let mut chars = bare.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let len = match c {
            ';' => 1,
            '&' if chars.peek().is_some_and(|(_, n)| *n == '&') => {
                chars.next();
                2
            }
            _ => continue,
        };
        steps.push(&line[start..i]);
        start = i + len;
    }
    steps.push(&line[start..]);
    let steps: Vec<&str> = steps
        .into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    // A heredoc feeds exactly one command.
    if script.contains('\n') && steps.len() != 1 {
        return None;
    }
    steps.into_iter().map(step_written_path).collect()
}

fn step_written_path(step: &str) -> Option<String> {
    // Substitutions run commands even inside double quotes.
    if step.contains("$(") || step.contains('`') {
        return None;
    }
    let bare = unquoted(step);
    // Unquoted `(` runs code in many forms: `<(…)`, zsh `=(…)` and glob qualifiers, subshells.
    if bare.contains(['&', ';', '(']) {
        return None;
    }
    let heredoc = bare.contains("<<");
    let redirects = bare.matches('>').count() - bare.matches(">>").count()
        + bare.matches('<').count()
        - if heredoc { 2 } else { 0 };
    let pipes = bare.matches('|').count();
    let mut tokens = shlex::split(step)?;
    if let Some(idx) = tokens.iter().position(|t| t.starts_with("<<")) {
        let bare_op = tokens[idx] == "<<" || tokens[idx] == "<<-";
        tokens.drain(idx..(idx + if bare_op { 2 } else { 1 }).min(tokens.len()));
    }
    let (head, rest) = tokens.split_first()?;
    match head.as_str() {
        "cat" | "echo" | "printf" if pipes == 1 && redirects == 0 => {
            let idx = tokens.iter().position(|t| t == "|")?;
            tee_target(&tokens[idx + 1..])
        }
        "cat" | "echo" | "printf" if pipes == 0 && redirects == 1 => redirect_target(step, &bare),
        "sed" if pipes == 0 && redirects == 0 => sed_in_place_target(rest),
        _ => None,
    }
}

/// `sed [-E|-r] -i[suffix] [''] [-e] 's/a/b/flags' file` only: other sed commands and the
/// `e`/`w` flags can run commands or write other files, so they are not summarized as an edit.
fn sed_in_place_target(args: &[String]) -> Option<String> {
    let mut in_place = false;
    let mut script: Option<&String> = None;
    let mut operands: Vec<&String> = Vec::new();
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-E" | "-r" | "--regexp-extended" => {}
            "-e" => {
                if script.replace(iter.next()?).is_some() {
                    return None;
                }
            }
            flag if flag.starts_with("-i") => {
                if in_place {
                    return None;
                }
                in_place = true;
                // macOS `-i ''` takes an empty backup suffix as the next argument.
                if flag == "-i" && iter.peek().is_some_and(|next| next.is_empty()) {
                    iter.next();
                }
            }
            flag if flag.starts_with('-') => return None,
            _ => operands.push(arg),
        }
    }
    let (script, file) = match (script, operands.as_slice()) {
        (Some(script), [file]) => (script, *file),
        (None, [script, file]) => (*script, *file),
        _ => return None,
    };
    (in_place && is_plain_substitution(script)).then(|| file.clone())
}

fn is_plain_substitution(script: &str) -> bool {
    let mut chars = script.chars();
    let (Some('s'), Some(delim)) = (chars.next(), chars.next()) else {
        return false;
    };
    if delim.is_alphanumeric() || delim == '\\' || delim.is_whitespace() {
        return false;
    }
    // Split on unescaped delimiters: pattern, replacement, flags.
    let mut parts = vec![String::new()];
    let mut escaped = false;
    for c in chars {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == delim {
            parts.push(String::new());
            continue;
        }
        if let Some(part) = parts.last_mut() {
            part.push(c);
        }
    }
    matches!(parts.as_slice(), [_, _, flags]
        if flags.chars().all(|c| matches!(c, 'g' | 'i' | 'I' | 'p') || c.is_ascii_digit()))
}

/// The word after the step's one unquoted `>`/`>>`, located in `bare` so a quoted `'>x'`
/// argument is never mistaken for the redirect.
fn redirect_target(step: &str, bare: &str) -> Option<String> {
    let at = bare.find('>')?;
    // `2> f` captures stderr only; it is not the command writing `f`.
    if bare[..at].ends_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let after = step[at..].trim_start_matches('>');
    shlex::split(after)?.into_iter().next()
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
        && !path.starts_with('&')
        && !path.contains([
            '$', '*', '?', '`', '(', ')', '{', '}', '<', '>', '&', '|', ';',
        ])
        && !path.contains(char::is_whitespace)
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
    // Review Focus 1: a write must never hide another command that runs with it.
    #[test]
    fn heredoc_followed_by_command_is_not_edit() {
        let script = "cat > run.sh <<'EOF'\necho hi\nEOF\nchmod +x run.sh && ./run.sh";
        assert_eq!(edited_paths(&sh(script)), None);
    }

    #[test]
    fn quoted_heredoc_marker_is_not_heredoc() {
        assert_eq!(edited_paths(&sh("echo \"a << b\" > f\nrm -rf x")), None);
    }

    #[test]
    fn other_separators_are_not_edits() {
        for script in [
            "cat > f || rm -rf build",
            "echo x > f & rm -rf ~",
            "echo x > f |sh",
            "echo x > f | sh",
            "echo x > f > g",
        ] {
            assert_eq!(edited_paths(&sh(script)), None, "{script}");
        }
    }

    #[test]
    fn command_substitution_is_not_edit() {
        for script in [
            "echo $(rm -rf build) > f",
            "echo `curl x|sh` > f",
            "cat <(rm -rf x) > f",
        ] {
            assert_eq!(edited_paths(&sh(script)), None, "{script}");
        }
    }

    #[test]
    fn python_followed_by_command_is_not_edit() {
        for script in [
            "python3 -c \"open('a','w').write('')\" && rm -rf build",
            "python3 - <<'PY'\nopen('a','w').write('')\nPY\nrm -rf build",
        ] {
            assert_eq!(edited_paths(&sh(script)), None, "{script}");
        }
    }

    #[test]
    fn stderr_redirect_is_not_edit() {
        assert_eq!(edited_paths(&sh("echo \"error: missing\" >&2")), None);
    }

    #[test]
    fn tee_and_chained_writes_still_detected() {
        assert_eq!(edited_paths(&sh("echo x | tee -a f.txt")), some(&["f.txt"]));
        assert_eq!(
            edited_paths(&sh("echo a > x.txt && echo b >> y.txt")),
            some(&["x.txt", "y.txt"])
        );
    }
    #[test]
    fn python_c_with_trailing_command_is_not_edit() {
        for script in [
            "python3 -c \"open('a','w').write('')\";id",
            "python3 -c \"open('a','w').write('')\"|sh",
            "python3 -c \"open('a','w').write('')\">log",
        ] {
            assert_eq!(edited_paths(&sh(script)), None, "{script}");
        }
    }

    #[test]
    fn heredoc_delimiter_must_be_plain_word() {
        assert_eq!(edited_paths(&sh("cat > f <<'E;rm x'\nhi\nE;rm x")), None);
    }

    #[test]
    fn sed_that_executes_or_writes_elsewhere_is_not_edit() {
        for script in [
            "sed -i 's/x/y/e' f.txt",
            "sed -i 's/x/y/w other' f.txt",
            "sed -i '1e rm -rf x' f.txt",
            "sed -i -f prog.sed f.txt",
            "sed -i 's/a/b/' a.css b.css",
            "sed -i -e 's/a/b/' -e 'r x' f.txt",
        ] {
            assert_eq!(edited_paths(&sh(script)), None, "{script}");
        }
    }

    #[test]
    fn sed_substitution_variants_are_edits() {
        for script in [
            "sed -i 's/a/b/g' f.txt",
            "sed -i.bak 's|a|b|' f.txt",
            "sed -i '' 's#a/b#c#2' f.txt",
        ] {
            assert_eq!(edited_paths(&sh(script)), some(&["f.txt"]), "{script}");
        }
    }
    #[test]
    fn ansi_c_quoting_is_not_edit() {
        for script in [
            r#"echo $'\'';rm -rf x;echo '\' > f"#,
            r#"python3 -c $'\'';rm -rf x;echo 'open("a","w")\'"#,
            r#"echo $"x" > f"#,
        ] {
            assert_eq!(edited_paths(&sh(script)), None, "{script}");
        }
    }

    #[test]
    fn unquoted_parens_are_not_edit() {
        for script in [
            "echo =(rm -rf x) > f",
            "echo *(e:'rm -rf x':) > f",
            "echo @(Remove-Item x) > f",
        ] {
            assert_eq!(edited_paths(&sh(script)), None, "{script}");
        }
    }

    #[test]
    fn powershell_is_never_edit() {
        let cmd = vec![
            "pwsh".to_string(),
            "-Command".to_string(),
            "echo hi > f.txt".to_string(),
        ];
        assert_eq!(edited_paths(&cmd), None);
    }

    #[test]
    fn redirect_target_ignores_quoted_angle() {
        assert_eq!(
            edited_paths(&sh("echo '>seq1' > seqs.fa")),
            some(&["seqs.fa"])
        );
        assert_eq!(edited_paths(&sh("echo \">\" x > f")), some(&["f"]));
        assert_eq!(edited_paths(&sh("echo hi 2> err.log")), None);
    }

    #[test]
    fn python_with_side_effects_is_not_edit() {
        for body in [
            "import shutil; shutil.rmtree('build')\nopen('a','w').write('')",
            "import subprocess\nsubprocess.run(['ls'])\nopen('a','w').write('')",
            "import os\nos.remove('b')\nopen('a','w').write('')",
            "open('a','w').write(x)\nopen(name,'w').write(y)",
            "from pathlib import Path\np = Path('a')\np = p.with_suffix('.bak')\np.write_text('x')",
            "from pathlib import Path\nPath('a').write_text('x')\nget().write_text('y')",
        ] {
            let script = format!("python3 - <<'PY'\n{body}\nPY");
            assert_eq!(edited_paths(&sh(&script)), None, "{body}");
        }
    }

    #[test]
    fn sed_common_flag_forms_are_edits() {
        for script in [
            "sed -i -e 's/a/b/' f.txt",
            "sed -i '' -e 's/a/b/' f.txt",
            "sed -E -i 's/a+/b/' f.txt",
            "sed -i -E 's/a+/b/' f.txt",
        ] {
            assert_eq!(edited_paths(&sh(script)), some(&["f.txt"]), "{script}");
        }
    }
}
