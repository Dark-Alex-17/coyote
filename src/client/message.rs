use super::Model;

use crate::{function::ToolResult, multiline_text, utils::dimmed_text};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: MessageContent,
}

impl Default for Message {
    fn default() -> Self {
        Self {
            role: MessageRole::User,
            content: MessageContent::Text(String::new()),
        }
    }
}

impl Message {
    pub fn new(role: MessageRole, content: MessageContent) -> Self {
        Self { role, content }
    }

    pub fn merge_system(&mut self, system: MessageContent) {
        match (&mut self.content, system) {
            (MessageContent::Text(text), MessageContent::Text(system_text)) => {
                self.content = MessageContent::Array(vec![
                    MessageContentPart::Text { text: system_text },
                    MessageContentPart::Text {
                        text: text.to_string(),
                    },
                ])
            }
            (MessageContent::Array(list), MessageContent::Text(system_text)) => {
                list.insert(0, MessageContentPart::Text { text: system_text })
            }
            (MessageContent::Text(text), MessageContent::Array(mut system_list)) => {
                system_list.push(MessageContentPart::Text {
                    text: text.to_string(),
                });
                self.content = MessageContent::Array(system_list);
            }
            (MessageContent::Array(list), MessageContent::Array(mut system_list)) => {
                system_list.append(list);
                self.content = MessageContent::Array(system_list);
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    Assistant,
    User,
    Tool,
}

#[allow(dead_code)]
impl MessageRole {
    pub fn is_system(&self) -> bool {
        matches!(self, MessageRole::System)
    }

    pub fn is_user(&self) -> bool {
        matches!(self, MessageRole::User)
    }

    pub fn is_assistant(&self) -> bool {
        matches!(self, MessageRole::Assistant)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Array(Vec<MessageContentPart>),
    // Note: This type is primarily for convenience and does not exist in OpenAI's API.
    ToolCalls(MessageContentToolCalls),
}

impl MessageContent {
    pub fn render_input(
        &self,
        resolve_url_fn: impl Fn(&str) -> String,
        agent_info: &Option<(String, Vec<String>)>,
    ) -> String {
        match self {
            MessageContent::Text(text) => multiline_text(text),
            MessageContent::Array(list) => {
                let (mut concatenated_text, mut files) = (String::new(), vec![]);
                for item in list {
                    match item {
                        MessageContentPart::Text { text } => {
                            concatenated_text = format!("{concatenated_text} {text}")
                        }
                        MessageContentPart::ImageUrl { image_url } => {
                            files.push(resolve_url_fn(&image_url.url))
                        }
                    }
                }
                if !concatenated_text.is_empty() {
                    concatenated_text = format!(" -- {}", multiline_text(&concatenated_text))
                }
                format!(".file {}{}", files.join(" "), concatenated_text)
            }
            MessageContent::ToolCalls(MessageContentToolCalls {
                tool_results, text, ..
            }) => {
                let mut lines = vec![];
                if !text.is_empty() {
                    lines.push(text.clone())
                }
                for tool_result in tool_results {
                    if let Some(round_text) = &tool_result.text {
                        lines.push(round_text.clone())
                    }
                    let mut parts = vec!["Call".to_string()];
                    if let Some((agent_name, functions)) = agent_info
                        && functions.contains(&tool_result.call.name)
                    {
                        parts.push(agent_name.clone())
                    }
                    parts.push(tool_result.call.name.clone());
                    parts.push(tool_result.call.arguments.to_string());
                    lines.push(dimmed_text(&parts.join(" ")));
                }
                lines.join("\n")
            }
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            MessageContent::Text(text) => Some(text),
            _ => None,
        }
    }

    pub fn merge_prompt(&mut self, replace_fn: impl Fn(&str) -> String) {
        match self {
            MessageContent::Text(text) => *text = replace_fn(text),
            MessageContent::Array(list) => {
                if list.is_empty() {
                    list.push(MessageContentPart::Text {
                        text: replace_fn(""),
                    })
                } else if let Some(MessageContentPart::Text { text }) = list.get_mut(0) {
                    *text = replace_fn(text)
                }
            }
            MessageContent::ToolCalls(_) => {}
        }
    }

    pub fn to_text(&self) -> String {
        match self {
            MessageContent::Text(text) => text.to_string(),
            MessageContent::Array(list) => {
                let mut parts = vec![];
                for item in list {
                    if let MessageContentPart::Text { text } = item {
                        parts.push(text.clone())
                    }
                }
                parts.join("\n\n")
            }
            MessageContent::ToolCalls(_) => String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ImageUrl {
    pub url: String,
}

/// An extended-thinking block returned by Anthropic-protocol models or an
/// OpenAI Responses reasoning item. Serialized to match each API's wire
/// format (`type: thinking` / `type: redacted_thinking` / `type: reasoning`)
/// so blocks can be replayed verbatim, signature intact, in subsequent
/// tool-loop rounds as the APIs require.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThinkingBlock {
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    Reasoning {
        id: String,
        summary: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MessageContentToolCalls {
    pub tool_results: Vec<ToolResult>,
    pub text: String,
    pub sequence: bool,
}

impl MessageContentToolCalls {
    pub fn new(tool_results: Vec<ToolResult>, text: String) -> Self {
        Self {
            tool_results,
            text,
            sequence: false,
        }
    }

    pub fn merge(&mut self, mut tool_results: Vec<ToolResult>, text: String) {
        if !text.is_empty()
            && let Some(first) = tool_results.first_mut()
        {
            first.text = Some(text);
        }
        self.tool_results.extend(tool_results);
        self.sequence = true;
    }
}

pub fn patch_messages(messages: &mut Vec<Message>, model: &Model) {
    if messages.is_empty() {
        return;
    }
    if let Some(prefix) = model.system_prompt_prefix() {
        if messages[0].role.is_system() {
            messages[0].merge_system(MessageContent::Text(prefix.to_string()));
        } else {
            messages.insert(
                0,
                Message {
                    role: MessageRole::System,
                    content: MessageContent::Text(prefix.to_string()),
                },
            );
        }
    }
    if model.no_system_message() && messages[0].role.is_system() {
        let system_message = messages.remove(0);
        if let (Some(message), system) = (messages.get_mut(0), system_message.content) {
            message.merge_system(system);
        }
    }
}

pub fn extract_system_message(messages: &mut Vec<Message>) -> Option<String> {
    if messages.is_empty() {
        return None;
    }

    if messages[0].role.is_system() {
        let system_message = messages.remove(0);
        return Some(system_message.content.to_text());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn old_session_thinking_blocks_still_deserialize() {
        let json = json!([
            { "type": "thinking", "thinking": "hmm", "signature": "sig123" },
            { "type": "redacted_thinking", "data": "b64data" },
        ]);

        let blocks: Vec<ThinkingBlock> = serde_json::from_value(json.clone()).unwrap();

        assert!(
            matches!(&blocks[0], ThinkingBlock::Thinking { thinking, signature } if thinking == "hmm" && signature == "sig123")
        );
        assert!(
            matches!(&blocks[1], ThinkingBlock::RedactedThinking { data } if data == "b64data")
        );
        assert_eq!(serde_json::to_value(&blocks).unwrap(), json);
    }

    #[test]
    fn reasoning_block_round_trips() {
        let json = json!({
            "type": "reasoning",
            "id": "rs_1",
            "summary": [{ "type": "summary_text", "text": "thinking about it" }],
            "encrypted_content": "enc123",
        });

        let block: ThinkingBlock = serde_json::from_value(json.clone()).unwrap();

        assert!(
            matches!(&block, ThinkingBlock::Reasoning { id, encrypted_content, .. } if id == "rs_1" && encrypted_content.as_deref() == Some("enc123"))
        );
        assert_eq!(serde_json::to_value(&block).unwrap(), json);
    }

    #[test]
    fn reasoning_block_without_encrypted_content_round_trips() {
        let json = json!({
            "type": "reasoning",
            "id": "rs_1",
            "summary": [],
        });

        let block: ThinkingBlock = serde_json::from_value(json.clone()).unwrap();

        assert!(
            matches!(&block, ThinkingBlock::Reasoning { encrypted_content, .. } if encrypted_content.is_none())
        );
        assert_eq!(serde_json::to_value(&block).unwrap(), json);
    }

    #[test]
    fn thinking_blocks_round_trip_through_yaml() {
        let json = json!([
            { "type": "thinking", "thinking": "hmm", "signature": "sig123" },
            { "type": "redacted_thinking", "data": "b64data" },
            {
                "type": "reasoning",
                "id": "rs_1",
                "summary": [{ "type": "summary_text", "text": "thinking about it" }],
                "encrypted_content": "enc123",
            },
            {
                "type": "reasoning",
                "id": "rs_2",
                "summary": [],
            },
        ]);

        let blocks: Vec<ThinkingBlock> = serde_json::from_value(json.clone()).unwrap();

        let yaml = serde_yaml::to_string(&blocks).unwrap();
        assert!(!yaml.contains("encrypted_content: null"));

        let restored: Vec<ThinkingBlock> = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(serde_json::to_value(&restored).unwrap(), json);
    }
}
