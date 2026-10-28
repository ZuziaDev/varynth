use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use varynth::config::Config;
use varynth::providers::build;
use varynth::session::{ChatMessage, ImageAttachment, ToolCall};

#[derive(Clone, Debug)]
struct RequestRecord {
    target: String,
    headers: String,
    body: Value,
}

async fn serve_sequence(
    responses: Vec<(&'static str, &'static str)>,
) -> (
    String,
    Arc<Mutex<Vec<RequestRecord>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let records = Arc::new(Mutex::new(Vec::new()));
    let captured = records.clone();
    let task = tokio::spawn(async move {
        for (content_type, body) in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            let (target, headers, body_value) = parse_request(&request);
            captured.lock().await.push(RequestRecord {
                target,
                headers,
                body: body_value,
            });
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    (format!("http://{}", addr), records, task)
}

async fn serve_error(body: &'static str) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let _ = read_request(&mut socket).await;
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    (format!("http://{}", addr), task)
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end;
    loop {
        let n = socket.read(&mut chunk).await.unwrap();
        assert!(n > 0, "loopback client closed before sending request");
        bytes.extend_from_slice(&chunk[..n]);
        if let Some(pos) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
    }
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.strip_prefix("Content-Length:")
                .or_else(|| line.strip_prefix("content-length:"))
        })
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while bytes.len() < header_end + content_length {
        let n = socket.read(&mut chunk).await.unwrap();
        assert!(n > 0, "loopback client closed before sending request body");
        bytes.extend_from_slice(&chunk[..n]);
    }
    bytes
}

fn parse_request(request: &[u8]) -> (String, String, Value) {
    let pos = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let headers = String::from_utf8_lossy(&request[..pos]).into_owned();
    let target = headers
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_string();
    // GET requests carry no body; represent that as JSON null.
    let body = if request[pos + 4..].is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&request[pos + 4..]).unwrap()
    };
    (target, headers, body)
}

fn google_config(base: &str) -> Config {
    let mut cfg = Config::default();
    cfg.provider = "google".into();
    cfg.google_api_key = Some("loopback-test-key".into());
    cfg.google_base_url = Some(base.into());
    cfg.google_auth = "api-key".into();
    cfg
}

#[tokio::test]
async fn gemini_request_uses_v1beta_path_header_and_official_shapes() {
    let response = r#"{"candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}],"modelVersion":"gemini-test","usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":2}}"#;
    let (base, records, task) = serve_sequence(vec![("application/json", response)]).await;
    let provider = build(&google_config(&(base + "/v1beta/"))).unwrap();
    let messages = vec![ChatMessage {
        role: "user".into(),
        content: "describe this".into(),
        images: vec![ImageAttachment {
            media_type: "image/png".into(),
            data: "QUJD".into(),
        }],
        ..Default::default()
    }];
    let tools = vec![json!({
        "type":"function",
        "function": {
            "name":"read_file",
            "description":"Read a file",
            "parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}
        }
    })];
    let completion = provider
        .complete("gemini-test", "system rule", &messages, &tools)
        .await
        .unwrap();
    assert_eq!(completion.text, "ok");
    assert_eq!(completion.model, "gemini-test");
    assert_eq!(completion.usage.unwrap().input_tokens, 7);

    task.await.unwrap();
    let records = records.lock().await;
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].target,
        "/v1beta/models/gemini-test:generateContent"
    );
    assert!(records[0].target.contains("generateContent"));
    assert!(!records[0].target.contains("key"));
    assert!(records[0]
        .headers
        .contains("x-goog-api-key: loopback-test-key"));
    assert!(!records[0].headers.contains("Authorization"));
    assert_eq!(
        records[0].body["systemInstruction"]["parts"][0]["text"],
        "system rule"
    );
    assert_eq!(records[0].body["contents"][0]["role"], "user");
    assert_eq!(
        records[0].body["contents"][0]["parts"][0]["text"],
        "describe this"
    );
    assert_eq!(
        records[0].body["contents"][0]["parts"][1]["inlineData"],
        json!({"mimeType":"image/png","data":"QUJD"})
    );
    assert_eq!(
        records[0].body["tools"][0]["functionDeclarations"][0]["parameters"]["required"],
        json!(["path"])
    );
}

