//! C/C++ parser for semantic analysis.
//!
//! Extracts functions, classes, structs, namespaces, enums, type aliases,
//! includes, and preprocessor definitions from C and C++ source code using
//! tree-sitter-cpp and tree-sitter-c.
//!
//! # Supported Entity Types
//!
//! | C/C++ Construct | tree-sitter node kind | EntityKind |
//! |----------------|----------------------|------------|
//! | `void foo()` | `function_definition` | Function |
//! | `class Foo {}` | `class_specifier` | Class |
//! | `struct Bar {}` | `struct_specifier` | Class |
//! | `namespace ns {}` | `namespace_definition` | Module |
//! | `enum Color {}` | `enum_specifier` | Enum |
//! | `template<>` | `template_declaration` | (extract inner) |
//! | `using X = Y` | `type_alias_declaration` | TypeAlias |
//! | `typedef int X` | `type_definition` | TypeAlias |
//! | `#include <foo>` | `preproc_include` | Import |
//! | `#define FOO` | `preproc_def` | Const |

use super::{Language, LanguageParser};
use crate::entity::{Entity, EntityKind};
use tree_sitter::{Node, Parser};

/// C/C++ AST entity extractor using tree-sitter.
pub struct CppParser {
    parser: Parser,
    is_c: bool,
}

