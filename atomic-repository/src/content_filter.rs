//! Working-copy/repository byte conversion.
//!
//! Atomic hashes and stores repository bytes. A content filter is therefore a
//! boundary adapter only: `clean` converts bytes read from disk before record,
//! and `smudge` converts stored bytes immediately before materialization.

use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use atomic_config::{ContentFilterConfig, ExternalFilterConfig};
use thiserror::Error;

/// Direction of a repository-byte conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterDirection {
    /// Working-copy bytes to repository bytes.
    Clean,
    /// Repository bytes to working-copy bytes.
    Smudge,
}

impl fmt::Display for FilterDirection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Clean => "clean",
            Self::Smudge => "smudge",
        })
    }
}

/// Result of a content-filter boundary conversion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilteredContent {
    /// Converted bytes. These are repository bytes after `clean` and working
    /// bytes after `smudge`.
    pub bytes: Vec<u8>,
    /// Non-fatal diagnostics, such as an optional external driver fallback.
    pub warnings: Vec<String>,
}

impl FilteredContent {
    fn exact(bytes: &[u8]) -> Self {
        Self {
            bytes: bytes.to_vec(),
            warnings: Vec::new(),
        }
    }
}

/// A failure at the working/repository byte boundary.
#[derive(Debug, Error)]
pub enum ContentFilterError {
    /// Attribute files could not be read.
    #[error("cannot read attributes for '{path}': {source}")]
    Attributes {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// A required external driver was not configured for this direction.
    #[error("required filter '{driver}' has no {direction} command for '{path}'")]
    MissingRequiredDriver {
        driver: String,
        direction: FilterDirection,
        path: String,
    },
    /// A required external driver failed or exceeded a bound.
    #[error("required filter '{driver}' {direction} failed for '{path}': {reason}")]
    RequiredDriverFailed {
        driver: String,
        direction: FilterDirection,
        path: String,
        reason: String,
    },
}

/// Explicit clean/smudge boundary for repository bytes.
pub trait ContentFilter: Send + Sync {
    /// Convert working-copy bytes into the bytes Atomic hashes and stores.
    fn clean(
        &self,
        path: &Path,
        working_bytes: &[u8],
    ) -> Result<FilteredContent, ContentFilterError>;

    /// Convert stored repository bytes into bytes written to the working copy.
    fn smudge(
        &self,
        path: &Path,
        repository_bytes: &[u8],
    ) -> Result<FilteredContent, ContentFilterError>;

    /// Whether Git currently tracks this path in a colocated repository.
    fn is_git_tracked(&self, _path: &Path) -> bool {
        false
    }
}

/// Deterministic `.gitattributes` content conversion with bounded drivers.
#[derive(Clone, Debug)]
pub struct GitAttributesFilter {
    root: PathBuf,
    config: ContentFilterConfig,
}

impl GitAttributesFilter {
    /// Construct a filter rooted at a repository working copy.
    pub fn new(root: impl Into<PathBuf>, config: ContentFilterConfig) -> Self {
        Self {
            root: root.into(),
            config,
        }
    }

    /// Load filter bounds and driver definitions from `.atomic/config.toml`.
    pub fn for_repository(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let mut config = atomic_config::RepoConfig::load(&root.join(".atomic/config.toml"))
            .map(|config| config.filters)
            .unwrap_or_default();
        // Git-defined filter drivers are authoritative for a colocated
        // working copy (CB-3C filter policy): `filter.<name>.clean`,
        // `filter.<name>.smudge`, and `filter.<name>.required` from the
        // repository's Git config seed the driver map for every
        // .gitattributes filter name the Git-side operations must honor.
        // Atomic-declared drivers win on name collision, because they can
        // only narrow an already-Git-honored contract deliberately.
        if let Ok(output) = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["config", "--get-regexp", r"^filter\."])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
        {
            if output.status.success() {
                let mut drivers: std::collections::BTreeMap<String, ExternalFilterConfig> =
                    std::collections::BTreeMap::new();
                for line in String::from_utf8_lossy(&output.stdout).lines() {
                    let Some((key, value)) = line.split_once(' ') else {
                        continue;
                    };
                    // key: "filter.<name>.<clean|smudge|required>"
                    let Some(rest) = key.strip_prefix("filter.") else {
                        continue;
                    };
                    let Some((name, property)) = rest.rsplit_once('.') else {
                        continue;
                    };
                    if !matches!(property, "clean" | "smudge" | "required") {
                        continue;
                    }
                    let driver = drivers.entry(name.to_string()).or_default();
                    match property {
                        "clean" => driver.clean = Some(value.to_string()),
                        "smudge" => driver.smudge = Some(value.to_string()),
                        "required" => driver.required = value == "true",
                        _ => {}
                    }
                }
                for (name, driver) in drivers {
                    config.drivers.entry(name).or_insert(driver);
                }
            }
        }
        Self::new(root, config)
    }