#[tokio::test]
async fn function_call_id_round_trips_signature_without_changing_arguments() {
    let first = r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"id":"server-call","name":"read_file","args":{"path":"src/lib.rs"}},"thoughtSignature":"opaque-signature"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":3}}"#;
    let second = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"done"}]},"finishReason":"STOP"}]}"#;
    let (base, records, task) = serve_sequence(vec![
        ("application/json", first),
        ("application/json", second),
    ])
    .await;
    let provider = build(&google_config(&base)).unwrap();
    let initial = ChatMessage {
        role: "user".into(),
        content: "read it".into(),
        ..Default::default()
    };
    let call_completion = provider
        .complete("gemini-test", "", &[initial.clone()], &[])
        .await
        .unwrap();
    assert_eq!(call_completion.tool_calls.len(), 1);
    let call = &call_completion.tool_calls[0];
    assert!(call.id.starts_with("google:"));
    assert_eq!(call.arguments, json!({"path":"src/lib.rs"}));
    let tool_result = ChatMessage {
        role: "tool".into(),
        content: r#"{"contents":["file text"]}"#.into(),
        tool_call_id: Some(call.id.clone()),
        ..Default::default()
    };
    let assistant = ChatMessage {
        role: "assistant".into(),
        content: String::new(),
        tool_calls: Some(vec![ToolCall {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
        }]),
        ..Default::default()
    };
    let completion = provider
        .complete("gemini-test", "", &[initial, assistant, tool_result], &[])
        .await
        .unwrap();
    assert_eq!(completion.text, "done");
    task.await.unwrap();
    let records = records.lock().await;
    assert_eq!(records.len(), 2);
    let second_body = &records[1].body;
    let function_call = &second_body["contents"][1]["parts"][0]["functionCall"];
    assert_eq!(function_call["id"], "server-call");
    assert_eq!(function_call["args"], json!({"path":"src/lib.rs"}));
    assert_eq!(
        second_body["contents"][2]["parts"][0]["functionResponse"]["id"],
        "server-call"
    );
    assert_eq!(
        second_body["contents"][2]["parts"][0]["functionResponse"]["response"],
        json!({"contents":["file text"]})
    );
    assert_eq!(
        second_body["contents"][1]["parts"][0]["thoughtSignature"],
        "opaque-signature"
    );
}

#[tokio::test]
async fn streaming_uses_alt_sse_and_emits_deltas() {
    let stream = concat!(
        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"hel\"}]}}]}\n\n",
        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"lo\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":2,\"candidatesTokenCount\":2}}\n\n"
    );
    let (base, records, task) = serve_sequence(vec![("text/event-stream", stream)]).await;
    let provider = build(&google_config(&base)).unwrap();
    let mut deltas = Vec::new();
    let completion = provider
        .complete_streaming(
            "gemini-test",
            "",
            &[ChatMessage {
                role: "user".into(),
                content: "hi".into(),
                ..Default::default()
            }],
            &[],
            &mut |delta| deltas.push(delta.to_string()),
        )
        .await
        .unwrap();
    assert_eq!(deltas, vec!["hel", "lo"]);
    assert_eq!(completion.text, "hello");
    task.await.unwrap();
    let records = records.lock().await;
    assert_eq!(
        records[0].target,
        "/v1beta/models/gemini-test:streamGenerateContent?alt=sse"
    );
    assert!(records[0]
        .headers
        .to_ascii_lowercase()
        .contains("accept: text/event-stream"));
}

#[tokio::test]
async fn catalog_follows_pages_and_filters_generate_content_models() {
    let page_one = r#"{"models":[{"name":"models/gemini-supported","displayName":"Supported","supportedGenerationMethods":["generateContent"]},{"name":"models/embedding-only","supportedGenerationMethods":["embedContent"]}],"nextPageToken":"page-2"}"#;
    let page_two = r#"{"models":[{"name":"models/gemini-second","supportedGenerationMethods":["generateContent","countTokens"]}]}"#;
    let (base, records, task) = serve_sequence(vec![
        ("application/json", page_one),
        ("application/json", page_two),
    ])
    .await;
    let provider = build(&google_config(&(base + "/v1beta"))).unwrap();
    let models = provider.list_models().await.unwrap();
    assert_eq!(
        models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        vec!["gemini-supported", "gemini-second"]
    );
    assert_eq!(models[0].display_name.as_deref(), Some("Supported"));
    task.await.unwrap();
    let records = records.lock().await;
    assert_eq!(records[0].target, "/v1beta/models?pageSize=1000");
    assert_eq!(
        records[1].target,
        "/v1beta/models?pageSize=1000&pageToken=page-2"
    );
    assert!(records[0]
        .headers
        .contains("x-goog-api-key: loopback-test-key"));
}

#[tokio::test]
async fn vertex_catalog_is_empty_and_does_not_invent_availability() {
    let mut cfg = Config::default();
    cfg.provider = "google-vertex".into();
    cfg.google_auth = "oauth".into();
    cfg.google_project = Some("project-test".into());
    cfg.google_location = Some("us-central1".into());
    cfg.google_base_url = Some("https://aiplatform.googleapis.com/v1".into());
    let provider = build(&cfg).unwrap();
    assert!(provider.list_models().await.unwrap().is_empty());
}

#[tokio::test]
async fn http_errors_are_status_only_and_never_echo_body_or_key() {
    let (base, task) =
        serve_error(r#"{"error":"prompt=private-input key=loopback-test-key token=secret"}"#).await;
    let provider = build(&google_config(&base)).unwrap();
    let error = provider
        .complete(
            "gemini-test",
            "private-input",
            &[ChatMessage {
                role: "user".into(),
                content: "private-input".into(),
                ..Default::default()
            }],
            &[],
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("Google HTTP 400"));
    assert!(!error.contains("private-input"));
    assert!(!error.contains("loopback-test-key"));
    assert!(!error.contains("secret"));
    task.await.unwrap();
}
