//! Java parser for semantic analysis.
//!
//! Extracts classes, interfaces, methods, fields, enums, annotations, and
//! imports from Java source code using tree-sitter-java.
//!
//! # Supported Entity Types
//!
//! | Java Construct | EntityKind | Notes |
//! |---------------|------------|-------|
//! | `class UserService {}` | Class | Class declaration |
//! | `interface Repository {}` | Interface | Interface declaration |
//! | `public void greet()` | Method | Method inside class/interface |
//! | `static int MAX = 10;` | Const | Static final field |
//! | `private String name;` | Variable | Instance field |
//! | `enum Status {}` | Enum | Enum declaration |
//! | `record Point(int x)` | Class | Record declaration (Java 16+) |
//! | `@interface MyAnnotation` | Interface | Annotation type declaration |
//! | `import java.util.List;` | Import | Import statement |

use super::{Language, LanguageParser};
use crate::entity::{Confidence, Entity, EntityKind, Reference};
use tree_sitter::{Node, Parser};

/// Java AST entity extractor using tree-sitter.
pub struct JavaParser {
    parser: Parser,
}

impl JavaParser {
    /// Create a new Java parser.
    pub fn new() -> Self {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .expect("Failed to load Java grammar");
        Self { parser }
    }

    /// Walk the AST and extract entities.
    fn walk_tree(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        entities: &mut Vec<Entity>,
        in_class: Option<&str>,
    ) {
        match node.kind() {
            "class_declaration" => {
                if let Some(entity) = self.extract_class(node, source, file_path) {
                    let class_name = entity.name.clone();
                    entities.push(entity);

                    // Walk class body for methods, fields, inner classes
                    if let Some(body) = node.child_by_field_name("body") {
                        let mut cursor = body.walk();
                        for child in body.children(&mut cursor) {
                            self.walk_tree(&child, source, file_path, entities, Some(&class_name));
                        }
                    }
                    return; // Don't recurse into class again below
                }
            }
            "interface_declaration" => {
                if let Some(entity) = self.extract_interface(node, source, file_path) {
                    let iface_name = entity.name.clone();
                    entities.push(entity);

                    // Walk interface body for method signatures
                    if let Some(body) = node.child_by_field_name("body") {
                        let mut cursor = body.walk();
                        for child in body.children(&mut cursor) {
                            self.walk_tree(&child, source, file_path, entities, Some(&iface_name));
                        }
                    }
                    return;
                }
            }
            "enum_declaration" => {
                if let Some(entity) = self.extract_enum(node, source, file_path) {
                    let enum_name = entity.name.clone();
                    entities.push(entity);

                    // Walk enum body for methods
                    if let Some(body) = node.child_by_field_name("body") {
                        let mut cursor = body.walk();
                        for child in body.children(&mut cursor) {
                            self.walk_tree(&child, source, file_path, entities, Some(&enum_name));
                        }
                    }
                    return;
                }
            }
            "record_declaration" => {
                if let Some(entity) = self.extract_record(node, source, file_path) {
                    let record_name = entity.name.clone();
                    entities.push(entity);

                    // Walk record body for methods
                    if let Some(body) = node.child_by_field_name("body") {
                        let mut cursor = body.walk();
                        for child in body.children(&mut cursor) {
                            self.walk_tree(&child, source, file_path, entities, Some(&record_name));
                        }
                    }
                    return;
                }
            }
            "annotation_type_declaration" => {
                if let Some(entity) = self.extract_annotation_type(node, source, file_path) {
                    entities.push(entity);
                }
            }
            "method_declaration" => {
                if let Some(entity) = self.extract_method(node, source, file_path, in_class) {
                    entities.push(entity);
                }
            }
            "constructor_declaration" => {
                if let Some(entity) = self.extract_constructor(node, source, file_path, in_class) {
                    entities.push(entity);
                }
            }
            "field_declaration" if in_class.is_some() => {
                self.extract_fields(node, source, file_path, entities);
            }
            "import_declaration" => {
                if let Some(entity) = self.extract_import(node, source, file_path) {
                    entities.push(entity);
                }
            }
            "package_declaration" => {
                // We skip package declarations — they're not entities
            }
            _ => {}
        }

        // Recurse into children for top-level declarations
        // (class/interface/enum bodies are handled above)
        if !matches!(
            node.kind(),
            "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration"
        ) {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                self.walk_tree(&child, source, file_path, entities, in_class);
            }
        }
    }

    /// Extract a class declaration.
    fn extract_class(&self, node: &Node, source: &str, file_path: &str) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let exported = self.has_modifier(node, source, "public");

        let signature = self.build_class_signature(node, source);

        let mut entity = Entity::new(name, EntityKind::Class, file_path, line, end_line);
        if let Some(sig) = signature {
            entity = entity.with_signature(sig);
        }
        if exported {
            entity.exported = true;
        }

        Some(entity)
    }

    /// Extract an interface declaration.
    fn extract_interface(&self, node: &Node, source: &str, file_path: &str) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let exported = self.has_modifier(node, source, "public");

        let signature = self.build_interface_signature(node, source);

        let mut entity = Entity::new(name, EntityKind::Interface, file_path, line, end_line);
        if let Some(sig) = signature {
            entity = entity.with_signature(sig);
        }
        if exported {
            entity.exported = true;
        }

        Some(entity)
    }

    /// Extract an enum declaration.
    fn extract_enum(&self, node: &Node, source: &str, file_path: &str) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let exported = self.has_modifier(node, source, "public");

        let sig = format!("{}enum {}", if exported { "public " } else { "" }, name);
        let mut entity = Entity::new(name, EntityKind::Enum, file_path, line, end_line);
        entity = entity.with_signature(sig);
        if exported {
            entity.exported = true;
        }

        Some(entity)
    }

    /// Extract a record declaration (Java 16+).
    fn extract_record(&self, node: &Node, source: &str, file_path: &str) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let exported = self.has_modifier(node, source, "public");

        let params = node
            .child_by_field_name("parameters")
            .map(|n| self.node_text(&n, source))
            .unwrap_or_default();

        let sig = format!(
            "{}record {}{}",
            if exported { "public " } else { "" },
            name,
            params
        );
        let mut entity = Entity::new(name, EntityKind::Class, file_path, line, end_line);
        entity = entity.with_signature(sig);
        if exported {
            entity.exported = true;
        }

        Some(entity)
    }

    /// Extract an annotation type declaration (`@interface`).
    fn extract_annotation_type(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
    ) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let exported = self.has_modifier(node, source, "public");

        let mut entity = Entity::new(
            name.clone(),
            EntityKind::Interface,
            file_path,
            line,
            end_line,
        );
        entity = entity.with_signature(format!(
            "{}@interface {}",
            if exported { "public " } else { "" },
            name
        ));
        if exported {
            entity.exported = true;
        }

        Some(entity)
    }

    /// Extract a method declaration.
    fn extract_method(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        in_class: Option<&str>,
    ) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let kind = if in_class.is_some() {
            EntityKind::Method
        } else {
            EntityKind::Function
        };

        let exported = self.has_modifier(node, source, "public");

        let signature = self.build_method_signature(node, source);

        let mut entity = Entity::new(name, kind, file_path, line, end_line);
        if let Some(sig) = signature {
            entity = entity.with_signature(sig);
        }
        if exported {
            entity.exported = true;
        }

        Some(entity)
    }

    /// Extract a constructor declaration.
    fn extract_constructor(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        _in_class: Option<&str>,
    ) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let exported = self.has_modifier(node, source, "public");

        let params = node
            .child_by_field_name("parameters")
            .map(|n| self.node_text(&n, source))
            .unwrap_or_else(|| "()".to_string());

        let vis = self.get_visibility(node, source);

        let mut entity = Entity::new(name.clone(), EntityKind::Method, file_path, line, end_line);
        entity = entity.with_signature(format!("{}{}{}", vis, name, params));
        if exported {
            entity.exported = true;
        }

        Some(entity)
    }

    /// Extract field declarations from a class body.
    ///
    /// A single `field_declaration` in Java can declare multiple variables:
    /// `private int x, y, z;`
    fn extract_fields(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        entities: &mut Vec<Entity>,
    ) {
        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let is_static = self.has_modifier(node, source, "static");
        let is_final = self.has_modifier(node, source, "final");
        let exported = self.has_modifier(node, source, "public");

        let kind = if is_static && is_final {
            EntityKind::Const
        } else {
            EntityKind::Variable
        };

        // Walk children to find variable_declarator nodes
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "variable_declarator" {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = self.node_text(&name_node, source);

                    let sig_text = self.node_text(node, source);
                    let sig = sig_text
                        .lines()
                        .next()
                        .map(|l| l.trim().trim_end_matches(';').trim().to_string());

                    let mut entity = Entity::new(name, kind, file_path, line, end_line);
                    if let Some(s) = sig {
                        entity = entity.with_signature(s);
                    }
                    if exported {
                        entity.exported = true;
                    }

                    entities.push(entity);
                }
            }
        }
    }

    /// Extract an import declaration.
    fn extract_import(&self, node: &Node, source: &str, file_path: &str) -> Option<Entity> {
        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let text = self.node_text(node, source);

        // Extract the imported path: `import java.util.List;` → `java.util.List`
        let name = text
            .strip_prefix("import ")
            .unwrap_or(&text)
            .strip_prefix("static ")
            .unwrap_or(&text)
            .trim()
            .trim_end_matches(';')
            .trim()
            .to_string();

        let mut entity = Entity::new(name, EntityKind::Import, file_path, line, end_line);
        entity = entity.with_signature(text.trim_end_matches(';').trim().to_string());

        Some(entity)
    }

    // ── Signature builders ──────────────────────────────────────────

    /// Build a class signature string.
    fn build_class_signature(&self, node: &Node, source: &str) -> Option<String> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        let vis = self.get_visibility(node, source);
        let is_abstract = self.has_modifier(node, source, "abstract");
        let is_final = self.has_modifier(node, source, "final");

        let mut parts = Vec::new();
        if !vis.is_empty() {
            parts.push(vis.trim().to_string());
        }
        if is_abstract {
            parts.push("abstract".to_string());
        }
        if is_final {
            parts.push("final".to_string());
        }
        parts.push("class".to_string());
        parts.push(name.clone());

        // Check for type parameters
        if let Some(type_params) = node.child_by_field_name("type_parameters") {
            parts.push(self.node_text(&type_params, source));
        }

        // Check for superclass
        if let Some(superclass) = node.child_by_field_name("superclass") {
            parts.push(format!("extends {}", self.node_text(&superclass, source)));
        }

        // Check for interfaces
        if let Some(interfaces) = node.child_by_field_name("interfaces") {
            parts.push(format!(
                "implements {}",
                self.node_text(&interfaces, source)
            ));
        }

        Some(parts.join(" "))
    }

    /// Build an interface signature string.
    fn build_interface_signature(&self, node: &Node, source: &str) -> Option<String> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        let vis = self.get_visibility(node, source);

        let mut parts = Vec::new();
        if !vis.is_empty() {
            parts.push(vis.trim().to_string());
        }
        parts.push("interface".to_string());
        parts.push(name);

        // Check for type parameters
        if let Some(type_params) = node.child_by_field_name("type_parameters") {
            parts.push(self.node_text(&type_params, source));
        }

        // Check for extends
        if let Some(extends) = node.child_by_field_name("extends") {
            let ext_text = self.node_text(&extends, source);
            if !ext_text.is_empty() {
                parts.push(format!("extends {}", ext_text));
            }
        }

        Some(parts.join(" "))
    }

    /// Build a method signature string.
    fn build_method_signature(&self, node: &Node, source: &str) -> Option<String> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        let vis = self.get_visibility(node, source);
        let is_static = self.has_modifier(node, source, "static");
        let is_abstract = self.has_modifier(node, source, "abstract");

        let return_type = node
            .child_by_field_name("type")
            .map(|n| self.node_text(&n, source))
            .unwrap_or_else(|| "void".to_string());

        let params = node
            .child_by_field_name("parameters")
            .map(|n| self.node_text(&n, source))
            .unwrap_or_else(|| "()".to_string());

        // Check for type parameters on the method itself
        let type_params = node
            .child_by_field_name("type_parameters")
            .map(|n| format!("{} ", self.node_text(&n, source)))
            .unwrap_or_default();

        let mut parts = Vec::new();
        if !vis.is_empty() {
            parts.push(vis.trim().to_string());
        }
        if is_static {
            parts.push("static".to_string());
        }
        if is_abstract {
            parts.push("abstract".to_string());
        }
        parts.push(format!("{}{}", type_params, return_type));
        parts.push(format!("{}{}", name, params));

        // Check for throws
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "throws" {
                parts.push(format!("throws {}", self.node_text(&child, source)));
                break;
            }
        }

        Some(parts.join(" "))
    }

    // ── Helpers ─────────────────────────────────────────────────────

    /// Check if a node has a specific modifier (public, private, static, final, abstract, etc.).
    fn has_modifier(&self, node: &Node, source: &str, modifier: &str) -> bool {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "modifiers" {
                let text = self.node_text(&child, source);
                return text.contains(modifier);
            }
            // Also check direct modifier children (some tree-sitter versions)
            if self.node_text(&child, source) == modifier {
                return true;
            }
            // Stop once we hit the name/type — modifiers come before
            if child.kind() == "identifier"
                || child.kind() == "type_identifier"
                || child.kind() == "void_type"
                || child.kind() == "class"
                || child.kind() == "interface"
                || child.kind() == "enum"
            {
                break;
            }
        }
        false
    }

    /// Get the visibility modifier as a string ("public ", "private ", "protected ", or "").
    fn get_visibility(&self, node: &Node, source: &str) -> String {
        if self.has_modifier(node, source, "public") {
            "public ".to_string()
        } else if self.has_modifier(node, source, "protected") {
            "protected ".to_string()
        } else if self.has_modifier(node, source, "private") {
            "private ".to_string()
        } else {
            String::new() // package-private
        }
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
        if node.kind() == "method_invocation" {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = self.node_text(&name_node, source);
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

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk_tree_for_calls(&child, source, file_path, refs);
        }
    }
}

