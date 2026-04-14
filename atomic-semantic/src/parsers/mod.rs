//! Multi-language AST parsers for semantic analysis.
//!
//! Each supported language has its own submodule with a parser that implements
//! the [`LanguageParser`] trait. The [`ParserRegistry`] provides a single entry
//! point for extracting entities from any supported file.
//!
//! # Architecture
//!
//! ```text
//! parsers/
//! ├── mod.rs          # LanguageParser trait, ParserRegistry, Language detection
//! ├── typescript.rs   # TypeScript / TSX via tree-sitter-typescript
//! ├── python.rs       # Python via tree-sitter-python
//! ├── rust.rs         # Rust via tree-sitter-rust
//! ├── go.rs           # Go via tree-sitter-go
//! └── java.rs         # Java via tree-sitter-java
//! ```
//!
//! # Adding a new language
//!
//! 1. Add the `tree-sitter-{lang}` crate to `Cargo.toml`
//! 2. Create `parsers/{lang}.rs` implementing [`LanguageParser`]
//! 3. Add the language variant to [`Language`]
//! 4. Register the parser in [`ParserRegistry::new`]
//!
//! # Example
//!
//! ```rust,ignore
//! use crate::semantic::parsers::ParserRegistry;
//!
//! let mut registry = ParserRegistry::new();
//!
//! let entities = registry.extract("src/auth.ts", source_code);
//! let refs = registry.extract_references("main.py", source_code);
//!
//! if registry.is_supported("lib.rs") {
//!     println!("Rust is supported!");
//! }
//! ```

pub mod go;
pub mod java;
pub mod python;
pub mod rust;
pub mod typescript;

use crate::entity::{Entity, Reference};
use std::collections::HashMap;
use std::fmt;

// ═══════════════════════════════════════════════════════════════════════
// Language — supported programming languages
// ═══════════════════════════════════════════════════════════════════════

/// A programming language supported for AST extraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    /// TypeScript (.ts, .mts, .cts)
    TypeScript,
    /// TypeScript with JSX (.tsx)
    Tsx,
    /// Python (.py, .pyi)
    Python,
    /// Rust (.rs)
    Rust,
    /// Go (.go)
    Go,
    /// Java (.java)
    Java,
}

impl Language {
    /// Detect language from a file path extension.
    ///
    /// Returns `None` for unsupported or unknown file types.
    pub fn detect(path: &str) -> Option<Self> {
        let lower = path.to_ascii_lowercase();

        if lower.ends_with(".tsx") {
            Some(Language::Tsx)
        } else if lower.ends_with(".ts") || lower.ends_with(".mts") || lower.ends_with(".cts") {
            Some(Language::TypeScript)
        } else if lower.ends_with(".py") || lower.ends_with(".pyi") {
            Some(Language::Python)
        } else if lower.ends_with(".rs") {
            Some(Language::Rust)
        } else if lower.ends_with(".go") {
            Some(Language::Go)
        } else if lower.ends_with(".java") {
            Some(Language::Java)
        } else {
            None
        }
    }

    /// Returns the human-readable language name.
    pub fn name(&self) -> &'static str {
        match self {
            Language::TypeScript => "TypeScript",
            Language::Tsx => "TSX",
            Language::Python => "Python",
            Language::Rust => "Rust",
            Language::Go => "Go",
            Language::Java => "Java",
        }
    }

    /// Returns the file extensions associated with this language.
    pub fn extensions(&self) -> &'static [&'static str] {
        match self {
            Language::TypeScript => &[".ts", ".mts", ".cts"],
            Language::Tsx => &[".tsx"],
            Language::Python => &[".py", ".pyi"],
            Language::Rust => &[".rs"],
            Language::Go => &[".go"],
            Language::Java => &[".java"],
        }
    }
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Check if a file path is supported for AST extraction.
pub fn is_supported(path: &str) -> bool {
    Language::detect(path).is_some()
}

/// All supported file extensions across all languages.
pub const ALL_EXTENSIONS: &[&str] = &[
    ".ts", ".mts", ".cts", ".tsx", ".py", ".pyi", ".rs", ".go", ".java",
];

// ═══════════════════════════════════════════════════════════════════════
// LanguageParser — trait that each language parser implements
// ═══════════════════════════════════════════════════════════════════════

