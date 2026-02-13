//! Text encoding detection and handling for change contents
//!
//! When recording changes to text files, Atomic needs to know the encoding
//! to properly handle line endings, character boundaries, and display.
//!
//! # Supported Encodings
//!
//! - **UTF-8**: The default and most common encoding
//! - **UTF-16 LE/BE**: Windows Unicode files
//! - **Latin-1**: Legacy Western European encoding
//! - **Binary**: Non-text files (no encoding applied)
//!
//! # Detection Strategy
//!
//! Encoding is detected via:
//! 1. BOM (Byte Order Mark) if present
//! 2. File extension hints
//! 3. Content analysis (valid UTF-8 sequences, null bytes, etc.)
//!
//! # Example
//!
//! ```rust
//! use atomic_core::change::Encoding;
//!
//! // Detect encoding from content
//! let content = b"Hello, world!\n";
//! let encoding = Encoding::detect(content);
//! assert_eq!(encoding, Encoding::Utf8);
//!
//! // Binary files are detected by null bytes
//! let binary = b"\x00\x01\x02\x03";
//! assert_eq!(Encoding::detect(binary), Encoding::Binary);
//! ```

use serde::{Deserialize, Serialize};
use std::fmt;

/// Text encoding for file contents.
///
/// This enum represents the detected or specified encoding of a file's
/// content. It's used during change recording to properly handle text
/// transformations and during output to restore the correct encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Encoding {
    /// UTF-8 encoding (default for text files)
    ///
    /// This is the most common encoding for modern text files and
    /// is backwards-compatible with ASCII.
    #[default]
    Utf8,

    /// UTF-16 Little Endian
    ///
    /// Common in Windows applications. Identified by BOM `FF FE`.
    Utf16Le,

    /// UTF-16 Big Endian
    ///
    /// Less common, but used in some environments. Identified by BOM `FE FF`.
    Utf16Be,

    /// ISO-8859-1 (Latin-1)
    ///
    /// Legacy encoding for Western European languages. Used when a file
    /// contains bytes > 127 that don't form valid UTF-8 sequences.
    Latin1,

    /// Binary content (not text)
    ///
    /// Used for files that contain non-text data. No encoding transformations
    /// are applied, and the content is stored/retrieved as raw bytes.
    Binary,
}

impl Encoding {
    /// Detect the encoding of the given content.
    ///
    /// This function analyzes the content to determine its encoding:
    ///
    /// 1. Checks for BOM (Byte Order Mark)
    /// 2. Checks for null bytes (indicates binary)
    /// 3. Validates as UTF-8
    /// 4. Falls back to Latin-1 for high bytes
    ///
    /// # Arguments
    ///
    /// * `content` - The raw bytes to analyze
    ///
    /// # Returns
    ///
    /// The detected encoding.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::Encoding;
    ///
    /// // UTF-8 text
    /// assert_eq!(Encoding::detect(b"Hello"), Encoding::Utf8);
    ///
    /// // UTF-8 with BOM
    /// assert_eq!(Encoding::detect(b"\xef\xbb\xbfHello"), Encoding::Utf8);
    ///
    /// // Binary (contains null)
    /// assert_eq!(Encoding::detect(b"Hello\x00World"), Encoding::Binary);
    /// ```
    pub fn detect(content: &[u8]) -> Self {
        // Check for BOM first
        if let Some(encoding) = Self::detect_bom(content) {
            return encoding;
        }

        // Check for null bytes (binary indicator)
        if content.contains(&0) {
            return Encoding::Binary;
        }

        // Try UTF-8 validation
        if std::str::from_utf8(content).is_ok() {
            return Encoding::Utf8;
        }

        // Contains high bytes but not valid UTF-8, assume Latin-1
        Encoding::Latin1
    }

    /// Detect encoding from BOM (Byte Order Mark).
    ///
    /// Returns `Some(encoding)` if a BOM is detected, `None` otherwise.
    fn detect_bom(content: &[u8]) -> Option<Self> {
        if content.len() < 2 {
            return None;
        }

        // UTF-8 BOM: EF BB BF
        if content.len() >= 3 && content[0] == 0xEF && content[1] == 0xBB && content[2] == 0xBF {
            return Some(Encoding::Utf8);
        }

        // UTF-16 LE BOM: FF FE
        if content[0] == 0xFF && content[1] == 0xFE {
            return Some(Encoding::Utf16Le);
        }

        // UTF-16 BE BOM: FE FF
        if content[0] == 0xFE && content[1] == 0xFF {
            return Some(Encoding::Utf16Be);
        }

        None
    }