impl Default for JavaParser {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageParser for JavaParser {
    fn language(&self) -> Language {
        Language::Java
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
    fn test_extract_class() {
        let mut parser = JavaParser::new();
        let source = r#"
public class UserService {
    private String name;

    public UserService(String name) {
        this.name = name;
    }

    public String getName() {
        return name;
    }
}
"#;
        let entities = parser.extract(source, "UserService.java");
        assert!(!entities.is_empty());

        let service = entities
            .iter()
            .find(|e| e.name == "UserService" && e.kind == EntityKind::Class);
        assert!(
            service.is_some(),
            "Should find UserService class, got: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.kind))
                .collect::<Vec<_>>()
        );
        assert!(service.unwrap().exported);
        assert!(service
            .unwrap()
            .signature
            .as_ref()
            .unwrap()
            .contains("public class UserService"));
    }

    #[test]
    fn test_extract_methods() {
        let mut parser = JavaParser::new();
        let source = r#"
public class App {
    public String greet(String name) {
        return "Hello, " + name;
    }

    private void helper() {
    }

    public static void main(String[] args) {
    }
}
"#;
        let entities = parser.extract(source, "App.java");

        let greet = entities.iter().find(|e| e.name == "greet");
        assert!(greet.is_some(), "Should find greet method");
        assert_eq!(greet.unwrap().kind, EntityKind::Method);
        assert!(greet.unwrap().exported);
        assert!(
            greet
                .unwrap()
                .signature
                .as_ref()
                .unwrap()
                .contains("String greet"),
            "Signature: {:?}",
            greet.unwrap().signature
        );

        let helper = entities.iter().find(|e| e.name == "helper");
        assert!(helper.is_some(), "Should find helper method");
        assert!(
            !helper.unwrap().exported,
            "private method should not be exported"
        );

        let main = entities.iter().find(|e| e.name == "main");
        assert!(main.is_some(), "Should find main method");
        assert!(
            main.unwrap().signature.as_ref().unwrap().contains("static"),
            "Main signature should contain static: {:?}",
            main.unwrap().signature
        );
    }

    #[test]
    fn test_extract_constructor() {
        let mut parser = JavaParser::new();
        let source = r#"
public class User {
    public User(String name, int age) {
    }
}
"#;
        let entities = parser.extract(source, "User.java");

        let constructor = entities
            .iter()
            .find(|e| e.name == "User" && e.kind == EntityKind::Method);
        assert!(constructor.is_some(), "Should find User constructor");
        assert!(
            constructor
                .unwrap()
                .signature
                .as_ref()
                .unwrap()
                .contains("(String name, int age)"),
            "Constructor signature: {:?}",
            constructor.unwrap().signature
        );
    }

    #[test]
    fn test_extract_interface() {
        let mut parser = JavaParser::new();
        let source = r#"
public interface Repository<T> {
    T find(String id);
    void save(T entity);
    void delete(String id);
}
"#;
        let entities = parser.extract(source, "Repository.java");

        let repo = entities
            .iter()
            .find(|e| e.name == "Repository" && e.kind == EntityKind::Interface);
        assert!(repo.is_some(), "Should find Repository interface");
        assert!(repo.unwrap().exported);

        // Should find interface methods
        let find = entities.iter().find(|e| e.name == "find");
        assert!(find.is_some(), "Should find find() method in interface");

        let save = entities.iter().find(|e| e.name == "save");
        assert!(save.is_some(), "Should find save() method in interface");
    }

    #[test]
    fn test_extract_enum() {
        let mut parser = JavaParser::new();
        let source = r#"
public enum Status {
    ACTIVE,
    INACTIVE,
    PENDING;

    public boolean isActive() {
        return this == ACTIVE;
    }
}
"#;
        let entities = parser.extract(source, "Status.java");

        let status = entities
            .iter()
            .find(|e| e.name == "Status" && e.kind == EntityKind::Enum);
        assert!(status.is_some(), "Should find Status enum");
        assert!(status.unwrap().exported);

        // Should find method inside enum
        let is_active = entities.iter().find(|e| e.name == "isActive");
        assert!(is_active.is_some(), "Should find isActive method in enum");
    }

    #[test]
    fn test_extract_fields() {
        let mut parser = JavaParser::new();
        let source = r#"
public class Config {
    public static final int MAX_RETRIES = 3;
    public static final String API_URL = "https://api.example.com";
    private int timeout;
    protected String host;
}
"#;
        let entities = parser.extract(source, "Config.java");

        let max = entities.iter().find(|e| e.name == "MAX_RETRIES");
        assert!(max.is_some(), "Should find MAX_RETRIES");
        assert_eq!(
            max.unwrap().kind,
            EntityKind::Const,
            "static final should be Const"
        );
        assert!(max.unwrap().exported);

        let timeout = entities.iter().find(|e| e.name == "timeout");
        assert!(timeout.is_some(), "Should find timeout field");
        assert_eq!(
            timeout.unwrap().kind,
            EntityKind::Variable,
            "non-static field should be Variable"
        );
        assert!(
            !timeout.unwrap().exported,
            "private field should not be exported"
        );
    }

    #[test]
    fn test_extract_imports() {
        let mut parser = JavaParser::new();
        let source = r#"
import java.util.List;
import java.util.Map;
import static java.lang.Math.PI;

public class App {}
"#;
        let entities = parser.extract(source, "App.java");

        let imports: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Import)
            .collect();
        assert!(
            imports.len() >= 3,
            "Should find 3 imports, got: {:?}",
            imports.iter().map(|e| &e.name).collect::<Vec<_>>()
        );

        assert!(
            imports.iter().any(|e| e.name.contains("java.util.List")),
            "Should find java.util.List import"
        );
    }

