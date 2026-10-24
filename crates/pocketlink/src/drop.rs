// SPDX-License-Identifier: GPL-3.0-or-later

//! Turning drag-and-drop data into something we can send.

use std::borrow::Cow;
use std::path::PathBuf;

/// MIME types we accept, most preferred first.
pub fn mime_types() -> Vec<Cow<'static, str>> {
    vec![
        Cow::Borrowed("text/uri-list"),
        Cow::Borrowed("text/plain;charset=utf-8"),
        Cow::Borrowed("UTF8_STRING"),
        Cow::Borrowed("text/plain"),
    ]
}

#[derive(Debug, Clone, PartialEq)]
pub enum Payload {
    Files(Vec<PathBuf>),
    Text(String),
}

pub fn parse(mime: &str, data: &[u8]) -> Option<Payload> {
    let text = String::from_utf8_lossy(data);
    let text = text.trim_matches(|c: char| c.is_whitespace() || c == '\0');
    if text.is_empty() {
        return None;
    }

    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    let files: Vec<PathBuf> = lines
        .iter()
        .filter_map(|l| url::Url::parse(l).ok())
        .filter(|u| u.scheme() == "file")
        .filter_map(|u| u.to_file_path().ok())
        .collect();

    // A plain-text drag can still be a list of file URLs (some apps do this).
    if !files.is_empty() && files.len() == lines.len() {
        return Some(Payload::Files(files));
    }
    if mime == "text/uri-list" && lines.len() == 1 {
        return Some(Payload::Text(lines[0].to_owned()));
    }
    Some(Payload::Text(text.to_owned()))
}

/// "photo.jpg", or "photo.jpg and 2 more".
pub fn describe(files: &[PathBuf]) -> String {
    let first = files
        .first()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match files.len() {
        0 | 1 => first,
        n => format!("{first} and {} more", n - 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_list_with_files() {
        let data = b"# comment\r\nfile:///home/me/a%20b.jpg\r\nfile:///tmp/c.pdf\r\n";
        assert_eq!(
            parse("text/uri-list", data),
            Some(Payload::Files(vec![
                PathBuf::from("/home/me/a b.jpg"),
                PathBuf::from("/tmp/c.pdf"),
            ]))
        );
    }

    #[test]
    fn link_and_text() {
        assert_eq!(
            parse("text/uri-list", b"https://example.com\n"),
            Some(Payload::Text("https://example.com".into()))
        );
        assert_eq!(
            parse("text/plain", b"hello\nworld"),
            Some(Payload::Text("hello\nworld".into()))
        );
        assert_eq!(parse("text/plain", b"  \0"), None);
    }

    #[test]
    fn plain_text_file_urls() {
        assert_eq!(
            parse("text/plain", b"file:///tmp/x.txt"),
            Some(Payload::Files(vec![PathBuf::from("/tmp/x.txt")]))
        );
    }

    #[test]
    fn describes_files() {
        let one = vec![PathBuf::from("/a/photo.jpg")];
        assert_eq!(describe(&one), "photo.jpg");
        let three = vec![one[0].clone(), "/b".into(), "/c".into()];
        assert_eq!(describe(&three), "photo.jpg and 2 more");
    }
}
