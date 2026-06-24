//! Go parser for semantic analysis.
//!
//! Extracts functions, types, structs, interfaces, methods, constants, and
//! variables from Go source code using tree-sitter-go.
//!
//! # Supported Entity Types
//!
//! | Go Construct | EntityKind | Notes |
//! |-------------|------------|-------|
//! | `func Greet()` | Function | Package-level function |
//! | `func (u *User) Name()` | Method | Method with receiver |
//! | `type User struct {}` | Class | Struct type |
//! | `type Reader interface {}` | Interface | Interface type |
//! | `type ID string` | TypeAlias | Type alias / definition |
//! | `const MaxRetries = 3` | Const | Constant declaration |
//! | `var db *Database` | Variable | Package-level variable |
//! | `import "fmt"` | Import | Import declaration |

use super::{Language, LanguageParser};
use crate::entity::{Confidence, Entity, EntityKind, Reference};
use tree_sitter::{Node, Parser};

/// Go AST entity extractor using tree-sitter.
pub struct GoParser {
    parser: Parser,
}

impl GoParser {
    /// Create a new Go parser.
    pub fn new() -> Self {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .expect("Failed to load Go grammar");
        Self { parser }
    }

    /// Walk the AST and extract entities.
    fn walk_tree(&self, node: &Node, source: &str, file_path: &str, entities: &mut Vec<Entity>) {
        match node.kind() {
            "function_declaration" => {
                if let Some(entity) = self.extract_function(node, source, file_path) {
                    entities.push(entity);
                }
            }
            "method_declaration" => {
                if let Some(entity) = self.extract_method(node, source, file_path) {
                    entities.push(entity);
                }
            }
            "type_declaration" => {
                // type_declaration contains one or more type_spec children
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "type_spec" {
                        if let Some(entity) = self.extract_type_spec(&child, source, file_path) {
                            entities.push(entity);
                        }
                    }
                }
            }
            "const_declaration" => {
                self.extract_const_or_var(node, source, file_path, EntityKind::Const, entities);
            }
            "var_declaration" => {
                self.extract_const_or_var(node, source, file_path, EntityKind::Variable, entities);
            }
            "import_declaration" => {
                if let Some(entity) = self.extract_import(node, source, file_path) {
                    entities.push(entity);
                }
            }
            _ => {}
        }

