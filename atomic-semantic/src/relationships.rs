//! Scoped source relationships shared by callers, review and other consumers.
//! A symbol spelling alone is never sufficient evidence of a binding.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Component, Path, PathBuf};

use crate::source::{CallSite, SourceAnalysis, Symbol};
use crate::Language;

#[derive(Clone, Debug)]
pub struct BoundCall<'a> {
    pub call: &'a CallSite,
    pub symbol: &'a Symbol,
    pub present: bool,
    pub accessible: bool,
    pub expected_min: usize,
    pub expected_max: Option<usize>,
    pub reason: String,
    pub configuration_dependent: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Placement {
    root: String,
    module: Vec<String>,
}

struct ModuleAccess {
    public: Option<bool>,
    conditional: bool,
}

pub struct SourceGraph {
    pub files: BTreeMap<String, SourceAnalysis>,
    pub symbols: BTreeMap<String, Symbol>,
    pub paths: BTreeSet<String>,
    pub ambiguous: BTreeSet<String>,
    placements: BTreeMap<String, BTreeSet<Placement>>,
    names: BTreeMap<(String, Vec<String>, String), BTreeSet<String>>,
    modules: BTreeMap<(String, Vec<String>), ModuleAccess>,
    complete: bool,
}

pub fn module_candidates(file: &str, scope: &[String]) -> Vec<String> {
    let path = Path::new(file);
    let mut base = path.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
    if !matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("lib.rs" | "main.rs" | "mod.rs")
    ) {
        base.push(path.file_stem().unwrap_or_default());
    }
    for name in scope {
        base.push(name);
    }
    vec![
        base.with_extension("rs").to_string_lossy().into_owned(),
        base.join("mod.rs").to_string_lossy().into_owned(),
    ]
}

/// Standard explicit relative script import candidates. Full source inventory is
/// consulted even when only matching files are parsed, preserving ambiguity.
pub fn script_candidates(importer: &str, module: &str, paths: &BTreeSet<String>) -> Vec<String> {
    let script = |path: &str| {
        matches!(
            Path::new(path)
                .extension()
                .and_then(|extension| extension.to_str()),
            Some("ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs")
        )
    };
    if !script(importer) || !module.starts_with('.') {
        return vec![];
    }
    let Some(requested) = Path::new(importer)
        .parent()
        .and_then(|base| normalize(base.join(module)))
    else {
        return vec![];
    };
    paths
        .iter()
        .filter(|target| {
            if !script(target) {
                return false;
            }
            let target = Path::new(target);
            let stem = target.with_extension("");
            target == requested || stem == requested || stem == requested.join("index")
        })
        .cloned()
        .collect()
}

/// Declaration/import context needed to interpret a selected source file.
/// This returns paths only; repository adapters own recorded-byte retrieval.
pub fn context_paths(file: &SourceAnalysis, paths: &BTreeSet<String>) -> BTreeSet<String> {
    let mut result = BTreeSet::new();
    if file.language == Language::Rust {
        let mut ancestor = Path::new(&file.path).parent();
        while let Some(directory) = ancestor {
            for candidate in [
                directory.join("lib.rs"),
                directory.join("main.rs"),
                directory.join("mod.rs"),
                directory.with_extension("rs"),
            ] {
                let candidate = candidate.to_string_lossy().into_owned();
                if paths.contains(&candidate) {
                    result.insert(candidate);
                }
            }
            ancestor = directory.parent().filter(|parent| *parent != directory);
        }
    }
    for import in &file.imports {
        if let Some(module) = &import.script_module {
            result.extend(script_candidates(&file.path, module, paths));
            continue;
        }
        if file.language != Language::Rust {
            continue;
        }
        let mut base = Path::new(&file.path)
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .to_path_buf();
        if !matches!(
            Path::new(&file.path)
                .file_name()
                .and_then(|name| name.to_str()),
            Some("lib.rs" | "main.rs" | "mod.rs")
        ) {
            base.push(Path::new(&file.path).file_stem().unwrap_or_default());
        }
        for part in &import.scope {
            base.push(part);
        }
        let mut target = import.target.as_slice();
        match target.first().map(String::as_str) {
            Some("crate") => {
                let mut root = PathBuf::new();
                for component in Path::new(&file.path).components() {
                    root.push(component.as_os_str());
                    if component.as_os_str() == "src" {
                        break;
                    }
                }
                base = root;
                target = &target[1..];
            }
            Some("self") => target = &target[1..],
            Some("super") => {
                while target.first().is_some_and(|part| part == "super") {
                    base.pop();
                    target = &target[1..];
                }
            }
            _ => continue,
        }
        for end in 1..=target.len() {
            let mut module = base.clone();
            for part in &target[..end] {
                module.push(part);
            }
            for candidate in [module.with_extension("rs"), module.join("mod.rs")] {
                let candidate = candidate.to_string_lossy().into_owned();
                if paths.contains(&candidate) {
                    result.insert(candidate);
                }
            }
        }
    }
    result.remove(&file.path);
    result
}

