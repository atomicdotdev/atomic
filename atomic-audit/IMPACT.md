# Change-rooted semantic impact

Review consumes the existing semantic and recorded-content search layers. It
does not maintain its own AST parser or scan the first 256 repository files.

## Ownership

- `atomic-semantic::ParserRegistry::analyze_source` invokes the registered grammar
  once, reuses its existing entity visitor, and retains qualified call spelling,
  receiver, lexical owner, imports, visibility and declaration context.
- Existing `Reference` results retain the backward-compatible simple `symbol`
  and optionally include structured `call` context. A syntactic reference's high
  confidence is not proof that its target has been resolved.
- `atomic-semantic::relationships::SourceGraph` binds supported relationships from
  those shared facts. Same-name methods/functions do not establish relationships.
  It borrows declaration facts per call rather than cloning function bodies.
- Existing repository caller queries use the same source graph. The former global
  simple-name join and duplicate relative-import resolver have been removed.
- `atomic-repository::RecordedSemanticView` pins before/after recorded view
  projections and uses the existing `RecordedContentSearch` syntext path/regex
  machinery to select source **before** AST budgets. No ambient working-copy
  index is treated as authoritative for a historical snapshot.
- `atomic-audit::impact::analyze_semantic` applies review policy to semantic facts:
  changed declarations, incoming/outgoing relationships, availability/access/arity
  checks, introduction evidence and scoped limitations. It does not traverse ASTs.
- CLI code translates established contract issues into numbered findings and
  preserves evidence in the graph/JSON. It does not infer criterion failures.

## Selection and performance

1. Load and parse changed supported source first; retain it for later stages.
2. Compare semantic entities and source tokens to derive changed functions/methods.
3. Search for those targets. Private lexical regions constrain eligible paths;
   owned methods include owner context instead of globally searching `value`/`id`.
4. Parse matching files plus explicit declaration/import context using the same
   registry. Unrelated nonmatching syntax, macros and cfg declarations never
   consume the candidate AST budget or produce per-file review warnings.

Search results are candidates, not confirmed callers. Recorded search may verify
source bytes; reduced AST work is not a promise of zero content I/O or a new
persistent semantic index. `searched_files_before/after` and
`inspected_files_before/after` expose the distinction. A regression fixture with
300 unrelated files parses only the two affected/caller files in each snapshot.

The bounded adapter accepts up to four selected source changes, 256 changed plus
matching source files and 64 additional declaration/import context files. Roots,
matches and context each have an 8 MiB source allowance; individual parsing is
limited to 1 MiB, 100,000 AST nodes and 256 nesting levels. Truncation is a scoped
selection gap, not a guessed zero or a reason to replace the selected change.

## Evidence and limits

Incoming/outgoing references retain call sites, declaration identity, qualified
spelling and binding reason. Before callers survive declaration removal. Removed
and inaccessible references are not counted as legal current callers. Full path
inventory is preserved even when only relevant files are parsed, so an omitted
alternative cannot silently become a unique import target.

Ordinary attributes, cfg/test declarations and macros are retained as source
context, not blanket file exclusions. Configuration/expansion-dependent evidence
is labeled; it is not authenticated active-build configuration. Ambiguous scopes,
custom visibility and unresolved receiver/import context are not guessed.

The source-level contract assessment covers supported availability/access/argument
counts (currently Rust and TypeScript/TSX conventions), not arbitrary type checking
or runtime behavior. Known lexical/declared receivers and explicit module/import
paths can establish bindings; dynamic/inferred/cross-crate cases may remain
unresolved. Public APIs may have external consumers. Absence of findings does not
prove low implementation risk. Reviewers still inspect relevant types, behavior,
persistence, security and tests where needed.

The terminal-only Thinking guard is retained. It clears on success/error and never
animates in JSON, pipes or dumb terminals. No compiler, build script, language
server, model invocation or application-code remediation is launched by this pass.
