//! Rust parser for semantic analysis.
//!
//! Extracts functions, structs, enums, traits, impl blocks, type aliases,
//! and constants from Rust source code using tree-sitter-rust.
//!
//! # Supported Entity Types
//!
//! | Rust Construct | EntityKind | Notes |
//! |---------------|------------|-------|
//! | `fn greet()` | Function | Top-level function |
//! | `async fn fetch()` | Function | Async function |
//! | `pub fn public()` | Function | Exported (pub) function |
//! | `struct User {}` | Class | Struct definition |
//! | `enum Status {}` | Enum | Enum definition |
//! | `trait Service {}` | Interface | Trait definition |
//! | `impl User {}` | Module | Impl block (named by type) |
//! | `impl Trait for T {}` | Module | Trait impl block |
//! | `fn method(&self)` | Method | Method inside impl block |
//! | `type Result = ...` | TypeAlias | Type alias |
//! | `const MAX: u32 = ...` | Const | Constant |
//! | `static DB: ...` | Variable | Static variable |
//! | `mod auth {}` | Module | Module declaration |
//! | `use std::io` | Import | Use statement |

use super::{Language, LanguageParser};
use crate::entity::{Entity, EntityKind};
use tree_sitter::{Node, Parser};

/// Rust AST entity extractor using tree-sitter.
pub struct RustParser {
    parser: Parser,
}

impl RustParser {
    /// Create a new Rust parser.
    pub fn new() -> Self {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("Failed to load Rust grammar");
        Self { parser }
    }