    fn convert(
        &self,
        direction: FilterDirection,
        path: &Path,
        input: &[u8],
    ) -> Result<FilteredContent, ContentFilterError> {
        let attrs = self.attributes(path)?;

        // A repository-side LFS pointer is already canonical metadata, not the
        // large object. Never feed it to an external smudge driver here.
        if is_git_lfs_pointer(input) {
            return Ok(FilteredContent::exact(input));
        }

        let mut bytes = input.to_vec();
        if attrs.is_text(&bytes) {
            match direction {
                FilterDirection::Clean => {
                    bytes = normalize_lf(&bytes);
                    if attrs.ident {
                        bytes = collapse_ident(&bytes);
                    }
                }
                FilterDirection::Smudge => {
                    if attrs.ident {
                        bytes = expand_ident(&bytes);
                    }
                    if attrs.eol == Some(Eol::CrLf) {
                        bytes = expand_crlf(&bytes);
                    }
                }
            }
        }

        let Some(driver_name) = attrs.filter.as_deref() else {
            return Ok(FilteredContent {
                bytes,
                warnings: Vec::new(),
            });
        };
        let Some(driver) = self.config.drivers.get(driver_name) else {
            return if attrs.filter_required {
                Err(ContentFilterError::MissingRequiredDriver {
                    driver: driver_name.to_string(),
                    direction,
                    path: path.display().to_string(),
                })
            } else {
                Ok(FilteredContent {
                    bytes,
                    warnings: vec![format!(
                        "optional filter '{driver_name}' is not configured for '{}'",
                        path.display()
                    )],
                })
            };
        };
        self.run_driver(driver_name, driver, direction, path, &bytes)
    }

    fn run_driver(
        &self,
        name: &str,
        driver: &ExternalFilterConfig,
        direction: FilterDirection,
        path: &Path,
        input: &[u8],
    ) -> Result<FilteredContent, ContentFilterError> {
        let command = match direction {
            FilterDirection::Clean => driver.clean.as_deref(),
            FilterDirection::Smudge => driver.smudge.as_deref(),
        };
        let required = driver.required;
        let Some(command) = command else {
            return if required {
                Err(ContentFilterError::MissingRequiredDriver {
                    driver: name.to_string(),
                    direction,
                    path: path.display().to_string(),
                })
            } else {
                Ok(FilteredContent {
                    bytes: input.to_vec(),
                    warnings: vec![format!(
                        "optional filter '{name}' has no {direction} command for '{}'",
                        path.display()
                    )],
                })
            };
        };

        match run_bounded_command(
            command,
            path,
            input,
            Duration::from_millis(self.config.timeout_ms),
            self.config.max_output_bytes,
        ) {
            Ok(bytes) => Ok(FilteredContent {
                bytes,
                warnings: Vec::new(),
            }),
            Err(reason) if required => Err(ContentFilterError::RequiredDriverFailed {
                driver: name.to_string(),
                direction,
                path: path.display().to_string(),
                reason,
            }),
            Err(reason) => Ok(FilteredContent {
                bytes: input.to_vec(),
                warnings: vec![format!(
                    "optional filter '{name}' {direction} failed for '{}': {reason}; using input bytes",
                    path.display()
                )],
            }),
        }
    }

    fn attributes(&self, path: &Path) -> Result<Attributes, ContentFilterError> {
        let relative = path.to_string_lossy().replace('\\', "/");
        let mut result = Attributes::default();
        let mut directories = vec![PathBuf::new()];
        if let Some(parent) = path.parent() {
            let mut current = PathBuf::new();
            for component in parent.components() {
                current.push(component);
                directories.push(current.clone());
            }
        }

        for directory in directories {
            let attribute_path = self.root.join(&directory).join(".gitattributes");
            let text = match std::fs::read_to_string(&attribute_path) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => {
                    return Err(ContentFilterError::Attributes {
                        path: attribute_path.display().to_string(),
                        source,
                    })
                }
            };
            let local_path = path.strip_prefix(&directory).unwrap_or(path);
            let local = local_path.to_string_lossy().replace('\\', "/");
            for line in text.lines() {
                apply_attribute_line(line, &relative, &local, &mut result);
            }
        }
        if let Some(name) = result.filter.as_ref() {
            result.filter_required = self
                .config
                .drivers
                .get(name)
                .is_some_and(|driver| driver.required);
        }
        Ok(result)
    }
}

