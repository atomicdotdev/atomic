//! Python parser for semantic analysis.
//!
//! Extracts functions, classes, methods, decorators, and module-level variables
//! from Python source code using tree-sitter-python.
//!
//! # Supported Entity Types
//!
//! | Python Construct | EntityKind | Notes |
//! |-----------------|------------|-------|
//! | `def greet():` | Function | Top-level function |
//! | `async def fetch():` | Function | Async function |
//! | `class User:` | Class | Class definition |
//! | `def method(self):` | Method | Method inside a class |
//! | `X = 42` | Variable | Module-level assignment |
//! | `X: int = 42` | Variable | Annotated assignment |
//! | `import os` | Import | Import statement |
//! | `from x import y` | Import | From-import statement |

use super::{Language, LanguageParser};
use crate::entity::{Entity, EntityKind};
use tree_sitter::{Node, Parser};

/// Python AST entity extractor using tree-sitter.
pub struct PythonParser {
    parser: Parser,
}

impl PythonParser {
    /// Create a new Python parser.
    pub fn new() -> Self {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .expect("Failed to load Python grammar");
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
            "function_definition" => {
                if let Some(entity) = self.extract_function(node, source, file_path, in_class) {
                    entities.push(entity);
                }
            }
            "class_definition" => {
                if let Some(entity) = self.extract_class(node, source, file_path) {
                    let class_name = entity.name.clone();
                    entities.push(entity);

                    // Walk class body for methods
                    if let Some(body) = node.child_by_field_name("body") {
                        let mut cursor = body.walk();
                        for child in body.children(&mut cursor) {
                            self.walk_tree(&child, source, file_path, entities, Some(&class_name));
                        }
                    }
                    return; // Don't recurse into class again below
                }
            }
            "expression_statement" => {
                // Module-level assignments: `X = 42` or `X: int = 42`
                if in_class.is_none() {
                    if let Some(child) = node.child(0) {
                        if child.kind() == "assignment" || child.kind() == "augmented_assignment" {
                            if let Some(entity) = self.extract_assignment(&child, source, file_path)
                            {
                                entities.push(entity);
                            }
                        }
                    }
                }
            }
            "import_statement" | "import_from_statement" => {
                if let Some(entity) = self.extract_import(node, source, file_path) {
                    entities.push(entity);
                }
            }
            "decorated_definition" => {
                // Decorated functions/classes — walk into the inner definition
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "function_definition" || child.kind() == "class_definition" {
                        self.walk_tree(&child, source, file_path, entities, in_class);
                    }
                }
                return; // Don't double-recurse
            }
            _ => {}
        }

        // Recurse into children (but not class bodies — handled above)
        if node.kind() != "class_definition" {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                self.walk_tree(&child, source, file_path, entities, in_class);
            }
        }
    }

    /// Extract a function or method definition.
    fn extract_function(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        in_class: Option<&str>,
    ) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        // Skip dunder methods that are noise (__module__, __qualname__, etc.)
        // but keep __init__, __str__, __repr__, __eq__, etc.
        if name.starts_with("__") && name.ends_with("__") {
            let keep = [
                "__init__",
                "__new__",
                "__del__",
                "__str__",
                "__repr__",
                "__eq__",
                "__ne__",
                "__lt__",
                "__le__",
                "__gt__",
                "__ge__",
                "__hash__",
                "__bool__",
                "__len__",
                "__iter__",
                "__next__",
                "__contains__",
                "__getitem__",
                "__setitem__",
                "__delitem__",
                "__call__",
                "__enter__",
                "__exit__",
                "__aenter__",
                "__aexit__",
                "__await__",
                "__aiter__",
                "__anext__",
            ];
            if !keep.contains(&name.as_str()) {
                return None;
            }
        }

        let kind = if in_class.is_some() {
            EntityKind::Method
        } else {
            EntityKind::Function
        };

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        // Build signature
        let signature = self.build_function_signature(node, source);

        // Check for decorators that indicate export (@property, @staticmethod, @classmethod)
        let _is_decorated = node
            .parent()
            .map(|p| p.kind() == "decorated_definition")
            .unwrap_or(false);

        // In Python, "exported" means not prefixed with underscore
        let exported = !name.starts_with('_') || name.starts_with("__");

        let mut entity = Entity::new(name, kind, file_path, line, end_line);
        if let Some(sig) = signature {
            entity = entity.with_signature(sig);
        }
        if exported {
            entity.exported = true;
        }

        Some(entity)
    }

    /// Extract a class definition.
    fn extract_class(&self, node: &Node, source: &str, file_path: &str) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        // Build class signature with base classes
        let signature = self.build_class_signature(node, source);

        let exported = !name.starts_with('_');

        let mut entity = Entity::new(name, EntityKind::Class, file_path, line, end_line);
        if let Some(sig) = signature {
            entity = entity.with_signature(sig);
        }
        if exported {
            entity.exported = true;
        }

        Some(entity)
    }

    /// Extract a module-level assignment (`X = 42` or `X: int = 42`).
    fn extract_assignment(&self, node: &Node, source: &str, file_path: &str) -> Option<Entity> {
        // Get the left side of the assignment
        let left = node.child_by_field_name("left")?;

        // Only extract simple name assignments, not tuple/attribute assignments
        if left.kind() != "identifier" {
            return None;
        }

        let name = self.node_text(&left, source);

        // Skip private variables
        if name.starts_with('_') && !name.starts_with("__") {
            return None;
        }

        // Skip common non-semantic assignments
        if name == "logger" || name == "log" {
            return None;
        }

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        // Check if it's ALL_CAPS (constant-like)
        let is_const = name.chars().all(|c| c.is_uppercase() || c == '_');
        let kind = if is_const {
            EntityKind::Const
        } else {
            EntityKind::Variable
        };

        let exported = !name.starts_with('_');
        let sig = self.node_text(node, source);

        let mut entity = Entity::new(name, kind, file_path, line, end_line);
        entity = entity.with_signature(sig);
        if exported {
            entity.exported = true;
        }

        Some(entity)
    }

    /// Extract an import statement.
    fn extract_import(&self, node: &Node, source: &str, file_path: &str) -> Option<Entity> {
        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let text = self.node_text(node, source);

        // Extract the module name from `import X` or `from X import Y`
        let name = if node.kind() == "import_from_statement" {
            // from X import Y → name is "X"
            node.child_by_field_name("module_name")
                .map(|n| self.node_text(&n, source))
                .unwrap_or_else(|| text.clone())
        } else {
            // import X → name is "X"
            let mut cursor = node.walk();
            let children: Vec<_> = node.children(&mut cursor).collect();
            children
                .iter()
                .find(|c| c.kind() == "dotted_name")
                .map(|n| self.node_text(n, source))
                .unwrap_or_else(|| text.clone())
        };

        let mut entity = Entity::new(name, EntityKind::Import, file_path, line, end_line);
        entity = entity.with_signature(text);

        Some(entity)
    }

    /// Build a function signature string.
    fn build_function_signature(&self, node: &Node, source: &str) -> Option<String> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        // Check if async
        let is_async = node
            .parent()
            .map(|p| {
                let mut cursor = p.walk();
                let children: Vec<_> = p.children(&mut cursor).collect();
                children
                    .iter()
                    .any(|c| c.kind() == "async" || self.node_text(c, source) == "async")
            })
            .unwrap_or(false)
            || {
                let mut cursor = node.walk();
                let children: Vec<_> = node.children(&mut cursor).collect();
                children
                    .iter()
                    .any(|c| self.node_text(c, source) == "async")
            };

        let params = node
            .child_by_field_name("parameters")
            .map(|n| self.node_text(&n, source))
            .unwrap_or_else(|| "()".to_string());

        let return_type = node
            .child_by_field_name("return_type")
            .map(|n| format!(" -> {}", self.node_text(&n, source)));

        let prefix = if is_async { "async def" } else { "def" };

        Some(format!(
            "{} {}{}{}",
            prefix,
            name,
            params,
            return_type.unwrap_or_default()
        ))
    }

    /// Build a class signature string.
    fn build_class_signature(&self, node: &Node, source: &str) -> Option<String> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        // Check for base classes in superclasses/argument_list
        let bases = node
            .child_by_field_name("superclasses")
            .map(|n| self.node_text(&n, source));

        match bases {
            Some(b) if !b.is_empty() && b != "()" => Some(format!("class {}{}", name, b)),
            _ => Some(format!("class {}", name)),
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
}

