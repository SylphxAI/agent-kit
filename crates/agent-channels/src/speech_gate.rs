//! Outbound speech gate for an agent's terminal text.
//!
//! An agent turn ends with terminal assistant text. Whether that text may be
//! posted to an external channel is decided here, in one place, and the
//! decision fails closed: any reason to stay silent wins.
//!
//! ```
//! use agent_channels::speech_gate::{
//!     OutboundSpeechMode, SuppressReason, TerminalDeliveryDecision, TerminalDeliveryInput,
//!     decide_terminal_text_delivery,
//! };
//!
//! let decision = decide_terminal_text_delivery(&TerminalDeliveryInput {
//!     mode: OutboundSpeechMode::DirectTerminalText,
//!     context_only: true,
//!     already_sent_via_tool: false,
//!     text: "Note to self: nobody asked me.",
//! });
//! assert_eq!(decision, TerminalDeliveryDecision::Suppress(SuppressReason::ContextOnly));
//! ```

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How an agent speaks on external channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboundSpeechMode {
    /// Only explicit send-tool calls reach a channel. Terminal text is the
    /// agent's private monologue and is never delivered.
    ToolOnly,
    /// Terminal text is delivered as the reply when no other gate objects.
    DirectTerminalText,
}

/// Inputs to [`decide_terminal_text_delivery`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalDeliveryInput<'a> {
    /// The agent's configured speech mode.
    pub mode: OutboundSpeechMode,
    /// The turn answers only context-only items (unaddressed group messages).
    pub context_only: bool,
    /// The agent already sent a message for this turn through a tool.
    pub already_sent_via_tool: bool,
    /// The terminal assistant text.
    pub text: &'a str,
}

/// Why terminal text is not delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuppressReason {
    /// [`OutboundSpeechMode::ToolOnly`].
    ToolOnly,
    /// The turn was context only.
    ContextOnly,
    /// A tool already delivered a message this turn.
    AlreadySentViaTool,
    /// The text is empty or a silence sentinel
    /// (see [`is_suppressed_terminal_sentinel`]).
    Sentinel,
}

/// The outcome of [`decide_terminal_text_delivery`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "decision", content = "reason", rename_all = "snake_case")]
pub enum TerminalDeliveryDecision {
    /// Deliver the terminal text to the reply target.
    Deliver,
    /// Deliver nothing.
    Suppress(SuppressReason),
}

impl TerminalDeliveryDecision {
    /// Whether the text should be delivered.
    #[must_use]
    pub fn should_deliver(self) -> bool {
        self == Self::Deliver
    }
}

/// Decide whether terminal text may be delivered to an external channel.
///
/// Checked in order, first match wins: tool-only mode, context-only turn,
/// already sent via tool, silence sentinel. Only when none applies is the
/// text delivered.
#[must_use]
pub fn decide_terminal_text_delivery(
    input: &TerminalDeliveryInput<'_>,
) -> TerminalDeliveryDecision {
    let reason = if input.mode == OutboundSpeechMode::ToolOnly {
        SuppressReason::ToolOnly
    } else if input.context_only {
        SuppressReason::ContextOnly
    } else if input.already_sent_via_tool {
        SuppressReason::AlreadySentViaTool
    } else if is_suppressed_terminal_sentinel(input.text) {
        SuppressReason::Sentinel
    } else {
        return TerminalDeliveryDecision::Deliver;
    };
    TerminalDeliveryDecision::Suppress(reason)
}

/// Whether `text` means "say nothing".
///
/// True for empty or whitespace-only text, and for `NO_REPLY` or
/// `completed` in any case, with any surrounding whitespace, punctuation,
/// quotes or Markdown (`` `NO_REPLY` ``, `**no reply**`, `Completed.`,
/// `no-reply`). Text whose first non-empty line is `NO_REPLY` is also
/// silent: the agent declared silence and the rest is a private note.
///
/// ```
/// use agent_channels::speech_gate::is_suppressed_terminal_sentinel;
///
/// assert!(is_suppressed_terminal_sentinel("  "));
/// assert!(is_suppressed_terminal_sentinel("`NO_REPLY`"));
/// assert!(is_suppressed_terminal_sentinel("Completed."));
/// assert!(is_suppressed_terminal_sentinel("NO_REPLY\n\n(thinking: nothing to add)"));
/// assert!(!is_suppressed_terminal_sentinel("The build completed."));
/// assert!(!is_suppressed_terminal_sentinel("👍"));
/// ```
#[must_use]
pub fn is_suppressed_terminal_sentinel(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return true;
    }
    let whole = sentinel_key(trimmed);
    if whole == "NOREPLY" || whole == "COMPLETED" {
        return true;
    }
    trimmed
        .lines()
        .find(|line| !line.trim().is_empty())
        .is_some_and(|first| sentinel_key(first) == "NOREPLY")
}