impl ContentFilter for GitAttributesFilter {
    fn clean(
        &self,
        path: &Path,
        working_bytes: &[u8],
    ) -> Result<FilteredContent, ContentFilterError> {
        self.convert(FilterDirection::Clean, path, working_bytes)
    }

    fn smudge(
        &self,
        path: &Path,
        repository_bytes: &[u8],
    ) -> Result<FilteredContent, ContentFilterError> {
        self.convert(FilterDirection::Smudge, path, repository_bytes)
    }

    fn is_git_tracked(&self, path: &Path) -> bool {
        if !self.root.join(".git").exists() {
            return false;
        }
        Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["ls-files", "--error-unmatch", "--"])
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum TextAttribute {
    #[default]
    Unspecified,
    Set,
    Auto,
    Unset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Eol {
    Lf,
    CrLf,
}

#[derive(Clone, Debug, Default)]
struct Attributes {
    text: TextAttribute,
    eol: Option<Eol>,
    ident: bool,
    filter: Option<String>,
    filter_required: bool,
}

impl Attributes {
    fn is_text(&self, input: &[u8]) -> bool {
        match self.text {
            TextAttribute::Set => true,
            TextAttribute::Auto => !looks_binary(input),
            TextAttribute::Unset => false,
            TextAttribute::Unspecified => self.eol.is_some() || self.ident,
        }
    }
}

fn apply_attribute_line(line: &str, relative: &str, local: &str, attrs: &mut Attributes) {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
        return;
    }
    let mut fields = line.split_whitespace();
    let Some(pattern) = fields.next() else {
        return;
    };
    let candidate = if pattern.contains('/') {
        relative
    } else {
        local
    };
    if !wildmatch(pattern.trim_start_matches('/'), candidate) {
        return;
    }
    for field in fields {
        match field {
            "text" => attrs.text = TextAttribute::Set,
            "text=auto" => attrs.text = TextAttribute::Auto,
            "-text" => attrs.text = TextAttribute::Unset,
            "!text" => attrs.text = TextAttribute::Unspecified,
            "ident" => attrs.ident = true,
            "-ident" | "!ident" => attrs.ident = false,
            "eol=lf" => attrs.eol = Some(Eol::Lf),
            "eol=crlf" => attrs.eol = Some(Eol::CrLf),
            "!eol" => attrs.eol = None,
            "-filter" | "!filter" => attrs.filter = None,
            _ => {
                if let Some(name) = field.strip_prefix("filter=") {
                    attrs.filter = Some(name.to_string());
                }
            }
        }
    }
}

fn wildmatch(pattern: &str, text: &str) -> bool {
    fn matches(pattern: &[u8], text: &[u8]) -> bool {
        match pattern.split_first() {
            None => text.is_empty(),
            Some((&b'*', rest)) => {
                let rest = rest.strip_prefix(b"*").unwrap_or(rest);
                (0..=text.len()).any(|index| matches(rest, &text[index..]))
            }
            Some((&b'?', rest)) => !text.is_empty() && text[0] != b'/' && matches(rest, &text[1..]),
            Some((&expected, rest)) => {
                !text.is_empty() && expected == text[0] && matches(rest, &text[1..])
            }
        }
    }
    matches(pattern.as_bytes(), text.as_bytes())
}

/// Whether bytes are a canonical Git LFS pointer. Pointers pass through Atomic;
/// LFS object transfer remains Git LFS's responsibility.
pub fn is_git_lfs_pointer(input: &[u8]) -> bool {
    if input.len() > 1024 || !input.starts_with(b"version https://git-lfs.github.com/spec/v1\n") {
        return false;
    }
    let text = match std::str::from_utf8(input) {
        Ok(text) => text,
        Err(_) => return false,
    };
    let mut has_oid = false;
    let mut has_size = false;
    for line in text.lines().skip(1) {
        if let Some(oid) = line.strip_prefix("oid sha256:") {
            has_oid = oid.len() == 64 && oid.bytes().all(|byte| byte.is_ascii_hexdigit());
        } else if let Some(size) = line.strip_prefix("size ") {
            has_size = size.parse::<u64>().is_ok();
        }
    }
    has_oid && has_size
}

pub(crate) fn read_working_bytes(path: &Path) -> std::io::Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            return Ok(target.into_os_string().into_vec());
        }
        #[cfg(not(unix))]
        return Ok(target.to_string_lossy().as_bytes().to_vec());
    }
    if metadata.is_dir() {
        // A graph-backed gitlink materializes as a directory whose exact
        // object-id payload is retained in `.git`.
        return std::fs::read(path.join(".git"));
    }
    std::fs::read(path)
}

