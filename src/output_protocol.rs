use std::collections::HashMap;
use std::path::{Path, PathBuf};

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize};

pub const OUTPUT_PROTOCOL_VERSION: u8 = 1;
pub const BROADCAST_ALL_REF: &str = "all";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputCapability {
    #[default]
    Disabled,
    Text,
    Native,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputCapabilities {
    pub user_mentions: OutputCapability,
    pub agent_mentions: OutputCapability,
    pub broadcast_all: OutputCapability,
    pub agent_handoff: bool,
    pub images: OutputCapability,
    pub files: OutputCapability,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputEntityKind {
    User,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputEntity {
    pub reference: String,
    pub kind: OutputEntityKind,
    pub label: String,
}

impl OutputEntity {
    pub fn new(
        reference: impl Into<String>,
        kind: OutputEntityKind,
        label: impl Into<String>,
    ) -> Self {
        Self {
            reference: reference.into(),
            kind,
            label: label.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputProtocolContext {
    pub version: u8,
    pub surface: String,
    pub conversation_id: String,
    pub chat_type: String,
    pub self_reference: String,
    pub capabilities: OutputCapabilities,
    pub entities: Vec<OutputEntity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct A2aConversationContext {
    pub conversation_id: String,
    pub handoff_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_handoff_id: Option<String>,
    pub sequence: u64,
    pub source_message_id: String,
    pub initiator: String,
    pub caller: String,
    pub recipient: String,
    pub participants: Vec<String>,
}

impl OutputProtocolContext {
    pub fn new(
        surface: impl Into<String>,
        conversation_id: impl Into<String>,
        chat_type: impl Into<String>,
        self_reference: impl Into<String>,
    ) -> Self {
        Self {
            version: OUTPUT_PROTOCOL_VERSION,
            surface: surface.into(),
            conversation_id: conversation_id.into(),
            chat_type: chat_type.into(),
            self_reference: self_reference.into(),
            capabilities: OutputCapabilities::default(),
            entities: Vec::new(),
        }
    }

    pub fn prompt(&self) -> String {
        let mut caps = Vec::new();
        if self.capabilities.user_mentions != OutputCapability::Disabled {
            caps.push("user");
        }
        if self.capabilities.agent_mentions != OutputCapability::Disabled {
            caps.push("agent");
        }
        if self.capabilities.broadcast_all != OutputCapability::Disabled {
            caps.push("all");
        }
        if self.capabilities.images != OutputCapability::Disabled {
            caps.push("image");
        }
        if self.capabilities.files != OutputCapability::Disabled {
            caps.push("file");
        }

        let mut lines = vec![format!(
            "<remi-output v=\"{}\" self=\"{}\" caps=\"{}\">",
            self.version,
            self.self_reference,
            caps.join(",")
        )];
        lines.push("Use Markdown.".into());
        if self.capabilities.user_mentions != OutputCapability::Disabled
            || self.capabilities.agent_mentions != OutputCapability::Disabled
        {
            lines.push("Mention: @[name](remi-mention:REF). Use only listed refs.".into());
        }
        if self.capabilities.broadcast_all != OutputCapability::Disabled {
            lines.push("All: @[所有人](remi-mention:all).".into());
        }
        if self.capabilities.images != OutputCapability::Disabled
            || self.capabilities.files != OutputCapability::Disabled
        {
            lines.push(
                "Image/file: ![name](remi-resource:PATH) / [name](remi-resource:PATH).".into(),
            );
        }
        if self.capabilities.agent_handoff {
            lines.push("Mentioning an agent hands off the conversation.".into());
        }
        if !self.entities.is_empty() {
            lines.push("entities:".into());
            lines.extend(self.entities.iter().map(|entity| {
                format!(
                    "{}|{}|{}",
                    entity.reference,
                    match entity.kind {
                        OutputEntityKind::User => "user",
                        OutputEntityKind::Agent => "agent",
                    },
                    compact_label(&entity.label)
                )
            }));
        }
        lines.push("</remi-output>".into());
        lines.join("\n")
    }

    /// Keep the identities that matter to an A2A recipient at the front of the
    /// compact directory without duplicating entries.
    pub fn prioritize_entities(&mut self, caller_reference: Option<&str>) {
        let mut prioritized = Vec::with_capacity(self.entities.len());
        for reference in [Some(self.self_reference.as_str()), caller_reference]
            .into_iter()
            .flatten()
        {
            if let Some(entity) = self
                .entities
                .iter()
                .find(|entity| entity.reference == reference)
            {
                if !prioritized
                    .iter()
                    .any(|existing: &OutputEntity| existing.reference == entity.reference)
                {
                    prioritized.push(entity.clone());
                }
            }
        }
        for entity in &self.entities {
            if !prioritized
                .iter()
                .any(|existing| existing.reference == entity.reference)
            {
                prioritized.push(entity.clone());
            }
        }
        self.entities = prioritized;
    }

    pub fn entity(&self, reference: &str) -> Option<&OutputEntity> {
        self.entities
            .iter()
            .find(|entity| entity.reference == reference)
    }
}

fn compact_label(label: &str) -> String {
    label
        .chars()
        .filter(|character| !matches!(character, '\n' | '\r' | '|'))
        .take(80)
        .collect::<String>()
        .trim()
        .to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputDocument {
    pub source: String,
    pub nodes: Vec<OutputNode>,
    pub diagnostics: Vec<OutputDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputNode {
    pub start: usize,
    pub end: usize,
    pub kind: OutputNodeKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputNodeKind {
    EntityMention {
        reference: String,
        entity_kind: OutputEntityKind,
        label: String,
    },
    BroadcastAll {
        label: String,
    },
    Resource {
        path: String,
        label: String,
        image: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputDiagnostic {
    pub code: &'static str,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug)]
struct PendingLink {
    start: usize,
    destination: String,
    label: String,
    image: bool,
}

pub fn parse_output(text: &str, context: &OutputProtocolContext) -> OutputDocument {
    let entities = context
        .entities
        .iter()
        .map(|entity| (entity.reference.as_str(), entity))
        .collect::<HashMap<_, _>>();
    let mut nodes = Vec::new();
    let mut diagnostics = Vec::new();
    let mut pending = Vec::<PendingLink>::new();

    for (event, range) in Parser::new_ext(text, Options::all()).into_offset_iter() {
        match event {
            Event::Start(Tag::Link { dest_url, .. }) => pending.push(PendingLink {
                start: range.start,
                destination: dest_url.into_string(),
                label: String::new(),
                image: false,
            }),
            Event::Start(Tag::Image { dest_url, .. }) => pending.push(PendingLink {
                start: range.start,
                destination: dest_url.into_string(),
                label: String::new(),
                image: true,
            }),
            Event::Text(value) | Event::Code(value) => {
                if let Some(link) = pending.last_mut() {
                    link.label.push_str(&value);
                }
            }
            Event::End(TagEnd::Link) | Event::End(TagEnd::Image) => {
                let Some(mut link) = pending.pop() else {
                    continue;
                };
                let mut start = link.start;
                if !link.image && start > 0 && text.as_bytes()[start - 1] == b'@' {
                    start -= 1;
                }
                let end = range.end;
                if let Some(reference) = link.destination.strip_prefix("remi-mention:") {
                    if link.image
                        || text.as_bytes().get(link.start.wrapping_sub(1)) != Some(&b'@')
                        || is_escaped(text, link.start - 1)
                    {
                        diagnostics.push(OutputDiagnostic {
                            code: "invalid_mention_syntax",
                            start,
                            end,
                        });
                    } else if reference == BROADCAST_ALL_REF {
                        if context.capabilities.broadcast_all == OutputCapability::Disabled {
                            diagnostics.push(OutputDiagnostic {
                                code: "broadcast_not_available",
                                start,
                                end,
                            });
                        } else {
                            nodes.push(OutputNode {
                                start,
                                end,
                                kind: OutputNodeKind::BroadcastAll { label: link.label },
                            });
                        }
                    } else if let Some(entity) = entities.get(reference) {
                        let enabled = match entity.kind {
                            OutputEntityKind::User => {
                                context.capabilities.user_mentions != OutputCapability::Disabled
                            }
                            OutputEntityKind::Agent => {
                                context.capabilities.agent_mentions != OutputCapability::Disabled
                            }
                        };
                        if enabled {
                            nodes.push(OutputNode {
                                start,
                                end,
                                kind: OutputNodeKind::EntityMention {
                                    reference: reference.to_string(),
                                    entity_kind: entity.kind,
                                    label: std::mem::take(&mut link.label),
                                },
                            });
                        } else {
                            diagnostics.push(OutputDiagnostic {
                                code: "mention_not_available",
                                start,
                                end,
                            });
                        }
                    } else {
                        diagnostics.push(OutputDiagnostic {
                            code: "unknown_entity",
                            start,
                            end,
                        });
                    }
                } else if let Some(path) = link.destination.strip_prefix("remi-resource:") {
                    let enabled = if link.image {
                        context.capabilities.images != OutputCapability::Disabled
                    } else {
                        context.capabilities.files != OutputCapability::Disabled
                    };
                    if path.trim().is_empty() {
                        diagnostics.push(OutputDiagnostic {
                            code: "empty_resource_path",
                            start,
                            end,
                        });
                    } else if enabled {
                        nodes.push(OutputNode {
                            start,
                            end,
                            kind: OutputNodeKind::Resource {
                                path: path.to_string(),
                                label: link.label,
                                image: link.image,
                            },
                        });
                    } else {
                        diagnostics.push(OutputDiagnostic {
                            code: "resource_not_available",
                            start,
                            end,
                        });
                    }
                }
            }
            _ => {}
        }
    }
    nodes.sort_by_key(|node| node.start);
    OutputDocument {
        source: text.to_string(),
        nodes,
        diagnostics,
    }
}

fn is_escaped(text: &str, index: usize) -> bool {
    text.as_bytes()[..index]
        .iter()
        .rev()
        .take_while(|byte| **byte == b'\\')
        .count()
        % 2
        == 1
}

/// Resolve a model-authored resource without allowing it to escape the roots
/// the embedding host has explicitly exposed.
pub fn resolve_resource_path(
    raw_path: &str,
    workspace: &Path,
    readable_roots: &[PathBuf],
) -> anyhow::Result<PathBuf> {
    let raw_path = raw_path.trim();
    if raw_path.is_empty() {
        anyhow::bail!("resource path is empty");
    }
    let requested = PathBuf::from(raw_path);
    let requested = if requested.is_absolute() {
        requested
    } else {
        workspace.join(requested)
    };
    let resolved = requested.canonicalize().map_err(|error| {
        anyhow::anyhow!("cannot read resource {}: {error}", requested.display())
    })?;
    if !resolved.is_file() {
        anyhow::bail!("resource is not a file: {}", resolved.display());
    }
    let roots = if readable_roots.is_empty() {
        vec![workspace.to_path_buf()]
    } else {
        readable_roots.to_vec()
    };
    let allowed = roots.into_iter().any(|root| {
        root.canonicalize()
            .ok()
            .is_some_and(|root| resolved.starts_with(root))
    });
    if !allowed {
        anyhow::bail!("resource is outside the channel-readable sandbox");
    }
    Ok(resolved)
}

impl OutputDocument {
    pub fn render_mentions<F, A>(&self, mut entity: F, mut all: A) -> String
    where
        F: FnMut(&str, OutputEntityKind, &str) -> Option<String>,
        A: FnMut(&str) -> Option<String>,
    {
        let mut rendered = self.source.clone();
        for node in self.nodes.iter().rev() {
            let replacement = match &node.kind {
                OutputNodeKind::EntityMention {
                    reference,
                    entity_kind,
                    label,
                } => entity(reference, *entity_kind, label),
                OutputNodeKind::BroadcastAll { label } => all(label),
                OutputNodeKind::Resource { .. } => None,
            };
            if let Some(replacement) = replacement {
                rendered.replace_range(node.start..node.end, &replacement);
            }
        }
        rendered
    }

    pub fn render_resources<F>(&self, mut resource: F) -> String
    where
        F: FnMut(&str, bool, &str) -> Option<String>,
    {
        let mut rendered = self.source.clone();
        for node in self.nodes.iter().rev() {
            if let OutputNodeKind::Resource { path, label, image } = &node.kind {
                if let Some(replacement) = resource(path, *image, label) {
                    rendered.replace_range(node.start..node.end, &replacement);
                }
            }
        }
        rendered
    }

    pub fn agent_mentions(&self) -> Vec<&str> {
        let mut result = Vec::new();
        for node in &self.nodes {
            if let OutputNodeKind::EntityMention {
                reference,
                entity_kind: OutputEntityKind::Agent,
                ..
            } = &node.kind
            {
                if !result.contains(&reference.as_str()) {
                    result.push(reference.as_str());
                }
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> OutputProtocolContext {
        let mut context = OutputProtocolContext::new("tui", "c1", "p2p", "a0");
        context.capabilities = OutputCapabilities {
            user_mentions: OutputCapability::Native,
            agent_mentions: OutputCapability::Native,
            broadcast_all: OutputCapability::Native,
            agent_handoff: true,
            images: OutputCapability::Native,
            files: OutputCapability::Native,
        };
        context.entities = vec![
            OutputEntity::new("a0", OutputEntityKind::Agent, "Self"),
            OutputEntity::new("a1", OutputEntityKind::Agent, "Reviewer"),
            OutputEntity::new("u1", OutputEntityKind::User, "Alice"),
        ];
        context
    }

    #[test]
    fn prompt_is_compact_and_capability_driven() {
        let prompt = context().prompt();
        let fixed = prompt.split("entities:\n").next().unwrap();
        assert!(fixed.chars().count() <= 300, "{fixed}");
        assert!(prompt.contains("a0|agent|Self"));
        assert!(prompt.contains("All: @[所有人](remi-mention:all)."));
    }

    #[test]
    fn parses_mentions_broadcast_and_resources() {
        let document = parse_output(
            "@[Alice](remi-mention:u1) @[All](remi-mention:all) @[R](remi-mention:a1) ![x](remi-resource:a.png) [f](remi-resource:f.pdf)",
            &context(),
        );
        assert_eq!(document.nodes.len(), 5);
        assert_eq!(document.agent_mentions(), vec!["a1"]);
        assert!(document.diagnostics.is_empty());
    }

    #[test]
    fn unknown_and_plain_mentions_do_not_become_nodes() {
        let document = parse_output("@Alice @[Unknown](remi-mention:u9)", &context());
        assert!(document.nodes.is_empty());
        assert_eq!(document.diagnostics[0].code, "unknown_entity");
    }

    #[test]
    fn escaped_mentions_have_no_protocol_semantics() {
        let document = parse_output(r"\@[Alice](remi-mention:u1)", &context());
        assert!(document.nodes.is_empty());
        assert_eq!(document.diagnostics[0].code, "invalid_mention_syntax");
    }

    #[test]
    fn prompt_prioritizes_self_then_caller() {
        let mut context = context();
        context.self_reference = "a1".into();
        context.prioritize_entities(Some("u1"));
        assert_eq!(
            context
                .entities
                .iter()
                .map(|entity| entity.reference.as_str())
                .collect::<Vec<_>>(),
            vec!["a1", "u1", "a0"]
        );
    }

    #[test]
    fn renderer_replaces_only_protocol_mentions() {
        let document = parse_output(
            "Hi @[Alice](remi-mention:u1) and @[all](remi-mention:all)",
            &context(),
        );
        assert_eq!(
            document.render_mentions(
                |reference, _, label| Some(format!("<{reference}:{label}>")),
                |_| Some("<ALL>".into())
            ),
            "Hi <u1:Alice> and <ALL>"
        );
    }

    #[test]
    fn resource_resolution_stays_inside_readable_roots() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("result.txt");
        std::fs::write(&file, "ok").unwrap();
        assert_eq!(
            resolve_resource_path("result.txt", root.path(), &[]).unwrap(),
            file.canonicalize().unwrap()
        );

        let outside = tempfile::NamedTempFile::new().unwrap();
        assert!(resolve_resource_path(
            outside.path().to_str().unwrap(),
            root.path(),
            &[root.path().to_path_buf()]
        )
        .is_err());
    }
}
