//! Default ignore patterns shared by native initialization and Git import.

/// Get the .atomicignore content for a given project kind.
///
/// This function returns appropriate ignore patterns for common project
/// types, helping users start with sensible defaults.
///
/// # Arguments
///
/// * `kind` - The project kind (e.g., "rust", "python", "node")
///
/// # Returns
///
/// The content for the .atomicignore file, or `None` if the kind is unknown.
///
/// # Supported Kinds
///
/// - `rust`: Ignores `target/`, `Cargo.lock` (for libraries)
/// - `python`: Ignores `__pycache__/`, `.venv/`, `*.pyc`
/// - `node` / `javascript` / `typescript`: Ignores `node_modules/`, `dist/`
/// - `go`: Ignores `bin/`, `*.exe`
/// - `java`: Ignores `target/`, `*.class`, `*.jar`
/// - `c` / `cpp`: Ignores `*.o`, `*.a`, `*.so`, `build/`
pub fn get_ignore_template(kind: &str) -> Option<&'static str> {
    match kind.to_lowercase().as_str() {
        "rust" => Some(
            r#"# Rust
target/
**/*.rs.bk
Cargo.lock

# IDE
.idea/
.vscode/
*.swp
*.swo
*~

# OS
.DS_Store
Thumbs.db
"#,
        ),
        "python" => Some(
            r#"# Python
__pycache__/
*.py[cod]
*$py.class
*.so
.Python
build/
dist/
*.egg-info/
.eggs/
.venv/
venv/
ENV/
.pytest_cache/
.mypy_cache/
.coverage
htmlcov/

# IDE
.idea/
.vscode/
*.swp
*~

# OS
.DS_Store
Thumbs.db
"#,
        ),
        "node" | "javascript" | "typescript" | "js" | "ts" => Some(
            r#"# Node.js
node_modules/
npm-debug.log*
yarn-debug.log*
yarn-error.log*
.npm
.yarn
dist/
build/
coverage/
.env
.env.local
.env.*.local

# IDE
.idea/
.vscode/
*.swp
*~

# OS
.DS_Store
Thumbs.db
"#,
        ),
        "go" | "golang" => Some(
            r#"# Go
bin/
pkg/
*.exe
*.exe~
*.dll
*.so
*.dylib
*.test
*.out
vendor/

# IDE
.idea/
.vscode/
*.swp
*~

# OS
.DS_Store
Thumbs.db
"#,
        ),
        "java" | "kotlin" => Some(
            r#"# Java/Kotlin
target/
*.class
*.jar
*.war
*.ear
*.logs
*.log
.gradle/
build/
out/

# IDE
.idea/
*.iml
.vscode/
*.swp
*~

# OS
.DS_Store
Thumbs.db
"#,
        ),
        "c" | "cpp" | "c++" => Some(
            r#"# C/C++
*.o
*.a
*.so
*.dylib
*.dll
*.exe
*.out
build/
cmake-build-*/
CMakeFiles/
CMakeCache.txt
Makefile
*.make

# IDE
.idea/
.vscode/
*.swp
*~
compile_commands.json

# OS
.DS_Store
Thumbs.db
"#,
        ),
        _ => None,
    }
}
