//! Editor settings that change how much build output there is:
//! rust-analyzer's own target directory.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use super::{Level, say};

const TARGET_DIR_KEY: &str = "rust-analyzer.cargo.targetDir";

/// User settings files of VS Code and Cursor on this platform.
pub fn user_settings_files(home: &Path) -> Vec<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        home.join("Library/Application Support")
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
    };
    ["Code", "Cursor"]
        .iter()
        .map(|app| base.join(app).join("User/settings.json"))
        .collect()
}

/// Removes `//` and `/* */` comments and trailing commas from JSON with
/// comments (the format of VS Code settings), leaving strings alone.
pub fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();
    let mut in_string = false;
    while let Some((at, c)) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                '\\' => {
                    if let Some((_, next)) = chars.next() {
                        out.push(next);
                    }
                }
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match (c, chars.peek().map(|&(_, n)| n)) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for (_, skipped) in chars.by_ref() {
                    if skipped == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut last = '\0';
                for (_, skipped) in chars.by_ref() {
                    if last == '*' && skipped == '/' {
                        break;
                    }
                    last = skipped;
                }
            }
            (',', _) => {
                // A comma followed only by whitespace, comments and a closing
                // bracket is a trailing comma, which JSON doesn't allow.
                let next = next_significant(&text[at + 1..]);
                if next != Some('}') && next != Some(']') {
                    out.push(c);
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// The next significant character, skipping whitespace and comments.
fn next_significant(rest: &str) -> Option<char> {
    let mut rest = rest.trim_start();
    loop {
        if let Some(after) = rest.strip_prefix("//") {
            rest = after.split_once('\n').map_or("", |(_, r)| r).trim_start();
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, r)| r).trim_start();
        } else {
            return rest.chars().next();
        }
    }
}

/// What a settings file sets `rust-analyzer.cargo.targetDir` to, at the top
/// level or under `"[rust]"`, when that gives rust-analyzer its own target
/// directory (`true` or a path).
pub fn separate_target_dir(settings_text: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(&strip_jsonc(settings_text)).ok()?;
    [
        value.get(TARGET_DIR_KEY),
        value.get("[rust]").and_then(|r| r.get(TARGET_DIR_KEY)),
    ]
    .into_iter()
    .flatten()
    .find_map(|v| match v {
        serde_json::Value::Bool(true) => Some("true".to_owned()),
        serde_json::Value::String(s) if !s.is_empty() => Some(format!("{s:?}")),
        _ => None,
    })
}

/// Checks the user's editor settings and each workspace's
/// `.vscode/settings.json`.
pub fn report(home: Option<&Path>, workspace_roots: &BTreeSet<PathBuf>) {
    let files: Vec<PathBuf> = home
        .map(user_settings_files)
        .unwrap_or_default()
        .into_iter()
        .chain(
            workspace_roots
                .iter()
                .map(|w| w.join(".vscode/settings.json")),
        )
        .collect();
    let mut found = Vec::new();
    let mut read = 0;
    for file in files {
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        read += 1;
        if let Some(value) = separate_target_dir(&text) {
            found.push(format!(
                "{} sets {TARGET_DIR_KEY} = {value}",
                file.display()
            ));
        }
    }
    if !found.is_empty() {
        say(
            Level::Info,
            format!(
                "{}\nrust-analyzer then checks into its own target directory, so every project's\n\
                 check output exists twice. In exchange, rust-analyzer never waits on cargo's build\n\
                 lock. Remove the setting if disk space matters more.",
                found.join("\n")
            ),
        );
    } else if read > 0 {
        say(
            Level::Ok,
            "rust-analyzer shares each project's target directory (no cargo.targetDir set)",
        );
    } else {
        say(Level::Info, "No VS Code or Cursor settings found");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_comments_and_trailing_commas() {
        let text = r#"{
            // a comment with "quotes"
            "url": "http://example.com/*not a comment*/", /* block
            comment */
            "list": [1, 2,],
            "escaped": "a \" // still a string",
        }"#;
        let value: serde_json::Value = serde_json::from_str(&strip_jsonc(text)).unwrap();
        assert_eq!(value["url"], "http://example.com/*not a comment*/");
        assert_eq!(value["list"], serde_json::json!([1, 2]));
        assert_eq!(value["escaped"], "a \" // still a string");
    }

    #[test]
    fn finds_rust_analyzer_target_dirs() {
        assert_eq!(
            separate_target_dir("{\n  // mine\n  \"rust-analyzer.cargo.targetDir\": true,\n}"),
            Some("true".to_owned())
        );
        assert_eq!(
            separate_target_dir(r#"{"[rust]": {"rust-analyzer.cargo.targetDir": "target/ra"}}"#),
            Some("\"target/ra\"".to_owned())
        );
        assert_eq!(
            separate_target_dir(r#"{"rust-analyzer.cargo.targetDir": false}"#),
            None
        );
        assert_eq!(separate_target_dir(r#"{"editor.fontSize": 13}"#), None);
        assert_eq!(separate_target_dir("not json"), None);
    }
}