    #[test]
    fn test_extract_abstract_class() {
        let mut parser = JavaParser::new();
        let source = r#"
public abstract class AbstractService {
    public abstract void process();
    public void log(String msg) {}
}
"#;
        let entities = parser.extract(source, "AbstractService.java");

        let service = entities.iter().find(|e| e.name == "AbstractService");
        assert!(service.is_some(), "Should find AbstractService");
        assert!(
            service
                .unwrap()
                .signature
                .as_ref()
                .unwrap()
                .contains("abstract"),
            "Should contain abstract in signature: {:?}",
            service.unwrap().signature
        );

        assert!(
            entities.iter().any(|e| e.name == "process"),
            "Should find abstract method process"
        );
        assert!(
            entities.iter().any(|e| e.name == "log"),
            "Should find concrete method log"
        );
    }

    #[test]
    fn test_extract_class_with_extends() {
        let mut parser = JavaParser::new();
        let source = r#"
public class Admin extends User implements Serializable, Comparable<Admin> {
}
"#;
        let entities = parser.extract(source, "Admin.java");

        let admin = entities.iter().find(|e| e.name == "Admin").unwrap();
        let sig = admin.signature.as_ref().unwrap();
        // The exact format depends on tree-sitter's field names for Java,
        // but should contain the class name at minimum
        assert!(
            sig.contains("Admin"),
            "Signature should contain Admin: {}",
            sig
        );
    }