/// Binary heuristic used only to select opaque representation, never to reject
/// Git-tracked data.
pub fn looks_binary(input: &[u8]) -> bool {
    input.iter().take(8 * 1024).any(|byte| *byte == 0)
}

fn normalize_lf(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'\r' && input.get(index + 1) == Some(&b'\n') {
            output.push(b'\n');
            index += 2;
        } else {
            output.push(input[index]);
            index += 1;
        }
    }
    output
}

fn expand_crlf(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    let mut previous = None;
    for &byte in input {
        if byte == b'\n' && previous != Some(b'\r') {
            output.push(b'\r');
        }
        output.push(byte);
        previous = Some(byte);
    }
    output
}

fn collapse_ident(input: &[u8]) -> Vec<u8> {
    transform_idents(input, None)
}

fn expand_ident(input: &[u8]) -> Vec<u8> {
    let oid = git_blob_sha1(input);
    transform_idents(input, Some(&oid))
}

fn transform_idents(input: &[u8], oid: Option<&str>) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len() + 48);
    let mut index = 0;
    while index < input.len() {
        if input[index..].starts_with(b"$Id$") {
            if let Some(oid) = oid {
                output.extend_from_slice(b"$Id: ");
                output.extend_from_slice(oid.as_bytes());
                output.extend_from_slice(b" $");
            } else {
                output.extend_from_slice(b"$Id$");
            }
            index += 4;
        } else if input[index..].starts_with(b"$Id:") {
            if let Some(end) = input[index + 4..].iter().position(|byte| *byte == b'$') {
                output.extend_from_slice(b"$Id$");
                index += 5 + end;
            } else {
                output.push(input[index]);
                index += 1;
            }
        } else {
            output.push(input[index]);
            index += 1;
        }
    }
    output
}

fn run_bounded_command(
    command: &str,
    path: &Path,
    input: &[u8],
    timeout: Duration,
    output_limit: usize,
) -> Result<Vec<u8>, String> {
    let command = command.replace("%f", &path.to_string_lossy());
    #[cfg(unix)]
    let mut child = Command::new("sh")
        .args(["-c", &command])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    #[cfg(windows)]
    let mut child = Command::new("cmd")
        .args(["/C", &command])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;

    let mut stdin = child.stdin.take().expect("piped stdin");
    let owned_input = input.to_vec();
    let input_thread = std::thread::spawn(move || stdin.write_all(&owned_input));
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdout_thread = std::thread::spawn(move || read_bounded(stdout, output_limit));
    let stderr_thread = std::thread::spawn(move || read_bounded(stderr, output_limit));

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = input_thread.join();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(format!("timed out after {} ms", timeout.as_millis()));
            }
            Err(error) => return Err(error.to_string()),
        }
    };
    let _ = input_thread.join();
    let stdout = stdout_thread
        .join()
        .map_err(|_| "stdout reader panicked".to_string())??;
    let stderr = stderr_thread
        .join()
        .map_err(|_| "stderr reader panicked".to_string())??;
    if !status.success() {
        return Err(format!(
            "exited with {status}: {}",
            String::from_utf8_lossy(&stderr)
        ));
    }
    Ok(stdout)
}

fn read_bounded(mut reader: impl Read, limit: usize) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    reader
        .by_ref()
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut output)
        .map_err(|error| error.to_string())?;
    if output.len() > limit {
        Err(format!("output exceeded {limit} bytes"))
    } else {
        Ok(output)
    }
}