impl Default for PythonParser {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageParser for PythonParser {
    fn language(&self) -> Language {
        Language::Python
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
        let mut parser = PythonParser::new();
        let source = r#"
def greet(name: str) -> str:
    return f"Hello, {name}!"
"#;
        let entities = parser.extract(source, "main.py");
        assert!(!entities.is_empty());
        let greet = entities.iter().find(|e| e.name == "greet").unwrap();
        assert_eq!(greet.kind, EntityKind::Function);
        assert!(greet.exported);
        assert!(greet.signature.as_ref().unwrap().contains("def greet"));
    }

    #[test]
    fn test_extract_async_function() {
        let mut parser = PythonParser::new();
        let source = r#"
async def fetch_data(url: str) -> dict:
    pass
"#;
        let entities = parser.extract(source, "main.py");
        let fetch = entities.iter().find(|e| e.name == "fetch_data");
        assert!(fetch.is_some(), "Should find fetch_data");
    }

    #[test]
    fn test_extract_class() {
        let mut parser = PythonParser::new();
        let source = r#"
class UserService:
    def __init__(self, db):
        self.db = db

    def get_user(self, user_id: str) -> dict:
        return self.db.find(user_id)

    def _private_method(self):
        pass
"#;
        let entities = parser.extract(source, "service.py");

        let service = entities
            .iter()
            .find(|e| e.name == "UserService")
            .expect("Should find UserService");
        assert_eq!(service.kind, EntityKind::Class);
        assert!(service.exported);

        let init = entities.iter().find(|e| e.name == "__init__");
        assert!(init.is_some(), "Should find __init__");
        assert_eq!(init.unwrap().kind, EntityKind::Method);

        let get_user = entities.iter().find(|e| e.name == "get_user");
        assert!(get_user.is_some(), "Should find get_user");
        assert_eq!(get_user.unwrap().kind, EntityKind::Method);

        let private = entities.iter().find(|e| e.name == "_private_method");
        assert!(private.is_some(), "Should find _private_method");
        assert!(
            !private.unwrap().exported,
            "Private method should not be exported"
        );
    }