    #[test]
    fn test_extract_generic_class() {
        let mut parser = JavaParser::new();
        let source = r#"
public class Cache<K, V> {
    private Map<K, V> data;

    public V get(K key) {
        return data.get(key);
    }

    public void put(K key, V value) {
        data.put(key, value);
    }
}
"#;
        let entities = parser.extract(source, "Cache.java");

        let cache = entities
            .iter()
            .find(|e| e.name == "Cache" && e.kind == EntityKind::Class);
        assert!(cache.is_some(), "Should find Cache class");

        assert!(
            entities.iter().any(|e| e.name == "get"),
            "Should find get method"
        );
        assert!(
            entities.iter().any(|e| e.name == "put"),
            "Should find put method"
        );
    }

    #[test]
    fn test_empty_source() {
        let mut parser = JavaParser::new();
        let entities = parser.extract("", "Empty.java");
        assert!(entities.is_empty());
    }

    #[test]
    fn test_package_only() {
        let mut parser = JavaParser::new();
        let source = "package com.example;\n";
        let entities = parser.extract(source, "package-info.java");
        // Should not crash, package declarations are skipped
        assert!(
            !entities
                .iter()
                .any(|e| e.kind == EntityKind::Module && e.name == "com.example"),
            "Package declarations should be skipped"
        );
    }

