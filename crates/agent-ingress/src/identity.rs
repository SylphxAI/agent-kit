use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

/// Conversation shape, resolved by the host from channel metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatType {
    Direct,
    Group,
    Channel,
    Unknown,
}

/// The sender's relationship to the receiving agent, resolved by the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SenderRelationship {
    Owner,
    Admin,
    Parent,
    Child,
    Peer,
    Trusted,
    Stranger,
}

impl SenderRelationship {
    /// Protected senders always pass ingress and cannot be block targets.
    pub const fn is_protected(self) -> bool {
        matches!(self, Self::Owner | Self::Admin | Self::Parent | Self::Peer)
    }
}

/// Identity envelope built by the host, never accepted from the message body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SenderIdentity {
    pub channel: String,
    pub sender_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    pub chat_type: ChatType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub relationship: SenderRelationship,
}

impl SenderIdentity {
    /// Render one line of structured context with fixed keys and JSON-escaped
    /// identifiers. Display metadata is deliberately excluded: it is often
    /// user-editable and never confers a relationship.
    ///
    /// Put this header in host-controlled context, separate from the body.
    /// Escaping prevents spoofing the header structure, not manipulation of a
    /// model by text. Authorization must use verified identity, not the prompt.
    pub fn format_awareness_header(&self) -> String {
        #[derive(Serialize)]
        struct Awareness<'a> {
            relationship: SenderRelationship,
            channel: &'a str,
            sender_id: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            chat_id: Option<&'a str>,
            chat_type: ChatType,
        }

        let json = serde_json::to_string(&Awareness {
            relationship: self.relationship,
            channel: &self.channel,
            sender_id: &self.sender_id,
            chat_id: self.chat_id.as_deref(),
            chat_type: self.chat_type,
        })
        .expect("awareness serializes only strings and unit enums");
        let mut header = String::with_capacity("sender_awareness: ".len() + json.len());
        header.push_str("sender_awareness: ");
        for ch in json.chars() {
            // Keep the header one visual line, preserve bidi ordering, and prevent
            // embedded IDs from closing markup delimiters in caller prompts.
            match ch {
                '<'
                | '>'
                | '&'
                | '`'
                | '\u{061c}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}' => write!(header, "\\u{:04x}", ch as u32).unwrap(),
                ch if ch.is_control() => write!(header, "\\u{:04x}", ch as u32).unwrap(),
                _ => header.push(ch),
            }
        }
        header
    }
}
