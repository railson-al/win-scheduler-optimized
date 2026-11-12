//! The text-shaped parts of starting a program with `CreateProcessAsUserW`: the command line, the
//! environment block, and finding the program on the *user's* `PATH`.
//!
//! `std::process::Command` does all of this for the service's own children, but it cannot start a process
//! with another user's token, so the interactive path (`interactive.rs`, Windows only) has to build these
//! by hand. They are plain string work with no system calls, kept apart so they run and are tested on any
//! platform; only Windows uses them at run time.
#![cfg_attr(not(windows), allow(dead_code))]

use std::fmt;

/// An argument `cmd.exe` cannot be given safely: `"` cannot be escaped on a `cmd /c` line, `%` is expanded
/// even inside quotes, and a line break ends the command. (Rust's own `Command` refuses the same for batch
/// files.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsafeBatchArgument(pub String);

impl fmt::Display for UnsafeBatchArgument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the argument {:?} has a character (a quote, %, or a line break) that cmd.exe cannot pass to a \
             batch file safely",
            self.0
        )
    }
}

/// One argument as `CreateProcess` programs (the C runtime, `CommandLineToArgvW`) read it back: quoted when
/// it has a space, tab or quote or is empty, with backslashes before a quote doubled and the quote escaped.
pub fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\u{b}', '"']) {
        return arg.to_owned();
    }
    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                // Every backslash before a quote doubles, and the quote itself is escaped.
                quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            other => {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
                quoted.push(other);
                backslashes = 0;
            }
        }
    }
    // Backslashes at the end sit before the closing quote, so they double too.
    quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
    quoted.push('"');
    quoted
}

/// `"program" arg1 "arg 2"`: the program is always quoted (its path may have spaces).
pub fn command_line(program: &str, args: &[String]) -> String {
    let mut line = format!("\"{program}\"");
    for arg in args {
        line.push(' ');
        line.push_str(&quote_arg(arg));
    }
    line
}

pub fn is_batch(program: &str) -> bool {
    let lower = program.to_ascii_lowercase();
    lower.ends_with(".bat") || lower.ends_with(".cmd")
}

/// `"cmd.exe" /d /s /c ""script.bat" arg1 "arg 2""`. With `/s`, cmd strips the outer pair of quotes and
/// leaves the inner quoting as written, which is what lets a script path with spaces work. Arguments are
/// quoted only when they need it (a space, or a character cmd treats as an operator).
pub fn batch_command_line(comspec: &str, script: &str, args: &[String]) -> Result<String, UnsafeBatchArgument> {
    let mut inner = format!("\"{script}\"");
    for arg in args {
        if arg.contains(['"', '%', '\r', '\n', '\0']) {
            return Err(UnsafeBatchArgument(arg.clone()));
        }
        inner.push(' ');
        if arg.is_empty() || arg.contains([' ', '\t', '&', '|', '<', '>', '^', '(', ')']) {
            inner.push('"');
            inner.push_str(arg);
            inner.push('"');
        } else {
            inner.push_str(arg);
        }
    }
    Ok(format!("\"{comspec}\" /d /s /c \"{inner}\""))
}

/// The variables of a user's environment, as `CreateEnvironmentBlock` returns them: UTF-16, `NAME=value`
/// entries each ended by NUL, the whole list ended by an extra NUL. A name may itself start with `=`
/// (`=C:` remembers the current directory of drive C), so the split is at the first `=` after the first
/// character.
pub fn parse_environment_block(block: &[u16]) -> Vec<(String, String)> {
    let mut variables = Vec::new();
    for entry in block.split(|unit| *unit == 0) {
        if entry.is_empty() {
            // The empty entry that ends the list (the block continues with its own terminator).
            break;
        }
        let text = String::from_utf16_lossy(entry);
        let split_at = text
            .char_indices()
            .skip(1)
            .find(|(_, c)| *c == '=')
            .map(|(index, _)| index);
        if let Some(index) = split_at {
            variables.push((text[..index].to_owned(), text[index + 1..].to_owned()));
        }
    }
    variables
}

/// `base` with `overrides` applied. Windows names are case-insensitive, so `Path` and `PATH` are one
/// variable, and an override keeps the casing of the one it replaces.
pub fn merge_environment(mut base: Vec<(String, String)>, overrides: &[(String, String)]) -> Vec<(String, String)> {
    for (name, value) in overrides {
        match base
            .iter_mut()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
        {
            Some(slot) => slot.1 = value.clone(),
            None => base.push((name.clone(), value.clone())),
        }
    }
    base
}

