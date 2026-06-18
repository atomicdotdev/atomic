//! Swift parser for semantic analysis.
//!
//! Extracts functions, classes, structs, enums, protocols, methods, initializers,
//! properties, imports, and type aliases from Swift source code using
//! tree-sitter-swift.
//!
//! # Supported Entity Types
//!
//! | Swift Construct | EntityKind | Notes |
//! |----------------|------------|-------|
//! | `func greet()` | Function | Top-level function |
//! | `class User {}` | Class | Class declaration |
//! | `struct Point {}` | Class | Struct (mapped to Class, like Go) |
//! | `enum Direction {}` | Enum | Enum declaration |
//! | `protocol Drawable {}` | Interface | Protocol declaration |
//! | `func method()` in type | Method | Method inside class/struct/enum/extension |
//! | `init()` in type | Method | Initializer (name = "init") |
//! | `var x = 42` top-level | Variable | Top-level property |
//! | `import Foundation` | Import | Import statement |
//! | `typealias ID = UUID` | TypeAlias | Type alias |
//!
//! # Visibility Rules
//!
//! Swift items are `internal` by default (visible within the module).
//! This parser treats `internal` as exported since module-level visibility
//! is the closest analogue to "public API" in most Swift projects.
//!
//! | Modifier | `exported` |
//! |----------|-----------|
//! | `public` / `open` | `true` |
//! | (none — internal) | `true` |
//! | `private` / `fileprivate` | `false` |

use super::{Language, LanguageParser};
use crate::entity::{Confidence, Entity, EntityKind, Reference};
use tree_sitter::{Node, Parser};

/// Swift AST entity extractor using tree-sitter.
pub struct SwiftParser {
    parser: Parser,
}

