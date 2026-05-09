use serde_json::json;
use xbot::tools::{ExecTool, Tool, ToolOutput, WebFetchTool};

#[tokio::test]
async fn exec_blocks_metadata_url() {
    let tool = ExecTool::new(5, None, false, String::new());
    let result = tool
        .execute(json!({"command": r#"curl -s -H "Metadata-Flavor: Google" http://169.254.169.254/computeMetadata/v1/"#}))
        .await;
    match result {
        ToolOutput::Text(text) => {
            assert!(text.contains("Error"));
            assert!(
                text.to_lowercase().contains("internal") || text.to_lowercase().contains("private")
            );
        }
        other => panic!("unexpected output: {other:?}"),
    }
}

#[tokio::test]
async fn exec_blocks_localhost_url() {
    let tool = ExecTool::new(5, None, false, String::new());
    let result = tool
        .execute(json!({"command": "wget http://localhost:8080/secret -O /tmp/out"}))
        .await;
    match result {
        ToolOutput::Text(text) => assert!(text.contains("Error")),
        other => panic!("unexpected output: {other:?}"),
    }
}

#[tokio::test]
async fn exec_allows_normal_commands() {
    let tool = ExecTool::new(5, None, false, String::new());
    let result = tool.execute(json!({"command": "echo hello"})).await;
    match result {
        ToolOutput::Text(text) => {
            assert!(text.contains("hello"));
            assert!(!text.lines().next().unwrap_or_default().contains("Error"));
        }
        other => panic!("unexpected output: {other:?}"),
    }
}

#[tokio::test]
async fn web_fetch_blocks_private_ip() {
    let tool = WebFetchTool::new(50_000, None);
    let result = tool
        .execute(json!({"url": "http://169.254.169.254/computeMetadata/v1/"}))
        .await;
    match result {
        ToolOutput::Text(text) => {
            let data: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert!(data.get("error").is_some());
        }
        other => panic!("unexpected output: {other:?}"),
    }
}

#[tokio::test]
async fn web_fetch_blocks_localhost() {
    let tool = WebFetchTool::new(50_000, None);
    let result = tool.execute(json!({"url": "http://localhost/admin"})).await;
    match result {
        ToolOutput::Text(text) => {
            let data: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert!(data.get("error").is_some());
        }
        other => panic!("unexpected output: {other:?}"),
    }
}

// Success JSON shape (`untrusted`, external-content banner) is covered by unit tests in
// `src/tools.rs` (`web_fetch_text_payload_includes_untrusted_and_banner`). A live fetch to
// example.com would require outbound network and fails in offline/CI sandboxes.
