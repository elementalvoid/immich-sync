# Building a surface: accessibility and kit rules

Rules this repo paid for in review. Each one was a real defect that a green test suite did not
catch, so they are requirements, not advice. They were learned adopting gpui-kit 0.8.1 (#74-#82).

Read `.agents/skills/gpui-kit/SKILL.md` (Coding Guides) and
`.agents/skills/gpui-kit-design-guides/SKILL.md` before changing a screen, and
`.agents/skills/rust-skills/SKILL.md` before changing Rust. Where a skill and this file disagree,
this file wins: it is about *this* app on *this* pinned version.

## Accessibility

**A named element needs a role.** gpui builds an accessibility node only when an element has both
an id and a role (`gpui-pre` `element.rs`: `if let Some(role) = self.element.a11y_role()`), and a
plain `div()` reports `GenericContainer`, which is filtered to `None`. So
`div().id(..).aria_label(..)` produces no node at all, and the snapshots still read the
declaration, so no test notices. Use `crate::a11y::named(role, id, name, description)` — it takes
the role and the name together so they cannot drift apart.

**Nothing announces without a live region.** gpui-pre 0.3.10 has no `live` property, and every
accesskit adapter gates announcements on `live != Off`. The one public hook is
`crate::notice::polite_live_region`, applied to every non-quiet `Notice` and `EmptyState`. A new
element that must be announced needs it, and on macOS the announcement fires on a changed
*value*, not a changed name, so a repeat needs both to move.

**One feedback vocabulary.** Notices come from `crate::notice::Notice`, empty states from
`crate::empty_state::EmptyState`. Do not add a third card or a second coloured edge. `Notice::quiet()`
is for a strip whose container already names the same message, and only where severity is uniform
across the list; it deliberately emits no node at all.

**A success is not an alert.** `Severity::Success` maps to `Role::Status`; the assertive roles are
for errors and warnings. A success mapped to `Alert` is a regression, not a style choice.

**Announce once.** Before adding a name, check what the surrounding container already says. The
failed row that carried its class and error twice is why `quiet()` exists.

## Use the kit, and verify it against the pin

**Never hand-build what the kit provides** — and never trust an API from memory or from docs for a
newer version. The pin is exact (ADR 0001), so read the real source in the cargo registry
(`$CARGO_HOME/registry/src/*/gpui-kit-<version>/`) or `cargo doc`, and cite what you found in a
comment when a workaround needs explaining.

**When the kit cannot do the job, say so where the code is**, in one comment naming the limitation
and the version, and record the decision in ADR 0001 if it changes how the shell is assembled.

Known kit traps on 0.8.1, each of which cost a review round:

| Trap | What it does | What to do |
| --- | --- | --- |
| `Dialog` binds `enter → Confirm` | Dispatch returns before the focused control sees the key, so Enter is dead on every non-text control inside a dialog | `.keyboard(false)` on the host, or accept that Enter means "confirm" |
| A context-scoped `escape` binding | Does not fire when the focus trap falls back to the layer host, so Escape dies for backward traversal | Bind `escape` unscoped, and test Shift-Tab from the first control |
| `SidebarMenuItem` | A `div` with `on_click` and no focus handle: pointer-only, invisible to the keyboard | App-owned entries with role, label, selected state and a keyed tab stop |
| `Button` | Cannot carry `aria_selected` (it does not compile) | Use `Tab` for anything with a selected state |
| `Tab::ix` / `TabBar` | Renames a tab to its integer index, losing the stable id | App-authored tab elements, or `crate::tab::Tab` |
| `DescriptionList`, `SearchableList` | Render no role of their own, so the grouping is flat text | Wrap in a role-bearing container (`Role::DescriptionList`) and give the items their roles |
| `Tabs` | Does set `Role::TabList`, but is `RenderOnce`, so the snapshots cannot see it | Fine at runtime; use a div with the list role when a test must assert the grouping |

## Verification

**Check the narrowest window.** The shell minimum is 520×480 (`shell.rs`). A layout that only
looks right at the default size is unfinished, and a capture at the minimum in both themes is the
cheap way to find it.

**Never win a criterion by shrinking content.** If a control no longer fits, the layout is wrong,
not the content. A test-side scroll or a hidden row is the symptom, not the fix.

**Never loosen an assertion to fit an upstream change.** Widening a matcher (`TextInput |
PasswordInput`) to make a bump pass is how a silent regression ships. Assert the new expectation
per site, and say in the commit why the release forced it.

**Make claims the artefacts support.** "Both themes render it" needs a capture path; "three runs
are byte-identical" needs hashes. A report that overstates is worse than no report, because the
next agent builds on it.

## Build hygiene

Concurrent worktrees sharing one `CARGO_TARGET_DIR` serve each other's binaries: a test has run a
sibling branch's build more than once. Before a run you intend to trust, either use a private
target dir (`CARGO_TARGET_DIR=/data/home/projects/immich-sync-target-<ticket>`) or
`find crates -name '*.rs' -exec touch {} +`, and never believe a failure that contradicts the
source until you have.