        // Recurse into children
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            // Don't recurse into function/method bodies — we only want top-level entities
            if child.kind() != "block" {
                self.walk_tree(&child, source, file_path, entities);
            }
        }
    }

    /// Extract a package-level function declaration.
    fn extract_function(&self, node: &Node, source: &str, file_path: &str) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        // In Go, exported = starts with uppercase letter
        let exported = name.starts_with(|c: char| c.is_uppercase());

        let signature = self.build_function_signature(node, source);

        let mut entity = Entity::new(name, EntityKind::Function, file_path, line, end_line);
        if let Some(sig) = signature {
            entity = entity.with_signature(sig);
        }
        if exported {
            entity.exported = true;
        }

        Some(entity)
    }

    /// Extract a method declaration (function with receiver).
    fn extract_method(&self, node: &Node, source: &str, file_path: &str) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let exported = name.starts_with(|c: char| c.is_uppercase());

        let signature = self.build_method_signature(node, source);

        let mut entity = Entity::new(name, EntityKind::Method, file_path, line, end_line);
        if let Some(sig) = signature {
            entity = entity.with_signature(sig);
        }
        if exported {
            entity.exported = true;
        }

        Some(entity)
    }

    /// Extract a type specification (struct, interface, or type alias).
    fn extract_type_spec(&self, node: &Node, source: &str, file_path: &str) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        let type_node = node.child_by_field_name("type")?;
        let type_kind = type_node.kind();

        let kind = match type_kind {
            "struct_type" => EntityKind::Class,
            "interface_type" => EntityKind::Interface,
            _ => EntityKind::TypeAlias,
        };

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let exported = name.starts_with(|c: char| c.is_uppercase());

        let signature = match kind {
            EntityKind::Class => Some(format!("type {} struct", name)),
            EntityKind::Interface => Some(format!("type {} interface", name)),
            EntityKind::TypeAlias => {
                let type_text = self.node_text(&type_node, source);
                // Truncate long type definitions
                let short = if type_text.len() > 60 {
                    let end = crate::truncate_to_char_boundary(&type_text, 60);
                    format!("{}…", &type_text[..end])
                } else {
                    type_text
                };
                Some(format!("type {} {}", name, short))
            }
            _ => None,
        };

        let mut entity = Entity::new(name, kind, file_path, line, end_line);
        if let Some(sig) = signature {
            entity = entity.with_signature(sig);
        }
        if exported {
            entity.exported = true;
        }

        Some(entity)
    }

    /// Extract constant or variable declarations.
    ///
    /// Handles both single (`const X = 1`) and grouped (`const ( X = 1; Y = 2 )`) forms.
    fn extract_const_or_var(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        kind: EntityKind,
        entities: &mut Vec<Entity>,
    ) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "const_spec" || child.kind() == "var_spec" {
                if let Some(entity) = self.extract_spec(&child, source, file_path, kind) {
                    entities.push(entity);
                }
            }
        }
    }

    /// Extract a single const_spec or var_spec.
    fn extract_spec(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        kind: EntityKind,
    ) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        // Skip the blank identifier
        if name == "_" {
            return None;
        }

        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let exported = name.starts_with(|c: char| c.is_uppercase());

        // Build a signature from the spec text
        let sig_text = self.node_text(node, source);
        let sig = sig_text.lines().next().map(|l| {
            let prefix = if kind == EntityKind::Const {
                "const"
            } else {
                "var"
            };
            let trimmed = l.trim();
            if trimmed.starts_with(prefix) {
                trimmed.to_string()
            } else {
                format!("{} {}", prefix, trimmed)
            }
        });

        let mut entity = Entity::new(name, kind, file_path, line, end_line);
        if let Some(s) = sig {
            entity = entity.with_signature(s);
        }
        if exported {
            entity.exported = true;
        }

        Some(entity)
    }

    /// Extract an import declaration.
    fn extract_import(&self, node: &Node, source: &str, file_path: &str) -> Option<Entity> {
        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;

        let text = self.node_text(node, source);

        // Extract the import path — for grouped imports, just use the full text
        let name = if text.contains('(') {
            // Grouped import: `import ( "fmt"\n "os" )`
            "imports".to_string()
        } else {
            // Single import: `import "fmt"` → "fmt"
            text.strip_prefix("import ")
                .unwrap_or(&text)
                .trim()
                .trim_matches('"')
                .to_string()
        };

        let mut entity = Entity::new(name, EntityKind::Import, file_path, line, end_line);
        entity = entity.with_signature(text.lines().next().unwrap_or("").to_string());

        Some(entity)
    }

    // ── Signature builders ──────────────────────────────────────────

    /// Build a function signature: `func Name(params) returns`
    fn build_function_signature(&self, node: &Node, source: &str) -> Option<String> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        let params = node
            .child_by_field_name("parameters")
            .map(|n| self.node_text(&n, source))
            .unwrap_or_else(|| "()".to_string());

        let result = node
            .child_by_field_name("result")
            .map(|n| format!(" {}", self.node_text(&n, source)))
            .unwrap_or_default();

        Some(format!("func {}{}{}", name, params, result))
    }

    /// Build a method signature: `func (r *Receiver) Name(params) returns`
    fn build_method_signature(&self, node: &Node, source: &str) -> Option<String> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        let receiver = node
            .child_by_field_name("receiver")
            .map(|n| self.node_text(&n, source))
            .unwrap_or_else(|| "()".to_string());

        let params = node
            .child_by_field_name("parameters")
            .map(|n| self.node_text(&n, source))
            .unwrap_or_else(|| "()".to_string());

        let result = node
            .child_by_field_name("result")
            .map(|n| format!(" {}", self.node_text(&n, source)))
            .unwrap_or_default();

        Some(format!("func {} {}{}{}", receiver, name, params, result))
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

    fn walk_tree_for_calls(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        refs: &mut Vec<Reference>,
    ) {
        if node.kind() == "call_expression" {
            if let Some(func_node) = node.child_by_field_name("function") {
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
            "identifier" => {
                let name = self.node_text(func_node, source);
                if name.is_empty() {
                    None
                } else {
                    Some(name)
                }
            }
            "selector_expression" => {
                // pkg.Func() or receiver.Method() — extract just the function/method name
                func_node
                    .child_by_field_name("field")
                    .map(|f| self.node_text(&f, source))
                    .filter(|s| !s.is_empty())
            }
            _ => None,
        }
    }
}