fn normalize(path: PathBuf) -> Option<PathBuf> {
    let mut result = PathBuf::new();
    for part in path.components() {
        match part {
            Component::Normal(name) => result.push(name),
            Component::ParentDir if !result.pop() => return None,
            Component::ParentDir | Component::CurDir => {}
            _ => return None,
        }
    }
    Some(result)
}

impl SourceGraph {
    /// Build from selected semantic facts and the full visible path inventory.
    /// `complete` describes that inventory, not whether every file was parsed.
    pub fn new(
        files: Vec<SourceAnalysis>,
        paths: impl IntoIterator<Item = String>,
        complete: bool,
    ) -> Self {
        let mut graph = Self {
            files: files
                .into_iter()
                .map(|file| (file.path.clone(), file))
                .collect(),
            symbols: BTreeMap::new(),
            paths: paths.into_iter().collect(),
            ambiguous: BTreeSet::new(),
            placements: BTreeMap::new(),
            names: BTreeMap::new(),
            modules: BTreeMap::new(),
            complete,
        };
        let mut queue = VecDeque::new();
        for (path, file) in &graph.files {
            if file.language == Language::Rust
                && matches!(
                    Path::new(path).file_name().and_then(|name| name.to_str()),
                    Some("lib.rs" | "main.rs")
                )
            {
                queue.push_back((
                    path.clone(),
                    Placement {
                        root: path.clone(),
                        module: vec![],
                    },
                ));
            }
        }
        while let Some((path, placement)) = queue.pop_front() {
            if !graph
                .placements
                .entry(path.clone())
                .or_default()
                .insert(placement.clone())
            {
                continue;
            }
            for module in &graph.files[&path].modules {
                let mut namespace = placement.module.clone();
                namespace.extend(module.scope.clone());
                let known = module.visibility.as_deref().is_none_or(|visibility| {
                    matches!(
                        visibility,
                        "pub" | "pub(crate)" | "pub(self)" | "pub(super)"
                    )
                });
                let public = known.then_some(module.public);
                graph
                    .modules
                    .entry((placement.root.clone(), namespace.clone()))
                    .and_modify(|access| {
                        if access.public != public {
                            access.public = None;
                        }
                        access.conditional |= module.conditional;
                    })
                    .or_insert(ModuleAccess {
                        public,
                        conditional: module.conditional,
                    });
                if module.external {
                    let targets: Vec<_> = module_candidates(&path, &module.scope)
                        .into_iter()
                        .filter(|path| graph.paths.contains(path))
                        .collect();
                    if targets.len() == 1 && graph.files.contains_key(&targets[0]) {
                        queue.push_back((
                            targets[0].clone(),
                            Placement {
                                root: placement.root.clone(),
                                module: namespace,
                            },
                        ));
                    }
                }
            }
        }
        for (path, file) in &graph.files {
            for symbol in &file.symbols {
                if graph
                    .symbols
                    .insert(symbol.id.clone(), symbol.clone())
                    .is_some()
                {
                    graph.ambiguous.insert(symbol.id.clone());
                }
                for placement in graph.placements.get(path).into_iter().flatten() {
                    let mut namespace = placement.module.clone();
                    namespace.extend(symbol.scope.clone());
                    graph
                        .names
                        .entry((
                            placement.root.clone(),
                            namespace,
                            symbol.entity.name.clone(),
                        ))
                        .or_default()
                        .insert(symbol.id.clone());
                }
            }
        }
        for id in &graph.ambiguous {
            graph.symbols.remove(id);
        }
        graph
    }

    fn symbol<'a>(&'a self, id: &str, before: Option<&'a Self>) -> Option<&'a Symbol> {
        if self.ambiguous.contains(id) {
            return None;
        }
        let current = self.symbols.get(id);
        let symbol = current.or_else(|| before.and_then(|graph| graph.symbols.get(id)))?;
        if let Some(file) = self.files.get(&symbol.location.path) {
            if file
                .globals
                .get(&symbol.scope)
                .is_some_and(|names| names.contains(&symbol.entity.name))
            {
                return None;
            }
        } else if self.paths.contains(&symbol.location.path) {
            return None;
        }
        Some(symbol)
    }

