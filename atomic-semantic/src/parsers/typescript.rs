//! TypeScript / TSX parser for semantic analysis.
//!
//! Delegates to the existing [`TypeScriptExtractor`] which handles the full
//! tree-sitter extraction pipeline for TypeScript and TSX files.

use super::{Language, LanguageParser};
use crate::entity::{Entity, Reference};
use crate::parser::TypeScriptExtractor;

/// TypeScript parser wrapping the existing [`TypeScriptExtractor`].
pub struct TypeScriptParser {
    extractor: TypeScriptExtractor,
    language: Language,
}

impl Default for TypeScriptParser {
    fn default() -> Self {
        Self::new()
    }
}

impl TypeScriptParser {
    /// Create a new TypeScript parser (.ts, .mts, .cts).
    pub fn new() -> Self {
        Self {
            extractor: TypeScriptExtractor::new(),
            language: Language::TypeScript,
        }
    }

    /// Create a new TSX parser (.tsx — TypeScript with JSX).
    pub fn new_tsx() -> Self {
        Self {
            extractor: TypeScriptExtractor::new_tsx(),
            language: Language::Tsx,
        }
    }
}

impl LanguageParser for TypeScriptParser {
    fn language(&self) -> Language {
        self.language
    }

    fn extract(&mut self, source: &str, file_path: &str) -> Vec<Entity> {
        self.extractor.extract(source, file_path)
    }

    fn extract_references(&mut self, source: &str, file_path: &str) -> Vec<Reference> {
        self.extractor.extract_references(source, file_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::EntityKind;

    #[test]
    fn test_typescript_function() {
        let mut parser = TypeScriptParser::new();
        let source = r#"
export function greet(name: string): string {
    return `Hello, ${name}!`;
}
"#;
        let entities = parser.extract(source, "src/greet.ts");
        assert!(!entities.is_empty());
        let greet = entities.iter().find(|e| e.name == "greet");
        assert!(greet.is_some(), "Should find greet function");
        assert_eq!(greet.unwrap().kind, EntityKind::Function);
    }

    #[test]
    fn test_typescript_class() {
        let mut parser = TypeScriptParser::new();
        let source = r#"
export class UserService {
    private users: Map<string, string> = new Map();

    getUser(id: string): string | undefined {
        return this.users.get(id);
    }
}
"#;
        let entities = parser.extract(source, "src/user.ts");
        assert!(entities
            .iter()
            .any(|e| e.name == "UserService" && e.kind == EntityKind::Class));
    }

    #[test]
    fn test_typescript_interface() {
        let mut parser = TypeScriptParser::new();
        let source = r#"
export interface User {
    id: string;
    name: string;
    email?: string;
}
"#;
        let entities = parser.extract(source, "src/types.ts");
        assert!(entities
            .iter()
            .any(|e| e.name == "User" && e.kind == EntityKind::Interface));
    }

    #[test]
    fn test_tsx_component() {
        let mut parser = TypeScriptParser::new_tsx();
        let source = r#"
interface ButtonProps {
    label: string;
    onClick: () => void;
}

export function Button({ label, onClick }: ButtonProps) {
    return <button onClick={onClick}>{label}</button>;
}
"#;
        let entities = parser.extract(source, "components/Button.tsx");
        assert!(!entities.is_empty());
        assert!(entities.iter().any(|e| e.name == "ButtonProps"));
    }

    #[test]
    fn test_language_variant() {
        let ts = TypeScriptParser::new();
        assert_eq!(ts.language(), Language::TypeScript);

        let tsx = TypeScriptParser::new_tsx();
        assert_eq!(tsx.language(), Language::Tsx);
    }

    #[test]
    fn test_references() {
        let mut parser = TypeScriptParser::new();
        let source = r#"
import { createUser } from './user';
const user = createUser("Alice");
console.log(user.name);
"#;
        let refs = parser.extract_references(source, "src/main.ts");
        assert!(!refs.is_empty(), "Should extract references");
    }

    #[test]
    fn test_empty_source() {
        let mut parser = TypeScriptParser::new();
        let entities = parser.extract("", "empty.ts");
        assert!(entities.is_empty());
    }

    #[test]
    fn test_enum() {
        let mut parser = TypeScriptParser::new();
        let source = r#"
export enum Status {
    Active = "active",
    Inactive = "inactive",
}
"#;
        let entities = parser.extract(source, "src/types.ts");
        assert!(entities
            .iter()
            .any(|e| e.name == "Status" && e.kind == EntityKind::Enum));
    }

    #[test]
    fn test_const() {
        let mut parser = TypeScriptParser::new();
        let source = r#"
export const MAX_RETRIES = 3;
export const API_URL = "https://api.example.com";
"#;
        let entities = parser.extract(source, "src/config.ts");
        assert!(entities.iter().any(|e| e.name == "MAX_RETRIES"));
        assert!(entities.iter().any(|e| e.name == "API_URL"));
    }

    #[test]
    fn test_type_alias() {
        let mut parser = TypeScriptParser::new();
        let source = r#"
export type UserId = string;
export type Result<T> = { ok: true; value: T } | { ok: false; error: Error };
"#;
        let entities = parser.extract(source, "src/types.ts");
        assert!(entities
            .iter()
            .any(|e| e.name == "UserId" && e.kind == EntityKind::TypeAlias));
    }
}
