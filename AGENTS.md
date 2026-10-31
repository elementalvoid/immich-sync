# immich-sync

## Agent skills

### Issue tracker

Issues live in this repo's GitHub Issues and are managed with the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

The five canonical triage roles map to the same label strings in this repo. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: `GLOSSARY.md` and `docs/adr/` at the repo root. See `docs/agents/domain.md`.

### Vendored skills

`.agents/skills/` holds the skills this repo expects an agent to use, installed with `npx skills`
and pinned by `skills-lock.json` so every checkout has them:

- `rust-skills` — for any Rust edit or review.
- `gpui-kit` and `gpui-kit-design-guides` — for any UI, component, layout or state decision.

Update with `npx skills update`. Do not edit a vendored skill by hand; the lockfile pins its source.

## Building

### UI and accessibility

The shell is Rust with gpui-kit, pinned exactly (ADR 0001). Before changing a surface, read
`docs/agents/ui.md`: it records the accessibility and kit-component rules this repo paid for in
review — what makes an accessibility node exist, what makes it announce, which kit controls are
traps, and how to verify a layout. Those rules are requirements, not suggestions.