/// Read a context-only flag from item metadata (`contextOnly` or
/// `context_only`, boolean). Absent means not context only.
#[must_use]
pub fn context_only_from_metadata(metadata: &Value) -> bool {
    ["contextOnly", "context_only"]
        .iter()
        .any(|key| metadata.get(*key).and_then(Value::as_bool) == Some(true))
}

/// Alphanumerics only, uppercased: `"**No-Reply.**"` becomes `"NOREPLY"`.
fn sentinel_key(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_uppercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decide(
        mode: OutboundSpeechMode,
        context_only: bool,
        already_sent_via_tool: bool,
        text: &str,
    ) -> TerminalDeliveryDecision {
        decide_terminal_text_delivery(&TerminalDeliveryInput {
            mode,
            context_only,
            already_sent_via_tool,
            text,
        })
    }

    use OutboundSpeechMode::{DirectTerminalText as Direct, ToolOnly};
    use TerminalDeliveryDecision::{Deliver, Suppress};

    #[test]
    fn direct_mode_delivers_addressed_text() {
        assert_eq!(decide(Direct, false, false, "Here is the answer."), Deliver);
        assert!(decide(Direct, false, false, "👍").should_deliver());
    }

    #[test]
    fn tool_only_never_delivers_terminal_text() {
        for (context_only, sent) in [(false, false), (true, false), (false, true), (true, true)] {
            assert_eq!(
                decide(ToolOnly, context_only, sent, "A real answer"),
                Suppress(SuppressReason::ToolOnly)
            );
        }
    }

    #[test]
    fn context_only_private_note_is_never_delivered() {
        assert_eq!(
            decide(
                Direct,
                true,
                false,
                "Private note: they are talking about lunch."
            ),
            Suppress(SuppressReason::ContextOnly)
        );
    }

    #[test]
    fn already_sent_via_tool_is_not_delivered_twice() {
        assert_eq!(
            decide(Direct, false, true, "Sent you the report above."),
            Suppress(SuppressReason::AlreadySentViaTool)
        );
    }

    #[test]
    fn sentinels_are_suppressed() {
        for text in [
            "",
            "   \n\t",
            "NO_REPLY",
            "no_reply",
            " NO_REPLY. ",
            "`NO_REPLY`",
            "**NO_REPLY**",
            "\"NO_REPLY\"",
            "no-reply",
            "No reply",
            "completed",
            "Completed.",
            "COMPLETED!",
            "NO_REPLY\nI decided the group does not need me.",
            "\n\nNO_REPLY\n\nnote",
        ] {
            assert_eq!(
                decide(Direct, false, false, text),
                Suppress(SuppressReason::Sentinel),
                "{text:?}"
            );
        }
    }

    #[test]
    fn text_that_merely_contains_a_sentinel_is_delivered() {
        for text in [
            "The migration completed successfully.",
            "Reply NO_REPLY if you want silence.",
            "Status:\nNO_REPLY was handled",
            "Completed: 3 of 4 tasks",
        ] {
            assert_eq!(decide(Direct, false, false, text), Deliver, "{text:?}");
        }
    }

    #[test]
    fn decision_serializes_with_reason() {
        assert_eq!(
            serde_json::to_value(Suppress(SuppressReason::ContextOnly)).unwrap(),
            json!({"decision": "suppress", "reason": "context_only"})
        );
        assert_eq!(
            serde_json::to_value(Deliver).unwrap(),
            json!({"decision": "deliver"})
        );
        assert_eq!(
            serde_json::from_value::<OutboundSpeechMode>(json!("tool_only")).unwrap(),
            ToolOnly
        );
    }

    #[test]
    fn context_only_metadata_flag() {
        assert!(context_only_from_metadata(&json!({"contextOnly": true})));
        assert!(context_only_from_metadata(&json!({"context_only": true})));
        assert!(!context_only_from_metadata(&json!({"contextOnly": "true"})));
        assert!(!context_only_from_metadata(&json!({})));
    }
}
