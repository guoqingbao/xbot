use serde_json::json;
use xbot::storage::{ChatMessage, Session};

/// Test that incomplete tool calls are repaired (not trimmed) and subsequent history is preserved
#[test]
fn incomplete_tool_calls_repaired_history_preserved() {
    let mut session = Session::new("test:incomplete");
    
    // Add a valid user message to start the conversation
    session.messages.push(ChatMessage::text("user", "Hello, let's start"));
    
    // Add complete tool call sequences
    session.messages.push(ChatMessage {
        role: "assistant".to_string(),
        content: None,
        tool_calls: Some(vec![json!({"id": "call_complete_1", "type": "function", "function": {"name": "search", "arguments": "{}"}})]),
        tool_call_id: None,
        name: Some("search".to_string()),
        timestamp: None,
        reasoning_content: None,
        thinking_blocks: None,
        metadata: None,
    });
    session.messages.push(ChatMessage {
        role: "tool".to_string(),
        content: Some(json!("search results")),
        tool_calls: None,
        tool_call_id: Some("call_complete_1".to_string()),
        name: Some("search".to_string()),
        timestamp: None,
        reasoning_content: None,
        thinking_blocks: None,
        metadata: None,
    });
    
    session.messages.push(ChatMessage {
        role: "assistant".to_string(),
        content: None,
        tool_calls: Some(vec![json!({"id": "call_complete_2", "type": "function", "function": {"name": "read_file", "arguments": "{}"}})]),
        tool_call_id: None,
        name: Some("read_file".to_string()),
        timestamp: None,
        reasoning_content: None,
        thinking_blocks: None,
        metadata: None,
    });
    session.messages.push(ChatMessage {
        role: "tool".to_string(),
        content: Some(json!("file contents")),
        tool_calls: None,
        tool_call_id: Some("call_complete_2".to_string()),
        name: Some("read_file".to_string()),
        timestamp: None,
        reasoning_content: None,
        thinking_blocks: None,
        metadata: None,
    });
    
    // Add an INCOMPLETE tool call (no response follows)
    session.messages.push(ChatMessage {
        role: "assistant".to_string(),
        content: None,
        tool_calls: Some(vec![json!({"id": "call_incomplete_1", "type": "function", "function": {"name": "execute", "arguments": "{}"}})]),
        tool_call_id: None,
        name: None,
        timestamp: None,
        reasoning_content: None,
        thinking_blocks: None,
        metadata: None,
    });
    
    // Add more messages after the incomplete call - these should be PRESERVED
    session.messages.push(ChatMessage {
        role: "assistant".to_string(),
        content: Some(json!("I encountered an error while executing")),
        tool_calls: None,
        tool_call_id: None,
        name: None,
        timestamp: None,
        reasoning_content: None,
        thinking_blocks: None,
        metadata: None,
    });
    session.messages.push(ChatMessage::text("user", "Can you continue?"));
    session.messages.push(ChatMessage::text("assistant", "Yes, let me try again"));
    
    // Get history - should repair incomplete tool call but keep ALL subsequent messages
    let history = session.get_history(0);
    
    // Verify incomplete tool call was repaired (tool_calls removed)
    let incomplete_repaired = history
        .iter()
        .filter(|m| m.role == "assistant")
        .any(|m| {
            m.content_as_text()
                .map_or(false, |c| c.contains("encountered an error"))
                && m.tool_calls.is_none()
        });
    
    assert!(incomplete_repaired, "Incomplete tool call should be repaired, not trimmed");
    
    // Verify subsequent messages are preserved
    let continuation_found = history
        .iter()
        .any(|m| m.content_as_text() == Some("Can you continue?".to_string()));
    
    assert!(continuation_found, "Messages after incomplete call should be preserved");
    
    // Verify no incomplete tool calls remain in history
    let found_incomplete = history
        .iter()
        .filter(|m| m.role == "assistant")
        .flat_map(|m| m.tool_calls.clone().unwrap_or_default())
        .filter_map(|t| t.get("id").and_then(|id| id.as_str()).map(String::from))
        .any(|id| id == "call_incomplete_1");
    
    assert!(!found_incomplete, "No incomplete tool calls should remain in history");
    
    // All tool calls in history should have responses
    let declared_ids: Vec<String> = history
        .iter()
        .filter(|m| m.role == "assistant")
        .flat_map(|m| m.tool_calls.clone().unwrap_or_default())
        .filter_map(|t| t.get("id").and_then(|id| id.as_str()).map(String::from))
        .collect();
    
    let response_ids: Vec<String> = history
        .iter()
        .filter(|m| m.role == "tool")
        .filter_map(|m| m.tool_call_id.clone())
        .collect();
    
    for id in &declared_ids {
        assert!(response_ids.contains(id), "Tool call {} has no response in history", id);
    }
}