impl CppParser {
    /// Create a new C++ parser.
    pub fn new() -> Self {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_cpp::LANGUAGE.into())
            .expect("Failed to load C++ grammar");
        Self {
            parser,
            is_c: false,
        }
    }

    /// Create a new C parser.
    pub fn new_c() -> Self {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_c::LANGUAGE.into())
            .expect("Failed to load C grammar");
        Self { parser, is_c: true }
    }

    /// Walk the AST and extract entities.
    fn walk_tree(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        entities: &mut Vec<Entity>,
        scope: Option<&str>,
        in_class: bool,
    ) {
        match node.kind() {
            "function_definition" => {
                if let Some(entity) =
                    self.extract_function(node, source, file_path, scope, in_class)
                {
                    entities.push(entity);
                }
                return; // Don't recurse into function bodies
            }
            "declaration" => {
                // A top-level declaration may contain a function declarator (prototype)
                // or a variable/field. We skip plain declarations to avoid noise —
                // function_definition already covers implemented functions.
                // However, we still recurse below so nested class specifiers etc. are found.
            }
            "class_specifier" => {
                if let Some((entity, raw_name)) = self.extract_class(node, source, file_path, scope)
                {
                    entities.push(entity);

                    // Walk class body for methods, nested types.
                    // Use raw_name (not entity.name) to avoid scope doubling.
                    let scoped = match scope {
                        Some(s) => format!("{}::{}", s, raw_name),
                        None => raw_name,
                    };
                    if let Some(body) = node.child_by_field_name("body") {
                        let mut cursor = body.walk();
                        for child in body.children(&mut cursor) {
                            self.walk_tree(
                                &child,
                                source,
                                file_path,
                                entities,
                                Some(&scoped),
                                true,
                            );
                        }
                    }
                    return; // Don't recurse again below
                }
            }
            "struct_specifier" => {
                if let Some((entity, raw_name)) =
                    self.extract_struct(node, source, file_path, scope)
                {
                    entities.push(entity);

                    let scoped = match scope {
                        Some(s) => format!("{}::{}", s, raw_name),
                        None => raw_name,
                    };
                    if let Some(body) = node.child_by_field_name("body") {
                        let mut cursor = body.walk();
                        for child in body.children(&mut cursor) {
                            self.walk_tree(
                                &child,
                                source,
                                file_path,
                                entities,
                                Some(&scoped),
                                true,
                            );
                        }
                    }
                    return;
                }
            }
            "namespace_definition" => {
                if let Some((entity, raw_name)) =
                    self.extract_namespace(node, source, file_path, scope)
                {
                    entities.push(entity);

                    let scoped = match scope {
                        Some(s) => format!("{}::{}", s, raw_name),
                        None => raw_name,
                    };
                    if let Some(body) = node.child_by_field_name("body") {
                        let mut cursor = body.walk();
                        for child in body.children(&mut cursor) {
                            // Namespace does NOT set in_class — functions inside
                            // remain Functions, not Methods.
                            self.walk_tree(
                                &child,
                                source,
                                file_path,
                                entities,
                                Some(&scoped),
                                false,
                            );
                        }
                    }
                    return;
                }
            }
            "enum_specifier" => {
                if let Some(entity) = self.extract_enum(node, source, file_path, scope) {
                    entities.push(entity);
                }
                return;
            }
            "template_declaration" => {
                // Don't extract the template wrapper itself — recurse into
                // the inner declaration (class, function, etc.)
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    let k = child.kind();
                    if k != "template_parameter_list" {
                        self.walk_tree(&child, source, file_path, entities, scope, in_class);
                    }
                }
                return;
            }
            "alias_declaration" | "type_alias_declaration" => {
                // C++ `using X = Y;`
                // tree-sitter-cpp 0.23 uses "alias_declaration"
                if let Some(entity) = self.extract_type_alias(node, source, file_path, scope) {
                    entities.push(entity);
                }
                return;
            }
            "type_definition" => {
                // C `typedef int X;`
                if let Some(entity) = self.extract_typedef(node, source, file_path, scope) {
                    entities.push(entity);
                }
                return;
            }
            "preproc_include" => {
                if let Some(entity) = self.extract_include(node, source, file_path) {
                    entities.push(entity);
                }
                return;
            }
            "preproc_def" => {
                if let Some(entity) = self.extract_define(node, source, file_path) {
                    entities.push(entity);
                }
                return;
            }
            // Inside class/struct bodies, tree-sitter wraps members in
            // access specifiers or field_declaration_list items. We need
            // to recurse through these.
            "access_specifier" | "field_declaration_list" => {
                // Just recurse into children
            }
            _ => {}
        }

        // Recurse into children
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            // Don't recurse into compound_statement (function bodies)
            if child.kind() != "compound_statement" {
                self.walk_tree(&child, source, file_path, entities, scope, in_class);
            }
        }
    }

    /// Extract a function definition.
    fn extract_function(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        scope: Option<&str>,
        in_class: bool,
    ) -> Option<Entity> {
        let declarator = node.child_by_field_name("declarator")?;
        let (name, is_scoped) = self.extract_function_name(&declarator, source);
        let name = name?;

        if name.is_empty() {
            return None;
        }

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        // A function is a Method if:
        // - it has a qualified name (e.g. ClassName::method), or
        // - it's defined inside a class/struct body (in_class == true)
        let kind = if is_scoped || in_class {
            EntityKind::Method
        } else {
            EntityKind::Function
        };

        let signature = self.build_function_signature(node, source);

        // If the function has a qualified name (e.g., ClassName::method), use it
        // directly. Otherwise, prepend the scope if we're inside a class body.
        let full_name = if name.contains("::") {
            name
        } else if let Some(s) = scope {
            format!("{}::{}", s, name)
        } else {
            name
        };

        let mut entity = Entity::new(full_name, kind, file_path, line, end_line);
        if let Some(sig) = signature {
            entity = entity.with_signature(sig);
        }
        // C/C++ doesn't have a simple export convention; default to true for
        // non-static functions. For simplicity, mark all extracted entities
        // as exported.
        entity.exported = true;

        Some(entity)
    }

    /// Extract the function name from a declarator node.
    ///
    /// Returns `(Option<name>, is_scoped_method)`.
    /// For `ClassName::method`, returns the full qualified name.
    fn extract_function_name(&self, declarator: &Node, source: &str) -> (Option<String>, bool) {
        match declarator.kind() {
            "function_declarator" => {
                // The first child of function_declarator is the name or
                // a qualified_identifier / scoped identifier.
                if let Some(name_node) = declarator.child(0) {
                    match name_node.kind() {
                        "qualified_identifier" | "scoped_identifier" => {
                            let text = self.node_text(&name_node, source);
                            (Some(text), true)
                        }
                        "field_identifier" | "identifier" | "destructor_name" => {
                            let text = self.node_text(&name_node, source);
                            (Some(text), false)
                        }
                        "operator_name" => {
                            let text = self.node_text(&name_node, source);
                            (Some(text), false)
                        }
                        "template_function" => {
                            // template<> void foo<int>() — extract the inner name
                            if let Some(inner) = name_node.child(0) {
                                let text = self.node_text(&inner, source);
                                let scoped = text.contains("::");
                                (Some(text), scoped)
                            } else {
                                (None, false)
                            }
                        }
                        _ => {
                            let text = self.node_text(&name_node, source);
                            let scoped = text.contains("::");
                            (Some(text), scoped)
                        }
                    }
                } else {
                    (None, false)
                }
            }
            "reference_declarator" | "pointer_declarator" => {
                // e.g., `int &foo()` or `int *foo()` — unwrap to inner declarator
                if let Some(inner) = declarator.child(1).or_else(|| declarator.child(0)) {
                    self.extract_function_name(&inner, source)
                } else {
                    (None, false)
                }
            }
            _ => {
                // Try child_by_field_name for other wrapper nodes
                if let Some(inner) = declarator.child_by_field_name("declarator") {
                    self.extract_function_name(&inner, source)
                } else {
                    let text = self.node_text(declarator, source);
                    let scoped = text.contains("::");
                    (Some(text), scoped)
                }
            }
        }
    }

    /// Extract a class specifier.
    ///
    /// Returns `(Entity, raw_name)` where `raw_name` is the unscoped name
    /// for use in building child scopes.
    fn extract_class(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        scope: Option<&str>,
    ) -> Option<(Entity, String)> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        if name.is_empty() {
            return None;
        }

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let full_name = match scope {
            Some(s) => format!("{}::{}", s, name),
            None => name.clone(),
        };

        let signature = format!("class {}", name);

        let mut entity = Entity::new(full_name, EntityKind::Class, file_path, line, end_line);
        entity = entity.with_signature(signature);
        entity.exported = true;

        Some((entity, name))
    }

    /// Extract a struct specifier.
    ///
    /// Returns `(Entity, raw_name)` where `raw_name` is the unscoped name
    /// for use in building child scopes.
    fn extract_struct(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        scope: Option<&str>,
    ) -> Option<(Entity, String)> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        if name.is_empty() {
            return None;
        }

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let full_name = match scope {
            Some(s) => format!("{}::{}", s, name),
            None => name.clone(),
        };

        let signature = format!("struct {}", name);

        let mut entity = Entity::new(full_name, EntityKind::Class, file_path, line, end_line);
        entity = entity.with_signature(signature);
        entity.exported = true;

        Some((entity, name))
    }

    /// Extract a namespace definition.
    ///
    /// Returns `(Entity, raw_name)` where `raw_name` is the unscoped name
    /// for use in building child scopes.
    fn extract_namespace(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        scope: Option<&str>,
    ) -> Option<(Entity, String)> {
        // Anonymous namespaces have no name child — skip them
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        if name.is_empty() {
            return None;
        }

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let full_name = match scope {
            Some(s) => format!("{}::{}", s, name),
            None => name.clone(),
        };

        let signature = format!("namespace {}", name);

        let mut entity = Entity::new(full_name, EntityKind::Module, file_path, line, end_line);
        entity = entity.with_signature(signature);
        entity.exported = true;

        Some((entity, name))
    }

    /// Extract an enum specifier.
    fn extract_enum(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        scope: Option<&str>,
    ) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        if name.is_empty() {
            return None;
        }

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let full_name = match scope {
            Some(s) => format!("{}::{}", s, name),
            None => name.clone(),
        };

        let signature = format!("enum {}", name);

        let mut entity = Entity::new(full_name, EntityKind::Enum, file_path, line, end_line);
        entity = entity.with_signature(signature);
        entity.exported = true;

        Some(entity)
    }

    /// Extract a C++ type alias (`using X = Y;`).
    fn extract_type_alias(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        scope: Option<&str>,
    ) -> Option<Entity> {
        // tree-sitter-cpp 0.23 uses "alias_declaration" with the alias name
        // as a direct `type_identifier` child (not via a "name" field).
        // Try the "name" field first, then fall back to scanning children.
        let name = if let Some(name_node) = node.child_by_field_name("name") {
            self.node_text(&name_node, source)
        } else {
            // Scan children for the type_identifier that is the alias name
            let mut found = None;
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "type_identifier" {
                    found = Some(self.node_text(&child, source));
                    break;
                }
            }
            found?
        };

        if name.is_empty() {
            return None;
        }

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let full_name = match scope {
            Some(s) => format!("{}::{}", s, name),
            None => name.clone(),
        };

        let sig_text = self.node_text(node, source);
        let signature = sig_text
            .lines()
            .next()
            .map(|l| l.trim().trim_end_matches(';').to_string())
            .unwrap_or_else(|| format!("using {}", name));

        let mut entity = Entity::new(full_name, EntityKind::TypeAlias, file_path, line, end_line);
        entity = entity.with_signature(signature);
        entity.exported = true;

        Some(entity)
    }

    /// Extract a C typedef (`typedef int X;`).
    fn extract_typedef(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        scope: Option<&str>,
    ) -> Option<Entity> {
        // typedef nodes have a `declarator` field containing the new type name.
        let declarator = node.child_by_field_name("declarator")?;
        let name = self.find_typedef_name(&declarator, source)?;

        if name.is_empty() {
            return None;
        }

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let full_name = match scope {
            Some(s) => format!("{}::{}", s, name),
            None => name,
        };

        let sig_text = self.node_text(node, source);
        let signature = sig_text
            .lines()
            .next()
            .map(|l| l.trim().trim_end_matches(';').to_string())
            .unwrap_or_else(|| format!("typedef {}", full_name));

        let mut entity = Entity::new(full_name, EntityKind::TypeAlias, file_path, line, end_line);
        entity = entity.with_signature(signature);
        entity.exported = true;

        Some(entity)
    }

    /// Find the identifier name inside a typedef declarator.
    ///
    /// The declarator can be a plain `type_identifier`, a pointer declarator,
    /// or a function declarator wrapping the actual name.
    fn find_typedef_name(&self, node: &Node, source: &str) -> Option<String> {
        match node.kind() {
            "type_identifier" | "identifier" => Some(self.node_text(node, source)),
            "pointer_declarator" => {
                // typedef int *IntPtr; — find the identifier inside
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if let Some(name) = self.find_typedef_name(&child, source) {
                        return Some(name);
                    }
                }
                None
            }
            "function_declarator" => {
                // typedef void (*Callback)(int); — first child is the name
                if let Some(child) = node.child(0) {
                    self.find_typedef_name(&child, source)
                } else {
                    None
                }
            }
            "parenthesized_declarator" => {
                // typedef void (*Callback)(int); — unwrap parens
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() != "(" && child.kind() != ")" {
                        if let Some(name) = self.find_typedef_name(&child, source) {
                            return Some(name);
                        }
                    }
                }
                None
            }
            "array_declarator" => {
                // typedef int Arr[10]; — first child is the name
                if let Some(child) = node.child(0) {
                    self.find_typedef_name(&child, source)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Extract a `#include` directive.
    fn extract_include(&self, node: &Node, source: &str, file_path: &str) -> Option<Entity> {
        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let path_node = node.child_by_field_name("path")?;
        let path_text = self.node_text(&path_node, source);

        // Strip quotes/angle brackets for the name
        let name = path_text
            .trim_matches(|c| c == '"' || c == '<' || c == '>')
            .to_string();

        if name.is_empty() {
            return None;
        }

        let sig_text = self.node_text(node, source);
        let signature = sig_text.lines().next().unwrap_or("").trim().to_string();

        let mut entity = Entity::new(name, EntityKind::Import, file_path, line, end_line);
        entity = entity.with_signature(signature);

        Some(entity)
    }

    /// Extract a `#define` directive.
    fn extract_define(&self, node: &Node, source: &str, file_path: &str) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        if name.is_empty() {
            return None;
        }

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let sig_text = self.node_text(node, source);
        let signature = sig_text.lines().next().unwrap_or("").trim().to_string();

        let mut entity = Entity::new(name, EntityKind::Const, file_path, line, end_line);
        entity = entity.with_signature(signature);
        entity.exported = true;

        Some(entity)
    }

    // ── Signature builders ──────────────────────────────────────────

    /// Build a function signature from the return type and declarator.
    fn build_function_signature(&self, node: &Node, source: &str) -> Option<String> {
        let full_text = self.node_text(node, source);
        // Take everything up to the opening brace
        if let Some(brace_pos) = full_text.find('{') {
            let sig = full_text[..brace_pos].trim();
            // Collapse whitespace for cleaner signatures
            let collapsed: String = sig.split_whitespace().collect::<Vec<_>>().join(" ");
            Some(collapsed)
        } else {
            // No body (prototype / forward declaration) — use full text
            let trimmed = full_text.trim().trim_end_matches(';');
            let collapsed: String = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
            Some(collapsed)
        }
    }

    // ── Helpers ─────────────────────────────────────────────────────

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
}

