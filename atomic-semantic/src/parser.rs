//! TypeScript Parser using tree-sitter
//!
//! Extracts semantic entities (functions, classes, interfaces, etc.) from TypeScript source code.
//! Also extracts references (function calls, variable usages) for Impact Analysis.
//! Following AGENTS.md patterns for parser implementation.

use std::collections::HashSet;
use tree_sitter::{Node, Parser};

use crate::entity::{Confidence, Entity, EntityKind, Reference};
use crate::truncate_to_char_boundary;

/// TypeScript entity extractor using tree-sitter
pub struct TypeScriptExtractor {
    parser: Parser,
}

impl Default for TypeScriptExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl TypeScriptExtractor {
    /// Create a new TypeScript extractor
    pub fn new() -> Self {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
            .expect("Failed to load TypeScript grammar");
        Self { parser }
    }

    /// Create a new TSX extractor (TypeScript with JSX)
    pub fn new_tsx() -> Self {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_typescript::LANGUAGE_TSX.into())
            .expect("Failed to load TSX grammar");
        Self { parser }
    }

    /// Parse source code and extract all entities
    pub fn extract(&mut self, source: &str, file_path: &str) -> Vec<Entity> {
        let tree = match self.parser.parse(source, None) {
            Some(t) => t,
            None => return vec![],
        };

        let mut entities = Vec::new();
        self.walk_tree(&tree.root_node(), source, file_path, &mut entities, false);
        entities
    }

    /// Parse source code and extract all references (function calls, variable usages)
    pub fn extract_references(&mut self, source: &str, file_path: &str) -> Vec<Reference> {
        let tree = match self.parser.parse(source, None) {
            Some(t) => t,
            None => return vec![],
        };

        let mut references = Vec::new();
        let mut seen = HashSet::new();
        self.walk_tree_for_references(
            &tree.root_node(),
            source,
            file_path,
            &mut references,
            &mut seen,
        );
        references
    }

    /// Walk the AST and extract references (function calls, identifier usages)
    fn walk_tree_for_references(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        references: &mut Vec<Reference>,
        seen: &mut HashSet<(String, u32)>,
    ) {
        match node.kind() {
            // Function calls: foo(), obj.method(), etc.
            "call_expression" => {
                if let Some(reference) = self.extract_call_reference(node, source, file_path, seen)
                {
                    references.push(reference);
                }
            }

            // Member expressions: obj.property
            "member_expression" => {
                if let Some(reference) =
                    self.extract_member_reference(node, source, file_path, seen)
                {
                    references.push(reference);
                }
            }

            // Identifier usages (variables, type references)
            "identifier" => {
                // Skip if parent is a declaration (we want usages, not definitions)
                if let Some(parent) = node.parent() {
                    let parent_kind = parent.kind();
                    // Skip identifiers that are part of declarations
                    if !matches!(
                        parent_kind,
                        "function_declaration"
                            | "class_declaration"
                            | "interface_declaration"
                            | "type_alias_declaration"
                            | "variable_declarator"
                            | "method_definition"
                            | "property_signature"
                            | "import_specifier"
                            | "import_clause"
                    ) {
                        if let Some(reference) =
                            self.extract_identifier_reference(node, source, file_path, seen)
                        {
                            references.push(reference);
                        }
                    }
                }
            }

            // Type references: User, AuthToken, etc.
            "type_identifier" => {
                if let Some(reference) = self.extract_type_reference(node, source, file_path, seen)
                {
                    references.push(reference);
                }
            }

            _ => {}
        }

        // Recurse into children
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk_tree_for_references(&child, source, file_path, references, seen);
        }
    }

    /// Extract a reference from a call expression
    fn extract_call_reference(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        seen: &mut HashSet<(String, u32)>,
    ) -> Option<Reference> {
        // Get the function being called
        let function_node = node.child_by_field_name("function")?;
        let line = node.start_position().row as u32 + 1;

        let symbol = match function_node.kind() {
            "identifier" => {
                // Direct call: foo()
                function_node.utf8_text(source.as_bytes()).ok()?.to_string()
            }
            "member_expression" => {
                // Method call: obj.method()
                let property = function_node.child_by_field_name("property")?;
                property.utf8_text(source.as_bytes()).ok()?.to_string()
            }
            _ => return None,
        };

        // Skip common built-ins
        if matches!(
            symbol.as_str(),
            "console"
                | "log"
                | "error"
                | "warn"
                | "info"
                | "debug"
                | "toString"
                | "valueOf"
                | "push"
                | "pop"
                | "map"
                | "filter"
                | "reduce"
                | "forEach"
                | "find"
                | "some"
                | "every"
                | "slice"
                | "splice"
                | "join"
                | "split"
                | "trim"
                | "toLowerCase"
                | "toUpperCase"
                | "includes"
                | "indexOf"
                | "charAt"
                | "substring"
                | "replace"
                | "match"
                | "test"
                | "exec"
                | "parse"
                | "stringify"
                | "keys"
                | "values"
                | "entries"
                | "assign"
                | "freeze"
                | "seal"
                | "create"
                | "defineProperty"
                | "getOwnPropertyNames"
                | "hasOwnProperty"
                | "isArray"
                | "from"
                | "of"
                | "now"
                | "getTime"
                | "getDate"
                | "getMonth"
                | "getFullYear"
                | "setTime"
                | "setDate"
                | "floor"
                | "ceil"
                | "round"
                | "random"
                | "max"
                | "min"
                | "abs"
                | "sqrt"
                | "pow"
                | "setTimeout"
                | "setInterval"
                | "clearTimeout"
                | "clearInterval"
                | "fetch"
                | "then"
                | "catch"
                | "finally"
                | "resolve"
                | "reject"
                | "all"
                | "race"
                | "allSettled"
                | "any"
        ) {
            return None;
        }

        let key = (symbol.clone(), line);
        if seen.contains(&key) {
            return None;
        }
        seen.insert(key);

        // Get context (the line of code)
        let start = node.start_position();
        let context = source.lines().nth(start.row).map(|s| s.trim().to_string());

        Some(Reference {
            symbol,
            file: file_path.to_string(),
            line,
            column: Some(node.start_position().column as u32),
            context,
            confidence: Confidence::High,
        })
    }

    /// Extract a reference from a member expression (property access)
    fn extract_member_reference(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        seen: &mut HashSet<(String, u32)>,
    ) -> Option<Reference> {
        // Skip if parent is a call expression (we'll handle that separately)
        if let Some(parent) = node.parent() {
            if parent.kind() == "call_expression" {
                return None;
            }
        }

        let property = node.child_by_field_name("property")?;
        let symbol = property.utf8_text(source.as_bytes()).ok()?.to_string();
        let line = node.start_position().row as u32 + 1;

        // Skip common properties
        if matches!(
            symbol.as_str(),
            "length"
                | "size"
                | "prototype"
                | "constructor"
                | "name"
                | "message"
                | "stack"
                | "code"
                | "data"
                | "error"
                | "success"
                | "result"
                | "value"
                | "key"
                | "id"
                | "type"
                | "status"
        ) {
            return None;
        }

        let key = (symbol.clone(), line);
        if seen.contains(&key) {
            return None;
        }
        seen.insert(key);

        let context = source
            .lines()
            .nth(node.start_position().row)
            .map(|s| s.trim().to_string());

        Some(Reference {
            symbol,
            file: file_path.to_string(),
            line,
            column: Some(property.start_position().column as u32),
            context,
            confidence: Confidence::Medium,
        })
    }

    /// Extract a reference from an identifier
    fn extract_identifier_reference(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        seen: &mut HashSet<(String, u32)>,
    ) -> Option<Reference> {
        let symbol = node.utf8_text(source.as_bytes()).ok()?.to_string();
        let line = node.start_position().row as u32 + 1;

        // Skip keywords and common built-ins
        if matches!(
            symbol.as_str(),
            "true"
                | "false"
                | "null"
                | "undefined"
                | "this"
                | "super"
                | "console"
                | "window"
                | "document"
                | "global"
                | "process"
                | "module"
                | "exports"
                | "require"
                | "import"
                | "export"
                | "default"
                | "async"
                | "await"
                | "new"
                | "delete"
                | "typeof"
                | "instanceof"
                | "void"
                | "yield"
                | "return"
                | "throw"
                | "try"
                | "catch"
                | "finally"
                | "if"
                | "else"
                | "switch"
                | "case"
                | "break"
                | "continue"
                | "for"
                | "while"
                | "do"
                | "in"
                | "of"
                | "with"
                | "debugger"
                | "class"
                | "extends"
                | "implements"
                | "interface"
                | "enum"
                | "type"
                | "namespace"
                | "declare"
                | "abstract"
                | "public"
                | "private"
                | "protected"
                | "static"
                | "readonly"
                | "get"
                | "set"
                | "let"
                | "const"
                | "var"
                | "function"
                | "Date"
                | "Math"
                | "JSON"
                | "Array"
                | "Object"
                | "String"
                | "Number"
                | "Boolean"
                | "Symbol"
                | "Map"
                | "Set"
                | "WeakMap"
                | "WeakSet"
                | "Promise"
                | "Error"
                | "RegExp"
                | "Buffer"
                | "Uint8Array"
                | "Int32Array"
                | "Float64Array"
                | "ArrayBuffer"
                | "DataView"
        ) {
            return None;
        }

        // Skip short identifiers (likely loop variables)
        if symbol.len() < 2 {
            return None;
        }

        let key = (symbol.clone(), line);
        if seen.contains(&key) {
            return None;
        }
        seen.insert(key);

        let context = source
            .lines()
            .nth(node.start_position().row)
            .map(|s| s.trim().to_string());

        Some(Reference {
            symbol,
            file: file_path.to_string(),
            line,
            column: Some(node.start_position().column as u32),
            context,
            confidence: Confidence::Low,
        })
    }

    /// Extract a reference from a type identifier
    fn extract_type_reference(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        seen: &mut HashSet<(String, u32)>,
    ) -> Option<Reference> {
        let symbol = node.utf8_text(source.as_bytes()).ok()?.to_string();
        let line = node.start_position().row as u32 + 1;

        // Skip built-in types
        if matches!(
            symbol.as_str(),
            "string"
                | "number"
                | "boolean"
                | "void"
                | "null"
                | "undefined"
                | "never"
                | "any"
                | "unknown"
                | "object"
                | "symbol"
                | "bigint"
                | "Date"
                | "Array"
                | "Map"
                | "Set"
                | "Promise"
                | "Error"
                | "RegExp"
                | "Record"
                | "Partial"
                | "Required"
                | "Readonly"
                | "Pick"
                | "Omit"
                | "Exclude"
                | "Extract"
                | "NonNullable"
                | "ReturnType"
                | "Parameters"
                | "ConstructorParameters"
                | "InstanceType"
                | "ThisType"
                | "Uppercase"
                | "Lowercase"
                | "Capitalize"
                | "Uncapitalize"
        ) {
            return None;
        }

        let key = (symbol.clone(), line);
        if seen.contains(&key) {
            return None;
        }
        seen.insert(key);

        let context = source
            .lines()
            .nth(node.start_position().row)
            .map(|s| s.trim().to_string());

        Some(Reference {
            symbol,
            file: file_path.to_string(),
            line,
            column: Some(node.start_position().column as u32),
            context,
            confidence: Confidence::High,
        })
    }

    /// Walk the AST and extract entities
    fn walk_tree(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        entities: &mut Vec<Entity>,
        in_export: bool,
    ) {
        // Check if this node is an export statement
        let is_export = node.kind() == "export_statement";
        let child_in_export = in_export || is_export;

        match node.kind() {
            // Function declarations
            "function_declaration" => {
                if let Some(entity) =
                    self.extract_function(node, source, file_path, child_in_export)
                {
                    entities.push(entity);
                }
            }

            // Arrow functions assigned to variables (const foo = () => {})
            "lexical_declaration" | "variable_declaration" => {
                entities.extend(self.extract_variable_declarations(
                    node,
                    source,
                    file_path,
                    child_in_export,
                ));
            }

            // Class declarations
            "class_declaration" => {
                if let Some(entity) = self.extract_class(node, source, file_path, child_in_export) {
                    entities.push(entity);
                }
                // Also extract methods from the class
                self.extract_class_members(node, source, file_path, entities);
            }

            // Interface declarations (TypeScript)
            "interface_declaration" => {
                if let Some(entity) =
                    self.extract_interface(node, source, file_path, child_in_export)
                {
                    entities.push(entity);
                }
            }

            // Type alias declarations (TypeScript)
            "type_alias_declaration" => {
                if let Some(entity) =
                    self.extract_type_alias(node, source, file_path, child_in_export)
                {
                    entities.push(entity);
                }
            }

            // Enum declarations (TypeScript)
            "enum_declaration" => {
                if let Some(entity) = self.extract_enum(node, source, file_path, child_in_export) {
                    entities.push(entity);
                }
            }

            // Module/namespace declarations (TypeScript)
            "module" | "internal_module" => {
                if let Some(entity) = self.extract_module(node, source, file_path, child_in_export)
                {
                    entities.push(entity);
                }
            }

            _ => {}
        }

        // Recurse into children
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.walk_tree(&child, source, file_path, entities, child_in_export);
            }
        }
    }

    /// Extract a function declaration
    fn extract_function(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        exported: bool,
    ) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source)?;

        let signature = self.build_function_signature(node, source);

        Some(
            Entity::new(
                name,
                EntityKind::Function,
                file_path,
                node.start_position().row as u32 + 1,
                node.end_position().row as u32 + 1,
            )
            .with_column(node.start_position().column as u32)
            .with_signature(signature)
            .with_exported(exported),
        )
    }

    /// Extract variable declarations (const, let, var)
    /// Handles arrow functions assigned to variables
    fn extract_variable_declarations(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        exported: bool,
    ) -> Vec<Entity> {
        let mut entities = Vec::new();

        // Check if it's a const declaration
        let is_const = node.kind() == "lexical_declaration"
            && node
                .child(0)
                .map(|c| self.node_text(&c, source) == Some("const".to_string()))
                .unwrap_or(false);

        // Iterate through variable declarators
        for i in 0..node.child_count() {
            if let Some(declarator) = node.child(i) {
                if declarator.kind() == "variable_declarator" {
                    if let Some(entity) = self.extract_variable_declarator(
                        &declarator,
                        node,
                        source,
                        file_path,
                        exported,
                        is_const,
                    ) {
                        entities.push(entity);
                    }
                }
            }
        }

        entities
    }

    /// Extract a single variable declarator
    fn extract_variable_declarator(
        &self,
        declarator: &Node,
        parent: &Node,
        source: &str,
        file_path: &str,
        exported: bool,
        is_const: bool,
    ) -> Option<Entity> {
        let name_node = declarator.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source)?;

        // Check if the value is an arrow function or function expression
        let value_node = declarator.child_by_field_name("value");
        let (kind, signature) = if let Some(value) = value_node {
            match value.kind() {
                "arrow_function" | "function" | "function_expression" => {
                    let sig = self.build_arrow_function_signature(&name, &value, source);
                    (EntityKind::Function, Some(sig))
                }
                _ => {
                    if is_const {
                        let sig = self.build_const_signature(declarator, source);
                        (EntityKind::Const, Some(sig))
                    } else {
                        (EntityKind::Variable, None)
                    }
                }
            }
        } else if is_const {
            (EntityKind::Const, None)
        } else {
            (EntityKind::Variable, None)
        };

        let mut entity = Entity::new(
            name,
            kind,
            file_path,
            parent.start_position().row as u32 + 1,
            parent.end_position().row as u32 + 1,
        )
        .with_column(parent.start_position().column as u32)
        .with_exported(exported);

        if let Some(sig) = signature {
            entity = entity.with_signature(sig);
        }

        Some(entity)
    }

    /// Extract a class declaration
    fn extract_class(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        exported: bool,
    ) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source)?;

        let signature = self.build_class_signature(node, source);

        Some(
            Entity::new(
                name,
                EntityKind::Class,
                file_path,
                node.start_position().row as u32 + 1,
                node.end_position().row as u32 + 1,
            )
            .with_column(node.start_position().column as u32)
            .with_signature(signature)
            .with_exported(exported),
        )
    }

    /// Extract class methods
    fn extract_class_members(
        &self,
        class_node: &Node,
        source: &str,
        file_path: &str,
        entities: &mut Vec<Entity>,
    ) {
        let body = match class_node.child_by_field_name("body") {
            Some(b) => b,
            None => return,
        };

        for i in 0..body.child_count() {
            if let Some(member) = body.child(i) {
                if member.kind() == "method_definition" {
                    if let Some(entity) = self.extract_method(&member, source, file_path) {
                        entities.push(entity);
                    }
                }
            }
        }
    }

    /// Extract a method from a class
    fn extract_method(&self, node: &Node, source: &str, file_path: &str) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source)?;

        let signature = self.build_method_signature(node, source);

        Some(
            Entity::new(
                name,
                EntityKind::Method,
                file_path,
                node.start_position().row as u32 + 1,
                node.end_position().row as u32 + 1,
            )
            .with_column(node.start_position().column as u32)
            .with_signature(signature),
        )
    }

    /// Extract an interface declaration
    fn extract_interface(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        exported: bool,
    ) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source)?;

        // Build signature with extends if present
        let extends = self.get_interface_extends(node, source);
        let signature = if extends.is_empty() {
            format!("interface {}", name)
        } else {
            format!("interface {} extends {}", name, extends)
        };

        Some(
            Entity::new(
                name,
                EntityKind::Interface,
                file_path,
                node.start_position().row as u32 + 1,
                node.end_position().row as u32 + 1,
            )
            .with_column(node.start_position().column as u32)
            .with_signature(signature)
            .with_exported(exported),
        )
    }

    /// Extract a type alias declaration
    fn extract_type_alias(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        exported: bool,
    ) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source)?;

        let signature = self.build_type_alias_signature(node, source);

        Some(
            Entity::new(
                name,
                EntityKind::TypeAlias,
                file_path,
                node.start_position().row as u32 + 1,
                node.end_position().row as u32 + 1,
            )
            .with_column(node.start_position().column as u32)
            .with_signature(signature)
            .with_exported(exported),
        )
    }

    /// Extract an enum declaration
    fn extract_enum(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        exported: bool,
    ) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source)?;

        let signature = format!("enum {}", name);

        Some(
            Entity::new(
                name,
                EntityKind::Enum,
                file_path,
                node.start_position().row as u32 + 1,
                node.end_position().row as u32 + 1,
            )
            .with_column(node.start_position().column as u32)
            .with_signature(signature)
            .with_exported(exported),
        )
    }

    /// Extract a module/namespace declaration
    fn extract_module(
        &self,
        node: &Node,
        source: &str,
        file_path: &str,
        exported: bool,
    ) -> Option<Entity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source)?;

        let signature = format!("namespace {}", name);

        Some(
            Entity::new(
                name,
                EntityKind::Module,
                file_path,
                node.start_position().row as u32 + 1,
                node.end_position().row as u32 + 1,
            )
            .with_column(node.start_position().column as u32)
            .with_signature(signature)
            .with_exported(exported),
        )
    }

    // =========================================================================
    // Helper methods for building signatures
    // =========================================================================

    /// Get text content of a node
    fn node_text(&self, node: &Node, source: &str) -> Option<String> {
        node.utf8_text(source.as_bytes())
            .ok()
            .map(|s| s.to_string())
    }

    /// Build function signature from declaration
    fn build_function_signature(&self, node: &Node, source: &str) -> String {
        let start = node.start_byte();

        // Find the body to exclude it from signature
        let body_start = node
            .child_by_field_name("body")
            .map(|b| b.start_byte())
            .unwrap_or(node.end_byte());

        let sig = &source[start..body_start];

        // Clean up: remove newlines and extra whitespace
        sig.lines()
            .map(|l| l.trim())
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_string()
    }

    /// Build arrow function signature
    fn build_arrow_function_signature(&self, name: &str, node: &Node, source: &str) -> String {
        // Get parameters
        let params = node
            .child_by_field_name("parameters")
            .or_else(|| node.child_by_field_name("parameter"))
            .map(|p| self.node_text(&p, source).unwrap_or_default())
            .unwrap_or_else(|| "()".to_string());

        // Get return type if present
        let return_type = node
            .child_by_field_name("return_type")
            .map(|r| self.node_text(&r, source).unwrap_or_default())
            .unwrap_or_default();

        if return_type.is_empty() {
            format!("const {} = {} => ...", name, params)
        } else {
            format!("const {} = {}{} => ...", name, params, return_type)
        }
    }

    /// Build const signature
    fn build_const_signature(&self, declarator: &Node, source: &str) -> String {
        let name = declarator
            .child_by_field_name("name")
            .and_then(|n| self.node_text(&n, source))
            .unwrap_or_default();

        // Get type annotation if present
        let type_ann = declarator
            .child_by_field_name("type")
            .map(|t| self.node_text(&t, source).unwrap_or_default())
            .unwrap_or_default();

        // Get a short preview of the value (first 50 chars)
        let value = declarator
            .child_by_field_name("value")
            .map(|v| {
                let text = self.node_text(&v, source).unwrap_or_default();
                if text.len() > 50 {
                    let end = truncate_to_char_boundary(&text, 50);
                    format!("{}...", &text[..end])
                } else {
                    text
                }
            })
            .unwrap_or_default();

        if type_ann.is_empty() {
            format!("const {} = {}", name, value)
        } else {
            format!("const {}{} = {}", name, type_ann, value)
        }
    }

    /// Build class signature
    fn build_class_signature(&self, node: &Node, source: &str) -> String {
        let name = node
            .child_by_field_name("name")
            .and_then(|n| self.node_text(&n, source))
            .unwrap_or_default();

        // Check for extends
        let extends = self.get_class_heritage(node, source, "extends");
        // Check for implements
        let implements = self.get_class_heritage(node, source, "implements");

        let mut sig = format!("class {}", name);

        if !extends.is_empty() {
            sig.push_str(&format!(" extends {}", extends));
        }
        if !implements.is_empty() {
            sig.push_str(&format!(" implements {}", implements));
        }

        sig
    }

    /// Get class heritage (extends or implements)
    fn get_class_heritage(&self, node: &Node, source: &str, heritage_type: &str) -> String {
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                if child.kind() == "class_heritage" {
                    for j in 0..child.child_count() {
                        if let Some(clause) = child.child(j) {
                            if clause.kind() == format!("{}_clause", heritage_type) {
                                // Get all type names in this clause
                                let mut types = Vec::new();
                                for k in 0..clause.child_count() {
                                    if let Some(type_node) = clause.child(k) {
                                        if type_node.kind() != heritage_type {
                                            if let Some(text) = self.node_text(&type_node, source) {
                                                types.push(text);
                                            }
                                        }
                                    }
                                }
                                return types.join(", ");
                            }
                        }
                    }
                }
            }
        }
        String::new()
    }

    /// Get interface extends types
    fn get_interface_extends(&self, node: &Node, source: &str) -> String {
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                if child.kind() == "extends_type_clause" {
                    let mut types = Vec::new();
                    for j in 0..child.child_count() {
                        if let Some(type_node) = child.child(j) {
                            if type_node.kind() != "extends" && type_node.kind() != "," {
                                if let Some(text) = self.node_text(&type_node, source) {
                                    types.push(text);
                                }
                            }
                        }
                    }
                    return types.join(", ");
                }
            }
        }
        String::new()
    }

    /// Build method signature
    fn build_method_signature(&self, node: &Node, source: &str) -> String {
        let name = node
            .child_by_field_name("name")
            .and_then(|n| self.node_text(&n, source))
            .unwrap_or_default();

        // Get parameters
        let params = node
            .child_by_field_name("parameters")
            .map(|p| self.node_text(&p, source).unwrap_or_default())
            .unwrap_or_else(|| "()".to_string());

        // Get return type
        let return_type = node
            .child_by_field_name("return_type")
            .map(|r| self.node_text(&r, source).unwrap_or_default())
            .unwrap_or_default();

        // Check for async/static modifiers
        let mut modifiers = Vec::new();
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                match child.kind() {
                    "async" => modifiers.push("async"),
                    "static" => modifiers.push("static"),
                    "readonly" => modifiers.push("readonly"),
                    "accessibility_modifier" => {
                        if let Some(text) = self.node_text(&child, source) {
                            modifiers.push(if text == "public" {
                                "public"
                            } else if text == "private" {
                                "private"
                            } else {
                                "protected"
                            });
                        }
                    }
                    _ => {}
                }
            }
        }

        let modifier_str = if modifiers.is_empty() {
            String::new()
        } else {
            format!("{} ", modifiers.join(" "))
        };

        format!("{}{}{}{}", modifier_str, name, params, return_type)
    }

    /// Build type alias signature
    fn build_type_alias_signature(&self, node: &Node, source: &str) -> String {
        let name = node
            .child_by_field_name("name")
            .and_then(|n| self.node_text(&n, source))
            .unwrap_or_default();

        // Get the type value (truncated if too long)
        let value = node
            .child_by_field_name("value")
            .map(|v| {
                let text = self.node_text(&v, source).unwrap_or_default();
                if text.len() > 60 {
                    let end = truncate_to_char_boundary(&text, 60);
                    format!("{}...", &text[..end])
                } else {
                    text
                }
            })
            .unwrap_or_default();

        // Check for type parameters
        let type_params = node
            .child_by_field_name("type_parameters")
            .map(|p| self.node_text(&p, source).unwrap_or_default())
            .unwrap_or_default();

        format!("type {}{} = {}", name, type_params, value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_function() {
        let mut extractor = TypeScriptExtractor::new();
        let source = r#"
function authenticate(username: string, password: string): Promise<User> {
    return fetch('/api/login', { username, password });
}
"#;
        let entities = extractor.extract(source, "test.ts");

        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].name, "authenticate");
        assert_eq!(entities[0].kind, EntityKind::Function);
        assert!(!entities[0].exported);
    }

    #[test]
    fn test_extract_exported_function() {
        let mut extractor = TypeScriptExtractor::new();
        let source = r#"
export function validateToken(token: string): boolean {
    return token.length > 0;
}
"#;
        let entities = extractor.extract(source, "test.ts");

        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].name, "validateToken");
        assert!(entities[0].exported);
    }

    #[test]
    fn test_extract_arrow_function() {
        let mut extractor = TypeScriptExtractor::new();
        let source = r#"
const processData = (data: Data[]): Result => {
    return data.map(d => transform(d));
};
"#;
        let entities = extractor.extract(source, "test.ts");

        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].name, "processData");
        assert_eq!(entities[0].kind, EntityKind::Function);
    }

    #[test]
    fn test_extract_const() {
        let mut extractor = TypeScriptExtractor::new();
        let source = r#"
const MAX_RETRIES = 3;
const API_URL: string = "https://api.example.com";
"#;
        let entities = extractor.extract(source, "test.ts");

        assert_eq!(entities.len(), 2);
        assert_eq!(entities[0].name, "MAX_RETRIES");
        assert_eq!(entities[0].kind, EntityKind::Const);
        assert_eq!(entities[1].name, "API_URL");
        assert_eq!(entities[1].kind, EntityKind::Const);
    }

    #[test]
    fn test_extract_class() {
        let mut extractor = TypeScriptExtractor::new();
        let source = r#"
export class UserService {
    private cache: Map<string, User>;

    constructor() {
        this.cache = new Map();
    }

    async getUser(id: string): Promise<User> {
        return this.cache.get(id);
    }

    setUser(user: User): void {
        this.cache.set(user.id, user);
    }
}
"#;
        let entities = extractor.extract(source, "test.ts");

        // Should find: class, constructor, getUser, setUser
        assert!(entities.len() >= 3);

        let class_entity = entities.iter().find(|e| e.name == "UserService");
        assert!(class_entity.is_some());
        assert_eq!(class_entity.unwrap().kind, EntityKind::Class);
        assert!(class_entity.unwrap().exported);

        let method_entity = entities.iter().find(|e| e.name == "getUser");
        assert!(method_entity.is_some());
        assert_eq!(method_entity.unwrap().kind, EntityKind::Method);
    }

    #[test]
    fn test_extract_interface() {
        let mut extractor = TypeScriptExtractor::new();
        let source = r#"
export interface AuthConfig {
    clientId: string;
    clientSecret: string;
    redirectUri: string;
}

interface BaseEntity {
    id: string;
    createdAt: Date;
}

export interface User extends BaseEntity {
    name: string;
    email: string;
}
"#;
        let entities = extractor.extract(source, "test.ts");

        assert_eq!(entities.len(), 3);

        let auth_config = entities.iter().find(|e| e.name == "AuthConfig").unwrap();
        assert_eq!(auth_config.kind, EntityKind::Interface);
        assert!(auth_config.exported);

        let user = entities.iter().find(|e| e.name == "User").unwrap();
        assert!(user.signature.as_ref().unwrap().contains("extends"));
    }

    #[test]
    fn test_extract_type_alias() {
        let mut extractor = TypeScriptExtractor::new();
        let source = r#"
export type UserId = string;
type Status = 'pending' | 'active' | 'inactive';
type Result<T> = { data: T } | { error: Error };
"#;
        let entities = extractor.extract(source, "test.ts");

        assert_eq!(entities.len(), 3);

        let user_id = entities.iter().find(|e| e.name == "UserId").unwrap();
        assert_eq!(user_id.kind, EntityKind::TypeAlias);
        assert!(user_id.exported);

        let status = entities.iter().find(|e| e.name == "Status").unwrap();
        assert!(!status.exported);

        let result = entities.iter().find(|e| e.name == "Result").unwrap();
        assert!(result.signature.as_ref().unwrap().contains("<T>"));
    }

    #[test]
    fn test_extract_enum() {
        let mut extractor = TypeScriptExtractor::new();
        let source = r#"
export enum UserRole {
    Admin = 'admin',
    User = 'user',
    Guest = 'guest',
}

enum Status {
    Pending,
    Active,
    Inactive,
}
"#;
        let entities = extractor.extract(source, "test.ts");

        assert_eq!(entities.len(), 2);

        let user_role = entities.iter().find(|e| e.name == "UserRole").unwrap();
        assert_eq!(user_role.kind, EntityKind::Enum);
        assert!(user_role.exported);
    }

    #[test]
    fn test_extract_multiple_entities() {
        let mut extractor = TypeScriptExtractor::new();
        let source = r#"
export interface Config {
    apiUrl: string;
}

export const DEFAULT_CONFIG: Config = {
    apiUrl: 'https://api.example.com',
};

export function createClient(config: Config): Client {
    return new Client(config);
}

export class Client {
    constructor(private config: Config) {}

    async request(path: string): Promise<Response> {
        return fetch(this.config.apiUrl + path);
    }
}
"#;
        let entities = extractor.extract(source, "test.ts");

        // Should find: interface, const, function, class, constructor, request method
        assert!(entities.len() >= 5);

        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"Config"));
        assert!(names.contains(&"DEFAULT_CONFIG"));
        assert!(names.contains(&"createClient"));
        assert!(names.contains(&"Client"));
    }

    #[test]
    fn test_tsx_support() {
        let mut extractor = TypeScriptExtractor::new_tsx();
        let source = r#"
export function Button({ onClick, children }: ButtonProps): JSX.Element {
    return <button onClick={onClick}>{children}</button>;
}

export const Card: React.FC<CardProps> = ({ title, content }) => {
    return (
        <div className="card">
            <h2>{title}</h2>
            <p>{content}</p>
        </div>
    );
};
"#;
        let entities = extractor.extract(source, "test.tsx");

        assert_eq!(entities.len(), 2);
        assert!(entities.iter().any(|e| e.name == "Button"));
        assert!(entities.iter().any(|e| e.name == "Card"));
    }

    #[test]
    fn test_line_numbers() {
        let mut extractor = TypeScriptExtractor::new();
        let source = r#"// Line 1
// Line 2
function foo() {  // Line 3
    return 1;     // Line 4
}                 // Line 5
"#;
        let entities = extractor.extract(source, "test.ts");

        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].name, "foo");
        assert_eq!(entities[0].line, 3); // 1-based line number
        assert_eq!(entities[0].end_line, 5);
    }
}