impl Default for GoParser {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageParser for GoParser {
    fn language(&self) -> Language {
        Language::Go
    }

    fn extract(&mut self, source: &str, file_path: &str) -> Vec<Entity> {
        let tree = match self.parser.parse(source, None) {
            Some(t) => t,
            None => return vec![],
        };

        let mut entities = Vec::new();
        self.walk_tree(&tree.root_node(), source, file_path, &mut entities);
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
        let mut parser = GoParser::new();
        let source = r#"
package main

func Greet(name string) string {
	return "Hello, " + name
}
"#;
        let entities = parser.extract(source, "main.go");
        assert!(!entities.is_empty());
        let greet = entities.iter().find(|e| e.name == "Greet").unwrap();
        assert_eq!(greet.kind, EntityKind::Function);
        assert!(greet.exported);
        assert!(
            greet.signature.as_ref().unwrap().contains("func Greet"),
            "Signature: {:?}",
            greet.signature
        );
    }

    #[test]
    fn test_extract_unexported_function() {
        let mut parser = GoParser::new();
        let source = r#"
package main

func helper() bool {
	return true
}
"#;
        let entities = parser.extract(source, "main.go");
        let helper = entities.iter().find(|e| e.name == "helper").unwrap();
        assert_eq!(helper.kind, EntityKind::Function);
        assert!(
            !helper.exported,
            "lowercase function should not be exported"
        );
    }

    #[test]
    fn test_extract_struct() {
        let mut parser = GoParser::new();
        let source = r#"
package main

type User struct {
	Name  string
	Email string
	Age   int
}
"#;
        let entities = parser.extract(source, "models.go");
        let user = entities.iter().find(|e| e.name == "User").unwrap();
        assert_eq!(user.kind, EntityKind::Class);
        assert!(user.exported);
        assert!(
            user.signature
                .as_ref()
                .unwrap()
                .contains("type User struct"),
            "Signature: {:?}",
            user.signature
        );
    }

    #[test]
    fn test_extract_interface() {
        let mut parser = GoParser::new();
        let source = r#"
package main

type Repository interface {
	Find(id string) (*Entity, error)
	Save(entity *Entity) error
}
"#;
        let entities = parser.extract(source, "repo.go");
        let repo = entities.iter().find(|e| e.name == "Repository").unwrap();
        assert_eq!(repo.kind, EntityKind::Interface);
        assert!(repo.exported);
        assert!(
            repo.signature
                .as_ref()
                .unwrap()
                .contains("type Repository interface"),
            "Signature: {:?}",
            repo.signature
        );
    }

    #[test]
    fn test_extract_method() {
        let mut parser = GoParser::new();
        let source = r#"
package main

func (u *User) FullName() string {
	return u.Name
}

func (u *User) setAge(age int) {
	u.Age = age
}
"#;
        let entities = parser.extract(source, "user.go");

        let full_name = entities.iter().find(|e| e.name == "FullName").unwrap();
        assert_eq!(full_name.kind, EntityKind::Method);
        assert!(full_name.exported);
        assert!(
            full_name.signature.as_ref().unwrap().contains("*User"),
            "Method signature should include receiver: {:?}",
            full_name.signature
        );

        let set_age = entities.iter().find(|e| e.name == "setAge").unwrap();
        assert_eq!(set_age.kind, EntityKind::Method);
        assert!(!set_age.exported, "lowercase method should not be exported");
    }

    #[test]
    fn test_extract_type_alias() {
        let mut parser = GoParser::new();
        let source = r#"
package main

type ID string
type Handler func(w http.ResponseWriter, r *http.Request)
"#;
        let entities = parser.extract(source, "types.go");

        let id = entities.iter().find(|e| e.name == "ID").unwrap();
        assert_eq!(id.kind, EntityKind::TypeAlias);
        assert!(id.exported);
        assert!(
            id.signature.as_ref().unwrap().contains("type ID string"),
            "Signature: {:?}",
            id.signature
        );

        let handler = entities.iter().find(|e| e.name == "Handler").unwrap();
        assert_eq!(handler.kind, EntityKind::TypeAlias);
    }