impl SwiftParser {
    /// Create a new Swift parser.
    pub fn new() -> Self {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_swift::LANGUAGE.into())
            .expect("Failed to load Swift grammar");
        Self { parser }
    }

    /// Walk the AST and extract entities.
    ///
    /// `in_type` tracks the enclosing type name (class, struct, enum, protocol,
    /// or extension) so that nested `function_declaration` nodes are emitted as
    /// `Method` rather than `Function`.
    fn walk_tree(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        entities: &mut Vec<Entity>,
        in_type: Option<&str>,
    ) {
        match node.kind() {
            "function_declaration" | "protocol_function_declaration" => {
                if let Some(entity) = self.extract_function(node, source, file_path, in_type) {
                    entities.push(entity);
                }
            }
            // tree-sitter-swift uses `class_declaration` for class, struct,
            // AND enum — differentiated by the keyword child node.
            "class_declaration" => {
                let kind = self.detect_type_kind(node);
                if let Some(entity) = self.extract_type_declaration(node, source, file_path, kind) {
                    let type_name = entity.name.clone();
                    entities.push(entity);

                    // Walk the body for methods.  The body node kind varies:
                    //   class/struct → class_body
                    //   enum         → enum_class_body
                    if let Some(body) = self.find_type_body(node) {
                        let mut cursor = body.walk();
                        for child in body.children(&mut cursor) {
                            self.walk_tree(&child, source, file_path, entities, Some(&type_name));
                        }
                    }
                    return;
                }
            }
            "protocol_declaration" => {
                if let Some(entity) =
                    self.extract_type_declaration(node, source, file_path, EntityKind::Interface)
                {
                    let type_name = entity.name.clone();
                    entities.push(entity);

                    if let Some(body) = self.find_type_body(node) {
                        let mut cursor = body.walk();
                        for child in body.children(&mut cursor) {
                            self.walk_tree(&child, source, file_path, entities, Some(&type_name));
                        }
                    }
                    return;
                }
            }
            "extension_declaration" => {
                // Extensions don't produce their own entity, but functions
                // inside them become Method with the extended type as context.
                let type_name = self.find_type_identifier(node, source);

                if let Some(body) = self.find_type_body(node) {
                    let mut cursor = body.walk();
                    for child in body.children(&mut cursor) {
                        self.walk_tree(&child, source, file_path, entities, type_name.as_deref());
                    }
                }
                return;
            }
            "init_declaration" => {
                if let Some(entity) = self.extract_init(node, source, file_path) {
                    entities.push(entity);
                }
            }
            // Top-level properties only — properties inside types are skipped
            "property_declaration" if in_type.is_none() => {
                if let Some(entity) = self.extract_property(node, source, file_path) {
                    entities.push(entity);
                }
            }
            "import_declaration" => {
                if let Some(entity) = self.extract_import(node, source, file_path) {
                    entities.push(entity);
                }
            }
            "typealias_declaration" => {
                if let Some(entity) = self.extract_typealias(node, source, file_path) {
                    entities.push(entity);
                }
            }
            _ => {}
        }

        // Recurse into children (type bodies are handled above and return early)
        if !matches!(
            node.kind(),
            "class_declaration" | "protocol_declaration" | "extension_declaration"
        ) {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                self.walk_tree(&child, source, file_path, entities, in_type);
            }
        }
    }

    /// Detect whether a `class_declaration` node is a class, struct, or enum
    /// by inspecting its keyword child.
    fn detect_type_kind(&self, node: &Node) -> EntityKind {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "enum" => return EntityKind::Enum,
                "struct" => return EntityKind::Class, // struct → Class (like Go)
                "class" => return EntityKind::Class,
                _ => {}
            }
        }
        EntityKind::Class // fallback
    }

    /// Find the body node inside a type declaration.
    ///
    /// The body kind varies by type:
    ///   class/struct → `class_body`
    ///   enum         → `enum_class_body`
    ///   protocol     → `protocol_body`
    fn find_type_body<'a>(&self, node: &'a Node<'a>) -> Option<Node<'a>> {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "class_body" | "enum_class_body" | "protocol_body" => return Some(child),
                _ => {}
            }
        }
        None
    }

    /// Find the `type_identifier` child of a declaration node.
    fn find_type_identifier(&self, node: &Node, source: &str) -> Option<String> {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "type_identifier" || child.kind() == "user_type" {
                return Some(self.node_text(&child, source));
            }
        }
        None
    }

    // ── Extract helpers ─────────────────────────────────────────────

    /// Extract a function or method declaration.
    fn extract_function(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        in_type: Option<&str>,
    ) -> Option<Entity> {
        // Try named field first, fall back to first `simple_identifier` child
        let name = if let Some(n) = node.child_by_field_name("name") {
            self.node_text(&n, source)
        } else {
            self.find_first_simple_identifier(node, source)?
        };

        let kind = if in_type.is_some() {
            EntityKind::Method
        } else {
            EntityKind::Function
        };

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let exported = self.is_exported(node, source);
        let signature = self.build_function_signature(node, source);

        let mut entity = Entity::new(name, kind, file_path, line, end_line);
        if let Some(sig) = signature {
            entity = entity.with_signature(sig);
        }
        entity.exported = exported;

        Some(entity)
    }

    /// Extract an `init` declaration.
    ///
    /// Initializers have no explicit name — we use `"init"` as the entity name.
    fn extract_init(&self, node: &Node, source: &str, file_path: &str) -> Option<Entity> {
        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let exported = self.is_exported(node, source);
        let signature = self.build_init_signature(node, source);

        let mut entity = Entity::new("init", EntityKind::Method, file_path, line, end_line);
        if let Some(sig) = signature {
            entity = entity.with_signature(sig);
        }
        entity.exported = exported;

        Some(entity)
    }

    /// Extract a type declaration (class, struct, enum, protocol).
    ///
    /// The name is found via the `type_identifier` child (not a named field),
    /// and the keyword is detected from the first keyword child node.
    fn extract_type_declaration(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        kind: EntityKind,
    ) -> Option<Entity> {
        let name = self.find_type_identifier(node, source)?;

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let exported = self.is_exported(node, source);

        // Detect the keyword from the actual child node, not from EntityKind.
        // This handles `class_declaration` being used for struct and enum too.
        let keyword = self.detect_keyword(node).unwrap_or(match kind {
            EntityKind::Interface => "protocol",
            EntityKind::Enum => "enum",
            _ => "class",
        });
        let signature = format!("{} {}", keyword, name);

        let mut entity = Entity::new(name, kind, file_path, line, end_line);
        entity = entity.with_signature(signature);
        entity.exported = exported;

        Some(entity)
    }

    /// Find the keyword child of a declaration node ("class", "struct", "enum", "protocol").
    fn detect_keyword<'a>(&self, node: &'a Node<'a>) -> Option<&'static str> {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "class" => return Some("class"),
                "struct" => return Some("struct"),
                "enum" => return Some("enum"),
                "protocol" => return Some("protocol"),
                _ => {}
            }
        }
        None
    }

    /// Extract a top-level property declaration (`var` / `let`).
    fn extract_property(&self, node: &Node, source: &str, file_path: &str) -> Option<Entity> {
        let name = self.find_property_name(node, source)?;

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let exported = self.is_exported(node, source);

        // Use the first line as the signature
        let full_text = self.node_text(node, source);
        let signature = full_text.lines().next().map(|l| l.trim().to_string());

        let mut entity = Entity::new(name, EntityKind::Variable, file_path, line, end_line);
        if let Some(sig) = signature {
            entity = entity.with_signature(sig);
        }
        entity.exported = exported;

        Some(entity)
    }

    /// Extract an import declaration.
    fn extract_import(&self, node: &Node, source: &str, file_path: &str) -> Option<Entity> {
        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let text = self.node_text(node, source);

        // Try the grammar's "name" field first, fall back to text parsing
        let name = node
            .child_by_field_name("name")
            .map(|n| self.node_text(&n, source))
            .unwrap_or_else(|| {
                text.strip_prefix("import ")
                    .unwrap_or(&text)
                    .trim()
                    .to_string()
            });

        let mut entity = Entity::new(name, EntityKind::Import, file_path, line, end_line);
        entity = entity.with_signature(text.trim().to_string());

        Some(entity)
    }

    /// Extract a typealias declaration.
    fn extract_typealias(&self, node: &Node, source: &str, file_path: &str) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let exported = self.is_exported(node, source);

        let full_text = self.node_text(node, source);
        let signature = full_text.lines().next().map(|l| l.trim().to_string());

        let mut entity = Entity::new(name, EntityKind::TypeAlias, file_path, line, end_line);
        if let Some(sig) = signature {
            entity = entity.with_signature(sig);
        }
        entity.exported = exported;

        Some(entity)
    }

    // ── Visibility ──────────────────────────────────────────────────

    /// Determine if a declaration is exported (publicly visible).
    ///
    /// Walks the immediate children of the declaration node looking for
    /// access-level modifiers. Stops searching once it hits the main
    /// declaration keyword (`func`, `class`, etc.) to avoid scanning the body.
    ///
    /// Returns `true` by default (Swift `internal` = module-visible).
    fn is_exported(&self, node: &Node, source: &str) -> bool {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            let kind = child.kind();

            // Check a `modifiers` wrapper node (tree-sitter-swift groups
            // attributes and access-level modifiers under one node).
            if kind == "modifiers" {
                let mut inner = child.walk();
                for modifier in child.children(&mut inner) {
                    let text = self.node_text(&modifier, source);
                    if text.starts_with("private") || text.starts_with("fileprivate") {
                        return false;
                    }
                    if text.starts_with("public") || text.starts_with("open") {
                        return true;
                    }
                }
                continue;
            }

            // Direct access_level_modifier child (some grammar versions)
            if kind == "access_level_modifier" || kind == "visibility_modifier" {
                let text = self.node_text(&child, source);
                if text.starts_with("private") || text.starts_with("fileprivate") {
                    return false;
                }
                if text.starts_with("public") || text.starts_with("open") {
                    return true;
                }
                continue;
            }

            // Stop at the main declaration keyword so we don't scan the body
            let text = self.node_text(&child, source);
            match text.as_str() {
                "func" | "class" | "struct" | "enum" | "protocol" | "import" | "typealias"
                | "init" | "var" | "let" | "extension" | "actor" => break,
                _ => {}
            }
        }
        // Default: internal = module-visible = exported
        true
    }

    // ── Signature builders ──────────────────────────────────────────

    /// Build `func name(params) -> ReturnType`.
    fn build_function_signature(&self, node: &Node, source: &str) -> Option<String> {
        // Try named field, fall back to first simple_identifier
        let name = if let Some(n) = node.child_by_field_name("name") {
            self.node_text(&n, source)
        } else {
            self.find_first_simple_identifier(node, source)?
        };

        // Try named field for params, fall back to first parenthesized group
        let params = if let Some(p) = node.child_by_field_name("parameters") {
            self.node_text(&p, source)
        } else {
            let mut found = "()".to_string();
            let mut c = node.walk();
            for child in node.children(&mut c) {
                let text = self.node_text(&child, source);
                if text.starts_with('(') {
                    found = text;
                    break;
                }
            }
            found
        };

        // function_result is a child node (not a named field) containing `-> Type`
        let mut return_type: Option<String> = None;
        let mut c = node.walk();
        for child in node.children(&mut c) {
            if child.kind() == "function_result" {
                return_type = Some(format!(" {}", self.node_text(&child, source).trim()));
                break;
            }
        }

        Some(format!(
            "func {}{}{}",
            name,
            params,
            return_type.unwrap_or_default()
        ))
    }

    /// Build `init(params)` (with optional `?`/`!` for failable initializers).
    fn build_init_signature(&self, node: &Node, source: &str) -> Option<String> {
        let params = node
            .child_by_field_name("parameters")
            .map(|n| self.node_text(&n, source))
            .unwrap_or_else(|| "()".to_string());

        // Check for failable init marker (? or !)
        let mut failable = String::new();
        let mut c = node.walk();
        for child in node.children(&mut c) {
            let text = self.node_text(&child, source);
            if text == "?" || text == "!" {
                failable = text;
                break;
            }
        }

        Some(format!("init{}{}", failable, params))
    }

    // ── Helpers ─────────────────────────────────────────────────────

    /// Find the name of a property declaration.
    ///
    /// Property declarations in Swift have varying tree structures depending
    /// on the grammar version. We try the `name` field first, then search for
    /// the first `simple_identifier` after the `var`/`let` keyword.
    fn find_property_name(&self, node: &Node, source: &str) -> Option<String> {
        // Strategy 1: named field
        if let Some(name_node) = node.child_by_field_name("name") {
            return Some(self.node_text(&name_node, source));
        }

        // Strategy 2: first simple_identifier after the binding keyword
        let mut past_keyword = false;
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            let text = self.node_text(&child, source);
            if text == "var" || text == "let" {
                past_keyword = true;
                continue;
            }
            if past_keyword {
                if child.kind() == "simple_identifier" {
                    return Some(text);
                }
                // Search inside binding patterns
                if let Some(name) =
                    self.find_first_child_of_kind(&child, "simple_identifier", source)
                {
                    return Some(name);
                }
            }
        }
        None
    }

    /// Recursively find the first descendant node of a given kind.
    fn find_first_child_of_kind(&self, node: &Node, kind: &str, source: &str) -> Option<String> {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == kind {
                return Some(self.node_text(&child, source));
            }
            if let Some(found) = self.find_first_child_of_kind(&child, kind, source) {
                return Some(found);
            }
        }
        None
    }

    /// Find the first `simple_identifier` child of a node.
    fn find_first_simple_identifier(&self, node: &Node, source: &str) -> Option<String> {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "simple_identifier" {
                return Some(self.node_text(&child, source));
            }
        }
        None
    }

    /// Get the text content of a node.
    fn node_text(&self, node: &Node, source: &str) -> String {
        let start = node.start_byte();
        let end = node.end_byte();
        if end <= source.len() {
            source[start..end].to_string()
        } else {
            String::new()
        }
    }

    fn walk_tree_for_calls(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        refs: &mut Vec<Reference>,
    ) {
        if node.kind() == "call_expression" {
            // In tree-sitter-swift the callee is the first child (no named field)
            if let Some(func_node) = node.child(0) {
                if let Some(name) = self.extract_call_name(&func_node, source) {
                    if !name.is_empty() {
                        refs.push(Reference {
                            symbol: name,
                            file: file_path.to_string(),
                            line: (node.start_position().row as u32) + 1,
                            column: Some(node.start_position().column as u32),
                            context: None,
                            confidence: Confidence::High,
                        });
                    }
                }
            }
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk_tree_for_calls(&child, source, file_path, refs);
        }
    }

    fn extract_call_name(&self, func_node: &Node, source: &str) -> Option<String> {
        match func_node.kind() {
            "simple_identifier" => {
                let name = self.node_text(func_node, source);
                if name.is_empty() {
                    None
                } else {
                    Some(name)
                }
            }
            "navigation_expression" => {
                // obj.method() — find the last navigation_suffix to get the method name
                for i in (0..func_node.child_count()).rev() {
                    if let Some(suffix) = func_node.child(i) {
                        if suffix.kind() == "navigation_suffix" {
                            for j in 0..suffix.child_count() {
                                if let Some(name_node) = suffix.child(j) {
                                    if name_node.kind() == "simple_identifier" {
                                        let name = self.node_text(&name_node, source);
                                        if !name.is_empty() {
                                            return Some(name);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                None
            }
            _ => None,
        }
    }
}

impl Default for SwiftParser {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageParser for SwiftParser {
    fn language(&self) -> Language {
        Language::Swift
    }

    fn extract(&mut self, source: &str, file_path: &str) -> Vec<Entity> {
        let tree = match self.parser.parse(source, None) {
            Some(t) => t,
            None => return vec![],
        };

        let mut entities = Vec::new();
        self.walk_tree(&tree.root_node(), source, file_path, &mut entities, None);
        entities
    }

    fn extract_references(&mut self, source: &str, file_path: &str) -> Vec<Reference> {
        let tree = match self.parser.parse(source, None) {
            Some(t) => t,
            None => return vec![],
        };
        let mut refs = Vec::new();
        self.walk_tree_for_calls(&tree.root_node(), source, file_path, &mut refs);
        refs
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_function() {
        let mut parser = SwiftParser::new();
        let source = r#"
func greet(name: String) -> String {
    return "Hello, \(name)!"
}
"#;
        let entities = parser.extract(source, "main.swift");
        assert!(!entities.is_empty());
        let greet = entities.iter().find(|e| e.name == "greet").unwrap();
        assert_eq!(greet.kind, EntityKind::Function);
        assert!(greet.exported);
        assert!(
            greet.signature.as_ref().unwrap().contains("func greet"),
            "Signature: {:?}",
            greet.signature
        );
    }

    #[test]
    fn test_extract_class() {
        let mut parser = SwiftParser::new();
        let source = r#"
class UserService {
    func getUser(id: String) -> String {
        return id
    }

    func createUser(name: String) -> String {
        return name
    }
}
"#;
        let entities = parser.extract(source, "service.swift");

        let service = entities
            .iter()
            .find(|e| e.name == "UserService")
            .expect("Should find UserService");
        assert_eq!(service.kind, EntityKind::Class);
        assert!(service.exported);
        assert!(
            service
                .signature
                .as_ref()
                .unwrap()
                .contains("class UserService"),
            "Signature: {:?}",
            service.signature
        );

        let get_user = entities.iter().find(|e| e.name == "getUser");
        assert!(get_user.is_some(), "Should find getUser method");
        assert_eq!(get_user.unwrap().kind, EntityKind::Method);

        let create_user = entities.iter().find(|e| e.name == "createUser");
        assert!(create_user.is_some(), "Should find createUser method");
        assert_eq!(create_user.unwrap().kind, EntityKind::Method);
    }

    #[test]
    fn test_extract_struct() {
        let mut parser = SwiftParser::new();
        let source = r#"
struct Point {
    var x: Double
    var y: Double

    func distance(to other: Point) -> Double {
        let dx = x - other.x
        let dy = y - other.y
        return (dx * dx + dy * dy).squareRoot()
    }
}
"#;
        let entities = parser.extract(source, "geometry.swift");

        let point = entities
            .iter()
            .find(|e| e.name == "Point")
            .expect("Should find Point");
        assert_eq!(point.kind, EntityKind::Class); // Structs map to Class
        assert!(
            point.signature.as_ref().unwrap().contains("struct Point"),
            "Signature should say 'struct': {:?}",
            point.signature
        );

        let distance = entities.iter().find(|e| e.name == "distance");
        assert!(distance.is_some(), "Should find distance method");
        assert_eq!(distance.unwrap().kind, EntityKind::Method);
    }

    #[test]
    fn test_extract_enum() {
        let mut parser = SwiftParser::new();
        let source = r#"
enum Direction {
    case north
    case south
    case east
    case west
}
"#;
        let entities = parser.extract(source, "types.swift");
        let dir = entities
            .iter()
            .find(|e| e.name == "Direction")
            .expect("Should find Direction");
        assert_eq!(dir.kind, EntityKind::Enum);
        assert!(dir.exported);
        assert!(
            dir.signature.as_ref().unwrap().contains("enum Direction"),
            "Signature: {:?}",
            dir.signature
        );
    }

    #[test]
    fn test_extract_protocol() {
        let mut parser = SwiftParser::new();
        let source = r#"
protocol Drawable {
    func draw()
    var color: String { get }
}
"#;
        let entities = parser.extract(source, "protocols.swift");
        let drawable = entities
            .iter()
            .find(|e| e.name == "Drawable")
            .expect("Should find Drawable");
        assert_eq!(drawable.kind, EntityKind::Interface);
        assert!(drawable.exported);
        assert!(
            drawable
                .signature
                .as_ref()
                .unwrap()
                .contains("protocol Drawable"),
            "Signature: {:?}",
            drawable.signature
        );

        // Protocol methods should be extracted as Method
        let draw = entities.iter().find(|e| e.name == "draw");
        assert!(draw.is_some(), "Should find draw method in protocol");
        assert_eq!(draw.unwrap().kind, EntityKind::Method);
    }

    #[test]
    fn test_extract_import() {
        let mut parser = SwiftParser::new();
        let source = r#"
import Foundation
import UIKit
"#;
        let entities = parser.extract(source, "app.swift");
        let imports: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Import)
            .collect();
        assert!(
            imports.len() >= 2,
            "Should find at least 2 imports, got: {:?}",
            imports.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_extract_method_in_class() {
        let mut parser = SwiftParser::new();
        let source = r#"
class Calculator {
    func add(a: Int, b: Int) -> Int {
        return a + b
    }
}
"#;
        let entities = parser.extract(source, "calc.swift");

        let calc = entities
            .iter()
            .find(|e| e.name == "Calculator")
            .expect("Should find Calculator");
        assert_eq!(calc.kind, EntityKind::Class);

        let add = entities
            .iter()
            .find(|e| e.name == "add")
            .expect("Should find add");
        assert_eq!(
            add.kind,
            EntityKind::Method,
            "Function inside a class should be Method"
        );
        assert!(add.exported);
    }

    #[test]
    fn test_extract_init() {
        let mut parser = SwiftParser::new();
        let source = r#"
class User {
    var name: String

    init(name: String) {
        self.name = name
    }
}
"#;
        let entities = parser.extract(source, "user.swift");

        let init = entities
            .iter()
            .find(|e| e.name == "init")
            .expect("Should find init");
        assert_eq!(init.kind, EntityKind::Method);
        assert!(
            init.signature.as_ref().unwrap().contains("init"),
            "Signature: {:?}",
            init.signature
        );
    }

    #[test]
    fn test_empty_source() {
        let mut parser = SwiftParser::new();
        let entities = parser.extract("", "empty.swift");
        assert!(entities.is_empty());
    }

    #[test]
    fn test_language() {
        let parser = SwiftParser::new();
        assert_eq!(parser.language(), Language::Swift);
    }

    #[test]
    fn test_realistic_swift_app() {
        let mut parser = SwiftParser::new();
        let source = r#"
import SwiftUI
import Combine

struct ContentView: View {
    @State private var items: [Item] = []
    @State private var showingAddSheet = false

    var body: some View {
        NavigationView {
            List(items) { item in
                ItemRow(item: item)
            }
            .navigationTitle("My Items")
            .toolbar {
                Button(action: { showingAddSheet = true }) {
                    Image(systemName: "plus")
                }
            }
        }
    }
}

struct ItemRow: View {
    let item: Item

    var body: some View {
        HStack {
            Text(item.name)
            Spacer()
            Text(item.date, style: .date)
        }
    }
}

class ItemStore: ObservableObject {
    @Published var items: [Item] = []

    init() {
        loadItems()
    }

    func loadItems() {
        items = []
    }

    func addItem(_ item: Item) {
        items.append(item)
    }

    private func saveItems() {
        // persist to disk
    }
}

protocol ItemProvider {
    func fetchItems() async throws -> [Item]
    func saveItem(_ item: Item) async throws
}

enum AppError: Error {
    case networkError
    case decodingError
    case unauthorized
}

typealias ItemID = UUID
"#;
        let entities = parser.extract(source, "App.swift");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        // Imports
        assert!(
            entities.iter().any(|e| e.kind == EntityKind::Import),
            "Should find import statements"
        );

        // Structs (mapped to Class)
        assert!(
            names.contains(&"ContentView"),
            "Should find ContentView struct"
        );
        assert!(names.contains(&"ItemRow"), "Should find ItemRow struct");

        // Class
        assert!(names.contains(&"ItemStore"), "Should find ItemStore class");

        // Protocol (mapped to Interface)
        assert!(
            names.contains(&"ItemProvider"),
            "Should find ItemProvider protocol"
        );

        // Enum
        assert!(names.contains(&"AppError"), "Should find AppError enum");

        // Methods inside ItemStore
        assert!(names.contains(&"loadItems"), "Should find loadItems method");
        assert!(names.contains(&"addItem"), "Should find addItem method");

        // Init
        assert!(names.contains(&"init"), "Should find init method");

        // Verify entity kinds
        let content_view = entities.iter().find(|e| e.name == "ContentView").unwrap();
        assert_eq!(content_view.kind, EntityKind::Class);
        assert!(
            content_view
                .signature
                .as_ref()
                .unwrap()
                .contains("struct ContentView"),
            "Signature: {:?}",
            content_view.signature
        );

        let provider = entities.iter().find(|e| e.name == "ItemProvider").unwrap();
        assert_eq!(provider.kind, EntityKind::Interface);

        let app_error = entities.iter().find(|e| e.name == "AppError").unwrap();
        assert_eq!(app_error.kind, EntityKind::Enum);

        // Verify reasonable entity count
        assert!(
            entities.len() >= 8,
            "Expected >= 8 entities for a realistic SwiftUI app, got {}",
            entities.len()
        );
    }
}
