use serde_json::json;
use xbot::storage::{ChatMessage, Session};

fn tool_turn(prefix: &str, idx: usize) -> Vec<ChatMessage> {
    vec![
        ChatMessage {
            role: "assistant".to_string(),
            content: None,
            tool_calls: Some(vec![
                json!({"id": format!("{prefix}_{idx}_a"), "type": "function", "function": {"name": "x", "arguments": "{}"}}),
                json!({"id": format!("{prefix}_{idx}_b"), "type": "function", "function": {"name": "y", "arguments": "{}"}}),
            ]),
            tool_call_id: None,
            name: None,
            timestamp: None,
            reasoning_content: None,
            thinking_blocks: None,
            metadata: None,
        },
        ChatMessage {
            role: "tool".to_string(),
            content: Some(json!("ok")),
            tool_calls: None,
            tool_call_id: Some(format!("{prefix}_{idx}_a")),
            name: Some("x".to_string()),
            timestamp: None,
            reasoning_content: None,
            thinking_blocks: None,
            metadata: None,
        },
        ChatMessage {
            role: "tool".to_string(),
            content: Some(json!("ok")),
            tool_calls: None,
            tool_call_id: Some(format!("{prefix}_{idx}_b")),
            name: Some("y".to_string()),
            timestamp: None,
            reasoning_content: None,
            thinking_blocks: None,
            metadata: None,
        },
    ]
}

fn assert_no_orphans(history: &[ChatMessage]) {
    let declared = history
        .iter()
        .filter(|message| message.role == "assistant")
        .flat_map(|message| message.tool_calls.clone().unwrap_or_default())
        .filter_map(|tool_call| {
            tool_call
                .get("id")
                .and_then(|id| id.as_str())
                .map(ToOwned::to_owned)
        })
        .collect::<Vec<_>>();
    let orphans = history
        .iter()
        .filter(|message| message.role == "tool")
        .filter_map(|message| message.tool_call_id.clone())
        .filter(|tool_id| !declared.contains(tool_id))
        .collect::<Vec<_>>();
    assert!(orphans.is_empty(), "orphan tool call ids: {orphans:?}");
}

#[test]
fn get_history_drops_orphan_tool_results_when_window_cuts_tool_calls() {
    let mut session = Session::new("telegram:test");
    session.messages.push(ChatMessage::text("user", "old turn"));
    for index in 0..20 {
        session.messages.extend(tool_turn("old", index));
    }
    session
        .messages
        .push(ChatMessage::text("user", "problem turn"));
    for index in 0..25 {
        session.messages.extend(tool_turn("cur", index));
    }
    session
        .messages
        .push(ChatMessage::text("user", "new telegram question"));

    let history = session.get_history(100);
    assert_no_orphans(&history);
}

#[test]
fn legitimate_tool_pairs_preserved_after_trim() {
    let mut session = Session::new("test:positive");
    session.messages.push(ChatMessage::text("user", "hello"));
    for index in 0..5 {
        session.messages.extend(tool_turn("ok", index));
    }
    session
        .messages
        .push(ChatMessage::text("assistant", "done"));

    let history = session.get_history(500);
    assert_no_orphans(&history);
    let tool_ids = history
        .iter()
        .filter(|message| message.role == "tool")
        .filter_map(|message| message.tool_call_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(tool_ids.len(), 10);
    assert_eq!(history.first().unwrap().role, "user");
}

#[test]
fn orphan_trim_with_last_consolidated() {
    let mut session = Session::new("test:consolidated");
    for index in 0..10 {
        session
            .messages
            .push(ChatMessage::text("user", format!("old {index}")));
        session.messages.extend(tool_turn("cons", index));
    }
    session.last_consolidated = 30;
    session.messages.push(ChatMessage::text("user", "recent"));
    for index in 0..15 {
        session.messages.extend(tool_turn("new", index));
    }
    session.messages.push(ChatMessage::text("user", "latest"));

    let history = session.get_history(20);
    assert_no_orphans(&history);
    assert!(
        history
            .iter()
            .filter(|message| message.role == "tool")
            .all(|message| message
                .tool_call_id
                .as_deref()
                .unwrap_or_default()
                .starts_with("new_"))
    );
}

#[test]
fn all_orphan_prefix_stripped() {
    let mut session = Session::new("test:all-orphan");
    session.messages.push(ChatMessage {
        role: "tool".to_string(),
        content: Some(json!("ok")),
        tool_calls: None,
        tool_call_id: Some("gone_1".to_string()),
        name: Some("x".to_string()),
        timestamp: None,
        reasoning_content: None,
        thinking_blocks: None,
        metadata: None,
    });
    session.messages.push(ChatMessage {
        role: "tool".to_string(),
        content: Some(json!("ok")),
        tool_calls: None,
        tool_call_id: Some("gone_2".to_string()),
        name: Some("y".to_string()),
        timestamp: None,
        reasoning_content: None,
        thinking_blocks: None,
        metadata: None,
    });
    session
        .messages
        .push(ChatMessage::text("user", "fresh start"));
    session.messages.push(ChatMessage::text("assistant", "hi"));

    let history = session.get_history(500);
    assert_no_orphans(&history);
    assert_eq!(history.first().unwrap().role, "user");
    assert_eq!(history.len(), 2);
}

#[test]
fn get_history_preserves_assistant_reasoning_for_prefix_cache() {
    let mut session = Session::new("test:reasoning");
    session.messages.push(ChatMessage::text("user", "question"));
    session.messages.push(ChatMessage {
        role: "assistant".to_string(),
        content: Some(json!("answer")),
        tool_calls: None,
        tool_call_id: None,
        name: None,
        timestamp: Some("stored timestamp".to_string()),
        reasoning_content: Some("stored reasoning".to_string()),
        thinking_blocks: Some(vec![json!({
            "type": "thinking",
            "thinking": "stored thinking",
            "signature": "sig",
        })]),
        metadata: None,
    });

    let history = session.get_history(0);

    assert_eq!(history.len(), 2);
    assert_eq!(
        history[1].reasoning_content.as_deref(),
        Some("stored reasoning")
    );
    assert_eq!(
        history[1].thinking_blocks.as_ref().unwrap()[0]
            .get("signature")
            .and_then(|value| value.as_str()),
        Some("sig")
    );
    assert!(history[1].timestamp.is_none());
}
