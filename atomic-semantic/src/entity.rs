//! Entity Types for Semantic Analysis
//!
//! Defines the core types representing code entities extracted by tree-sitter.
//! Following AGENTS.md patterns for type definitions.

use serde::{Deserialize, Serialize};

/// The kind of code entity
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    /// A function declaration or expression
    Function,
    /// A method within a class
    Method,
    /// A class declaration
    Class,
    /// An interface declaration (TypeScript)
    Interface,
    /// A type alias declaration
    TypeAlias,
    /// A constant declaration
    Const,
    /// A variable declaration (let/var)
    Variable,
    /// An enum declaration
    Enum,
    /// A module/namespace declaration
    Module,
    /// An import statement
    Import,
    /// An export statement
    Export,
}

impl EntityKind {
    /// Convert to string representation for storage
    pub fn as_str(&self) -> &'static str {
        match self {
            EntityKind::Function => "function",
            EntityKind::Method => "method",
            EntityKind::Class => "class",
            EntityKind::Interface => "interface",
            EntityKind::TypeAlias => "type_alias",
            EntityKind::Const => "const",
            EntityKind::Variable => "variable",
            EntityKind::Enum => "enum",
            EntityKind::Module => "module",
            EntityKind::Import => "import",
            EntityKind::Export => "export",
        }
    }
}

impl std::str::FromStr for EntityKind {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "function" => Ok(EntityKind::Function),
            "method" => Ok(EntityKind::Method),
            "class" => Ok(EntityKind::Class),
            "interface" => Ok(EntityKind::Interface),
            "type_alias" => Ok(EntityKind::TypeAlias),
            "const" => Ok(EntityKind::Const),
            "variable" => Ok(EntityKind::Variable),
            "enum" => Ok(EntityKind::Enum),
            "module" => Ok(EntityKind::Module),
            "import" => Ok(EntityKind::Import),
            "export" => Ok(EntityKind::Export),
            _ => Err(()),
        }
    }
}

impl std::fmt::Display for EntityKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// The type of change applied to an entity
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeType {
    /// Entity was added
    Added,
    /// Entity was modified
    Modified,
    /// Entity was deleted
    Deleted,
    /// Entity was renamed
    Renamed,
}

impl ChangeType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChangeType::Added => "added",
            ChangeType::Modified => "modified",
            ChangeType::Deleted => "deleted",
            ChangeType::Renamed => "renamed",
        }
    }
}

impl std::str::FromStr for ChangeType {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "added" => Ok(ChangeType::Added),
            "modified" => Ok(ChangeType::Modified),
            "deleted" => Ok(ChangeType::Deleted),
            "renamed" => Ok(ChangeType::Renamed),
            _ => Err(()),
        }
    }
}

/// A code entity extracted from source code
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    /// The name of the entity (e.g., function name, class name)
    pub name: String,

    /// The kind of entity
    pub kind: EntityKind,

    /// The file path where this entity is defined
    pub file: String,

    /// Starting line number (1-based)
    pub line: u32,

    /// Ending line number (1-based, inclusive)
    pub end_line: u32,

    /// Starting column (0-based, optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,

    /// The signature or declaration text
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,

    /// Whether this entity is exported
    pub exported: bool,
}

/// Special name used for placeholder entities when parent state cannot be reconstructed
const PLACEHOLDER_NAME: &str = "__PLACEHOLDER_MODIFIED_FILE__";

impl Entity {
    /// Create a new entity
    pub fn new(
        name: impl Into<String>,
        kind: EntityKind,
        file: impl Into<String>,
        line: u32,
        end_line: u32,
    ) -> Self {
        Self {
            name: name.into(),
            kind,
            file: file.into(),
            line,
            end_line,
            column: None,
            signature: None,
            exported: false,
        }
    }

    /// Create a placeholder entity to indicate a file existed but couldn't be parsed
    /// (e.g., due to conflict markers in parent state)
    pub fn placeholder_for_modified_file(file: impl Into<String>) -> Self {
        Self {
            name: PLACEHOLDER_NAME.to_string(),
            kind: EntityKind::Module,
            file: file.into(),
            line: 0,
            end_line: 0,
            column: None,
            signature: None,
            exported: false,
        }
    }

