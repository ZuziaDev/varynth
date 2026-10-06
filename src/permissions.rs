use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    Prompt,
    AcceptEdits,
    Bypass,
}

impl PermissionMode {
    pub fn parse(s: &str) -> Self {
        match s {
            "prompt" | "manual" => Self::Prompt,
            "bypassPermissions" | "bypass" | "dangerously-skip-permissions" => Self::Bypass,
            _ => Self::AcceptEdits,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::AcceptEdits => "acceptEdits",
            Self::Bypass => "bypass",
        }
    }

    /// True when every gated tool call needs an interactive approval.
    pub fn is_prompt(self) -> bool {
        matches!(self, Self::Prompt)
    }

    pub fn auto_approve_write(self) -> bool {
        matches!(self, Self::AcceptEdits | Self::Bypass)
    }

    pub fn auto_approve_shell(self) -> bool {
        matches!(self, Self::Bypass | Self::AcceptEdits)
    }
}