    #[test]
    fn test_extract_class_with_bases() {
        let mut parser = PythonParser::new();
        let source = r#"
class Admin(User, Serializable):
    pass
"#;
        let entities = parser.extract(source, "models.py");
        let admin = entities.iter().find(|e| e.name == "Admin").unwrap();
        assert!(
            admin.signature.as_ref().unwrap().contains("User"),
            "Signature should include base classes: {:?}",
            admin.signature
        );
    }

    #[test]
    fn test_extract_module_constants() {
        let mut parser = PythonParser::new();
        let source = r#"
MAX_RETRIES = 3
API_URL = "https://api.example.com"
DEFAULT_TIMEOUT = 30
"#;
        let entities = parser.extract(source, "config.py");

        let max = entities.iter().find(|e| e.name == "MAX_RETRIES");
        assert!(max.is_some(), "Should find MAX_RETRIES");
        assert_eq!(max.unwrap().kind, EntityKind::Const);

        let url = entities.iter().find(|e| e.name == "API_URL");
        assert!(url.is_some(), "Should find API_URL");
    }

    #[test]
    fn test_extract_module_variables() {
        let mut parser = PythonParser::new();
        let source = r#"
default_config = {"timeout": 30}
"#;
        let entities = parser.extract(source, "config.py");
        let cfg = entities.iter().find(|e| e.name == "default_config");
        assert!(cfg.is_some(), "Should find default_config");
        assert_eq!(cfg.unwrap().kind, EntityKind::Variable);
    }

