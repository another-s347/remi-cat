use crate::skill::BuiltinSkill;

pub const BUILTIN_REMI_SKILL_NAME: &str = "remi";

const BUILTIN_REMI_SKILL_DESCRIPTION: &str =
    "Builtin guide for using Remi to inspect and manage its own profiles, agents, workflows, and runtime settings.";

const BUILTIN_REMI_SKILL_TEMPLATE: &str = r#"---
name: remi
description: Builtin guide for using Remi to inspect and manage its own profiles, agents, workflows, and runtime settings.
pin: true
---

# Remi Self-Management

This is a builtin guide for operating Remi itself through the local `remi-cat` CLI via the `manage_yourself` tool.

Use this skill when the user asks Remi to inspect, configure, start, stop, or manage Remi runtime profiles, agent profiles, supervisor workflows, sandbox settings, ports, or setup state.
Also use it when the user asks about updating remi-cat, configuring Codex ACP, configuring Remi hooks, checking Feishu/Lark readiness, choosing a local CLI channel/session, or discovering runtime slash commands and skills.

## Safety

- Prefer read-only commands first: `profile list`, `profile show`, `profile status`, `profile channel list`, `profile agent list`, and `workflow list`.
- Do not remove or overwrite profiles, channels, agents, or workflows unless the user explicitly asks for that destructive change.
- `profile unregister` removes only the global registry entry; it preserves the manifest and all referenced resources/state.
- For background instances, use `profile status <reference>` before `profile stop`, `profile restart`, or `profile unregister`.
- Use `manage_yourself` for Remi CLI commands. It only supports one argument shape: `{"command":"profile list"}`.
- Telemetry for the active profile can be inspected or changed with `{"command":"telemetry status"}`, `telemetry enable`, and `telemetry disable`.
- Use help through `manage_yourself` when unsure about syntax, for example `{"command":"help"}`, `{"command":"profile --help"}`, or `{"command":"profile channel --help"}`.
- Current-session runtime settings use slash commands in chat. To change reasoning strength for the active session, tell the user or runtime to run `/model reasoning set <auto|none|minimal|low|medium|high|xhigh|max>`; use `/model reasoning reset` to return to the model profile default.
- Use `{"command":"tools --json"}` before adding tools to an agent profile. It lists all runtime-known tools, ignores the active allowlist, and includes configuration warnings/errors.
- The `command` value is the arguments after `remi-cat`; do not include the binary name and do not wrap it in another `arguments` field.
- `manage_yourself` runs the current host `remi-cat` binary directly, so it works even when sandboxed shell commands cannot see the binary.
- Registered profiles use the process-global registry at `~/.remi-cat/profile-registry.json`, independent of the current directory and profile state directory.
- Prefer registered references such as `@travel` for every command after initialization.
- When a command may affect a different profile, include the profile name explicitly.
- Preserve an explicit destination path exactly as the user supplied it. Do not shorten an absolute path to a workspace-relative path. After `profile init`, use `profile show @alias --sources --format yaml` and confirm that `manifest` and `workspace` resolve beneath the intended destination before making further changes.
- Feishu/Lark chat channels are normally resolved from incoming IM events. For local CLI testing, use `cli --channel <id>` to reuse a persistent local session.

## Profile Commands

Tool call example:

```json
{"command":"profile list"}
{"command":"tools --json"}
{"command":"profile agent --help"}
```

Inspect profiles:

```bash
profile list
profile show <profile>
profile status <profile>
profile status --all
```

Create, register, and edit a profile manifest:

```bash
profile init ./profiles/travel --id travel.planner --name "Travel Planner" --template remi-cat --with-runtime --register travel
profile set @travel description "Plans and manages travel"
profile set @travel capabilities.tags travel,planner
profile set @travel capabilities.intents plan-travel,manage-itinerary
profile check @travel --strict
```

Use an absolute or workspace-relative destination with `profile init`; it writes a manifest and referenced resources there. `profile set` edits only typed manifest fields. Runtime settings use the selected profile:

```bash
--profile @travel config set admin.enabled=false im.mode=disabled
--profile @travel sandbox set kind=no_sandbox
--profile @travel acp setup --client codex
```