/// Trait for language-specific AST entity extraction.
///
/// Each language parser implements this trait, providing entity extraction
/// (functions, classes, interfaces, etc.) and optionally reference extraction
/// (function calls, variable usages).
///
/// # Implementation Notes
///
/// - Parsers are **stateful** (they hold a `tree_sitter::Parser` instance).
///   Create one per thread or per request, not globally.
/// - `extract()` should be fast (~5ms for a typical source file).
/// - `extract_references()` may be slower and is optional for display.
/// - Entity names should be the *simple* name (e.g., `greet`, not `module.greet`).
/// - Line numbers are 1-based (matching editor conventions).
pub trait LanguageParser: Send {
    /// The language this parser handles.
    fn language(&self) -> Language;

    /// Extract AST entities (functions, classes, etc.) from source code.
    ///
    /// # Arguments
    ///
    /// * `source` - The source code text.
    /// * `file_path` - The file path (for Entity.file field).
    ///
    /// # Returns
    ///
    /// A list of entities found in the source code.
    fn extract(&mut self, source: &str, file_path: &str) -> Vec<Entity>;

    /// Extract references (function calls, variable usages) from source code.
    ///
    /// The default implementation returns an empty list. Languages that
    /// support reference extraction should override this.
    ///
    /// # Arguments
    ///
    /// * `source` - The source code text.
    /// * `file_path` - The file path (for Reference.file field).
    ///
    /// # Returns
    ///
    /// A list of references found in the source code.
    fn extract_references(&mut self, source: &str, file_path: &str) -> Vec<Reference> {
        let _ = (source, file_path);
        Vec::new()
    }
}

// ═══════════════════════════════════════════════════════════════════════
// ParserRegistry — unified entry point for all languages
// ═══════════════════════════════════════════════════════════════════════

/// Registry of language parsers.
///
/// Provides a single entry point for extracting entities from any supported
/// file. Lazily creates parsers on first use for each language.
///
/// # Thread Safety
///
/// `ParserRegistry` is NOT `Send`/`Sync` because tree-sitter parsers are
/// stateful. Create one per thread or per request.
///
/// # Example
///
/// ```rust,ignore
/// let mut registry = ParserRegistry::new();
///
/// // Automatically selects the right parser based on file extension
/// let entities = registry.extract("src/auth.ts", ts_source);
/// let entities = registry.extract("main.py", py_source);
/// let entities = registry.extract("lib.rs", rs_source);
/// let entities = registry.extract("main.go", go_source);
/// let entities = registry.extract("App.java", java_source);
/// ```
pub struct ParserRegistry {
    parsers: HashMap<Language, Box<dyn LanguageParser>>,
}

impl ParserRegistry {
    /// Create a new registry with all supported language parsers.
    pub fn new() -> Self {
        let mut parsers: HashMap<Language, Box<dyn LanguageParser>> = HashMap::new();

        parsers.insert(
            Language::TypeScript,
            Box::new(typescript::TypeScriptParser::new()),
        );
        parsers.insert(
            Language::Tsx,
            Box::new(typescript::TypeScriptParser::new_tsx()),
        );
        parsers.insert(Language::Python, Box::new(python::PythonParser::new()));
        parsers.insert(Language::Rust, Box::new(rust::RustParser::new()));
        parsers.insert(Language::Go, Box::new(go::GoParser::new()));
        parsers.insert(Language::Java, Box::new(java::JavaParser::new()));

        Self { parsers }
    }

    /// Check if a file path is supported.
    pub fn is_supported(&self, path: &str) -> bool {
        Language::detect(path)
            .map(|lang| self.parsers.contains_key(&lang))
            .unwrap_or(false)
    }

    /// Detect the language for a file path.
    pub fn detect_language(&self, path: &str) -> Option<Language> {
        Language::detect(path)
    }

    /// Extract entities from a source file.
    ///
    /// Automatically selects the right parser based on the file extension.
    /// Returns an empty list if the language is not supported.
    pub fn extract(&mut self, file_path: &str, source: &str) -> Vec<Entity> {
        let language = match Language::detect(file_path) {
            Some(l) => l,
            None => return Vec::new(),
        };

        match self.parsers.get_mut(&language) {
            Some(parser) => parser.extract(source, file_path),
            None => Vec::new(),
        }
    }

