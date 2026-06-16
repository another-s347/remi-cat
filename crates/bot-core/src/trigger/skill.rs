use crate::skill::BuiltinSkill;

pub const BUILTIN_TRIGGER_SKILL_NAME: &str = "trigger";

const BUILTIN_TRIGGER_SKILL_DESCRIPTION: &str =
    "Builtin reference for remi-cat trigger capabilities and rule examples.";

const BUILTIN_TRIGGER_SKILL_CONTENT: &str = r#"---
name: trigger
description: Builtin reference for remi-cat trigger capabilities and rule examples.
---

# Trigger Capability Reference

This is a builtin read-only skill. Use it as the canonical reference before calling `trigger__upsert`.

## Scope

- Only the owner can create, update, list, or delete triggers.
- Fired triggers run in the original chat thread where they were created.
- The user's semantic request is stored in a bound Thing; the trigger rules decide when that request should run.

## Tool Surface

- `trigger__upsert` creates or updates one trigger.
- `trigger__list` shows the active triggers for the current thread.
- `trigger__delete` removes one trigger by local id.

## Rule Shape

Both `precondition` and `condition` are arrays of objects with this shape:

```json
{
  "rule": "timer('5m')",
  "description": "Once, five minutes from now"
}
```

At least one rule must be provided across `precondition` and `condition`.

## Timing Support

Use `timer('...')` for one-shot reminders and relative waits. This is the right choice when the user asks for "in 5 minutes", "after 30 minutes", "tonight at 21:00", or an exact timestamp.

Timer values can be relative durations like `5m`, `30min`, `2h`, or absolute RFC3339 timestamps like `2026-04-05T09:00:00+08:00`.

Use `cron('...')` only for recurring schedules, such as every day at 09:00 or every Monday morning. A cron rule is recurring; do not use it for a one-time "five minutes later" reminder.

One-shot example:

```json
{
  "name": "Check long-running job",
  "request": "Five minutes have passed. Check the long-running job and report the result to the user.",
  "precondition": [
    {
      "rule": "timer('5m')",
      "description": "Once, five minutes from now"
    }
  ],
  "condition": []
}
```

Recurring example:


```json
{
  "name": "Morning summary",
  "request": "Send me a concise work summary for today.",
  "precondition": [
    {
      "rule": "cron('0 9 * * *')",
      "description": "Every day at 09:00"
    }
  ],
  "condition": []
}
```

## Practical Guidance

- Keep one trigger focused on one user intent.
- Use a short, explicit name so `trigger__list` stays easy to scan.
- Put the full user-facing instruction in `request`; do not compress it into the rule description.
- When new SDK trigger conditions are added later, this skill is the canonical place to document them. Prefer updating this skill over expanding tool schema prose.
"#;

pub fn builtin_trigger_skill() -> BuiltinSkill {
    BuiltinSkill {
        name: BUILTIN_TRIGGER_SKILL_NAME,
        description: BUILTIN_TRIGGER_SKILL_DESCRIPTION,
        content: BUILTIN_TRIGGER_SKILL_CONTENT.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::builtin_trigger_skill;

    #[test]
    fn trigger_skill_guides_one_shot_waits_to_timer() {
        let skill = builtin_trigger_skill();

        assert!(skill.content.contains("timer('5m')"));
        assert!(skill.content.contains("cron rule is recurring"));
        assert!(skill.content.contains("do not use it for a one-time"));
    }
}