    /// Check if this encoding represents text (not binary).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::Encoding;
    ///
    /// assert!(Encoding::Utf8.is_text());
    /// assert!(Encoding::Latin1.is_text());
    /// assert!(!Encoding::Binary.is_text());
    /// ```
    #[inline]
    pub fn is_text(&self) -> bool {
        !matches!(self, Encoding::Binary)
    }

    /// Check if this encoding is binary.
    #[inline]
    pub fn is_binary(&self) -> bool {
        matches!(self, Encoding::Binary)
    }

    /// Get the BOM bytes for this encoding, if applicable.
    ///
    /// # Returns
    ///
    /// The BOM bytes, or an empty slice for encodings without BOM.
    pub fn bom(&self) -> &'static [u8] {
        match self {
            Encoding::Utf8 => &[], // UTF-8 BOM is optional, we don't add it
            Encoding::Utf16Le => &[0xFF, 0xFE],
            Encoding::Utf16Be => &[0xFE, 0xFF],
            Encoding::Latin1 => &[],
            Encoding::Binary => &[],
        }
    }

    /// Get the canonical name of this encoding.
    ///
    /// Returns a string suitable for display or serialization.
    pub fn name(&self) -> &'static str {
        match self {
            Encoding::Utf8 => "UTF-8",
            Encoding::Utf16Le => "UTF-16LE",
            Encoding::Utf16Be => "UTF-16BE",
            Encoding::Latin1 => "ISO-8859-1",
            Encoding::Binary => "binary",
        }
    }

    /// Parse an encoding from its name.
    ///
    /// Case-insensitive matching with common aliases.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::Encoding;
    ///
    /// assert_eq!(Encoding::from_name("utf-8"), Some(Encoding::Utf8));
    /// assert_eq!(Encoding::from_name("UTF8"), Some(Encoding::Utf8));
    /// assert_eq!(Encoding::from_name("latin1"), Some(Encoding::Latin1));
    /// assert_eq!(Encoding::from_name("unknown"), None);
    /// ```
    pub fn from_name(name: &str) -> Option<Self> {
        let lower = name.to_lowercase();
        match lower.as_str() {
            "utf-8" | "utf8" => Some(Encoding::Utf8),
            "utf-16le" | "utf16le" | "utf-16-le" => Some(Encoding::Utf16Le),
            "utf-16be" | "utf16be" | "utf-16-be" => Some(Encoding::Utf16Be),
            "latin1" | "latin-1" | "iso-8859-1" | "iso88591" => Some(Encoding::Latin1),
            "binary" | "bin" => Some(Encoding::Binary),
            _ => None,
        }
    }

    /// Detect encoding from file extension.
    ///
    /// Some file extensions strongly suggest a particular encoding.
    ///
    /// # Arguments
    ///
    /// * `extension` - The file extension (without the leading dot)
    ///
    /// # Returns
    ///
    /// `Some(Encoding::Binary)` for known binary extensions, `None` otherwise.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::Encoding;
    ///
    /// assert_eq!(Encoding::from_extension("png"), Some(Encoding::Binary));
    /// assert_eq!(Encoding::from_extension("jpg"), Some(Encoding::Binary));
    /// assert_eq!(Encoding::from_extension("rs"), None); // Could be any text encoding
    /// ```
    pub fn from_extension(extension: &str) -> Option<Self> {
        let lower = extension.to_lowercase();
        match lower.as_str() {
            // Images
            "png" | "jpg" | "jpeg" | "gif" | "bmp" | "ico" | "webp" | "svg" | "tiff" | "tif" => {
                Some(Encoding::Binary)
            }
            // Audio/Video
            "mp3" | "mp4" | "wav" | "ogg" | "flac" | "avi" | "mkv" | "mov" | "webm" => {
                Some(Encoding::Binary)
            }
            // Archives
            "zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "rar" => Some(Encoding::Binary),
            // Executables
            "exe" | "dll" | "so" | "dylib" | "o" | "a" => Some(Encoding::Binary),
            // Documents
            "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" => Some(Encoding::Binary),
            // Fonts
            "ttf" | "otf" | "woff" | "woff2" | "eot" => Some(Encoding::Binary),
            // Other binary
            "bin" | "dat" | "db" | "sqlite" | "sqlite3" => Some(Encoding::Binary),
            // Unknown - let content detection decide
            _ => None,
        }
    }

    /// Skip BOM bytes in content if present.
    ///
    /// Returns the content without the BOM prefix for this encoding.
    ///
    /// # Arguments
    ///
    /// * `content` - The content potentially starting with a BOM
    ///
    /// # Returns
    ///
    /// The content after any BOM bytes.
    pub fn skip_bom<'a>(&self, content: &'a [u8]) -> &'a [u8] {
        match self {
            Encoding::Utf8 => {
                if content.len() >= 3
                    && content[0] == 0xEF
                    && content[1] == 0xBB
                    && content[2] == 0xBF
                {
                    &content[3..]
                } else {
                    content
                }
            }
            Encoding::Utf16Le | Encoding::Utf16Be => {
                if content.len() >= 2 {
                    let bom = self.bom();
                    if content.starts_with(bom) {
                        return &content[bom.len()..];
                    }
                }
                content
            }
            Encoding::Latin1 | Encoding::Binary => content,
        }
    }
}

