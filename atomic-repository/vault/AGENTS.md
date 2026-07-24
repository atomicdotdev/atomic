# Atomic VCS Agent

You use **Atomic VCS** (not git). A draft view is created for each session automatically.

## Version control rules

- **Never use `git` for repository operations** — no `git status`,
  `git diff`, `git log`, `git add`, `git commit`, `git checkout`,
  `git branch`, `git merge`, `git pull`, or `git push`. They bypass Atomic's
  graph and provenance, and `git checkout` / `git reset` / `git restore`
  fight the working copy Atomic materializes.
- Use the **Atomic CLI** instead:
  - `atomic status` instead of `git status`
  - `atomic diff` instead of `git diff`
  - `atomic log` instead of `git log`
  - `atomic change <hash>` instead of `git show <hash>`
  - `atomic view list` instead of `git branch`
  - `atomic pull` / `atomic push` instead of `git pull` / `git push`
- **Do NOT run `atomic add` or `atomic record`.** The Antigravity plugin
  records your changes automatically with full AI provenance (session,
  timing, decision graph) when the turn ends — running them yourself
  pre-empts the plugin's recording and loses the provenance graph.

## Code discovery: knowledge graph first

The vault knowledge graph indexes every function, struct, trait, enum, and
module, plus recorded changes. Search it before grep or reading whole files:

1. `atomic vault query search "term"` — find entities, files, and changes by
   keyword (use 1–2 specific words, not natural language)
2. `atomic vault query neighbors <node-id>` — explore callers, definitions,
   and changes; copy node IDs verbatim from search results
3. Read files only after the KG tells you where to look

Use grep only for literal strings the KG does not index. The KG is populated
from recorded changes — unrecorded work will not appear in it.

## Every prompt is a turn. Every turn follows this sequence.

### 1. Create an intent

Check `atomic vault intent list` first (do not duplicate an existing intent), then:

```bash
atomic vault intent create --title "<short title>"
```

This gives you an intent ID (e.g., HELL-4) and a file path.

### 2. Define the problem

The user's prompt is usually a **solution** ("build me X"). Reframe it as a **problem statement**.

Ask clarifying questions if the problem is ambiguous. Do not guess — ask.

Once the problem is clear, define:

- **Problem statement** — what problem are we solving and why
- **Success criteria** — concrete, testable conditions that mean "done"
- **Tasks** — ordered list of work items

Write all of this into the intent file, then run `atomic vault sync` to
persist the file into the vault database. The intent file lives on disk, but
`atomic vault intent show`/`update` read from the database — without `sync`
they see the original placeholder template, and `update` will overwrite your
file edits with it.

### 3. Execute the tasks

Work through the tasks in order. After completing each one:

1. **Verify** it meets its criteria — run the commands or checks specified.
2. **Edit the intent file** using your file editing tool to mark it done
   (`- [ ]` → `- [x]`), and check off any satisfied acceptance criteria.
   Use your file editing tool — not bash, not Python, not sed. Raw file
   manipulation bypasses the vault.
3. **Sync** so the database stays current:
   ```bash
   atomic vault sync
   ```

### 4. Update the intent

```bash
atomic vault sync                          # persist file edits to the database first
atomic vault intent update <ID> --status done
```

## Rules

- **One intent per turn.** Every prompt gets its own intent.
- **Problem first.** Reframe solution-requests as problems. Ask questions if unclear.
- **Write the intent file before coding.** The plan goes in the file, not just in chat.
- **Do run `atomic vault sync` after editing any `.vault/` file** — it moves
  your edits into the vault database; it is not `atomic record`, and hooks
  do not do it for you mid-turn.
- **Do not run `atomic add` or `atomic record`.** Hooks handle this with provenance.
- **Do not create or switch views.** The session view is created automatically.
- **Do not run `atomic agent enable`.** The integration is already configured.

## Skills

Use these for detailed reference when needed:

- `/code-intelligence` — knowledge graph queries for code exploration
- `/atomic-vault` — intent and goal lifecycle, memory operations
- `/atomic-vcs` — inspect repository state and history: `status`, `log`, `change`, `diff`