## IM Channel Commands

Concrete channel instances live in the profile's referenced `channels.yaml`; `capabilities.channels` is only discovery metadata.

```bash
profile channel list @travel
profile channel upsert-feishu @travel work --transport websocket --app-id-env TRAVEL_FEISHU_APP_ID --app-secret-env TRAVEL_FEISHU_APP_SECRET
profile channel upsert-feishu @travel webhook --transport event-hook --host 127.0.0.1 --port 8791 --path /feishu/events --verification-token-env TRAVEL_FEISHU_VERIFY_TOKEN
profile channel disable @travel work
profile channel enable @travel work
profile channel remove @travel work --force
profile set @travel capabilities.channels feishu
```

Channel commands store credential key references, never secret values. After changing enabled state or connector settings, restart the managed profile instance to apply the configuration.

Manage background instances:

```bash
profile start <profile>
profile status <profile>
profile stop <profile>
profile restart <profile>
profile status --all --format json
```

Managed instances are persistent host processes used for Web/IM serving. They are different from `profile ask`, which starts an A2A endpoint on demand for one conversation and cleans it up afterward. Start requires a manifest-backed profile. Use `--instance NAME` to run or control independent instances of the same profile.

Unregister a profile only when requested:

```bash
profile unregister <profile>
```

## Agent Profile Commands

```bash
profile agent list <profile>
profile agent show <profile> <agent_id>
profile agent upsert <profile> ./agents/<agent_id>.md
profile agent set-default <profile> <agent_id>
```

Agent files are markdown with YAML frontmatter. `agent upsert` validates the markdown and writes `<profile-data-dir>/agents/<id>.md`.
`profile agent list` shows both builtin agents and profile-specific overrides; `profile agent show` displays the resolved definition.

## Supervisor Workflow Commands

```bash
workflow list --profile <profile>
workflow show --profile <profile> <workflow_id>
workflow add --profile <profile> ./workflows/<workflow_id>.json
workflow rm --profile <profile> <workflow_id>
```

Workflow files are JSON graph definitions. `workflow upsert` validates the graph and writes `<profile-data-dir>/workflows/<id>.json`. The builtin `goal` workflow can be listed and shown, but it cannot be overwritten or deleted.

To create or modify a supervisor workflow:

1. Inspect existing workflows first:

```bash
workflow list --profile <profile>
workflow show --profile <profile> <workflow_id>
```

2. Write or edit a workflow JSON file in the workspace using normal file-editing tools. The workflow shape is:

```json
{
  "version": 1,
  "id": "review-loop",
  "name": "Review Loop",
  "description": "Review work until it is complete.",
  "prompt": "Evaluate the main agent's progress and choose the next edge.",
  "start_prompt": "Start by reviewing the task and history.",
  "initial_node": "review",
  "terminal_node": "done",
  "nodes": [
    { "id": "review", "prompt": "Decide whether more work is required." },
    { "id": "done", "prompt": "The workflow is complete." }
  ],
  "edges": [
    { "id": "continue", "from": "review", "to": "review", "prompt": "Use when more work is required." },
    { "id": "complete", "from": "review", "to": "done", "prompt": "Use when the task is complete." }
  ]
}
```

3. Upsert the file, which creates the workflow if it is new or modifies the existing workflow with the same `id`:

```bash
workflow add --profile <profile> ./workflows/review-loop.json
workflow show --profile <profile> review-loop
```

4. To use a workflow in the current conversation, send a runtime command in the session. Profile workflows can be started directly as slash commands by id; `/workflow ...` remains the management command:

```text
/review-loop --max-rounds 5 --context {"goal":"verify the task"}
/workflow status
/workflow clear
```

5. Delete only when the user explicitly requested deletion:

```bash
workflow rm --profile <profile> review-loop
workflow list --profile <profile>
```

## ACP Client Commands