impl fmt::Display for Encoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // Detection Tests
    // ========================================================================

    #[test]
    fn test_detect_utf8_ascii() {
        // Pure ASCII is valid UTF-8
        let content = b"Hello, World!\nThis is a test.";
        assert_eq!(Encoding::detect(content), Encoding::Utf8);
    }

    #[test]
    fn test_detect_utf8_multibyte() {
        // UTF-8 with multi-byte characters
        let content = "Hello, 世界! Привет! 🎉".as_bytes();
        assert_eq!(Encoding::detect(content), Encoding::Utf8);
    }

    #[test]
    fn test_detect_utf8_bom() {
        // UTF-8 with BOM
        let content = b"\xef\xbb\xbfHello with BOM";
        assert_eq!(Encoding::detect(content), Encoding::Utf8);
    }

    #[test]
    fn test_detect_utf16le_bom() {
        let content = b"\xff\xfeH\x00e\x00l\x00l\x00o\x00";
        assert_eq!(Encoding::detect(content), Encoding::Utf16Le);
    }

    #[test]
    fn test_detect_utf16be_bom() {
        let content = b"\xfe\xff\x00H\x00e\x00l\x00l\x00o";
        assert_eq!(Encoding::detect(content), Encoding::Utf16Be);
    }

    #[test]
    fn test_detect_binary_null_bytes() {
        // Content with null bytes is binary
        let content = b"Hello\x00World";
        assert_eq!(Encoding::detect(content), Encoding::Binary);
    }

    #[test]
    fn test_detect_latin1() {
        // High bytes that aren't valid UTF-8
        let content = b"Caf\xe9"; // "Café" in Latin-1
        assert_eq!(Encoding::detect(content), Encoding::Latin1);
    }

    #[test]
    fn test_detect_empty() {
        // Empty content is valid UTF-8
        assert_eq!(Encoding::detect(b""), Encoding::Utf8);
    }

    // ========================================================================
    // Property Tests
    // ========================================================================

    #[test]
    fn test_is_text() {
        assert!(Encoding::Utf8.is_text());
        assert!(Encoding::Utf16Le.is_text());
        assert!(Encoding::Utf16Be.is_text());
        assert!(Encoding::Latin1.is_text());
        assert!(!Encoding::Binary.is_text());
    }

    #[test]
    fn test_is_binary() {
        assert!(!Encoding::Utf8.is_binary());
        assert!(Encoding::Binary.is_binary());
    }

    #[test]
    fn test_default() {
        assert_eq!(Encoding::default(), Encoding::Utf8);
    }

    // ========================================================================
    // BOM Tests
    // ========================================================================

    #[test]
    fn test_bom_bytes() {
        assert!(Encoding::Utf8.bom().is_empty());
        assert_eq!(Encoding::Utf16Le.bom(), &[0xFF, 0xFE]);
        assert_eq!(Encoding::Utf16Be.bom(), &[0xFE, 0xFF]);
        assert!(Encoding::Latin1.bom().is_empty());
        assert!(Encoding::Binary.bom().is_empty());
    }

    #[test]
    fn test_skip_bom_utf8() {
        let with_bom = b"\xef\xbb\xbfHello";
        let without_bom = b"Hello";

        assert_eq!(Encoding::Utf8.skip_bom(with_bom), b"Hello");
        assert_eq!(Encoding::Utf8.skip_bom(without_bom), b"Hello");
    }

    #[test]
    fn test_skip_bom_utf16le() {
        let with_bom = b"\xff\xfeHello";
        assert_eq!(Encoding::Utf16Le.skip_bom(with_bom), b"Hello");
    }

    #[test]
    fn test_skip_bom_no_change() {
        let content = b"No BOM here";
        assert_eq!(Encoding::Utf8.skip_bom(content), content);
        assert_eq!(Encoding::Latin1.skip_bom(content), content);
        assert_eq!(Encoding::Binary.skip_bom(content), content);
    }

    // ========================================================================
    // Name/Parse Tests
    // ========================================================================

    #[test]
    fn test_name() {
        assert_eq!(Encoding::Utf8.name(), "UTF-8");
        assert_eq!(Encoding::Utf16Le.name(), "UTF-16LE");
        assert_eq!(Encoding::Utf16Be.name(), "UTF-16BE");
        assert_eq!(Encoding::Latin1.name(), "ISO-8859-1");
        assert_eq!(Encoding::Binary.name(), "binary");
    }

    #[test]
    fn test_display() {
        assert_eq!(format!("{}", Encoding::Utf8), "UTF-8");
        assert_eq!(format!("{}", Encoding::Binary), "binary");
    }

    #[test]
    fn test_from_name() {
        // Standard names
        assert_eq!(Encoding::from_name("utf-8"), Some(Encoding::Utf8));
        assert_eq!(Encoding::from_name("UTF-8"), Some(Encoding::Utf8));
        assert_eq!(Encoding::from_name("utf8"), Some(Encoding::Utf8));

        // UTF-16 variants
        assert_eq!(Encoding::from_name("utf-16le"), Some(Encoding::Utf16Le));
        assert_eq!(Encoding::from_name("utf-16be"), Some(Encoding::Utf16Be));

        // Latin-1 aliases
        assert_eq!(Encoding::from_name("latin1"), Some(Encoding::Latin1));
        assert_eq!(Encoding::from_name("iso-8859-1"), Some(Encoding::Latin1));

        // Binary
        assert_eq!(Encoding::from_name("binary"), Some(Encoding::Binary));
        assert_eq!(Encoding::from_name("bin"), Some(Encoding::Binary));

        // Unknown
        assert_eq!(Encoding::from_name("unknown"), None);
    }

    // ========================================================================
    // Extension Tests
    // ========================================================================

    #[test]
    fn test_from_extension_images() {
        assert_eq!(Encoding::from_extension("png"), Some(Encoding::Binary));
        assert_eq!(Encoding::from_extension("jpg"), Some(Encoding::Binary));
        assert_eq!(Encoding::from_extension("PNG"), Some(Encoding::Binary));
    }

    #[test]
    fn test_from_extension_archives() {
        assert_eq!(Encoding::from_extension("zip"), Some(Encoding::Binary));
        assert_eq!(Encoding::from_extension("tar"), Some(Encoding::Binary));
        assert_eq!(Encoding::from_extension("gz"), Some(Encoding::Binary));
    }

    #[test]
    fn test_from_extension_text() {
        // Text files should return None (let content detection decide)
        assert_eq!(Encoding::from_extension("txt"), None);
        assert_eq!(Encoding::from_extension("rs"), None);
        assert_eq!(Encoding::from_extension("md"), None);
        assert_eq!(Encoding::from_extension("json"), None);
    }

    // ========================================================================
    // Serialization Tests
    // ========================================================================

    #[test]
    fn test_json_roundtrip() {
        for encoding in [
            Encoding::Utf8,
            Encoding::Utf16Le,
            Encoding::Utf16Be,
            Encoding::Latin1,
            Encoding::Binary,
        ] {
            let json = serde_json::to_string(&encoding).unwrap();
            let parsed: Encoding = serde_json::from_str(&json).unwrap();
            assert_eq!(encoding, parsed);
        }
    }

    #[test]
    fn test_json_values() {
        // Verify serde renames to lowercase
        assert_eq!(serde_json::to_string(&Encoding::Utf8).unwrap(), "\"utf8\"");
        assert_eq!(
            serde_json::to_string(&Encoding::Binary).unwrap(),
            "\"binary\""
        );
    }

    #[test]
    fn test_bincode_roundtrip() {
        for encoding in [
            Encoding::Utf8,
            Encoding::Utf16Le,
            Encoding::Utf16Be,
            Encoding::Latin1,
            Encoding::Binary,
        ] {
            let bytes = bincode::serialize(&encoding).unwrap();
            let parsed: Encoding = bincode::deserialize(&bytes).unwrap();
            assert_eq!(encoding, parsed);
        }
    }

    // ========================================================================
    // Edge Cases
    // ========================================================================

    #[test]
    fn test_detect_single_byte() {
        assert_eq!(Encoding::detect(b"a"), Encoding::Utf8);
        assert_eq!(Encoding::detect(b"\x00"), Encoding::Binary);
        assert_eq!(Encoding::detect(b"\xff"), Encoding::Latin1);
    }

    #[test]
    fn test_detect_two_bytes() {
        // Two bytes that look like UTF-16 BOM
        assert_eq!(Encoding::detect(b"\xff\xfe"), Encoding::Utf16Le);
        assert_eq!(Encoding::detect(b"\xfe\xff"), Encoding::Utf16Be);
    }
}