// Minimal deterministic SHA-1 for Git's `ident` expansion. The digest covers
// the Git blob object header and repository bytes, exactly like `git hash-object`.
fn git_blob_sha1(content: &[u8]) -> String {
    let mut input = format!("blob {}\0", content.len()).into_bytes();
    input.extend_from_slice(content);
    let bit_len = (input.len() as u64) * 8;
    input.push(0x80);
    while input.len() % 64 != 56 {
        input.push(0);
    }
    input.extend_from_slice(&bit_len.to_be_bytes());
    let mut h = [
        0x67452301u32,
        0xefcdab89,
        0x98badcfe,
        0x10325476,
        0xc3d2e1f0,
    ];
    for chunk in input.as_chunks::<64>().0 {
        let mut words = [0u32; 80];
        for (index, word) in words[..16].iter_mut().enumerate() {
            *word = u32::from_be_bytes(chunk[index * 4..index * 4 + 4].try_into().unwrap());
        }
        for index in 16..80 {
            words[index] =
                (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                    .rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (index, word) in words.iter().enumerate() {
            let (f, k) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5a827999),
                20..=39 => (b ^ c ^ d, 0x6ed9eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1bbcdc),
                _ => (b ^ c ^ d, 0xca62c1d6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = next;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    h.iter().map(|word| format!("{word:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(root: &Path) -> GitAttributesFilter {
        GitAttributesFilter::new(root, ContentFilterConfig::default())
    }

    #[test]
    fn text_eol_and_ident_are_deterministic_boundaries() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join(".gitattributes"),
            "*.txt text eol=crlf ident\n",
        )
        .unwrap();
        let filter = filter(temp.path());
        let clean = filter
            .clean(Path::new("a.txt"), b"one\r\n$Id: old $\r\n")
            .unwrap();
        assert_eq!(clean.bytes, b"one\n$Id$\n");
        let smudged = filter.smudge(Path::new("a.txt"), &clean.bytes).unwrap();
        assert!(smudged.bytes.starts_with(b"one\r\n$Id: "));
        assert!(smudged.bytes.ends_with(b" $\r\n"));
        assert_eq!(
            filter
                .clean(Path::new("a.txt"), &smudged.bytes)
                .unwrap()
                .bytes,
            clean.bytes
        );
    }

    #[test]
    fn binary_text_auto_stays_exact() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(".gitattributes"), "* text=auto eol=crlf\n").unwrap();
        let input = b"a\0b\n";
        assert_eq!(
            filter(temp.path())
                .clean(Path::new("blob"), input)
                .unwrap()
                .bytes,
            input
        );
    }

    #[test]
    fn lfs_pointer_bypasses_smudge_driver() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(".gitattributes"), "*.bin filter=lfs\n").unwrap();
        let mut config = ContentFilterConfig::default();
        config.drivers.insert(
            "lfs".into(),
            ExternalFilterConfig {
                smudge: Some("exit 9".into()),
                required: true,
                ..ExternalFilterConfig::default()
            },
        );
        let pointer = b"version https://git-lfs.github.com/spec/v1\noid sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\nsize 42\n";
        let output = GitAttributesFilter::new(temp.path(), config)
            .smudge(Path::new("asset.bin"), pointer)
            .unwrap();
        assert_eq!(output.bytes, pointer);
    }

    #[test]
    fn required_driver_failure_is_an_error() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(".gitattributes"), "*.dat filter=broken\n").unwrap();
        let mut config = ContentFilterConfig::default();
        config.drivers.insert(
            "broken".into(),
            ExternalFilterConfig {
                clean: Some("exit 7".into()),
                required: true,
                ..ExternalFilterConfig::default()
            },
        );
        let error = GitAttributesFilter::new(temp.path(), config)
            .clean(Path::new("x.dat"), b"input")
            .unwrap_err();
        assert!(matches!(
            error,
            ContentFilterError::RequiredDriverFailed { .. }
        ));
    }

    #[test]
    fn output_bound_applies_to_required_driver() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(".gitattributes"), "*.dat filter=large\n").unwrap();
        let mut config = ContentFilterConfig {
            max_output_bytes: 3,
            ..ContentFilterConfig::default()
        };
        config.drivers.insert(
            "large".into(),
            ExternalFilterConfig {
                clean: Some("printf 1234".into()),
                required: true,
                ..ExternalFilterConfig::default()
            },
        );
        assert!(GitAttributesFilter::new(temp.path(), config)
            .clean(Path::new("x.dat"), b"")
            .is_err());
    }

    #[test]
    fn git_blob_sha1_matches_known_empty_blob() {
        assert_eq!(
            git_blob_sha1(b""),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
    }
}
