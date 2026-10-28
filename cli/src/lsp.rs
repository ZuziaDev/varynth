//! Language Server Protocol (LSP) and Diagnostics Integration for Varynth.
//!
//! Provides semantic navigation and compiler diagnostics feedback loop
//! for coding sessions without requiring full manual build cycles.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Diagnostic {
    pub file: PathBuf,
    pub line: u32,
    pub column: u32,
    pub message: String,
    pub severity: DiagnosticSeverity,
    pub source: Option<String>,
}

impl Diagnostic {
    pub fn format_line(&self) -> String {
        let sev = match self.severity {
            DiagnosticSeverity::Error => "ERROR",
            DiagnosticSeverity::Warning => "WARN",
            DiagnosticSeverity::Information => "INFO",
            DiagnosticSeverity::Hint => "HINT",
        };
        format!(
            "[{sev}] {}:{}:{}: {}",
            self.file.display(),
            self.line,
            self.column,
            self.message
        )
    }
}

/// Discovers installed language servers on the host system.
#[derive(Debug, Clone, Default)]
pub struct LspRegistry {
    pub rust_analyzer: bool,
    pub ts_server: bool,
    pub pyright: bool,
    pub gopls: bool,
}

impl LspRegistry {
    pub fn probe() -> Self {
        Self {
            rust_analyzer: Self::has_binary("rust-analyzer"),
            ts_server: Self::has_binary("typescript-language-server") || Self::has_binary("vtsls"),
            pyright: Self::has_binary("pyright-langserver") || Self::has_binary("pyright"),
            gopls: Self::has_binary("gopls"),
        }
    }

    fn has_binary(name: &str) -> bool {
        #[cfg(windows)]
        let cmd = format!("where {name} >nul 2>&1");
        #[cfg(not(windows))]
        let cmd = format!("which {name} >/dev/null 2>&1");

        std::process::Command::new(if cfg!(windows) { "cmd" } else { "sh" })
            .args([if cfg!(windows) { "/C" } else { "-c" }, &cmd])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    pub fn available_servers(&self) -> Vec<&'static str> {
        let mut list = Vec::new();
        if self.rust_analyzer {
            list.push("rust-analyzer");
        }
        if self.ts_server {
            list.push("typescript-language-server");
        }
        if self.pyright {
            list.push("pyright");
        }
        if self.gopls {
            list.push("gopls");
        }
        list
    }
}

/// Summarizes a collection of diagnostics for model feedback injection.
pub fn format_diagnostics_feedback(diagnostics: &[Diagnostic]) -> Option<String> {
    if diagnostics.is_empty() {
        return None;
    }
    let errors: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.severity == DiagnosticSeverity::Error)
        .collect();
    let warnings: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.severity == DiagnosticSeverity::Warning)
        .collect();

    let mut out = format!(
        "LSP Diagnostics: {} error(s), {} warning(s) detected:\n",
        errors.len(),
        warnings.len()
    );
    for d in errors.iter().take(5) {
        out.push_str(&format!("  {}\n", d.format_line()));
    }
    if errors.len() > 5 {
        out.push_str(&format!("  ... and {} more error(s)\n", errors.len() - 5));
    }
    for d in warnings.iter().take(3) {
        out.push_str(&format!("  {}\n", d.format_line()));
    }
    Some(out)
}

/// Runs a fast, non-mutating compiler/linter check on a target file to catch syntax or type errors.
pub fn check_file_diagnostics(file_path: &std::path::Path) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "py" => {
            let cmd = std::process::Command::new("python")
                .args(["-m", "py_compile", file_path.to_str().unwrap_or("")])
                .output();
            if let Ok(out) = cmd {
                if !out.status.success() {
                    let err = String::from_utf8_lossy(&out.stderr);
                    diags.push(Diagnostic {
                        file: file_path.to_path_buf(),
                        line: 1,
                        column: 1,
                        message: err.trim().to_string(),
                        severity: DiagnosticSeverity::Error,
                        source: Some("py_compile".into()),
                    });
                }
            }
        }
        _ => {}
    }
    diags
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_diagnostic_line() {
        let diag = Diagnostic {
            file: PathBuf::from("src/main.rs"),
            line: 42,
            column: 10,
            message: "cannot find value `x` in this scope".into(),
            severity: DiagnosticSeverity::Error,
            source: Some("rustc".into()),
        };
        assert_eq!(
            diag.format_line(),
            "[ERROR] src/main.rs:42:10: cannot find value `x` in this scope"
        );
    }

    #[test]
    fn format_feedback_summary() {
        let diags = vec![
            Diagnostic {
                file: PathBuf::from("src/lib.rs"),
                line: 10,
                column: 5,
                message: "type mismatch".into(),
                severity: DiagnosticSeverity::Error,
                source: None,
            },
            Diagnostic {
                file: PathBuf::from("src/lib.rs"),
                line: 25,
                column: 1,
                message: "unused variable".into(),
                severity: DiagnosticSeverity::Warning,
                source: None,
            },
        ];
        let summary = format_diagnostics_feedback(&diags).unwrap();
        assert!(summary.contains("1 error(s), 1 warning(s)"));
        assert!(summary.contains("[ERROR] src/lib.rs:10:5: type mismatch"));
        assert!(summary.contains("[WARN] src/lib.rs:25:1: unused variable"));
    }
}
