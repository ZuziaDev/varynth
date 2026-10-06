//! LLM session titling: after the first completed turn, a cheap background
//! model call summarizes the opening exchange into a 2-4 word title
//! (e.g. "OAuth Login Fix"). The runtime wires this in; the TUI shows the
//! refreshed title on its next snapshot.

use crate::providers::Provider;
use crate::session::ChatMessage;
use anyhow::Result;

/// Generate a short session title from the opening messages. Never fails the
/// caller's flow: on any provider or parse problem the error explains why and
/// the session simply keeps its fallback title.
pub async fn generate_title(
    model: &str,
    provider: &dyn Provider,
    messages: &[ChatMessage],
) -> Result<String> {
    let transcript: String = messages
        .iter()
        .take(6)
        .map(|m| {
            let body: String = m.content.chars().take(400).collect();
            format!("{}: {body}", m.role)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let ask = "Write a session title for this coding conversation. Reply with ONLY the \
               title: 2-4 words, no quotes, no trailing punctuation. It should name the \
               task, not describe the conversation.\n\nTranscript:\n";
    let completion = provider
        .complete(
            model,
            "You write concise session titles. Reply with the title only.",
            &[ChatMessage {
                role: "user".into(),
                content: format!("{ask}{transcript}"),
                ..Default::default()
            }],
            &[],
        )
        .await?;
    let title = sanitize(&completion.text);
    anyhow::ensure!(!title.is_empty(), "model returned an empty title");
    Ok(title)
}

/// One line, ≤ 48 chars, no quotes or trailing punctuation.
pub fn sanitize(raw: &str) -> String {
    let t = raw.trim();
    let t = t.trim_matches(['"', '\'', '`']).trim();
    let t = t.lines().next().unwrap_or("").trim();
    let t = t.trim_end_matches(['.', '!', '?', ':']).trim();
    let t = t.trim_matches(['"', '\'', '`']).trim();
    let t: String = t.chars().take(48).collect();
    t.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{CatalogModel, Completion, Usage};

    struct MockProvider {
        reply: String,
    }

    #[async_trait::async_trait]
    impl Provider for MockProvider {
        async fn complete(
            &self,
            _model: &str,
            _system: &str,
            _messages: &[ChatMessage],
            _tools: &[serde_json::Value],
        ) -> Result<Completion> {
            Ok(Completion {
                text: self.reply.clone(),
                tool_calls: Vec::new(),
                model: "mock".into(),
                usage: Some(Usage {
                    input_tokens: 10,
                    output_tokens: 4,
                }),
            })
        }

        async fn list_models(&self) -> Result<Vec<CatalogModel>> {
            Ok(Vec::new())
        }
    }

    fn msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.into(),
            content: content.into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn titles_from_the_first_exchange() {
        let p = MockProvider {
            reply: "\"OAuth Login Fix\"".into(),
        };
        let title = generate_title(
            "mock",
            &p,
            &[msg("user", "fix the oauth login redirect loop")],
        )
        .await
        .unwrap();
        assert_eq!(title, "OAuth Login Fix");
    }

    #[tokio::test]
    async fn empty_reply_is_an_error_not_a_blank_title() {
        let p = MockProvider {
            reply: String::new(),
        };
        assert!(generate_title("mock", &p, &[msg("user", "hi")])
            .await
            .is_err());
    }

    #[test]
    fn sanitize_takes_one_line_and_strips_decorations() {
        assert_eq!(sanitize("  \"Fix Login Loop\".  "), "Fix Login Loop");
        assert_eq!(sanitize("first line\nsecond line"), "first line");
        let long = sanitize(&"x".repeat(80));
        assert_eq!(long.chars().count(), 48);
        assert_eq!(sanitize("Fix:"), "Fix");
    }
}