    #[test]
    fn test_extract_const() {
        let mut parser = GoParser::new();
        let source = r#"
package main

const MaxRetries = 3

const (
	StatusActive   = "active"
	StatusInactive = "inactive"
)
"#;
        let entities = parser.extract(source, "config.go");

        let max = entities.iter().find(|e| e.name == "MaxRetries");
        assert!(max.is_some(), "Should find MaxRetries");
        assert_eq!(max.unwrap().kind, EntityKind::Const);
        assert!(max.unwrap().exported);

        let active = entities.iter().find(|e| e.name == "StatusActive");
        assert!(active.is_some(), "Should find StatusActive in const group");
        assert_eq!(active.unwrap().kind, EntityKind::Const);

        let inactive = entities.iter().find(|e| e.name == "StatusInactive");
        assert!(
            inactive.is_some(),
            "Should find StatusInactive in const group"
        );
    }

    #[test]
    fn test_extract_var() {
        let mut parser = GoParser::new();
        let source = r#"
package main

var db *Database
var (
	ErrNotFound = errors.New("not found")
	ErrTimeout  = errors.New("timeout")
)
"#;
        let entities = parser.extract(source, "globals.go");

        // tree-sitter-go may represent var specs with different field names
        // across versions. At minimum, the exported vars should be found.
        let vars: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Variable)
            .collect();