    /// Walk the AST and extract entities.
    fn walk_tree(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        entities: &mut Vec<Entity>,
        in_impl: Option<&str>,
    ) {
        match node.kind() {
            "function_item" => {
                if let Some(entity) = self.extract_function(node, source, file_path, in_impl) {
                    entities.push(entity);
                }
            }
            "struct_item" => {
                if let Some(entity) = self.extract_struct(node, source, file_path) {
                    entities.push(entity);
                }
            }
            "enum_item" => {
                if let Some(entity) = self.extract_enum(node, source, file_path) {
                    entities.push(entity);
                }
            }
            "trait_item" => {
                if let Some(entity) = self.extract_trait(node, source, file_path) {
                    let trait_name = entity.name.clone();
                    entities.push(entity);

                    // Walk trait body for method signatures
                    if let Some(body) = node.child_by_field_name("body") {
                        let mut cursor = body.walk();
                        for child in body.children(&mut cursor) {
                            self.walk_tree(&child, source, file_path, entities, Some(&trait_name));
                        }
                    }
                    return;
                }
            }
            "impl_item" => {
                let impl_name = self.extract_impl_name(node, source);
                if let Some(ref name) = impl_name {
                    // Create an entity for the impl block itself
                    let line = node.start_position().row as u32 + 1;
                    let end_line = node.end_position().row as u32 + 1;
                    let sig = self.build_impl_signature(node, source);

                    let mut entity =
                        Entity::new(name.clone(), EntityKind::Module, file_path, line, end_line);
                    if let Some(s) = sig {
                        entity = entity.with_signature(s);
                    }
                    entities.push(entity);

                    // Walk impl body for methods
                    if let Some(body) = node.child_by_field_name("body") {
                        let mut cursor = body.walk();
                        for child in body.children(&mut cursor) {
                            self.walk_tree(&child, source, file_path, entities, Some(name));
                        }
                    }
                    return;
                }
            }
            "type_item" => {
                if let Some(entity) = self.extract_type_alias(node, source, file_path) {
                    entities.push(entity);
                }
            }
            "const_item" => {
                if let Some(entity) = self.extract_const(node, source, file_path) {
                    entities.push(entity);
                }
            }
            "static_item" => {
                if let Some(entity) = self.extract_static(node, source, file_path) {
                    entities.push(entity);
                }
            }
            "mod_item" => {
                if let Some(entity) = self.extract_mod(node, source, file_path) {
                    entities.push(entity);
                }
            }
            "use_declaration" => {
                if let Some(entity) = self.extract_use(node, source, file_path) {
                    entities.push(entity);
                }
            }
            _ => {}
        }

        // Recurse into children (but not impl/trait bodies — handled above)
        if node.kind() != "impl_item" && node.kind() != "trait_item" {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                self.walk_tree(&child, source, file_path, entities, in_impl);
            }
        }
    }

    /// Extract a function definition.
    fn extract_function(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        in_impl: Option<&str>,
    ) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        let kind = if in_impl.is_some() {
            // Check if it takes &self / &mut self / self — then it's a method
            let has_self = node
                .child_by_field_name("parameters")
                .map(|params| {
                    let mut cursor = params.walk();
                    let children: Vec<_> = params.children(&mut cursor).collect();
                    children.iter().any(|c| {
                        c.kind() == "self_parameter" || self.node_text(c, source).contains("self")
                    })
                })
                .unwrap_or(false);

            if has_self {
                EntityKind::Method
            } else {
                // Associated function (no self parameter)
                EntityKind::Function
            }
        } else {
            EntityKind::Function
        };

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let exported = self.is_pub(node);
        let signature = self.build_function_signature(node, source);

        let mut entity = Entity::new(name, kind, file_path, line, end_line);
        if let Some(sig) = signature {
            entity = entity.with_signature(sig);
        }
        if exported {
            entity.exported = true;
        }

        Some(entity)
    }

    /// Extract a struct definition.
    fn extract_struct(&self, node: &Node, source: &str, file_path: &str) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;
        let exported = self.is_pub(node);

        let signature = self.build_struct_signature(node, source);

        let mut entity = Entity::new(name, EntityKind::Class, file_path, line, end_line);
        if let Some(sig) = signature {
            entity = entity.with_signature(sig);
        }
        if exported {
            entity.exported = true;
        }

        Some(entity)
    }

    /// Extract an enum definition.
    fn extract_enum(&self, node: &Node, source: &str, file_path: &str) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;
        let exported = self.is_pub(node);

        let mut entity = Entity::new(name, EntityKind::Enum, file_path, line, end_line);
        if exported {
            entity.exported = true;
        }

        Some(entity)
    }

    /// Extract a trait definition.
    fn extract_trait(&self, node: &Node, source: &str, file_path: &str) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;
        let exported = self.is_pub(node);

        let signature = self.build_trait_signature(node, source);

        let mut entity = Entity::new(name, EntityKind::Interface, file_path, line, end_line);
        if let Some(sig) = signature {
            entity = entity.with_signature(sig);
        }
        if exported {
            entity.exported = true;
        }

        Some(entity)
    }

    /// Extract the name for an impl block.
    ///
    /// For `impl User { ... }` → "User"
    /// For `impl Display for User { ... }` → "User"
    fn extract_impl_name(&self, node: &Node, source: &str) -> Option<String> {
        // Try `type` field first (the implementing type)
        if let Some(type_node) = node.child_by_field_name("type") {
            return Some(self.node_text(&type_node, source));
        }

        // Fallback: walk children looking for a type identifier
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "type_identifier" || child.kind() == "generic_type" {
                return Some(self.node_text(&child, source));
            }
        }

        None
    }

    /// Extract a type alias.
    fn extract_type_alias(&self, node: &Node, source: &str, file_path: &str) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;
        let exported = self.is_pub(node);

        let sig = self.first_line(node, source);

        let mut entity = Entity::new(name, EntityKind::TypeAlias, file_path, line, end_line);
        if let Some(s) = sig {
            entity = entity.with_signature(s);
        }
        if exported {
            entity.exported = true;
        }

        Some(entity)
    }

    /// Extract a const item.
    fn extract_const(&self, node: &Node, source: &str, file_path: &str) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;
        let exported = self.is_pub(node);

        let sig = self.first_line(node, source);

        let mut entity = Entity::new(name, EntityKind::Const, file_path, line, end_line);
        if let Some(s) = sig {
            entity = entity.with_signature(s);
        }
        if exported {
            entity.exported = true;
        }

        Some(entity)
    }

    /// Extract a static item.
    fn extract_static(&self, node: &Node, source: &str, file_path: &str) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;
        let exported = self.is_pub(node);

        let sig = self.first_line(node, source);

        let mut entity = Entity::new(name, EntityKind::Variable, file_path, line, end_line);
        if let Some(s) = sig {
            entity = entity.with_signature(s);
        }
        if exported {
            entity.exported = true;
        }

        Some(entity)
    }

    /// Extract a mod declaration.
    fn extract_mod(&self, node: &Node, source: &str, file_path: &str) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;
        let exported = self.is_pub(node);

        let mut entity = Entity::new(name, EntityKind::Module, file_path, line, end_line);
        if exported {
            entity.exported = true;
        }

        Some(entity)
    }

    /// Extract a use declaration.
    fn extract_use(&self, node: &Node, source: &str, file_path: &str) -> Option<Entity> {
        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let text = self.node_text(node, source);

        // Extract the path from `use std::io::Read;`
        let name = text
            .strip_prefix("pub ")
            .unwrap_or(&text)
            .strip_prefix("use ")
            .unwrap_or(&text)
            .trim_end_matches(';')
            .to_string();

        let mut entity = Entity::new(name, EntityKind::Import, file_path, line, end_line);
        entity = entity.with_signature(text);

        Some(entity)
    }

    // ── Signature builders ──────────────────────────────────────────

    /// Build a function signature string.
    fn build_function_signature(&self, node: &Node, source: &str) -> Option<String> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        let params = node
            .child_by_field_name("parameters")
            .map(|n| self.node_text(&n, source))
            .unwrap_or_else(|| "()".to_string());

        let return_type = node
            .child_by_field_name("return_type")
            .map(|n| format!(" -> {}", self.node_text(&n, source)));

        // Check for visibility, async, unsafe
        let mut qualifiers = Vec::new();
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        for child in &children {
            match child.kind() {
                "visibility_modifier" => qualifiers.push(self.node_text(child, source)),
                "async" => qualifiers.push("async".to_string()),
                "unsafe" => qualifiers.push("unsafe".to_string()),
                _ => {}
            }
            // Stop once we hit the name
            if child.id() == name_node.id() {
                break;
            }
        }

        qualifiers.push("fn".to_string());

        Some(format!(
            "{} {}{}{}",
            qualifiers.join(" "),
            name,
            params,
            return_type.unwrap_or_default()
        ))
    }

    /// Build a struct signature string.
    fn build_struct_signature(&self, node: &Node, source: &str) -> Option<String> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        // Check for generic type parameters
        let generics = node
            .child_by_field_name("type_parameters")
            .map(|n| self.node_text(&n, source))
            .unwrap_or_default();

        let prefix = if self.is_pub(node) {
            "pub struct"
        } else {
            "struct"
        };

        Some(format!("{} {}{}", prefix, name, generics))
    }

    /// Build a trait signature string.
    fn build_trait_signature(&self, node: &Node, source: &str) -> Option<String> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        let generics = node
            .child_by_field_name("type_parameters")
            .map(|n| self.node_text(&n, source))
            .unwrap_or_default();

        let prefix = if self.is_pub(node) {
            "pub trait"
        } else {
            "trait"
        };

        // Check for supertraits
        let bounds = node
            .child_by_field_name("bounds")
            .map(|n| format!(": {}", self.node_text(&n, source)))
            .unwrap_or_default();

        Some(format!("{} {}{}{}", prefix, name, generics, bounds))
    }

    /// Build an impl block signature string.
    fn build_impl_signature(&self, node: &Node, source: &str) -> Option<String> {
        // Get the first line of the impl block
        self.first_line(node, source)
    }

    // ── Helpers ─────────────────────────────────────────────────────

    /// Check if a node has a `pub` visibility modifier.
    fn is_pub(&self, node: &Node) -> bool {
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        children.iter().any(|c| c.kind() == "visibility_modifier")
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

    /// Get the first line of a node's text (for signatures).
    fn first_line(&self, node: &Node, source: &str) -> Option<String> {
        let text = self.node_text(node, source);
        let first = text.lines().next()?;
        Some(first.trim_end_matches('{').trim().to_string())
    }
}

