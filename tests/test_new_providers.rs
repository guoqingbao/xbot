//! Provider registry entries for GitHub Copilot and Cursor.

use rbot::config::ProviderConfig;
use rbot::providers::registry::find_by_name;
use rbot::runtime::build_provider_client;

#[test]
fn github_copilot_is_registered_by_name() {
    let spec = find_by_name("github_copilot").expect("github_copilot should be registered");
    assert_eq!(spec.name, "github_copilot");
}

#[test]
fn cursor_is_registered_by_name() {
    let spec = find_by_name("cursor").expect("cursor should be registered");
    assert_eq!(spec.name, "cursor");
}

#[test]
fn cursor_build_requires_api_base() {
    let cfg = ProviderConfig {
        api_key: "test-key".to_string(),
        api_base: None,
        extra_headers: Default::default(),
        reasoning_effort: None,
    };
    let err = match build_provider_client("cursor", &cfg, "cursor-model", None, None, None) {
        Err(e) => e,
        Ok(_) => panic!("expected cursor without apiBase to fail"),
    };
    let msg = err.to_string();
    assert!(
        msg.contains("apiBase") || msg.contains("api base"),
        "unexpected error (expected apiBase requirement): {msg}"
    );
}

#[test]
fn github_copilot_is_marked_oauth() {
    let spec = find_by_name("github_copilot").unwrap();
    assert!(
        spec.is_oauth,
        "github_copilot should be flagged as OAuth-capable in the registry"
    );
}