    /// Check if this entity is a placeholder
    pub fn is_placeholder(&self) -> bool {
        self.name == PLACEHOLDER_NAME && self.line == 0
    }

    /// Builder method to set column
    pub fn with_column(mut self, column: u32) -> Self {
        self.column = Some(column);
        self
    }

    /// Builder method to set signature
    pub fn with_signature(mut self, signature: impl Into<String>) -> Self {
        self.signature = Some(signature.into());
        self
    }

    /// Builder method to set exported flag
    pub fn with_exported(mut self, exported: bool) -> Self {
        self.exported = exported;
        self
    }
}

/// An entity with change information (for Change Request summaries)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityChange {
    /// The entity that changed
    #[serde(flatten)]
    pub entity: Entity,

    /// The type of change
    #[serde(rename = "changeType")]
    pub change_type: ChangeType,
}

impl EntityChange {
    pub fn new(entity: Entity, change_type: ChangeType) -> Self {
        Self {
            entity,
            change_type,
        }
    }

    pub fn added(entity: Entity) -> Self {
        Self::new(entity, ChangeType::Added)
    }

    pub fn modified(entity: Entity) -> Self {
        Self::new(entity, ChangeType::Modified)
    }

    pub fn deleted(entity: Entity) -> Self {
        Self::new(entity, ChangeType::Deleted)
    }
}

/// A reference to an entity from another location
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reference {
    /// The symbol being referenced
    pub symbol: String,

    /// The file containing the reference
    pub file: String,

    /// The line number of the reference
    pub line: u32,

    /// Optional column number
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,

    /// Code context around the reference
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,

    /// Confidence level of this reference match
    #[serde(default = "default_confidence")]
    pub confidence: Confidence,
}

fn default_confidence() -> Confidence {
    Confidence::Medium
}

/// Confidence level for approximate reference matching
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// High confidence - exact match
    High,
    /// Medium confidence - likely match
    #[default]
    Medium,
    /// Low confidence - possible match
    Low,
}

impl Confidence {
    pub fn as_str(&self) -> &'static str {
        match self {
            Confidence::High => "high",
            Confidence::Medium => "medium",
            Confidence::Low => "low",
        }
    }
}

impl std::str::FromStr for Confidence {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "high" => Ok(Confidence::High),
            "medium" => Ok(Confidence::Medium),
            "low" => Ok(Confidence::Low),
            _ => Ok(Confidence::Medium),
        }
    }
}

/// Summary of a file's entities
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSummary {
    /// File path
    pub path: String,

    /// Number of entities in this file
    #[serde(rename = "entityCount")]
    pub entity_count: usize,

    /// Entity names (optional, for quick display)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbols: Option<Vec<String>>,

    /// Exported entity names (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exports: Option<Vec<String>>,
}

impl FileSummary {
    pub fn new(path: impl Into<String>, entity_count: usize) -> Self {
        Self {
            path: path.into(),
            entity_count,
            symbols: None,
            exports: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_entity_kind_roundtrip() {
        let kinds = [
            EntityKind::Function,
            EntityKind::Method,
            EntityKind::Class,
            EntityKind::Interface,
            EntityKind::TypeAlias,
            EntityKind::Const,
            EntityKind::Enum,
        ];

        for kind in kinds {
            let s = kind.as_str();
            let parsed = s.parse::<EntityKind>();
            assert_eq!(parsed, Ok(kind));
        }
    }

    #[test]
    fn test_entity_builder() {
        let entity = Entity::new("authenticate", EntityKind::Function, "src/auth.ts", 10, 25)
            .with_column(0)
            .with_signature("async function authenticate(creds: Credentials): Promise<Token>")
            .with_exported(true);

        assert_eq!(entity.name, "authenticate");
        assert_eq!(entity.kind, EntityKind::Function);
        assert_eq!(entity.line, 10);
        assert_eq!(entity.end_line, 25);
        assert!(entity.exported);
        assert!(entity.signature.is_some());
    }

    #[test]
    fn test_entity_serialization() {
        let entity =
            Entity::new("MyClass", EntityKind::Class, "src/model.ts", 5, 50).with_exported(true);

        let json = serde_json::to_string(&entity).unwrap();
        assert!(json.contains("\"name\":\"MyClass\""));
        assert!(json.contains("\"kind\":\"class\""));
        assert!(json.contains("\"exported\":true"));
    }
}