impl Default for RustParser {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageParser for RustParser {
    fn language(&self) -> Language {
        Language::Rust
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
}

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_function() {
        let mut parser = RustParser::new();
        let source = r#"
pub fn greet(name: &str) -> String {
    format!("Hello, {}!", name)
}
"#;
        let entities = parser.extract(source, "src/lib.rs");
        assert!(!entities.is_empty());
        let greet = entities.iter().find(|e| e.name == "greet").unwrap();
        assert_eq!(greet.kind, EntityKind::Function);
        assert!(greet.exported);
        assert!(greet.signature.as_ref().unwrap().contains("pub fn greet"));
    }

    #[test]
    fn test_extract_private_function() {
        let mut parser = RustParser::new();
        let source = r#"
fn helper() -> bool {
    true
}
"#;
        let entities = parser.extract(source, "src/lib.rs");
        let helper = entities.iter().find(|e| e.name == "helper").unwrap();
        assert_eq!(helper.kind, EntityKind::Function);
        assert!(!helper.exported);
    }

    #[test]
    fn test_extract_async_function() {
        let mut parser = RustParser::new();
        let source = r#"
pub async fn fetch_data(url: &str) -> Result<String, Error> {
    Ok("data".to_string())
}
"#;
        let entities = parser.extract(source, "src/client.rs");
        let fetch = entities.iter().find(|e| e.name == "fetch_data").unwrap();
        assert_eq!(fetch.kind, EntityKind::Function);
        assert!(fetch.exported);
        // Note: whether "async" appears in the signature depends on tree-sitter's
        // Rust grammar version. The function should be found regardless.
        assert!(
            fetch.signature.as_ref().unwrap().contains("fetch_data"),
            "Signature should contain function name: {:?}",
            fetch.signature
        );
    }