    fn local(
        &self,
        file: &SourceAnalysis,
        caller: &Symbol,
        call: &CallSite,
        before: Option<&Self>,
    ) -> Option<String> {
        let name = call.target.first()?;
        let mut scope = caller.scope.clone();
        scope.push(caller.entity.name.clone());
        loop {
            let id = format!(
                "{}#{}",
                file.path,
                scope
                    .iter()
                    .chain(std::iter::once(name))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("::")
            );
            if self.ambiguous.contains(&id) {
                return None;
            }
            if let Some(symbol) = self.symbol(&id, before).filter(|symbol| {
                symbol.owner.is_none() && symbol.entity.kind != crate::EntityKind::Method
            }) {
                if call.start_byte >= symbol.lexical_range.0
                    && call.start_byte <= symbol.lexical_range.1
                {
                    return Some(id);
                }
            }
            if scope.len() <= caller.module_scope.len() {
                break;
            }
            scope.pop();
        }
        None
    }

    fn resolve(
        &self,
        file: &SourceAnalysis,
        call: &CallSite,
        before: Option<&Self>,
        check_access: bool,
    ) -> Option<(String, String)> {
        let caller = self.symbol(&call.caller, None)?;
        if !matches!(
            file.language,
            Language::Rust | Language::TypeScript | Language::Tsx
        ) && (call.receiver.is_some() || caller.owner.is_some())
        {
            return None;
        }
        let head = call.target.first()?;
        // Known lexical receiver, not a global lookup for a common method name.
        if let Some(receiver) = &call.receiver {
            let owner = if matches!(receiver.as_str(), "self" | "this") {
                caller.owner.as_ref()
            } else {
                caller.parameter_types.get(receiver)
            };
            if let Some(owner) = owner {
                let name = call.target.last()?;
                let candidates: Vec<_> = self
                    .symbols
                    .values()
                    .filter(|symbol| {
                        symbol.location.path == file.path
                            && symbol.owner.as_ref() == Some(owner)
                            && symbol.entity.name == *name
                            && symbol.module_scope == caller.module_scope
                    })
                    .collect();
                if candidates.len() == 1 {
                    if check_access
                        && candidates[0].member_private
                        && caller.owner != candidates[0].owner
                    {
                        return None;
                    }
                    return Some((
                        candidates[0].id.clone(),
                        "AST lexical/declared receiver type and unique owned declaration".into(),
                    ));
                }
                return None;
            }
        }
        if caller.shadows.contains(head)
            || file
                .globals
                .get(&call.scope)
                .is_some_and(|names| names.contains(head))
        {
            return None;
        }
        let mut lexical = caller.scope.clone();
        lexical.push(caller.entity.name.clone());
        let imports = loop {
            let imports: Vec<_> = file
                .imports
                .iter()
                .filter(|import| {
                    import.scope == lexical
                        && import.local == *head
                        && call.start_byte >= import.lexical_range.0
                        && call.start_byte <= import.lexical_range.1
                })
                .collect();
            if !imports.is_empty() || lexical.len() <= caller.module_scope.len() {
                break imports;
            }
            lexical.pop();
        };
        if imports.len() > 1 {
            return None;
        }
        if file.language != Language::Rust {
            if let Some(import) = imports.first() {
                if !self.complete {
                    return None;
                }
                let mut paths = self.paths.clone();
                if let Some(before) = before {
                    paths.extend(before.paths.iter().cloned());
                }
                let targets =
                    script_candidates(&file.path, import.script_module.as_deref()?, &paths);
                if targets.len() != 1 {
                    return None;
                }
                let name = if import.namespace && call.target.len() == 2 {
                    call.target[1].clone()
                } else if !import.namespace && call.target.len() == 1 {
                    import.target.first()?.clone()
                } else {
                    return None;
                };
                let id = format!("{}#{name}", targets[0]);
                let symbol = self.symbol(&id, before)?;
                if !symbol.public
                    && (!check_access && !self.files.get(&symbol.location.path)?.exports_complete)
                {
                    return None;
                }
                return (symbol.public || !check_access).then_some((
                    id,
                    "AST explicit relative import and exported declaration".into(),
                ));
            }
            if call.target.len() == 1 {
                return self.local(file, caller, call, before).map(|id| {
                    (
                        id,
                        "AST lexical declaration and scope, no shadow binding".into(),
                    )
                });
            }
            return None;
        }
        if imports.is_empty() && call.target.len() == 1 {
            if let Some(id) = self.local(file, caller, call, before) {
                return Some((
                    id,
                    "AST lexical declaration and scope, no shadow binding".into(),
                ));
            }
        }
        // Explicit associated functions in the same file use retained owner context.
        if imports.is_empty() && call.receiver.is_none() && call.target.len() == 2 {
            let owner = if head == "Self" {
                caller.owner.as_ref()?
            } else {
                head
            };
            let matches: Vec<_> = self
                .symbols
                .values()
                .filter(|symbol| {
                    symbol.location.path == file.path
                        && symbol.module_scope == caller.module_scope
                        && symbol.owner.as_ref() == Some(owner)
                        && symbol.entity.name == call.target[1]
                })
                .collect();
            if matches.len() == 1 {
                return Some((
                    matches[0].id.clone(),
                    "AST explicit associated type and unique owned declaration".into(),
                ));
            }
        }
        if !self.complete {
            return None;
        }
        let placements: BTreeSet<_> = self
            .placements
            .get(&file.path)
            .into_iter()
            .flatten()
            .chain(
                before
                    .into_iter()
                    .flat_map(|graph| graph.placements.get(&file.path).into_iter().flatten()),
            )
            .cloned()
            .collect();
        if placements.len() != 1 {
            return None;
        }
        let placement = placements.iter().next()?;
        let mut namespace = placement.module.clone();
        namespace.extend(caller.module_scope.clone());
        let mut target = if let Some(import) = imports.first() {
            let mut target = import.target.clone();
            target.extend(call.target.iter().skip(1).cloned());
            target
        } else {
            call.target.clone()
        };
        let mut module = namespace.clone();
        match target.first()?.as_str() {
            "crate" => {
                module.clear();
                target.remove(0);
            }
            "self" => {
                target.remove(0);
            }
            "super" => {
                while target.first().is_some_and(|part| part == "super") {
                    module.pop()?;
                    target.remove(0);
                }
            }
            _ if !imports.is_empty() => return None,
            _ => {}
        }
        let name = target.pop()?;
        module.extend(target);
        let key = (placement.root.clone(), module.clone(), name);
        let ids: BTreeSet<_> = self
            .names
            .get(&key)
            .into_iter()
            .flatten()
            .chain(
                before
                    .into_iter()
                    .flat_map(|graph| graph.names.get(&key).into_iter().flatten())
                    .filter(|id| !self.symbols.contains_key(*id)),
            )
            .cloned()
            .collect();
        if ids.len() != 1 {
            return None;
        }
        let id = ids.into_iter().next()?;
        let symbol = self.symbol(&id, before)?;
        let region = if symbol.parent_visible {
            &module[..module.len().saturating_sub(1)]
        } else {
            module.as_slice()
        };
        if symbol.restricted || (check_access && !symbol.public && !namespace.starts_with(region)) {
            return None;
        }
        for depth in 1..=module.len() {
            let key = (placement.root.clone(), module[..depth].to_vec());
            let access = self.modules.get(&key).or_else(|| {
                if !self.symbols.contains_key(&id) {
                    before.and_then(|graph| graph.modules.get(&key))
                } else {
                    None
                }
            })?;
            let public = access.public?;
            if check_access && !public && !namespace.starts_with(&module[..depth - 1]) {
                return None;
            }
        }
        Some((
            id,
            "AST explicit Rust import/module chain and declared access".into(),
        ))
    }