    #[test]
    fn test_extract_imports() {
        let mut parser = PythonParser::new();
        let source = r#"
import os
from pathlib import Path
from typing import Optional, List
"#;
        let entities = parser.extract(source, "main.py");
        let imports: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Import)
            .collect();
        assert!(
            imports.len() >= 2,
            "Should find imports, got: {:?}",
            imports.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_private_function_not_exported() {
        let mut parser = PythonParser::new();
        let source = r#"
def _private_helper():
    pass

def public_function():
    pass
"#;
        let entities = parser.extract(source, "utils.py");

        let private = entities.iter().find(|e| e.name == "_private_helper");
        assert!(private.is_some());
        assert!(!private.unwrap().exported);

        let public = entities.iter().find(|e| e.name == "public_function");
        assert!(public.is_some());
        assert!(public.unwrap().exported);
    }

    #[test]
    fn test_decorated_function() {
        let mut parser = PythonParser::new();
        let source = r#"
@app.route("/api/users")
def get_users():
    pass

@staticmethod
def helper():
    pass
"#;
        let entities = parser.extract(source, "routes.py");
        assert!(
            entities.iter().any(|e| e.name == "get_users"),
            "Should find decorated function get_users"
        );
    }

    #[test]
    fn test_decorated_class() {
        let mut parser = PythonParser::new();
        let source = r#"
@dataclass
class Config:
    host: str
    port: int = 8080
"#;
        let entities = parser.extract(source, "config.py");
        assert!(entities
            .iter()
            .any(|e| e.name == "Config" && e.kind == EntityKind::Class));
    }

    #[test]
    fn test_empty_source() {
        let mut parser = PythonParser::new();
        let entities = parser.extract("", "empty.py");
        assert!(entities.is_empty());
    }

    #[test]
    fn test_comment_only() {
        let mut parser = PythonParser::new();
        let source = r#"
# This is a comment
# Another comment
"#;
        let entities = parser.extract(source, "comments.py");
        assert!(entities.is_empty());
    }

    #[test]
    fn test_line_numbers() {
        let mut parser = PythonParser::new();
        let source = r#"
def first():
    pass

def second():
    pass
"#;
        let entities = parser.extract(source, "lines.py");
        let first = entities.iter().find(|e| e.name == "first").unwrap();
        let second = entities.iter().find(|e| e.name == "second").unwrap();
        assert!(first.line < second.line, "first should be before second");
        assert!(
            first.end_line < second.line,
            "first should end before second starts"
        );
    }

    #[test]
    fn test_multiple_entities() {
        let mut parser = PythonParser::new();
        let source = r#"
MAX_SIZE = 100

def validate(data: dict) -> bool:
    return len(data) > 0

class Validator:
    def __init__(self):
        self.rules = []

    def add_rule(self, rule):
        self.rules.append(rule)

    def validate(self, data):
        return all(r(data) for r in self.rules)
"#;
        let entities = parser.extract(source, "validation.py");

        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"MAX_SIZE"), "Should find MAX_SIZE");
        assert!(names.contains(&"validate"), "Should find validate function");
        assert!(names.contains(&"Validator"), "Should find Validator class");
        assert!(names.contains(&"__init__"), "Should find __init__ method");
        assert!(names.contains(&"add_rule"), "Should find add_rule method");
    }

    #[test]
    fn test_language() {
        let parser = PythonParser::new();
        assert_eq!(parser.language(), Language::Python);
    }

    #[test]
    fn test_realistic_flask_app() {
        let mut parser = PythonParser::new();
        let source = r#"
from flask import Flask, jsonify, request
from typing import Optional

app = Flask(__name__)

MAX_PAGE_SIZE = 100
DEFAULT_PAGE_SIZE = 20

class UserNotFoundError(Exception):
    pass

def get_pagination() -> tuple[int, int]:
    page = request.args.get("page", 1, type=int)
    size = min(request.args.get("size", DEFAULT_PAGE_SIZE, type=int), MAX_PAGE_SIZE)
    return page, size

@app.route("/api/users")
def list_users():
    page, size = get_pagination()
    return jsonify({"users": [], "page": page, "size": size})

@app.route("/api/users/<user_id>")
def get_user(user_id: str):
    return jsonify({"id": user_id, "name": "Alice"})

class UserService:
    def __init__(self, db):
        self.db = db

    async def find_user(self, user_id: str) -> Optional[dict]:
        return await self.db.users.find_one({"_id": user_id})

    async def create_user(self, data: dict) -> dict:
        result = await self.db.users.insert_one(data)
        return {**data, "_id": str(result.inserted_id)}
"#;
        let entities = parser.extract(source, "app.py");

        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        // Functions
        assert!(
            names.contains(&"get_pagination"),
            "Should find get_pagination"
        );
        assert!(names.contains(&"list_users"), "Should find list_users");
        assert!(names.contains(&"get_user"), "Should find get_user");

        // Classes
        assert!(
            names.contains(&"UserNotFoundError"),
            "Should find UserNotFoundError"
        );
        assert!(names.contains(&"UserService"), "Should find UserService");

        // Methods
        assert!(names.contains(&"__init__"), "Should find __init__");
        assert!(names.contains(&"find_user"), "Should find find_user");
        assert!(names.contains(&"create_user"), "Should find create_user");

        // Constants
        assert!(
            names.contains(&"MAX_PAGE_SIZE"),
            "Should find MAX_PAGE_SIZE"
        );
        assert!(
            names.contains(&"DEFAULT_PAGE_SIZE"),
            "Should find DEFAULT_PAGE_SIZE"
        );

        // Verify entity count is reasonable
        assert!(
            entities.len() >= 10,
            "Expected >= 10 entities for a realistic Flask app, got {}",
            entities.len()
        );
    }
}