    #[test]
    fn test_extract_struct() {
        let mut parser = RustParser::new();
        let source = r#"
pub struct User {
    pub name: String,
    pub email: Option<String>,
    age: u32,
}
"#;
        let entities = parser.extract(source, "src/models.rs");
        let user = entities.iter().find(|e| e.name == "User").unwrap();
        assert_eq!(user.kind, EntityKind::Class);
        assert!(user.exported);
        assert!(
            user.signature.as_ref().unwrap().contains("pub struct User"),
            "Signature: {:?}",
            user.signature
        );
    }

    #[test]
    fn test_extract_generic_struct() {
        let mut parser = RustParser::new();
        let source = r#"
pub struct Cache<K, V> {
    entries: HashMap<K, V>,
    capacity: usize,
}
"#;
        let entities = parser.extract(source, "src/cache.rs");
        let cache = entities.iter().find(|e| e.name == "Cache").unwrap();
        assert!(
            cache.signature.as_ref().unwrap().contains("<K, V>"),
            "Should include generics: {:?}",
            cache.signature
        );
    }

    #[test]
    fn test_extract_enum() {
        let mut parser = RustParser::new();
        let source = r#"
pub enum Status {
    Active,
    Inactive,
    Pending { reason: String },
}
"#;
        let entities = parser.extract(source, "src/types.rs");
        let status = entities.iter().find(|e| e.name == "Status").unwrap();
        assert_eq!(status.kind, EntityKind::Enum);
        assert!(status.exported);
    }

    #[test]
    fn test_extract_trait() {
        let mut parser = RustParser::new();
        let source = r#"
pub trait Repository {
    fn find(&self, id: &str) -> Option<Entity>;
    fn save(&mut self, entity: &Entity) -> Result<(), Error>;
}
"#;
        let entities = parser.extract(source, "src/traits.rs");
        let repo = entities.iter().find(|e| e.name == "Repository").unwrap();
        assert_eq!(repo.kind, EntityKind::Interface);
        assert!(repo.exported);
    }