    /// Resolve retained lexical/import/owner evidence. A prior graph preserves
    /// references to removed declarations; presence and access remain separate.
    pub fn bind_calls<'a>(&'a self, before: Option<&'a Self>) -> Vec<BoundCall<'a>> {
        let mut result = Vec::new();
        for file in self.files.values() {
            for call in &file.calls {
                let strict = self.resolve(file, call, before, true);
                let accessible = strict.is_some();
                if let Some((id, reason)) =
                    strict.or_else(|| self.resolve(file, call, before, false))
                {
                    let symbol = self.symbol(&id, before).expect("resolved symbol");
                    let explicit_receiver = usize::from(
                        file.language == Language::Rust
                            && symbol.receiver_parameter
                            && call.receiver.is_none(),
                    );
                    result.push(BoundCall {
                        call,
                        symbol,
                        present: self.symbols.contains_key(&id),
                        accessible,
                        expected_min: symbol.min + explicit_receiver,
                        expected_max: symbol.max.map(|max| max + explicit_receiver),
                        reason,
                        configuration_dependent: symbol.conditional
                            || self.conditional_path(&call.location.path)
                            || if self.symbols.contains_key(&id) {
                                self.conditional_path(&symbol.location.path)
                            } else {
                                before.is_some_and(|before| {
                                    before.conditional_path(&symbol.location.path)
                                })
                            }
                            || self.symbols.get(&call.caller).is_some_and(|symbol| {
                                symbol.conditional || symbol.expansion_dependent
                            }),
                    });
                }
            }
        }
        result.sort_by(|a, b| {
            (&a.call.location, &a.symbol.id).cmp(&(&b.call.location, &b.symbol.id))
        });
        result
    }

    fn conditional_path(&self, path: &str) -> bool {
        self.placements
            .get(path)
            .into_iter()
            .flatten()
            .any(|placement| {
                (1..=placement.module.len()).any(|depth| {
                    self.modules
                        .get(&(placement.root.clone(), placement.module[..depth].to_vec()))
                        .is_some_and(|access| access.conditional)
                })
            })
    }
}
