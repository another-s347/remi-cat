use crate::skill::BuiltinSkill;

pub const BUILTIN_REMI_SKILL_NAME: &str = "remi";

const BUILTIN_REMI_SKILL_DESCRIPTION: &str =
    "Builtin guide for using Remi to inspect and manage its own profiles, agents, workflows, and runtime settings.";

const BUILTIN_REMI_SKILL_TEMPLATE: &str = r#"---
name: remi
description: Builtin guide for using Remi to inspect and manage its own profiles, agents, workflows, and runtime settings.
---

# Remi Self-Management

This is a builtin read-only skill for operating Remi itself through the local `remi-cat` CLI via the `manage_yourself` tool.

Use this skill when the user asks Remi to inspect, configure, start, stop, or manage Remi runtime profiles, agent profiles, supervisor workflows, sandbox settings, ports, or setup state.

## Safety

- Prefer read-only commands first: `profile list`, `profile show`, `profile status`, `profile agent list`, and `profile workflow list`.
- Do not delete or overwrite profiles, agents, or workflows unless the user explicitly asks for that destructive change.
- For profile deletion, require `--force` only when the user clearly requested deletion.
- For background instances, use `profile status <name>` before `profile stop`, `profile restart`, or `profile delete`.
- Use `manage_yourself` for Remi CLI commands. It only supports one argument shape: `{"command":"profile list"}`.
- The `command` value is the arguments after `remi-cat`; do not include the binary name and do not wrap it in another `arguments` field.
- `manage_yourself` runs the current host `remi-cat` binary directly, so it works even when sandboxed shell commands cannot see the binary.
- Profile commands resolve `.remi-cat/profiles` relative to the Remi host process current directory.
- When a command may affect a different profile, include the profile name explicitly.

## Profile Commands

Tool call example:

```json
{"command":"profile list"}
```

Inspect profiles:

```bash
profile list
profile show <profile>
profile status <profile>
profile status --all
```

Create or update runtime configuration:

```bash
profile create <profile> admin.enabled=false sandbox.kind=no_sandbox shell.mode=local
--profile <profile> config set admin.port=8790
--profile <profile> sandbox set kind=no_sandbox
```

Manage background instances:

```bash
profile start <profile>
profile stop <profile>
profile restart <profile>
```

Delete a named profile only when requested:

```bash
profile delete <profile> --force
```

## Agent Profile Commands

```bash
profile agent list <profile>
profile agent show <profile> <agent_id>
profile agent upsert <profile> ./agents/<agent_id>.md
profile agent set-default <profile> <agent_id>
```

Agent files are markdown with YAML frontmatter. `agent upsert` validates the markdown and writes `<profile-data-dir>/agents/<id>.md`.

## Supervisor Workflow Commands

```bash
profile workflow list <profile>
profile workflow show <profile> <workflow_id>
profile workflow upsert <profile> ./workflows/<workflow_id>.json
profile workflow delete <profile> <workflow_id>
```

Workflow files are JSON graph definitions. `workflow upsert` validates the graph and writes `<profile-data-dir>/workflows/<id>.json`. The builtin `goal` workflow can be listed and shown, but it cannot be overwritten or deleted.

## Common Procedure

1. Read this skill with `skill__get` before changing Remi configuration.
2. Use `manage_yourself` to run the appropriate `remi-cat` command.
3. Verify the result with a read command such as `profile show`, `profile agent list`, or `profile workflow list`.
4. Report the exact profile name, changed file or setting, and verification result.
"#;

pub fn builtin_remi_skill() -> BuiltinSkill {
    BuiltinSkill {
        name: BUILTIN_REMI_SKILL_NAME,
        description: BUILTIN_REMI_SKILL_DESCRIPTION,
        content: BUILTIN_REMI_SKILL_TEMPLATE.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::builtin_remi_skill;
    use crate::skill::{BuiltinSkillStore, FileSkillStore, SkillStore};

    #[tokio::test]
    async fn builtin_remi_skill_is_searchable_and_readable() {
        let store = BuiltinSkillStore::new(FileSkillStore::with_roots([]), [builtin_remi_skill()]);

        let results = store.search("profile workflow").await.unwrap();
        assert!(results.iter().any(|skill| skill.name == "remi"));

        let doc = store.get("remi").await.unwrap().unwrap();
        assert_eq!(doc.name, "remi");
        assert!(doc.content.contains("Use `manage_yourself`"));
        assert!(!doc.content.contains("{{REMI_CAT_BIN}}"));
        assert!(!doc
            .content
            .contains("Use this exact `remi-cat` binary path"));
        assert!(doc.content.contains("profile agent list"));
        assert!(doc.content.contains("profile workflow upsert"));
    }
}
