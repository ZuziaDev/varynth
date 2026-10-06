//! Varynth ships as its own product: user-facing text (system prompt, CLI
//! help, channel notes, README) must not name other agents.

const SOURCES: [(&str, &str); 4] = [
    ("src/runtime.rs", include_str!("../src/runtime.rs")),
    ("src/main.rs", include_str!("../src/main.rs")),
    ("src/channels.rs", include_str!("../src/channels.rs")),
    ("README.md", include_str!("../README.md")),
];

#[test]
fn user_facing_text_names_no_other_agents() {
    for (path, text) in SOURCES {
        let lower = text.to_lowercase();
        for brand in [
            "openclaw",
            "claude code",
            "claude -p",
            "codex-style",
            "codex exec",
        ] {
            assert!(!lower.contains(brand), "{path} mentions `{brand}`");
        }
    }
}
