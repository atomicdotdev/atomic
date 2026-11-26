# Automatic metric conventions: atomic.metrics.syntax.v1

All counting is over recorded source supplied by the adapter. The existing
`atomic-semantic` parser registry supplies the nine syntax modes; no code is
executed and no compiler/type checker is run. A module boundary here is one
supplied source file, not an inferred whole package or runtime application.

## Subjects and availability

- Functions/methods/closures with known syntax are measured separately. Class
  metrics apply to TypeScript/TSX, Python, Java class/record/enum, C++ class/struct
  and Swift `class` declarations. Rust structs, Go/C structs and Swift value types
  are not silently relabeled classes.
- Qualified lexical names identify subjects. Renames are separate subjects;
  anonymous or overloaded matches have no inferred before-value, location or
  delta. Unavailable baseline is never replaced by zero.
- Known additions/deletions use `not_applicable` on the absent side. Invalid,
  binary, non-UTF-8, unreadable or unsupported source produces unavailable facts.
- A valid file without applicable declarations has not-applicable function or
  class observations, while an empty class genuinely has zero methods and zero
  implemented source complexity.
- Locations are one-based inclusive declaration ranges on each recorded side.
  Method length counts code-bearing body lines; a signature-only line is excluded,
  but an opening delimiter on a signature line is body code and counts.

## Length and parameters

Length is based on AST token ranges, skipping comment nodes and blank lines.
Strings count as source code (including Python docstrings), not comment text.
Nested function/type bodies do not inflate a containing function's length;
nested type bodies do not inflate the containing class. A nested declaration's
header can still contribute a code-bearing line. Class length includes methods.

Declared parameter slots are counted, not identifier tokens: Go `a, b int` has
two slots, while a destructured/rest parameter has one. Rust `self`, Go receiver
lists, TypeScript `this` pseudo-parameters and the first Python instance/class
receiver are excluded. Canonical `@staticmethod` leaves all declared slots.
Return slots and inferred/compiler-generated parameters are not reconstructed.
Unsupported/inferred parameter syntax is unavailable, not an assumed zero.

Method count includes directly declared constructors and syntactic methods;
overload/abstract signatures count as declarations. Function-valued fields are
not method declarations. Inherited and nested-type methods are excluded.
Aggregate class complexity sums concrete source bodies. Abstract/signature-only
and compiler-generated bodies do not contribute source complexity; an unresolved
C/C++ out-of-line or Java native implementation makes the aggregate unavailable.
C++ default arguments are not mistaken for pure-virtual method bodies.

## Branching and nesting

Function-body complexity starts at one. Explicit `if`/`elif`, loops, catch/except,
guard and conditional expressions add decisions. Logical `&&`/`||` and Python
`and`/`or` operators add short-circuit decisions. Non-default switch/match cases
add decisions; wildcard default arms without guards do not. Assertions, compiler
expansions, implicit failure propagation and parameter-default execution are not
reconstructed. Nested functions/types are separate subjects.

Nesting tracks control constructs, not ordinary lexical braces. An `else if` or
Python `elif` remains at the chain's depth. Nested functions/types do not inflate
the enclosing function's control metrics. These are syntactic proxies, not a
compiler CFG path count or a universal measure of readability.

## Declared dependencies and visibility

Dependency fan-out de-duplicates explicit declarations rather than counting
import statements. Units are Python module paths, TS/TSX import/re-export paths,
Rust imported crate/root namespaces, Java qualified import targets, Go import
paths, literal C/C++ headers and Swift imported module roots. Aliases/repeated
declarations do not inflate counts. Nonliteral header/import paths are unavailable.
Dynamic loads, implicit/prelude dependencies, macro expansion and type-resolved
coupling are not claimed by this metric.

Public surface counts unique named bindings under source visibility, including
visible class members. Overload sets count once. Private/local declarations and
hidden enclosing types are excluded where declared visibility is known. Python
uses naming conventions or a static literal `__all__`; dynamic export lists are
unavailable. Wildcard re-exports, dynamic CommonJS exports, unresolved public Rust
impl targets and preprocessing-dependent visibility are not guessed. It is not
a compiled ABI/export-table measurement or a proof of compatibility.

## Changed-token duplication

The comparison scope is the exact supplied paths, separated by language/syntax
mode. The default token window is 32, preserving names and literal values while
discarding comments and whitespace. This is a detector setting, not a universal
quality threshold. Hash matches are verified against actual tokens. Matches
inside the same file must not be overlapping copies; matched token positions are
unioned so overlapping windows do not multiply the count.

A bounded Myers token diff selects changed positions on each side. Only those
positions that also belong to a clone count toward the metric. Formatting-only
changes do not invent duplicated tokens. Unavailable baseline, incomplete
same-language comparison inputs, exhausted diff work or clone-comparison caps
produce unavailable counts rather than partial lower bounds disguised as totals.
Zero means zero within the declared supplied comparison scope, not the repository.

## Default work limits

Per request: 128 supplied file pairs, 8 MiB source across both sides, 80,000
tokens, 1,024 function/class subjects, 16 million token-diff work units and 100,000
clone candidate comparisons. Per source: 1 MiB, 20,000 tokens, 256 subjects,
300,000 AST nodes and 256 AST nesting levels. `ExtractionConfig` exposes the
request limits; exceeding them is disclosed, never interpreted as a clean zero.
Review's default clone scope is changed files only; no exhaustive source scan is
started to fill a missing measurement.

Token-index or clone/diff work exhaustion affects duplication availability, not
otherwise valid local AST counts. A long method does not lose its length or
complexity measurement just because a clone index cannot be completed.
