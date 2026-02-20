---
name: skill-creator
description: Use when the user asks to create or update a SKILL.md-based capability.
---

# Skill Creator

Use this skill when the user asks to add, edit, or improve a project skill.

## Goal

Create maintainable, concise skills that can be reused by the agent runtime.

## Workflow

1. Confirm the skill target.
- Default location: `groups/<group-folder>/skills/<skill-name>/SKILL.md`
- Shared skill location: `groups/global/skills/<skill-name>/SKILL.md`

2. Normalize the skill name.
- Use lowercase, digits, and hyphens only.
- Keep names short and action-oriented.

3. Write a focused `SKILL.md`.
- Include frontmatter `name` and `description`.
- Keep core instructions short and executable.
- Prefer checklists and explicit triggers over long prose.

4. Add only necessary extras.
- Add `references/` only when detailed domain docs are required.
- Add `scripts/` only when deterministic automation is needed.
- Do not add extra docs like `README.md` for the skill itself.

5. Validate quickly.
- Ensure the file path and name are correct.
- Ensure there are no missing referenced files.
- Run repository tests/build checks when the change touches runtime code.

## Quality Rules

- Keep context efficient: include only information the model needs to execute.
- Prefer reusable patterns over one-off instructions.
- Avoid vague instructions; specify when to trigger and what output is expected.
- If uncertain about scope, ask for one concrete usage example and optimize for that first.