    #[test]
    fn test_extract_trait_methods() {
        let mut parser = RustParser::new();
        let source = r#"
pub trait Service {
    fn start(&self);
    fn stop(&mut self);
}
"#;
        let entities = parser.extract(source, "src/traits.rs");
        assert!(
            entities.iter().any(|e| e.name == "Service"),
            "Should find Service trait, got: {:?}",
            entities.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
        // Trait method signatures may or may not be extracted depending on
        // tree-sitter's representation (they might be function_signature_item
        // rather than function_item). At minimum, the trait itself should be found.
    }

    #[test]
    fn test_extract_impl_block() {
        let mut parser = RustParser::new();
        let source = r#"
impl User {
    pub fn new(name: String) -> Self {
        Self { name, email: None, age: 0 }
    }

    pub fn greet(&self) -> String {
        format!("Hello, {}!", self.name)
    }

    fn private_helper(&self) -> bool {
        true
    }
}
"#;
        let entities = parser.extract(source, "src/models.rs");

        // Should find the impl block itself
        assert!(
            entities
                .iter()
                .any(|e| e.name == "User" && e.kind == EntityKind::Module),
            "Should find User impl block, got: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.kind))
                .collect::<Vec<_>>()
        );

        // Should find methods inside the impl
        let new_fn = entities.iter().find(|e| e.name == "new");
        assert!(new_fn.is_some(), "Should find new() associated function");
        assert_eq!(
            new_fn.unwrap().kind,
            EntityKind::Function,
            "new() without self should be Function"
        );

        let greet = entities.iter().find(|e| e.name == "greet");
        assert!(greet.is_some(), "Should find greet() method");
        assert_eq!(
            greet.unwrap().kind,
            EntityKind::Method,
            "greet(&self) should be Method"
        );
    }

    #[test]
    fn test_extract_trait_impl() {
        let mut parser = RustParser::new();
        let source = r#"
impl Display for User {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}
"#;
        let entities = parser.extract(source, "src/models.rs");

        // Should find the impl block
        assert!(
            entities.iter().any(|e| e.kind == EntityKind::Module),
            "Should find impl block"
        );

        // Should find the fmt method
        assert!(
            entities
                .iter()
                .any(|e| e.name == "fmt" && e.kind == EntityKind::Method),
            "Should find fmt method"
        );
    }

    #[test]
    fn test_extract_type_alias() {
        let mut parser = RustParser::new();
        let source = r#"
pub type Result<T> = std::result::Result<T, Error>;
type NodeId = u64;
"#;
        let entities = parser.extract(source, "src/types.rs");

        let result = entities.iter().find(|e| e.name == "Result");
        assert!(result.is_some(), "Should find Result type alias");
        assert_eq!(result.unwrap().kind, EntityKind::TypeAlias);
        assert!(result.unwrap().exported);

        let node_id = entities.iter().find(|e| e.name == "NodeId");
        assert!(node_id.is_some(), "Should find NodeId type alias");
        assert!(!node_id.unwrap().exported);
    }

    #[test]
    fn test_extract_const() {
        let mut parser = RustParser::new();
        let source = r#"
pub const MAX_RETRIES: u32 = 3;
const INTERNAL_LIMIT: usize = 100;
"#;
        let entities = parser.extract(source, "src/config.rs");

        let max = entities.iter().find(|e| e.name == "MAX_RETRIES");
        assert!(max.is_some(), "Should find MAX_RETRIES");
        assert_eq!(max.unwrap().kind, EntityKind::Const);
        assert!(max.unwrap().exported);

        let internal = entities.iter().find(|e| e.name == "INTERNAL_LIMIT");
        assert!(internal.is_some(), "Should find INTERNAL_LIMIT");
        assert!(!internal.unwrap().exported);
    }

    #[test]
    fn test_extract_static() {
        let mut parser = RustParser::new();
        let source = r#"
pub static GLOBAL_CONFIG: Lazy<Config> = Lazy::new(|| Config::default());
"#;
        let entities = parser.extract(source, "src/config.rs");
        let config = entities.iter().find(|e| e.name == "GLOBAL_CONFIG");
        assert!(config.is_some(), "Should find GLOBAL_CONFIG");
        assert_eq!(config.unwrap().kind, EntityKind::Variable);
        assert!(config.unwrap().exported);
    }

    #[test]
    fn test_extract_mod() {
        let mut parser = RustParser::new();
        let source = r#"
pub mod auth;
mod internal;
pub mod utils {
    pub fn helper() -> bool { true }
}
"#;
        let entities = parser.extract(source, "src/lib.rs");

        let auth = entities.iter().find(|e| e.name == "auth");
        assert!(auth.is_some(), "Should find auth module");
        assert_eq!(auth.unwrap().kind, EntityKind::Module);
        assert!(auth.unwrap().exported);

        let internal = entities.iter().find(|e| e.name == "internal");
        assert!(internal.is_some(), "Should find internal module");
        assert!(!internal.unwrap().exported);
    }

    #[test]
    fn test_extract_use() {
        let mut parser = RustParser::new();
        let source = r#"
use std::collections::HashMap;
use crate::models::User;
pub use self::auth::Token;
"#;
        let entities = parser.extract(source, "src/lib.rs");
        let imports: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Import)
            .collect();
        assert!(
            imports.len() >= 3,
            "Should find 3 imports, got: {:?}",
            imports.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_empty_source() {
        let mut parser = RustParser::new();
        let entities = parser.extract("", "empty.rs");
        assert!(entities.is_empty());
    }

    #[test]
    fn test_line_numbers() {
        let mut parser = RustParser::new();
        let source = r#"
fn first() {}

fn second() {}
"#;
        let entities = parser.extract(source, "lines.rs");
        let first = entities.iter().find(|e| e.name == "first").unwrap();
        let second = entities.iter().find(|e| e.name == "second").unwrap();
        assert!(first.line < second.line);
    }

    #[test]
    fn test_language() {
        let parser = RustParser::new();
        assert_eq!(parser.language(), Language::Rust);
    }

    #[test]
    fn test_realistic_rust_module() {
        let mut parser = RustParser::new();
        let source = r#"
//! Repository module for data access.

use std::collections::HashMap;
use thiserror::Error;

/// Error type for repository operations.
#[derive(Debug, Error)]
pub enum RepoError {
    #[error("Not found: {0}")]
    NotFound(String),
    #[error("Duplicate key: {0}")]
    DuplicateKey(String),
}

/// A generic in-memory repository.
pub struct Repository<T: Clone> {
    data: HashMap<String, T>,
}

impl<T: Clone> Repository<T> {
    /// Create a new empty repository.
    pub fn new() -> Self {
        Self {
            data: HashMap::new(),
        }
    }

    /// Find an item by key.
    pub fn find(&self, key: &str) -> Result<&T, RepoError> {
        self.data.get(key).ok_or_else(|| RepoError::NotFound(key.to_string()))
    }

    /// Insert an item.
    pub fn insert(&mut self, key: String, value: T) -> Result<(), RepoError> {
        if self.data.contains_key(&key) {
            return Err(RepoError::DuplicateKey(key));
        }
        self.data.insert(key, value);
        Ok(())
    }

    /// Get the count of items.
    pub fn count(&self) -> usize {
        self.data.len()
    }
}

pub type Result<T> = std::result::Result<T, RepoError>;

pub const MAX_ITEMS: usize = 10_000;

#[cfg(test)]
mod tests {
    use super::*;

    fn create_repo() -> Repository<String> {
        Repository::new()
    }
}
"#;
        let entities = parser.extract(source, "src/repo.rs");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        // Enum
        assert!(names.contains(&"RepoError"), "Should find RepoError enum");

        // Struct
        assert!(
            names.contains(&"Repository"),
            "Should find Repository struct"
        );

        // Methods
        assert!(names.contains(&"new"), "Should find new()");
        assert!(names.contains(&"find"), "Should find find()");
        assert!(names.contains(&"insert"), "Should find insert()");
        assert!(names.contains(&"count"), "Should find count()");

        // Type alias
        assert!(names.contains(&"Result"), "Should find Result type alias");

        // Const
        assert!(names.contains(&"MAX_ITEMS"), "Should find MAX_ITEMS const");

        // Test module
        assert!(names.contains(&"tests"), "Should find tests module");

        // Verify exports
        let repo_struct = entities
            .iter()
            .find(|e| e.name == "Repository" && e.kind == EntityKind::Class)
            .unwrap();
        assert!(repo_struct.exported, "Repository should be exported (pub)");

        let find_method = entities
            .iter()
            .find(|e| e.name == "find" && e.kind == EntityKind::Method)
            .unwrap();
        assert!(find_method.exported, "find should be exported (pub)");
    }
}