```bash
acp setup --client codex --bin /path/to/codex --agent default
acp setup --client codex --arg=--config --arg=model=\"gpt-5-codex\"
acp setup --client remi --tool-name acp__remi
acp setup --client remi --bin /path/to/remi-cat --tool-name acp__remi
acp agent
acp setup --client my-acp --mode remote --base-url http://127.0.0.1:8788 --tool-name acp__my_acp
acp doctor
```

`acp setup` writes the ACP runtime settings for the selected profile. `--client codex` writes an out-of-box local profile that launches `remi-cat acp-adapter codex`; repeated `--arg` values become adapter argv passed to Codex before `exec`, and the backend still speaks standard ACP stdio. `codex setup` and `codex doctor` remain convenience aliases for that profile. Other local clients use `acp.local_bin` plus repeated `--arg` values as their process argv. Named ACP tools default to `codex` for the Codex adapter setup command, or `acp__<client>` for other clients unless `--tool-name` is set. `acp agent` runs remi-cat as a standard ACP stdio agent and defaults to the HOME config root unless `REMI_DATA_DIR` is set. For `--client remi`, omitting `--bin` uses Remi's internal local runner; providing `--bin /path/to/remi-cat` configures an external stdio process and defaults local args to `["acp","agent"]` unless explicit `--arg` values are supplied.

## Hook Commands

```bash
hooks
hooks list --json
hooks trust <hash>
hooks enable <hash>
hooks disable <hash>
```

Remi hooks are Remi-owned and use Codex-compatible definitions. Default hook sources are `REMI_DATA_DIR/hooks.json`, `REMI_DATA_DIR/hooks/config.toml`, `.remi-cat/hooks.json`, and `.remi-cat/hooks.toml` under the workspace root. Codex hook files are imported only when `REMI_IMPORT_CODEX_HOOKS=1` or a Remi hooks TOML config sets `import_codex_hooks = true`. Trust and disabled state live under `REMI_DATA_DIR/hooks/`.

## Feishu/Lark Commands

```bash
feishu init
feishu doctor
```

`feishu init` configures Lark CLI and app credentials. `feishu doctor` checks CLI auth and remi-cat credentials. It does not create a chat channel; runtime channel bindings come from incoming Feishu/Lark messages and sub-session events.

## Update Commands

```bash
update check
update check --json
update self --dry-run
update self --version v0.2.1
```

`update check` queries GitHub releases. `update self` installs a release with Cargo; use `--dry-run` before making changes.

## Local Session And Skill Commands

```bash
cli --channel support --user alice --name Alice "Hello"
cli --channel support "/help"
cli --channel support "/skill list"
cli --channel support "/tools"
```

The same `--channel` id resumes the same local CLI session. Runtime `/help` lists slash commands; `/skill list` shows local and builtin skills.

## Skill Pinning

Local `SKILL.md` YAML frontmatter supports `pin: true` or `pin: false`. The default is false, so downloaded skills are not pinned unless edited locally. Pinned skills have only their name and description injected into a small pinned-skill prompt for discovery; full instructions still require `skill__get` or `/skill:<name>`. The pinned prompt is cached by the running Remi process and refreshes after restart.

## Common Procedure

1. Read this skill with `skill__get` before changing Remi configuration.
2. Use `manage_yourself` to run the appropriate `remi-cat` command.
3. Verify the result with a read command such as `profile show`, `profile channel list`, `profile status`, `profile agent list`, or `workflow list`.
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
        assert!(store
            .featured_summaries()
            .iter()
            .any(|skill| skill.name == "remi" && skill.pin));

        let doc = store.get("remi").await.unwrap().unwrap();
        assert_eq!(doc.name, "remi");
        assert!(doc.content.contains("Use `manage_yourself`"));
        assert!(!doc.content.contains("{{REMI_CAT_BIN}}"));
        assert!(!doc
            .content
            .contains("Use this exact `remi-cat` binary path"));
        assert!(doc.content.contains("profile agent list"));
        assert!(doc.content.contains("profile channel upsert-feishu"));
        assert!(doc.content.contains("profile restart"));
        assert!(doc.content.contains(r#"{"command":"help"}"#));
        assert!(doc
            .content
            .contains(r#"{"command":"profile channel --help"}"#));
    }
}