    /// Extract references from a source file.
    ///
    /// Automatically selects the right parser based on the file extension.
    /// Returns an empty list if the language is not supported or doesn't
    /// implement reference extraction.
    pub fn extract_references(&mut self, file_path: &str, source: &str) -> Vec<Reference> {
        let language = match Language::detect(file_path) {
            Some(l) => l,
            None => return Vec::new(),
        };

        match self.parsers.get_mut(&language) {
            Some(parser) => parser.extract_references(source, file_path),
            None => Vec::new(),
        }
    }

    /// Get a list of all supported languages.
    pub fn supported_languages(&self) -> Vec<Language> {
        self.parsers.keys().copied().collect()
    }
}

impl Default for ParserRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ParserRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let langs: Vec<&str> = self.parsers.keys().map(|l| l.name()).collect();
        f.debug_struct("ParserRegistry")
            .field("languages", &langs)
            .finish()
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    // ── Language detection ──────────────────────────────────────────

    #[test]
    fn test_detect_typescript() {
        assert_eq!(Language::detect("src/auth.ts"), Some(Language::TypeScript));
        assert_eq!(
            Language::detect("lib/utils.mts"),
            Some(Language::TypeScript)
        );
        assert_eq!(
            Language::detect("lib/utils.cts"),
            Some(Language::TypeScript)
        );
    }

    #[test]
    fn test_detect_tsx() {
        assert_eq!(
            Language::detect("components/Button.tsx"),
            Some(Language::Tsx)
        );
    }

    #[test]
    fn test_detect_python() {
        assert_eq!(Language::detect("main.py"), Some(Language::Python));
        assert_eq!(Language::detect("types.pyi"), Some(Language::Python));
        assert_eq!(Language::detect("src/utils.py"), Some(Language::Python));
    }

    #[test]
    fn test_detect_rust() {
        assert_eq!(Language::detect("src/main.rs"), Some(Language::Rust));
        assert_eq!(Language::detect("lib.rs"), Some(Language::Rust));
    }

    #[test]
    fn test_detect_go() {
        assert_eq!(Language::detect("main.go"), Some(Language::Go));
        assert_eq!(Language::detect("pkg/auth/handler.go"), Some(Language::Go));
    }

    #[test]
    fn test_detect_java() {
        assert_eq!(Language::detect("Main.java"), Some(Language::Java));
        assert_eq!(
            Language::detect("src/com/example/App.java"),
            Some(Language::Java)
        );
    }

    #[test]
    fn test_detect_unsupported() {
        assert_eq!(Language::detect("README.md"), None);
        assert_eq!(Language::detect("Cargo.toml"), None);
        assert_eq!(Language::detect("image.png"), None);
        assert_eq!(Language::detect("style.css"), None);
        assert_eq!(Language::detect(""), None);
    }

    #[test]
    fn test_detect_case_insensitive() {
        assert_eq!(Language::detect("FILE.PY"), Some(Language::Python));
        assert_eq!(Language::detect("Main.JAVA"), Some(Language::Java));
        assert_eq!(Language::detect("lib.RS"), Some(Language::Rust));
    }

    #[test]
    fn test_is_supported() {
        assert!(is_supported("main.ts"));
        assert!(is_supported("main.py"));
        assert!(is_supported("main.rs"));
        assert!(is_supported("main.go"));
        assert!(is_supported("Main.java"));
        assert!(!is_supported("README.md"));
    }

    // ── Language metadata ──────────────────────────────────────────

    #[test]
    fn test_language_name() {
        assert_eq!(Language::TypeScript.name(), "TypeScript");
        assert_eq!(Language::Tsx.name(), "TSX");
        assert_eq!(Language::Python.name(), "Python");
        assert_eq!(Language::Rust.name(), "Rust");
        assert_eq!(Language::Go.name(), "Go");
        assert_eq!(Language::Java.name(), "Java");
    }

    #[test]
    fn test_language_extensions() {
        assert!(Language::TypeScript.extensions().contains(&".ts"));
        assert!(Language::Python.extensions().contains(&".py"));
        assert!(Language::Rust.extensions().contains(&".rs"));
        assert!(Language::Go.extensions().contains(&".go"));
        assert!(Language::Java.extensions().contains(&".java"));
    }

    #[test]
    fn test_language_display() {
        assert_eq!(format!("{}", Language::Python), "Python");
        assert_eq!(format!("{}", Language::Rust), "Rust");
    }

    // ── ParserRegistry ─────────────────────────────────────────────

    #[test]
    fn test_registry_has_all_languages() {
        let registry = ParserRegistry::new();
        let langs = registry.supported_languages();
        assert!(langs.contains(&Language::TypeScript));
        assert!(langs.contains(&Language::Tsx));
        assert!(langs.contains(&Language::Python));
        assert!(langs.contains(&Language::Rust));
        assert!(langs.contains(&Language::Go));
        assert!(langs.contains(&Language::Java));
        assert_eq!(langs.len(), 6);
    }

    #[test]
    fn test_registry_is_supported() {
        let registry = ParserRegistry::new();
        assert!(registry.is_supported("main.ts"));
        assert!(registry.is_supported("main.py"));
        assert!(registry.is_supported("main.rs"));
        assert!(registry.is_supported("main.go"));
        assert!(registry.is_supported("Main.java"));
        assert!(!registry.is_supported("README.md"));
    }

    #[test]
    fn test_registry_detect_language() {
        let registry = ParserRegistry::new();
        assert_eq!(
            registry.detect_language("auth.ts"),
            Some(Language::TypeScript)
        );
        assert_eq!(registry.detect_language("main.py"), Some(Language::Python));
        assert_eq!(registry.detect_language("lib.rs"), Some(Language::Rust));
        assert_eq!(registry.detect_language("main.go"), Some(Language::Go));
        assert_eq!(registry.detect_language("App.java"), Some(Language::Java));
        assert_eq!(registry.detect_language("style.css"), None);
    }

    #[test]
    fn test_registry_extract_unsupported_returns_empty() {
        let mut registry = ParserRegistry::new();
        let entities = registry.extract("README.md", "# Hello");
        assert!(entities.is_empty());
    }

    #[test]
    fn test_registry_extract_references_unsupported_returns_empty() {
        let mut registry = ParserRegistry::new();
        let refs = registry.extract_references("README.md", "# Hello");
        assert!(refs.is_empty());
    }

    #[test]
    fn test_registry_debug() {
        let registry = ParserRegistry::new();
        let debug = format!("{:?}", registry);
        assert!(debug.contains("ParserRegistry"));
    }

    // ── Cross-language extraction ──────────────────────────────────

    #[test]
    fn test_extract_typescript() {
        let mut registry = ParserRegistry::new();
        let source = "export function greet(name: string): string { return `Hello, ${name}!`; }";
        let entities = registry.extract("src/greet.ts", source);
        assert!(!entities.is_empty());
        assert!(entities.iter().any(|e| e.name == "greet"));
    }

    #[test]
    fn test_extract_python() {
        let mut registry = ParserRegistry::new();
        let source = "def greet(name: str) -> str:\n    return f'Hello, {name}!'\n\nclass UserService:\n    pass\n";
        let entities = registry.extract("main.py", source);
        assert!(!entities.is_empty());
        assert!(
            entities.iter().any(|e| e.name == "greet"),
            "Should find greet function, got: {:?}",
            entities.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_extract_rust() {
        let mut registry = ParserRegistry::new();
        let source = "pub fn greet(name: &str) -> String {\n    format!(\"Hello, {}!\", name)\n}\n\npub struct User {\n    name: String,\n}\n";
        let entities = registry.extract("src/lib.rs", source);
        assert!(!entities.is_empty());
        assert!(
            entities.iter().any(|e| e.name == "greet"),
            "Should find greet function, got: {:?}",
            entities.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_extract_go() {
        let mut registry = ParserRegistry::new();
        let source = "package main\n\nfunc Greet(name string) string {\n\treturn \"Hello, \" + name\n}\n\ntype User struct {\n\tName string\n}\n";
        let entities = registry.extract("main.go", source);
        assert!(!entities.is_empty());
        assert!(
            entities.iter().any(|e| e.name == "Greet"),
            "Should find Greet function, got: {:?}",
            entities.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_extract_java() {
        let mut registry = ParserRegistry::new();
        let source = "public class UserService {\n    public String greet(String name) {\n        return \"Hello, \" + name;\n    }\n}\n";
        let entities = registry.extract("UserService.java", source);
        assert!(!entities.is_empty());
        assert!(
            entities.iter().any(|e| e.name == "UserService"),
            "Should find UserService class, got: {:?}",
            entities.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_extract_empty_source() {
        let mut registry = ParserRegistry::new();
        for path in &["empty.ts", "empty.py", "empty.rs", "empty.go", "Empty.java"] {
            let entities = registry.extract(path, "");
            assert!(
                entities.is_empty(),
                "{} should produce no entities for empty source",
                path
            );
        }
    }
}