impl Default for CppParser {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageParser for CppParser {
    fn language(&self) -> Language {
        if self.is_c {
            Language::C
        } else {
            Language::Cpp
        }
    }

    fn extract(&mut self, source: &str, file_path: &str) -> Vec<Entity> {
        let tree = match self.parser.parse(source, None) {
            Some(t) => t,
            None => return vec![],
        };

        let mut entities = Vec::new();
        self.walk_tree(
            &tree.root_node(),
            source,
            file_path,
            &mut entities,
            None,
            false,
        );
        entities
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    // ── C++ tests ──────────────────────────────────────────────────

    #[test]
    fn test_extract_function() {
        let mut parser = CppParser::new();
        let source = r#"
int add(int a, int b) {
    return a + b;
}
"#;
        let entities = parser.extract(source, "math.cpp");
        let add = entities.iter().find(|e| e.name == "add").unwrap();
        assert_eq!(add.kind, EntityKind::Function);
        assert!(
            add.signature
                .as_ref()
                .unwrap()
                .contains("int add(int a, int b)"),
            "Signature: {:?}",
            add.signature
        );
    }

    #[test]
    fn test_extract_void_function() {
        let mut parser = CppParser::new();
        let source = r#"
void greet(const char* name) {
    printf("Hello, %s\n", name);
}
"#;
        let entities = parser.extract(source, "greet.cpp");
        let greet = entities.iter().find(|e| e.name == "greet").unwrap();
        assert_eq!(greet.kind, EntityKind::Function);
        assert!(
            greet.signature.as_ref().unwrap().contains("void"),
            "Signature: {:?}",
            greet.signature
        );
    }

    #[test]
    fn test_extract_class() {
        let mut parser = CppParser::new();
        let source = r#"
class User {
public:
    std::string name;
    int age;
};
"#;
        let entities = parser.extract(source, "user.cpp");
        let user = entities.iter().find(|e| e.name == "User").unwrap();
        assert_eq!(user.kind, EntityKind::Class);
        assert!(
            user.signature.as_ref().unwrap().contains("class User"),
            "Signature: {:?}",
            user.signature
        );
    }

    #[test]
    fn test_extract_struct() {
        let mut parser = CppParser::new();
        let source = r#"
struct Point {
    double x;
    double y;
};
"#;
        let entities = parser.extract(source, "point.cpp");
        let point = entities.iter().find(|e| e.name == "Point").unwrap();
        assert_eq!(point.kind, EntityKind::Class);
        assert!(
            point.signature.as_ref().unwrap().contains("struct Point"),
            "Signature: {:?}",
            point.signature
        );
    }

    #[test]
    fn test_extract_namespace() {
        let mut parser = CppParser::new();
        let source = r#"
namespace utils {
    int helper() {
        return 42;
    }
}
"#;
        let entities = parser.extract(source, "utils.cpp");
        let ns = entities.iter().find(|e| e.name == "utils").unwrap();
        assert_eq!(ns.kind, EntityKind::Module);
        assert!(
            ns.signature.as_ref().unwrap().contains("namespace utils"),
            "Signature: {:?}",
            ns.signature
        );

        // Function inside namespace should be scoped
        let helper = entities.iter().find(|e| e.name == "utils::helper").unwrap();
        assert_eq!(helper.kind, EntityKind::Function);
    }

    #[test]
    fn test_extract_enum() {
        let mut parser = CppParser::new();
        let source = r#"
enum Color {
    Red,
    Green,
    Blue
};
"#;
        let entities = parser.extract(source, "color.cpp");
        let color = entities.iter().find(|e| e.name == "Color").unwrap();
        assert_eq!(color.kind, EntityKind::Enum);
        assert!(
            color.signature.as_ref().unwrap().contains("enum Color"),
            "Signature: {:?}",
            color.signature
        );
    }

    #[test]
    fn test_extract_include() {
        let mut parser = CppParser::new();
        let source = r#"
#include <iostream>
#include "myheader.h"
"#;
        let entities = parser.extract(source, "main.cpp");
        let includes: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Import)
            .collect();
        assert!(
            includes.len() >= 2,
            "Should find at least 2 includes, got: {:?}",
            includes.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
        assert!(
            includes.iter().any(|e| e.name == "iostream"),
            "Should find iostream include"
        );
        assert!(
            includes.iter().any(|e| e.name == "myheader.h"),
            "Should find myheader.h include"
        );
    }

    #[test]
    fn test_extract_define() {
        let mut parser = CppParser::new();
        let source = r#"
#define MAX_SIZE 1024
#define VERSION "1.0"
"#;
        let entities = parser.extract(source, "config.h");
        let max_size = entities.iter().find(|e| e.name == "MAX_SIZE").unwrap();
        assert_eq!(max_size.kind, EntityKind::Const);
        assert!(
            max_size
                .signature
                .as_ref()
                .unwrap()
                .contains("#define MAX_SIZE"),
            "Signature: {:?}",
            max_size.signature
        );
    }

    #[test]
    fn test_extract_method_out_of_line() {
        let mut parser = CppParser::new();
        let source = r#"
class Server {
public:
    void start();
    int port();
};

void Server::start() {
    running = true;
}

int Server::port() {
    return port_;
}
"#;
        let entities = parser.extract(source, "server.cpp");

        let start = entities.iter().find(|e| e.name == "Server::start").unwrap();
        assert_eq!(start.kind, EntityKind::Method);
        assert!(
            start.signature.as_ref().unwrap().contains("Server::start"),
            "Signature: {:?}",
            start.signature
        );

        let port = entities.iter().find(|e| e.name == "Server::port").unwrap();
        assert_eq!(port.kind, EntityKind::Method);
    }

    #[test]
    fn test_extract_inline_method() {
        let mut parser = CppParser::new();
        let source = r#"
class Calculator {
public:
    int add(int a, int b) {
        return a + b;
    }
};
"#;
        let entities = parser.extract(source, "calc.cpp");

        let calc = entities.iter().find(|e| e.name == "Calculator").unwrap();
        assert_eq!(calc.kind, EntityKind::Class);

        let add = entities
            .iter()
            .find(|e| e.name == "Calculator::add")
            .unwrap();
        assert_eq!(add.kind, EntityKind::Method);
    }

    #[test]
    fn test_extract_type_alias() {
        let mut parser = CppParser::new();
        let source = r#"
using StringVec = std::vector<std::string>;
"#;
        let entities = parser.extract(source, "types.cpp");
        let alias = entities.iter().find(|e| e.name == "StringVec").unwrap();
        assert_eq!(alias.kind, EntityKind::TypeAlias);
        assert!(
            alias
                .signature
                .as_ref()
                .unwrap()
                .contains("using StringVec"),
            "Signature: {:?}",
            alias.signature
        );
    }

    #[test]
    fn test_extract_typedef() {
        let mut parser = CppParser::new();
        let source = r#"
typedef unsigned long ulong;
"#;
        let entities = parser.extract(source, "types.cpp");
        let td = entities.iter().find(|e| e.name == "ulong").unwrap();
        assert_eq!(td.kind, EntityKind::TypeAlias);
        assert!(
            td.signature.as_ref().unwrap().contains("typedef"),
            "Signature: {:?}",
            td.signature
        );
    }

    #[test]
    fn test_anonymous_namespace_skipped() {
        let mut parser = CppParser::new();
        let source = r#"
namespace {
    int internal_helper() {
        return 0;
    }
}
"#;
        let entities = parser.extract(source, "internal.cpp");
        // Anonymous namespace should be skipped (no Module entity),
        // but the function inside may still be extracted.
        assert!(
            !entities.iter().any(|e| e.kind == EntityKind::Module),
            "Should not extract anonymous namespace"
        );
    }

    #[test]
    fn test_empty_source() {
        let mut parser = CppParser::new();
        let entities = parser.extract("", "empty.cpp");
        assert!(entities.is_empty());
    }

    #[test]
    fn test_line_numbers() {
        let mut parser = CppParser::new();
        let source = r#"
int first() {
    return 1;
}

int second() {
    return 2;
}
"#;
        let entities = parser.extract(source, "lines.cpp");
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
    fn test_language_cpp() {
        let parser = CppParser::new();
        assert_eq!(parser.language(), Language::Cpp);
    }

    #[test]
    fn test_language_c() {
        let parser = CppParser::new_c();
        assert_eq!(parser.language(), Language::C);
    }

    #[test]
    fn test_nested_namespace() {
        let mut parser = CppParser::new();
        let source = r#"
namespace outer {
    namespace inner {
        void deep_func() {}
    }
}
"#;
        let entities = parser.extract(source, "nested.cpp");

        let outer = entities.iter().find(|e| e.name == "outer").unwrap();
        assert_eq!(outer.kind, EntityKind::Module);

        let inner = entities.iter().find(|e| e.name == "outer::inner").unwrap();
        assert_eq!(inner.kind, EntityKind::Module);

        let func = entities
            .iter()
            .find(|e| e.name == "outer::inner::deep_func")
            .unwrap();
        assert_eq!(func.kind, EntityKind::Function);
    }

    #[test]
    fn test_template_class() {
        let mut parser = CppParser::new();
        let source = r#"
template<typename T>
class Container {
public:
    T value;
    T get() { return value; }
};
"#;
        let entities = parser.extract(source, "container.hpp");

        let container = entities.iter().find(|e| e.name == "Container").unwrap();
        assert_eq!(container.kind, EntityKind::Class);

        // Method inside template class
        let get = entities
            .iter()
            .find(|e| e.name == "Container::get")
            .unwrap();
        assert_eq!(get.kind, EntityKind::Method);
    }

    #[test]
    fn test_enum_class() {
        let mut parser = CppParser::new();
        let source = r#"
enum class Direction {
    North,
    South,
    East,
    West
};
"#;
        let entities = parser.extract(source, "direction.cpp");
        let dir = entities.iter().find(|e| e.name == "Direction").unwrap();
        assert_eq!(dir.kind, EntityKind::Enum);
    }

    #[test]
    fn test_realistic_cpp_header() {
        let mut parser = CppParser::new();
        let source = r#"
#include <string>
#include <vector>
#include <memory>

#define MAX_CONNECTIONS 100
#define API_VERSION "2.0"

namespace net {

enum class Protocol {
    TCP,
    UDP,
    HTTP
};

using Callback = std::function<void(int)>;

class Connection {
public:
    Connection(const std::string& host, int port);
    ~Connection();

    void connect();
    void disconnect();
    bool is_connected() const;

private:
    std::string host_;
    int port_;
};

class Server {
public:
    void listen(int port) {
        port_ = port;
    }

    void stop() {
        running_ = false;
    }

private:
    int port_;
    bool running_;
    std::vector<std::unique_ptr<Connection>> connections_;
};

struct Config {
    std::string host;
    int port;
    int max_connections;
};

} // namespace net
"#;
        let entities = parser.extract(source, "server.hpp");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        // Includes
        assert!(names.contains(&"string"), "Should find string include");
        assert!(names.contains(&"vector"), "Should find vector include");
        assert!(names.contains(&"memory"), "Should find memory include");

        // Defines
        assert!(
            names.contains(&"MAX_CONNECTIONS"),
            "Should find MAX_CONNECTIONS define"
        );
        assert!(
            names.contains(&"API_VERSION"),
            "Should find API_VERSION define"
        );

        // Namespace
        assert!(names.contains(&"net"), "Should find net namespace");

        // Enum
        assert!(
            names.contains(&"net::Protocol"),
            "Should find Protocol enum in net: got {:?}",
            names
        );

        // Type alias
        assert!(
            names.contains(&"net::Callback"),
            "Should find Callback type alias in net: got {:?}",
            names
        );

        // Classes
        assert!(
            names.contains(&"net::Connection"),
            "Should find Connection class in net"
        );
        assert!(
            names.contains(&"net::Server"),
            "Should find Server class in net"
        );

        // Methods inside class bodies
        assert!(
            names.contains(&"net::Server::listen"),
            "Should find Server::listen method: got {:?}",
            names
        );
        assert!(
            names.contains(&"net::Server::stop"),
            "Should find Server::stop method: got {:?}",
            names
        );

        // Struct
        assert!(
            names.contains(&"net::Config"),
            "Should find Config struct in net"
        );

        // Verify reasonable entity count
        assert!(
            entities.len() >= 10,
            "Expected >= 10 entities for a realistic C++ header, got {}",
            entities.len()
        );
    }

    // ── C tests ────────────────────────────────────────────────────

    #[test]
    fn test_c_function() {
        let mut parser = CppParser::new_c();
        let source = r#"
int add(int a, int b) {
    return a + b;
}
"#;
        let entities = parser.extract(source, "math.c");
        let add = entities.iter().find(|e| e.name == "add").unwrap();
        assert_eq!(add.kind, EntityKind::Function);
    }

    #[test]
    fn test_c_struct() {
        let mut parser = CppParser::new_c();
        let source = r#"
struct Point {
    int x;
    int y;
};
"#;
        let entities = parser.extract(source, "point.c");
        let point = entities.iter().find(|e| e.name == "Point").unwrap();
        assert_eq!(point.kind, EntityKind::Class);
    }

    #[test]
    fn test_c_typedef() {
        let mut parser = CppParser::new_c();
        let source = r#"
typedef unsigned int uint;
"#;
        let entities = parser.extract(source, "types.c");
        let uint = entities.iter().find(|e| e.name == "uint").unwrap();
        assert_eq!(uint.kind, EntityKind::TypeAlias);
    }

    #[test]
    fn test_c_include() {
        let mut parser = CppParser::new_c();
        let source = r#"
#include <stdio.h>
#include <stdlib.h>
"#;
        let entities = parser.extract(source, "main.c");
        let includes: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Import)
            .collect();
        assert!(
            includes.len() >= 2,
            "Should find at least 2 C includes, got: {:?}",
            includes.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_c_enum() {
        let mut parser = CppParser::new_c();
        let source = r#"
enum Status {
    OK,
    ERROR,
    PENDING
};
"#;
        let entities = parser.extract(source, "status.c");
        let status = entities.iter().find(|e| e.name == "Status").unwrap();
        assert_eq!(status.kind, EntityKind::Enum);
    }

    #[test]
    fn test_c_define() {
        let mut parser = CppParser::new_c();
        let source = r#"
#define BUFFER_SIZE 4096
"#;
        let entities = parser.extract(source, "config.h");
        let buf = entities.iter().find(|e| e.name == "BUFFER_SIZE").unwrap();
        assert_eq!(buf.kind, EntityKind::Const);
    }
}