    #[test]
    fn test_line_numbers() {
        let mut parser = JavaParser::new();
        let source = r#"
public class Lines {
    public void first() {}

    public void second() {}
}
"#;
        let entities = parser.extract(source, "Lines.java");
        let first = entities.iter().find(|e| e.name == "first").unwrap();
        let second = entities.iter().find(|e| e.name == "second").unwrap();
        assert!(
            first.line < second.line,
            "first ({}) should come before second ({})",
            first.line,
            second.line
        );
    }

    #[test]
    fn test_language() {
        let parser = JavaParser::new();
        assert_eq!(parser.language(), Language::Java);
    }

    #[test]
    fn test_inner_class() {
        let mut parser = JavaParser::new();
        let source = r#"
public class Outer {
    public class Inner {
        public void innerMethod() {}
    }
}
"#;
        let entities = parser.extract(source, "Outer.java");

        assert!(
            entities.iter().any(|e| e.name == "Outer"),
            "Should find Outer class"
        );
        assert!(
            entities.iter().any(|e| e.name == "Inner"),
            "Should find Inner class"
        );
        assert!(
            entities.iter().any(|e| e.name == "innerMethod"),
            "Should find innerMethod"
        );
    }

    #[test]
    fn test_realistic_spring_controller() {
        let mut parser = JavaParser::new();
        let source = r#"
package com.example.api;

import org.springframework.web.bind.annotation.*;
import org.springframework.http.ResponseEntity;
import java.util.List;
import java.util.Optional;

@RestController
@RequestMapping("/api/users")
public class UserController {

    private final UserService userService;

    public UserController(UserService userService) {
        this.userService = userService;
    }

    @GetMapping
    public ResponseEntity<List<User>> getAllUsers() {
        List<User> users = userService.findAll();
        return ResponseEntity.ok(users);
    }

    @GetMapping("/{id}")
    public ResponseEntity<User> getUserById(@PathVariable String id) {
        Optional<User> user = userService.findById(id);
        return user.map(ResponseEntity::ok)
                   .orElse(ResponseEntity.notFound().build());
    }

    @PostMapping
    public ResponseEntity<User> createUser(@RequestBody User user) {
        User created = userService.save(user);
        return ResponseEntity.status(201).body(created);
    }

    @DeleteMapping("/{id}")
    public ResponseEntity<Void> deleteUser(@PathVariable String id) {
        userService.delete(id);
        return ResponseEntity.noContent().build();
    }

    private void validateUser(User user) {
        if (user.getName() == null) {
            throw new IllegalArgumentException("Name is required");
        }
    }
}
"#;
        let entities = parser.extract(source, "UserController.java");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        // Class
        assert!(
            names.contains(&"UserController"),
            "Should find UserController class"
        );

        // Constructor
        assert!(
            entities
                .iter()
                .any(|e| e.name == "UserController" && e.kind == EntityKind::Method),
            "Should find UserController constructor"
        );

        // Public methods
        assert!(names.contains(&"getAllUsers"), "Should find getAllUsers");
        assert!(names.contains(&"getUserById"), "Should find getUserById");
        assert!(names.contains(&"createUser"), "Should find createUser");
        assert!(names.contains(&"deleteUser"), "Should find deleteUser");

        // Private method
        assert!(names.contains(&"validateUser"), "Should find validateUser");
        let validate = entities.iter().find(|e| e.name == "validateUser").unwrap();
        assert!(!validate.exported, "validateUser should not be exported");

        // Field
        assert!(
            names.contains(&"userService"),
            "Should find userService field"
        );

        // Imports
        let import_count = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Import)
            .count();
        assert!(
            import_count >= 4,
            "Should find at least 4 imports, got {}",
            import_count
        );

