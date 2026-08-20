//! Style by the numbers: 100 columns and 70 lines per function.
//!
//! Both limits are hard, and checked here so that `cargo test` enforces them
//! instead of code review.

use std::path::{Path, PathBuf};

/// Two copies of the code side by side on one screen.
const LINE_LEN_MAX: usize = 100;

/// A function that has to be scrolled cannot be read in one go.
const FUNCTION_LINES_MAX: usize = 70;

#[test]
fn no_line_is_wider_than_the_limit() {
    let mut problems = Vec::new();
    for file in sources() {
        let text = std::fs::read_to_string(&file).expect("a source file is readable");
        for (index, line) in text.lines().enumerate() {
            let columns = line.chars().count();
            if columns > LINE_LEN_MAX {
                problems.push(format!(
                    "{}:{}: {columns} columns",
                    file.display(),
                    index + 1
                ));
            }
        }
    }
    assert!(
        problems.is_empty(),
        "lines wider than {LINE_LEN_MAX}:\n{}",
        problems.join("\n")
    );
}

#[test]
fn no_function_is_longer_than_a_screen() {
    let mut problems = Vec::new();
    for file in sources() {
        let text = std::fs::read_to_string(&file).expect("a source file is readable");
        let lines: Vec<&str> = text.lines().collect();
        for (index, name, length) in functions(&lines) {
            if length > FUNCTION_LINES_MAX {
                problems.push(format!(
                    "{}:{}: {name} is {length} lines",
                    file.display(),
                    index + 1
                ));
            }
        }
    }
    assert!(
        problems.is_empty(),
        "functions longer than {FUNCTION_LINES_MAX} lines:\n{}",
        problems.join("\n")
    );
}

/// Every function in a file, as `(line index, name, length in lines)`.
///
/// A function ends at the first closing brace in its own column, which is what
/// `rustfmt` guarantees.
fn functions(lines: &[&str]) -> Vec<(usize, String, usize)> {
    let mut found = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let Some((indent, name)) = signature(line) else {
            continue;
        };
        let closing = format!("{}}}", " ".repeat(indent));
        let end = lines
            .iter()
            .enumerate()
            .skip(index + 1)
            .find(|(_, candidate)| **candidate == closing);
        // A signature with no closing brace is a one-line `fn` in a trait or an
        // `impl` block header, and has no body to measure.
        if let Some((end, _)) = end {
            found.push((index, name, end - index + 1));
        }
    }
    found
}

/// The indentation and name of a function defined on this line, if any.
fn signature(line: &str) -> Option<(usize, String)> {
    let indent = line.len() - line.trim_start().len();
    let mut rest = line.trim_start();
    for prefix in [
        "pub(crate) ",
        "pub(super) ",
        "pub ",
        "const ",
        "async ",
        "unsafe ",
    ] {
        rest = rest.strip_prefix(prefix).unwrap_or(rest);
    }
    let rest = rest.strip_prefix("fn ")?;
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some((indent, name))
}

/// Every Rust file in the workspace, tests and fuzz targets included.
fn sources() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate lives two levels below the workspace root")
        .to_path_buf();

    let mut files = Vec::new();
    let mut stack = vec![root.join("crates"), root.join("fuzz")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path.file_name().is_some_and(|name| name != "target") {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        }
    }
    assert!(files.len() > 10, "the workspace has more files than this");
    files.sort();
    files
}