/// The block `CreateProcessAsUserW` takes with `CREATE_UNICODE_ENVIRONMENT`: entries sorted by name,
/// case-insensitively (as Windows expects), each ended by NUL, and one more NUL at the end.
pub fn encode_environment_block(variables: &[(String, String)]) -> Vec<u16> {
    let mut sorted: Vec<&(String, String)> = variables.iter().collect();
    sorted.sort_by_key(|(name, _)| name.to_ascii_uppercase());
    let mut block = Vec::new();
    for (name, value) in sorted {
        block.extend(format!("{name}={value}").encode_utf16());
        block.push(0);
    }
    // An empty environment is two NULs, not one.
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

/// The value of a variable, by case-insensitive name.
pub fn environment_value<'a>(variables: &'a [(String, String)], name: &str) -> Option<&'a str> {
    variables
        .iter()
        .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// Where `program` is, searching the way the user's own shell would. Something with a directory in it is
/// taken as written (relative to `cwd` if relative). A bare name is looked for in each `PATH` directory
/// with each `PATHEXT` extension (or as is, if it already has one). `exists` is the file-system check, so
/// the search is testable.
///
/// This is needed because `CreateProcessAsUserW` searches the *calling* process's `PATH`, the service's,
/// where a `python` installed for one user is not.
pub fn resolve_program(
    program: &str,
    path_var: &str,
    path_ext: &str,
    cwd: Option<&str>,
    exists: impl Fn(&str) -> bool,
) -> Option<String> {
    if program.contains(['\\', '/', ':']) {
        let is_relative = !program.contains(':') && !program.starts_with(['\\', '/']);
        return Some(match (cwd, is_relative) {
            (Some(dir), true) => format!("{}\\{program}", dir.trim_end_matches(['\\', '/'])),
            _ => program.to_owned(),
        });
    }
    let has_extension = program.rsplit_once('.').is_some_and(|(_, ext)| !ext.is_empty());
    let extensions: Vec<&str> = path_ext.split(';').filter(|ext| !ext.is_empty()).collect();
    for dir in path_var.split(';').filter(|dir| !dir.is_empty()) {
        let dir = dir.trim_end_matches('\\');
        let mut candidates = Vec::new();
        if has_extension {
            candidates.push(format!("{dir}\\{program}"));
        }
        candidates.extend(extensions.iter().map(|ext| format!("{dir}\\{program}{ext}")));
        if let Some(found) = candidates.into_iter().find(|candidate| exists(candidate)) {
            return Some(found);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    // ---- quoting ----

    #[test]
    fn plain_arguments_are_left_alone_and_others_quoted() {
        assert_eq!(quote_arg("--flag"), "--flag");
        assert_eq!(quote_arg("com espaço"), "\"com espaço\"");
        assert_eq!(quote_arg(""), "\"\"");
        assert_eq!(quote_arg("a\tb"), "\"a\tb\"");
    }

    #[test]
    fn quotes_and_backslashes_follow_the_createprocess_rules() {
        // A quote inside is escaped.
        assert_eq!(quote_arg(r#"say "hi""#), r#""say \"hi\"""#);
        // Backslashes only matter before a quote, where they double (and the quote is escaped)...
        assert_eq!(quote_arg(r#"a\"b"#), r#""a\\\"b""#);
        // ...and at the end, where the closing quote follows them.
        assert_eq!(quote_arg(r"C:\Program Files\"), r#""C:\Program Files\\""#);
        // Elsewhere they are literal.
        assert_eq!(quote_arg(r"C:\a b\c"), r#""C:\a b\c""#);
    }

    #[test]
    fn the_command_line_always_quotes_the_program() {
        assert_eq!(
            command_line(
                r"C:\Program Files\python.exe",
                &strings(&["-u", r"C:\a b\x.py", "--n", "1"])
            ),
            r#""C:\Program Files\python.exe" -u "C:\a b\x.py" --n 1"#
        );
        assert_eq!(command_line("a.exe", &[]), r#""a.exe""#);
    }

    #[test]
    fn batch_files_go_through_cmd_with_the_inner_quoting_kept() {
        assert!(is_batch(r"C:\x\Run.BAT") && is_batch("a.cmd") && !is_batch("a.exe"));
        assert_eq!(
            batch_command_line(
                r"C:\Windows\System32\cmd.exe",
                r"C:\meus scripts\robô.bat",
                &strings(&["--todas", "um dois"])
            )
            .unwrap(),
            r#""C:\Windows\System32\cmd.exe" /d /s /c ""C:\meus scripts\robô.bat" --todas "um dois"""#
        );
        assert_eq!(
            batch_command_line("cmd.exe", "a.bat", &[]).unwrap(),
            r#""cmd.exe" /d /s /c ""a.bat"""#
        );
    }

    #[test]
    fn arguments_cmd_cannot_carry_safely_are_refused() {
        for bad in ["a\"b", "100%", "a\nb", "x\ry"] {
            assert_eq!(
                batch_command_line("cmd.exe", "a.bat", &strings(&[bad])),
                Err(UnsafeBatchArgument(bad.to_owned())),
                "{bad:?}"
            );
        }
        // Operators are fine inside quotes, so they are quoted instead of refused.
        assert_eq!(
            batch_command_line("cmd.exe", "a.bat", &strings(&["a&b", ""])).unwrap(),
            r#""cmd.exe" /d /s /c ""a.bat" "a&b" """"#
        );
    }

    // ---- environment ----

    fn block_of(text: &[&str]) -> Vec<u16> {
        let mut block = Vec::new();
        for entry in text {
            block.extend(entry.encode_utf16());
            block.push(0);
        }
        block.push(0);
        block
    }

    #[test]
    fn an_environment_block_is_read_into_names_and_values() {
        let block = block_of(&[
            "USERPROFILE=C:\\Users\\finance",
            "=C:=C:\\dev",
            "EMPTY=",
            "A=b=c",
            "Ção=ã",
        ]);
        assert_eq!(
            parse_environment_block(&block),
            pairs(&[
                ("USERPROFILE", "C:\\Users\\finance"),
                ("=C:", "C:\\dev"),
                ("EMPTY", ""),
                ("A", "b=c"),
                ("Ção", "ã"),
            ])
        );
        assert!(parse_environment_block(&[0]).is_empty());
        assert!(parse_environment_block(&[]).is_empty());
    }

    #[test]
    fn overrides_replace_by_case_insensitive_name_keeping_the_original_casing() {
        let merged = merge_environment(
            pairs(&[("Path", "C:\\a"), ("TEMP", "C:\\t")]),
            &pairs(&[("PATH", "C:\\b"), ("PYTHONUTF8", "1")]),
        );
        assert_eq!(
            merged,
            pairs(&[("Path", "C:\\b"), ("TEMP", "C:\\t"), ("PYTHONUTF8", "1")])
        );
    }

    #[test]
    fn the_encoded_block_is_sorted_nul_separated_and_double_terminated() {
        let block = encode_environment_block(&pairs(&[("b", "2"), ("A", "1"), ("c", "ã")]));
        let text: Vec<String> = block.split(|unit| *unit == 0).map(String::from_utf16_lossy).collect();
        assert_eq!(text, ["A=1", "b=2", "c=ã", "", ""]);
        assert_eq!(&block[block.len() - 2..], [0, 0]);
        // Round trip.
        assert_eq!(
            parse_environment_block(&block),
            pairs(&[("A", "1"), ("b", "2"), ("c", "ã")])
        );
        // Empty is two NULs.
        assert_eq!(encode_environment_block(&[]), [0, 0]);
    }

    #[test]
    fn variables_are_looked_up_without_regard_to_case() {
        let env = pairs(&[("ComSpec", "C:\\Windows\\System32\\cmd.exe")]);
        assert_eq!(
            environment_value(&env, "COMSPEC"),
            Some("C:\\Windows\\System32\\cmd.exe")
        );
        assert_eq!(environment_value(&env, "nope"), None);
    }

    // ---- finding the program ----

    #[test]
    fn a_name_with_a_directory_is_taken_as_written_or_relative_to_the_working_directory() {
        let never = |_: &str| false;
        assert_eq!(
            resolve_program(r"C:\a\b.exe", "", "", None, never).as_deref(),
            Some(r"C:\a\b.exe")
        );
        assert_eq!(
            resolve_program(r"sub\run.bat", "", "", Some(r"C:\work\"), never).as_deref(),
            Some(r"C:\work\sub\run.bat")
        );
        assert_eq!(
            resolve_program(r"\\srv\share\x.exe", "", "", Some("C:\\w"), never).as_deref(),
            Some(r"\\srv\share\x.exe")
        );
    }

    #[test]
    fn a_bare_name_is_searched_on_the_users_path_with_the_pathext_extensions() {
        let path = r"C:\Windows\System32;C:\Users\finance\AppData\Local\Programs\Python\Python312\";
        let ext = ".COM;.EXE;.BAT;.CMD";
        let present = |file: &str| file == r"C:\Users\finance\AppData\Local\Programs\Python\Python312\python.EXE";
        assert_eq!(
            resolve_program("python", path, ext, None, present).as_deref(),
            Some(r"C:\Users\finance\AppData\Local\Programs\Python\Python312\python.EXE")
        );
        // Already has an extension: tried as is, then with the extensions added.
        let exact = |file: &str| file == r"C:\Windows\System32\powershell.exe";
        assert_eq!(
            resolve_program("powershell.exe", path, ext, None, exact).as_deref(),
            Some(r"C:\Windows\System32\powershell.exe")
        );
        assert_eq!(resolve_program("nothing", path, ext, None, |_| false), None);
    }

    #[test]
    fn the_first_directory_on_the_path_wins() {
        let present = |file: &str| file.ends_with("\\tool.EXE");
        assert_eq!(
            resolve_program("tool", r"C:\one;C:\two", ".EXE", None, present).as_deref(),
            Some(r"C:\one\tool.EXE")
        );
    }
}