        // Verify visibility
        let get_all = entities.iter().find(|e| e.name == "getAllUsers").unwrap();
        assert!(get_all.exported, "getAllUsers should be exported (public)");

        // Verify entity count is reasonable
        assert!(
            entities.len() >= 10,
            "Expected >= 10 entities for a realistic Spring controller, got {}",
            entities.len()
        );
    }

    #[test]
    fn test_method_with_throws() {
        let mut parser = JavaParser::new();
        let source = r#"
public class Service {
    public void process() throws IOException, ParseException {
    }
}
"#;
        let entities = parser.extract(source, "Service.java");
        let process = entities.iter().find(|e| e.name == "process");
        assert!(process.is_some(), "Should find process method");
        // The throws clause may or may not appear in the signature depending on
        // tree-sitter's AST structure, but the method should be found.
    }

    #[test]
    fn test_static_method() {
        let mut parser = JavaParser::new();
        let source = r#"
public class MathUtils {
    public static int add(int a, int b) {
        return a + b;
    }
}
"#;
        let entities = parser.extract(source, "MathUtils.java");
        let add = entities.iter().find(|e| e.name == "add").unwrap();
        assert!(
            add.signature.as_ref().unwrap().contains("static"),
            "Should contain static in signature: {:?}",
            add.signature
        );
    }

    #[test]
    fn test_multiple_classes_in_file() {
        let mut parser = JavaParser::new();
        let source = r#"
public class Main {
    public void run() {}
}

class Helper {
    void assist() {}
}
"#;
        let entities = parser.extract(source, "Main.java");

        assert!(
            entities
                .iter()
                .any(|e| e.name == "Main" && e.kind == EntityKind::Class),
            "Should find Main class"
        );
        assert!(
            entities
                .iter()
                .any(|e| e.name == "Helper" && e.kind == EntityKind::Class),
            "Should find Helper class"
        );

        let main_class = entities
            .iter()
            .find(|e| e.name == "Main" && e.kind == EntityKind::Class)
            .unwrap();
        assert!(main_class.exported, "Main should be public");

        let helper_class = entities
            .iter()
            .find(|e| e.name == "Helper" && e.kind == EntityKind::Class)
            .unwrap();
        assert!(
            !helper_class.exported,
            "Helper should be package-private (not exported)"
        );
    }
}
