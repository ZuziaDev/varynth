//! Stdio MCP server for `web_search`. Keys stay on the Varynth API.

use std::io::{BufRead, BufReader, Write};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::config::Config;
use crate::varynth_auth;

pub fn entry(command: &str) -> Value {
    json!({
        "command": command,
        "args": ["mcp", "serve", "web-search"]
    })
}

pub fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut stdout = std::io::stdout();
    loop {
        let message = match read_message(&mut reader) {
            Ok(message) => message,
            Err(error) if error.to_string() == "eof" => return Ok(()),
            Err(error) => return Err(error),
        };
        if let Some(response) = handle(message)? {
            write_message(&mut stdout, &response)?;
        }
    }
}

fn handle(message: Value) -> Result<Option<Value>> {
    let method = message
        .get("method")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let id = message.get("id").cloned();
    if id.is_none() {
        return Ok(None);
    }
    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "varynth-web-search", "version": "0.1.1"}
        }),
        "tools/list" => json!({
            "tools": [{
                "name": "web_search",
                "description": "Search the web through the signed-in Varynth account. Anonymous sessions cannot use this.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "provider": {"type": "string", "enum": ["auto", "parallel", "exa", "firecrawl"]}
                    },
                    "required": ["query"]
                }
            }]
        }),
        "tools/call" => tool_call(&message)?,
        _ => return Ok(Some(error_response(id, -32601, "method not found"))),
    };
    Ok(Some(json!({"jsonrpc": "2.0", "id": id, "result": result})))
}

fn tool_call(message: &Value) -> Result<Value> {
    let name = message
        .pointer("/params/name")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if name != "web_search" {
        return Ok(tool_error("unknown tool"));
    }
    let query = message
        .pointer("/params/arguments/query")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if query.is_empty() {
        return Ok(tool_error("query is required"));
    }
    let provider = message
        .pointer("/params/arguments/provider")
        .and_then(|value| value.as_str())
        .unwrap_or("auto");
    let cfg = Config::load().unwrap_or_default();
    let Some(tokens) = varynth_auth::load_auth_env()? else {
        return Ok(tool_error("sign in with varynth login to use web search"));
    };
    let url = format!("{}/v1/search", cfg.varynth_api_url.trim_end_matches('/'));
    let response = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?
        .post(url)
        .bearer_auth(tokens.access_token)
        .json(&json!({"query": query, "provider": provider}))
        .send()
        .context("web search request failed")?;
    let status = response.status();
    let body = response.text().unwrap_or_default();
    if !status.is_success() {
        return Ok(tool_error(&format!("web search failed ({status})")));
    }
    Ok(json!({
        "content": [{"type": "text", "text": body}],
        "isError": false
    }))
}

fn tool_error(message: &str) -> Value {
    json!({
        "content": [{"type": "text", "text": message}],
        "isError": true
    })
}

fn error_response(id: Option<Value>, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message}
    })
}

fn read_message(reader: &mut impl BufRead) -> Result<Value> {
    let mut length = None;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            bail!("eof");
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            length = Some(value.trim().parse::<usize>().context("content length")?);
        }
    }
    let length = length.context("missing Content-Length")?;
    let mut buffer = vec![0u8; length];
    reader.read_exact(&mut buffer)?;
    Ok(serde_json::from_slice(&buffer)?)
}

fn write_message(stdout: &mut impl Write, message: &Value) -> Result<()> {
    let bytes = serde_json::to_vec(message)?;
    write!(stdout, "Content-Length: {}\r\n\r\n", bytes.len())?;
    stdout.write_all(&bytes)?;
    stdout.flush()?;
    Ok(())
}

pub fn install(path: &std::path::Path) -> Result<()> {
    let command = std::env::current_exe()
        .map_err(|_| anyhow!("could not resolve the varynth binary"))?
        .display()
        .to_string();
    crate::mcp::add_server_entry(path, "web-search", &entry(&command), true)
}
