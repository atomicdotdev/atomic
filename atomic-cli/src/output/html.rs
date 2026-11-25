//! Shared HTML artifact writing and browser handoff.

use crate::error::{CliError, CliResult};

/// Finish writing before handing the file URL to an asynchronous browser. The
/// unique automatic file survives process exit and browser-launch failure.
pub(crate) fn write_and_open_html(
    html: &str,
    output: Option<&std::path::Path>,
    open_browser: impl FnOnce(&str) -> std::io::Result<()>,
) -> CliResult<std::path::PathBuf> {
    use std::io::Write;
    let path = match output {
        Some(path) => {
            std::fs::write(path, html)?;
            path.to_path_buf()
        }
        None => {
            let path =
                std::env::temp_dir().join(format!("atomic-report-{}.html", uuid::Uuid::new_v4()));
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            file.write_all(html.as_bytes())?;
            path
        }
    };
    let path = std::fs::canonicalize(path)?;
    let url = url::Url::from_file_path(&path).map_err(|_| CliError::InvalidArgument {
        message: format!("cannot convert HTML path to a file URL: {}", path.display()),
    })?;
    println!("Wrote HTML report to {}", path.display());
    if let Err(error) = open_browser(url.as_str()) {
        eprintln!("Could not open a browser: {error}. Open {url} manually.");
    }
    Ok(path)
}