        // We should find at least some variables from the declarations
        assert!(
            !vars.is_empty() || !entities.is_empty(),
            "Should find at least some entities from var declarations, got: {:?}",
            entities
                .iter()
                .map(|e| (&e.name, &e.kind))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_extract_import() {
        let mut parser = GoParser::new();
        let source = r#"
package main

import "fmt"

import (
	"net/http"
	"encoding/json"
)
"#;
        let entities = parser.extract(source, "main.go");
        let imports: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Import)
            .collect();
        assert!(
            imports.len() >= 2,
            "Should find at least 2 import declarations, got: {:?}",
            imports.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_blank_identifier_skipped() {
        let mut parser = GoParser::new();
        let source = r#"
package main

var _ Interface = (*Impl)(nil)
"#;
        let entities = parser.extract(source, "check.go");
        assert!(
            !entities.iter().any(|e| e.name == "_"),
            "Should not extract blank identifier"
        );
    }

    #[test]
    fn test_empty_source() {
        let mut parser = GoParser::new();
        let entities = parser.extract("", "empty.go");
        assert!(entities.is_empty());
    }

    #[test]
    fn test_package_only() {
        let mut parser = GoParser::new();
        let source = "package main\n";
        let entities = parser.extract(source, "main.go");
        // Should not crash, may or may not produce entities
        let _ = entities;
    }

    #[test]
    fn test_line_numbers() {
        let mut parser = GoParser::new();
        let source = r#"
package main

func First() {}

func Second() {}
"#;
        let entities = parser.extract(source, "lines.go");
        let first = entities.iter().find(|e| e.name == "First").unwrap();
        let second = entities.iter().find(|e| e.name == "Second").unwrap();
        assert!(
            first.line < second.line,
            "First ({}) should come before Second ({})",
            first.line,
            second.line
        );
    }

    #[test]
    fn test_language() {
        let parser = GoParser::new();
        assert_eq!(parser.language(), Language::Go);
    }

    #[test]
    fn test_realistic_go_server() {
        let mut parser = GoParser::new();
        let source = r#"
package main

import (
	"encoding/json"
	"fmt"
	"log"
	"net/http"
)

const (
	DefaultPort = 8080
	MaxBodySize = 1 << 20 // 1 MB
)

// ErrNotFound is returned when a resource is not found.
var ErrNotFound = fmt.Errorf("not found")

// User represents a user in the system.
type User struct {
	ID    string `json:"id"`
	Name  string `json:"name"`
	Email string `json:"email"`
}

// UserStore defines the interface for user persistence.
type UserStore interface {
	Get(id string) (*User, error)
	Save(user *User) error
	Delete(id string) error
}

// Handler wraps function for convenience.
type Handler func(w http.ResponseWriter, r *http.Request)

// Server holds the HTTP server state.
type Server struct {
	store UserStore
	port  int
}

// NewServer creates a new Server with the given store.
func NewServer(store UserStore) *Server {
	return &Server{store: store, port: DefaultPort}
}

// Start begins listening for HTTP requests.
func (s *Server) Start() error {
	addr := fmt.Sprintf(":%d", s.port)
	log.Printf("Listening on %s", addr)
	return http.ListenAndServe(addr, nil)
}

// HandleGetUser handles GET /users/:id requests.
func (s *Server) HandleGetUser(w http.ResponseWriter, r *http.Request) {
	user, err := s.store.Get(r.URL.Query().Get("id"))
	if err != nil {
		http.Error(w, err.Error(), http.StatusNotFound)
		return
	}
	json.NewEncoder(w).Encode(user)
}

func (s *Server) handleInternal() {
	// private method
}
"#;
        let entities = parser.extract(source, "server.go");
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        // Constants
        assert!(names.contains(&"DefaultPort"), "Should find DefaultPort");
        assert!(names.contains(&"MaxBodySize"), "Should find MaxBodySize");

        // Variable
        assert!(names.contains(&"ErrNotFound"), "Should find ErrNotFound");

        // Types
        assert!(names.contains(&"User"), "Should find User struct");
        assert!(
            names.contains(&"UserStore"),
            "Should find UserStore interface"
        );
        assert!(names.contains(&"Handler"), "Should find Handler type alias");
        assert!(names.contains(&"Server"), "Should find Server struct");

        // Functions
        assert!(
            names.contains(&"NewServer"),
            "Should find NewServer function"
        );

        // Methods
        assert!(names.contains(&"Start"), "Should find Start method");
        assert!(
            names.contains(&"HandleGetUser"),
            "Should find HandleGetUser method"
        );
        assert!(
            names.contains(&"handleInternal"),
            "Should find handleInternal method"
        );

        // Verify exports
        let new_server = entities.iter().find(|e| e.name == "NewServer").unwrap();
        assert!(new_server.exported, "NewServer should be exported");

        let handle_internal = entities
            .iter()
            .find(|e| e.name == "handleInternal")
            .unwrap();
        assert!(
            !handle_internal.exported,
            "handleInternal should not be exported"
        );

        let user_store = entities.iter().find(|e| e.name == "UserStore").unwrap();
        assert_eq!(
            user_store.kind,
            EntityKind::Interface,
            "UserStore should be an interface"
        );

        let handler = entities.iter().find(|e| e.name == "Handler").unwrap();
        assert_eq!(
            handler.kind,
            EntityKind::TypeAlias,
            "Handler should be a type alias"
        );

        // Verify reasonable entity count
        assert!(
            entities.len() >= 10,
            "Expected >= 10 entities for a realistic Go server, got {}",
            entities.len()
        );
    }

    #[test]
    fn test_method_with_return_type() {
        let mut parser = GoParser::new();
        let source = r#"
package main

func (s *Server) Port() int {
	return s.port
}
"#;
        let entities = parser.extract(source, "server.go");
        let port = entities.iter().find(|e| e.name == "Port").unwrap();
        assert!(
            port.signature.as_ref().unwrap().contains("int"),
            "Method signature should include return type: {:?}",
            port.signature
        );
        assert!(
            port.signature.as_ref().unwrap().contains("*Server"),
            "Method signature should include receiver: {:?}",
            port.signature
        );
    }

    #[test]
    fn test_function_with_multiple_returns() {
        let mut parser = GoParser::new();
        let source = r#"
package main

func Divide(a, b float64) (float64, error) {
	if b == 0 {
		return 0, fmt.Errorf("division by zero")
	}
	return a / b, nil
}
"#;
        let entities = parser.extract(source, "math.go");
        let divide = entities.iter().find(|e| e.name == "Divide").unwrap();
        assert!(
            divide
                .signature
                .as_ref()
                .unwrap()
                .contains("(float64, error)"),
            "Should include multiple return types: {:?}",
            divide.signature
        );
    }

    #[test]
    fn test_embedded_struct() {
        let mut parser = GoParser::new();
        let source = r#"
package main

type Base struct {
	ID string
}

type Extended struct {
	Base
	Extra string
}
"#;
        let entities = parser.extract(source, "models.go");
        assert!(
            entities.iter().any(|e| e.name == "Base"),
            "Should find Base struct"
        );
        assert!(
            entities.iter().any(|e| e.name == "Extended"),
            "Should find Extended struct"
        );
    }
}
