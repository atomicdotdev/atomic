//! Strip credentials from URLs before text reaches the log file.

/// `text` with every URL's `user:password@`, query and fragment removed.
/// Remote errors name the URL they failed on, and remote URLs can carry
/// credentials in either place.
pub(super) fn urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let (before, after) = rest.split_at(at + "://".len());
        out.push_str(before);
        let end = after.find(char::is_whitespace).unwrap_or(after.len());
        let (url, tail) = after.split_at(end);
        out.push_str(host_and_path(url));
        rest = tail;
    }
    out.push_str(rest);
    out
}

/// The part of `url` (what follows `scheme://`) before its query or
/// fragment, with any `userinfo@` dropped.
fn host_and_path(url: &str) -> &str {
    let url = &url[..host_and_path_end(url)];
    let authority_end = url.find('/').unwrap_or(url.len());
    match url[..authority_end].rfind('@') {
        Some(at) => &url[at + 1..],
        None => url,
    }
}

fn host_and_path_end(url: &str) -> usize {
    url.find(['?', '#']).unwrap_or(url.len())
}

#[cfg(test)]
mod tests {
    use super::urls;

    #[test]
    fn credentials_leave_urls() {
        assert_eq!(
            urls("Authentication failed for https://alice:s3cret@example.com/org/repo: 401"),
            "Authentication failed for https://example.com/org/repo: 401"
        );
        assert_eq!(
            urls("Failed to connect to remote: https://example.com/api?token=abc&x=1"),
            "Failed to connect to remote: https://example.com/api"
        );
        assert_eq!(
            urls("fetch https://tok@example.com#frag and ssh://git@host/x.git\nthen"),
            "fetch https://example.com and ssh://host/x.git\nthen"
        );
    }

    #[test]
    fn text_without_credentials_is_unchanged() {
        for text in [
            "no url here",
            "https://example.com/repo failed",
            "path a@b/c is not a url",
            "",
        ] {
            assert_eq!(urls(text), text);
        }
    }
}