/// Test that multiple incomplete tool calls are all repaired and history preserved
#[test]
fn multiple_incomplete_tool_calls_all_repaired() {
    let mut session = Session::new("test:multiple-incomplete");
    
    session.messages.push(ChatMessage::text("user", "Start"));
    
    // Complete sequence
    session.messages.push(ChatMessage {
        role: "assistant".to_string(),
        content: None,
        tool_calls: Some(vec![json!({"id": "complete_1", "type": "function", "function": {"name": "a", "arguments": "{}"}})]),
        tool_call_id: None,
        name: None,
        timestamp: None,
        reasoning_content: None,
        thinking_blocks: None,
        metadata: None,
    });
    session.messages.push(ChatMessage {
        role: "tool".to_string(),
        content: Some(json!("ok")),
        tool_calls: None,
        tool_call_id: Some("complete_1".to_string()),
        name: Some("a".to_string()),
        timestamp: None,
        reasoning_content: None,
        thinking_blocks: None,
        metadata: None,
    });
    
    // First incomplete
    session.messages.push(ChatMessage {
        role: "assistant".to_string(),
        content: None,
        tool_calls: Some(vec![json!({"id": "incomplete_1", "type": "function", "function": {"name": "b", "arguments": "{}"}})]),
        tool_call_id: None,
        name: None,
        timestamp: None,
        reasoning_content: None,
        thinking_blocks: None,
        metadata: None,
    });
    
    // Conversation in between
    session.messages.push(ChatMessage::text("assistant", "Something went wrong"));
    session.messages.push(ChatMessage::text("user", "What happened?"));
    
    // Second incomplete
    session.messages.push(ChatMessage {
        role: "assistant".to_string(),
        content: None,
        tool_calls: Some(vec![json!({"id": "incomplete_2", "type": "function", "function": {"name": "c", "arguments": "{}"}})]),
        tool_call_id: None,
        name: None,
        timestamp: None,
        reasoning_content: None,
        thinking_blocks: None,
        metadata: None,
    });
    
    // More conversation after
    session.messages.push(ChatMessage::text("user", "Let's continue"));
    session.messages.push(ChatMessage::text("assistant", "OK, I'll try"));

    let history = session.get_history(0);
    
    // Neither incomplete call should have tool_calls
    let incomplete_in_history = history
        .iter()
        .filter(|m| m.role == "assistant")
        .flat_map(|m| m.tool_calls.clone().unwrap_or_default())
        .filter_map(|t| t.get("id").and_then(|id| id.as_str()).map(String::from))
        .any(|id| id == "incomplete_1" || id == "incomplete_2");
    
    assert!(!incomplete_in_history, "No incomplete tool calls should remain in history");
    
    // Complete tool call should be preserved
    let complete_in_history = history
        .iter()
        .filter(|m| m.role == "assistant")
        .flat_map(|m| m.tool_calls.clone().unwrap_or_default())
        .filter_map(|t| t.get("id").and_then(|id| id.as_str()).map(String::from))
        .any(|id| id == "complete_1");
    
    assert!(complete_in_history, "Complete tool call should be preserved in history");
    
    // All subsequent messages should be present
    assert!(history.iter().any(|m| m.content_as_text() == Some("OK, I'll try".to_string())));
}

/// Test that the repair strategy prevents 422 errors while preserving history
#[test]
fn repair_prevents_422_while_preserving_history() {
    let mut session = Session::new("test:422-prevention");
    
    session.messages.push(ChatMessage::text("user", "Question"));
    
    // Assistant makes a tool call
    session.messages.push(ChatMessage {
        role: "assistant".to_string(),
        content: None,
        tool_calls: Some(vec![json!({"id": "call_c3ac8759b87840db", "type": "function", "function": {"name": "test_tool", "arguments": "{}"}})]),
        tool_call_id: None,
        name: None,
        timestamp: None,
        reasoning_content: None,
        thinking_blocks: None,
        metadata: None,
    });
    
    // But NO tool response follows - this is the broken state that causes 422
    session.messages.push(ChatMessage {
        role: "assistant".to_string(),
        content: Some(json!("I forgot to wait for the response")),
        tool_calls: None,
        tool_call_id: None,
        name: None,
        timestamp: None,
        reasoning_content: None,
        thinking_blocks: None,
        metadata: None,
    });
    
    session.messages.push(ChatMessage::text("user", "Next question"));
    session.messages.push(ChatMessage::text("assistant", "Let me help with that"));

    let history = session.get_history(0);
    
    // The incomplete call should be repaired (tool_calls removed)
    let has_incomplete_call = history
        .iter()
        .filter(|m| m.role == "assistant")
        .flat_map(|m| m.tool_calls.clone().unwrap_or_default())
        .filter_map(|t| t.get("id").and_then(|id| id.as_str()).map(String::from))
        .any(|id| id == "call_c3ac8759b87840db");
    
    assert!(
        !has_incomplete_call,
        "The incomplete tool call should be repaired to prevent 422 error"
    );
    
    // But subsequent messages should be preserved
    assert!(history.iter().any(|m| m.content_as_text() == Some("Next question".to_string())));
    assert!(history.iter().any(|m| m.content_as_text() == Some("Let me help with that".to_string())));
    
    // History should NOT be truncated - all messages after first user should be present
    assert_eq!(history.len(), 5, "All messages should be preserved after repair");
}