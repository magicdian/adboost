<!-- PHRONIA:COPILOT-GUIDANCE:START -->
# Phronia Review Guidance

When reviewing a Phronia-enabled repository:

- Treat Phronia-generated runtime and platform payloads as copied integration files in consumer repositories. Examples include `.phronia/scripts/**`, `.phronia/agents/**`, `.phronia/workflow.md`, and platform Phronia files under `.agents/**`, `.claude/**`, `.codex/**`, `.cursor/**`, `.gemini/**`, `.github/**`, `.opencode/**`, `.qoder/**`, `.codebuddy/**`, `.factory/**`, `.pi/**`, `.reasonix/**`, `.trae/**`, and `.zcode/**`.
- Do not leave line-by-line style, wording, or local refactor comments on those copied files unless the PR explicitly changes Phronia integration, the copied file is the primary subject, it leaks secrets, it has syntax or wiring breakage, or it contradicts the PR goal.
- If a Phronia-generated file needs a behavior change, leave one handoff comment asking for an upstream Phronia fix. Include the affected path, expected behavior, observed evidence or reproduction steps, and why the fix belongs upstream.
- Continue reviewing repo-owned `.phronia/spec/**`, `.phronia/tasks/**`, application code, migrations, tests, docs, and non-Phronia scripts normally.
- Group duplicate root-cause findings into one comment and point to a deterministic local check when one already covers the issue class.
<!-- PHRONIA:COPILOT-GUIDANCE:END -->
