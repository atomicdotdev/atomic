//! Strip credentials from URLs before text reaches the log file.

/// `text` with every URL's credentials removed: everything up to the last
/// `@` becomes `***@`, and the query and fragment are dropped. Remote errors
/// name the URL they failed on, remote URLs can carry credentials in either
/// place, and an "invalid URL" error may quote one too malformed to parse,
/// so this over-redacts rather than parsing.
pub(super) fn urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let (before, after) = rest.split_at(at + "://".len());
        out.push_str(before);
        let end = after.find(char::is_whitespace).unwrap_or(after.len());
        let (url, tail) = after.split_at(end);
        push_redacted(&mut out, url);
        rest = tail;
    }
    out.push_str(rest);
    out
}

/// Push `url` (what follows `scheme://`, up to whitespace) without its
/// credentials.
fn push_redacted(out: &mut String, url: &str) {
    let host_and_rest = match url.rfind('@') {
        Some(at) => {
            out.push_str("***@");
            &url[at + 1..]
        }
        None => url,
    };
    let end = host_and_rest
        .find(['?', '#'])
        .unwrap_or(host_and_rest.len());
    out.push_str(&host_and_rest[..end]);
}

#[cfg(test)]
mod tests {
    use super::urls;

    #[test]
    fn credentials_leave_urls() {
        assert_eq!(
            urls("Authentication failed for https://alice:s3cret@example.com/org/repo: 401"),
            "Authentication failed for https://***@example.com/org/repo: 401"
        );
        assert_eq!(
            urls("Failed to connect to remote: https://example.com/api?token=abc&x=1"),
            "Failed to connect to remote: https://example.com/api"
        );
        assert_eq!(
            urls("fetch https://tok@example.com#frag and ssh://git@host/x.git\nthen"),
            "fetch https://***@example.com and ssh://***@host/x.git\nthen"
        );
    }

    #[test]
    fn malformed_urls_still_lose_their_credentials() {
        assert_eq!(
            urls("invalid URL https://alice:s3c?ret@host/r"),
            "invalid URL https://***@host/r"
        );
        assert_eq!(
            urls("invalid URL https://alice:p/ss@host/x"),
            "invalid URL https://***@host/x"
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
