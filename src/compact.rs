use crate::session::ChatMessage;

/// Token budget for the session context. When the estimated footprint
/// (chars/4, or the provider-reported input tokens when known) grows past
/// this, the runtime compacts the history before the next model call.
pub const DEFAULT_MESSAGE_BUDGET: usize = 96_000;
/// Recent messages never summarized away by compaction.
pub const KEEP_RECENT_MESSAGES: usize = 4;
pub const SUMMARY_BUDGET: usize = 8_000;
const SUMMARY_LINE_LIMIT: usize = 320;

/// Index where compaction splits the history: everything before it is
/// summarized into one message, the tail stays verbatim.
pub fn compaction_split(messages: &[ChatMessage]) -> usize {
    messages.len().saturating_sub(KEEP_RECENT_MESSAGES)
}

/// Bullet transcript (most recent first, budget-capped) of a message range.
/// Feeds the extractive summary and the provider-side compaction prompt.
pub fn transcript(messages: &[ChatMessage], budget: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    for message in messages.iter().rev() {
        let content = normalize(&message.content);
        if content.is_empty() && message.tool_calls.is_none() {
            continue;
        }
        let detail = if content.is_empty() {
            message
                .tool_calls
                .as_ref()
                .map(|calls| {
                    calls
                        .iter()
                        .map(|call| call.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default()
        } else {
            content
        };
        lines.push(format!(
            "- {}: {}",
            message.role,
            truncate(&detail, SUMMARY_LINE_LIMIT)
        ));
        if lines.join("\n").len() >= budget {
            break;
        }
    }
    let mut text = lines.join("\n");
    if text.len() > budget {
        text = truncate(&text, budget);
    }
    text
}

pub fn compact_messages(messages: &[ChatMessage], budget: usize) -> Vec<ChatMessage> {
    if messages.is_empty() || budget == 0 || estimate_messages(messages) <= budget {
        return messages.to_vec();
    }

    let summary_budget = SUMMARY_BUDGET.min(budget / 3).max(256).min(budget);
    let mut suffix_start = messages.len().saturating_sub(1);
    for index in (0..messages.len()).rev() {
        if messages[index].role != "user" {
            continue;
        }
        if estimate_messages(&messages[index..]) + summary_budget <= budget {
            suffix_start = index;
            break;
        }
    }

    let summary = summarize(&messages[..suffix_start], summary_budget);
    let mut compacted = Vec::with_capacity(messages.len() - suffix_start + 1);
    compacted.push(ChatMessage {
        role: "system".into(),
        content: summary,
        ..Default::default()
    });
    compacted.extend(messages[suffix_start..].iter().cloned());
    fit_budget(&mut compacted, budget);
    compacted
}

pub fn estimate_messages(messages: &[ChatMessage]) -> usize {
    messages.iter().map(estimate_message).sum()
}

fn estimate_message(message: &ChatMessage) -> usize {
    let tool_calls = message
        .tool_calls
        .as_ref()
        .map(|calls| {
            calls
                .iter()
                .map(|call| call.id.len() + call.name.len() + call.arguments.to_string().len())
                .sum::<usize>()
        })
        .unwrap_or_default();
    let images = message
        .images
        .iter()
        .map(|image| image.media_type.len() + image.data.len())
        .sum::<usize>();
    message.role.len()
        + message.content.len()
        + message.tool_call_id.as_deref().unwrap_or_default().len()
        + tool_calls
        + images
        + 32
}

fn summarize(messages: &[ChatMessage], budget: usize) -> String {
    let header = "[Earlier context compacted. The full session remains on disk.]".to_string();
    let body_budget = budget.saturating_sub(header.len() + 1).max(budget.min(64));
    let body = transcript(messages, body_budget);
    let mut summary = format!("{header}\n{body}");
    if summary.len() > budget {
        summary = truncate(&summary, budget);
    }
    summary
}

fn fit_budget(messages: &mut Vec<ChatMessage>, budget: usize) {
    let empty_message_cost = estimate_message(&ChatMessage {
        role: "system".into(),
        ..Default::default()
    });
    if budget < empty_message_cost {
        messages.clear();
        return;
    }
    while messages.len() > 2 && estimate_messages(messages) > budget {
        messages.remove(1);
    }
    if let Some(first_user) = messages.iter().position(|message| message.role == "user") {
        while first_user > 1 && estimate_messages(messages) > budget {
            messages.remove(1);
        }
    }
    if estimate_messages(messages) <= budget {
        return;
    }
    let remaining = budget.saturating_sub(estimate_message(&messages[0]));
    for index in 1..messages.len() {
        let allowed = remaining.min(messages[index].content.len());
        messages[index].content = truncate(&messages[index].content, allowed);
        messages[index].tool_calls = None;
        messages[index].images.clear();
        if estimate_messages(messages) <= budget {
            return;
        }
    }
    messages.truncate(1);
    let allowed = budget.saturating_sub(empty_message_cost);
    messages[0].content = truncate(&messages[0].content, allowed);
}

fn normalize(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.into(),
            content: content.into(),
            ..Default::default()
        }
    }

    #[test]
    fn short_history_is_unchanged() {
        let history = vec![message("user", "hello"), message("assistant", "hi")];
        let compacted = compact_messages(&history, 1_000);
        assert_eq!(compacted.len(), history.len());
        assert_eq!(compacted[0].role, history[0].role);
        assert_eq!(compacted[0].content, history[0].content);
        assert_eq!(compacted[1].role, history[1].role);
        assert_eq!(compacted[1].content, history[1].content);
    }

    #[test]
    fn old_history_becomes_a_summary_and_recent_turns_remain() {
        let history = vec![
            message("user", &"old request ".repeat(80)),
            message("assistant", &"old result ".repeat(80)),
            message("user", "keep this request"),
            message("assistant", "keep this answer"),
        ];
        let compacted = compact_messages(&history, 700);
        assert_eq!(compacted[0].role, "system");
        assert!(compacted[0].content.contains("old result"));
        assert!(compacted
            .iter()
            .any(|message| message.content == "keep this request"));
        assert!(estimate_messages(&compacted) <= 700);
    }

    #[test]
    fn compaction_never_exceeds_a_small_budget() {
        let history = vec![
            message("user", &"a ".repeat(2_000)),
            message("assistant", &"b ".repeat(2_000)),
        ];
        let compacted = compact_messages(&history, 256);
        assert!(estimate_messages(&compacted) <= 256);
    }
}
