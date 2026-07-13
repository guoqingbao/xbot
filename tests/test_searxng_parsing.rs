//! Tests for SearXNG JSON response parsing in web_search tool

use serde_json::json;

/// Test parsing a typical SearXNG JSON response with multiple results
#[test]
fn test_searxng_json_parsing_basic() {
    let response = json!({
        "query": "xinfer",
        "number_of_results": 18,
        "results": [
            {
                "title": "XInfer.AI — Your AI Customer Service Agent",
                "url": "https://xinfer.ai/",
                "content": "Your AI Customer Service Agent - Live in minutes.",
                "engine": "duckduckgo"
            },
            {
                "title": "GitHub - xorbitsai/inference",
                "url": "https://github.com/xorbitsai/inference",
                "content": "Swap GPT for any LLM by changing a single line of code.",
                "engine": "duckduckgo"
            }
        ],
        "answers": [],
        "suggestions": []
    });

    let results = response.get("results")
        .and_then(|r| r.as_array())
        .map_or(Vec::new(), |v| v.clone());

    assert_eq!(results.len(), 2);

    let first = &results[0];
    assert_eq!(
        first.get("title").and_then(|t| t.as_str()).unwrap_or(""),
        "XInfer.AI — Your AI Customer Service Agent"
    );
    assert_eq!(
        first.get("url").and_then(|u| u.as_str()).unwrap_or(""),
        "https://xinfer.ai/"
    );
    assert_eq!(
        first.get("content").and_then(|c| c.as_str()).unwrap_or(""),
        "Your AI Customer Service Agent - Live in minutes."
    );
}

/// Test parsing SearXNG response with missing fields (graceful handling)
#[test]
fn test_searxng_json_parsing_missing_fields() {
    let response = json!({
        "query": "test",
        "results": [
            {
                "title": "Has title only",
                "url": ""
            },
            {
                "url": "https://example.com/no-title",
                "title": ""
            },
            {
                "url": "https://example.com/both",
                "title": "Has both"
            },
            {
                "engine": "duckduckgo"
            }
        ]
    });

    let results = response.get("results")
        .and_then(|r| r.as_array())
        .map_or(Vec::new(), |v| v.clone());

    // Filter results that have either title or url
    let filtered: Vec<_> = results
        .iter()
        .filter(|r| {
            let title = r.get("title").and_then(|t| t.as_str()).unwrap_or("");
            let url = r.get("url").and_then(|u| u.as_str()).unwrap_or("");
            !title.is_empty() || !url.is_empty()
        })
        .collect();

    assert_eq!(filtered.len(), 3); // Fourth result has neither title nor url
}

/// Test parsing SearXNG response with empty results
#[test]
fn test_searxng_json_parsing_empty_results() {
    let response = json!({
        "query": "nothingfound",
        "number_of_results": 0,
        "results": [],
        "answers": [],
        "suggestions": []
    });

    let results = response.get("results")
        .and_then(|r| r.as_array())
        .map_or(Vec::new(), |v| v.clone());

    assert!(results.is_empty());
}

/// Test parsing SearXNG response with missing results key
#[test]
fn test_searxng_json_parsing_missing_results_key() {
    let response = json!({
        "query": "test",
        "number_of_results": 0
    });

    let results = response.get("results")
        .and_then(|r| r.as_array())
        .map_or(Vec::new(), |v| v.clone());

    assert!(results.is_empty());
}

/// Test parsing SearXNG response with null content fields
#[test]
fn test_searxng_json_parsing_null_fields() {
    let response = json!({
        "query": "test",
        "results": [
            {
                "title": "Test Result",
                "url": "https://example.com",
                "content": null,
                "publishedDate": null
            }
        ]
    });

    let results = response.get("results")
        .and_then(|r| r.as_array())
        .map_or(Vec::new(), |v| v.clone());

    assert_eq!(results.len(), 1);

    let first = &results[0];
    assert_eq!(
        first.get("title").and_then(|t| t.as_str()).unwrap_or(""),
        "Test Result"
    );
    assert_eq!(
        first.get("url").and_then(|u| u.as_str()).unwrap_or(""),
        "https://example.com"
    );
    // content is null, should return empty string
    assert_eq!(
        first.get("content").and_then(|c| c.as_str()).unwrap_or(""),
        ""
    );
}

/// Test parsing SearXNG response with special characters and unicode
#[test]
fn test_searxng_json_parsing_unicode() {
    let response = json!({
        "query": "xinfer",
        "results": [
            {
                "title": "用戸指南 — Xinference",
                "url": "https://example.com/中文",
                "content": "Xinference用戶指南提供了使用Xinference的詳細說明"
            },
            {
                "title": "XInfer — Blazing-fast LLM inference",
                "url": "https://github.com/guoqingbao/xinfer",
                "content": "No PyTorch. No Python runtime. Just fast, portable, production-ready inference."
            }
        ]
    });

    let results = response.get("results")
        .and_then(|r| r.as_array())
        .map_or(Vec::new(), |v| v.clone());

    assert_eq!(results.len(), 2);

    let first = &results[0];
    assert_eq!(
        first.get("title").and_then(|t| t.as_str()).unwrap_or(""),
        "用戸指南 — Xinference"
    );
    assert_eq!(
        first.get("url").and_then(|u| u.as_str()).unwrap_or(""),
        "https://example.com/中文"
    );
}

/// Test limit enforcement (take only N results)
#[test]
fn test_searxng_json_parsing_limit_enforcement() {
    let response = json!({
        "query": "test",
        "results": [
            {"title": "1", "url": "https://1.com"},
            {"title": "2", "url": "https://2.com"},
            {"title": "3", "url": "https://3.com"},
            {"title": "4", "url": "https://4.com"},
            {"title": "5", "url": "https://5.com"}
        ]
    });

    let results = response.get("results")
        .and_then(|r| r.as_array())
        .map_or(Vec::new(), |v| v.clone());

    // Simulate taking only 3 results (like count=3 in the tool)
    let limited: Vec<_> = results.iter().take(3).collect();
    assert_eq!(limited.len(), 3);
}

/// Test parsing real-world SearXNG response structure from the actual API
#[test]
fn test_searxng_json_parsing_real_world_structure() {
    // This mimics the actual SearXNG response format shown in the user's example
    let response = json!({
        "query": "xinfer",
        "number_of_results": 18,
        "results": [
            {
                "title": "XInfer.AI — Your AI Customer Service Agent - Live in Minutes",
                "url": "https://xinfer.ai/",
                "content": "Your AI Customer Service Agent - Live in minutes. Turns conversations into sales — automatically.",
                "engine": "duckduckgo",
                "template": "default.html",
                "parsed_url": ["https", "xinfer.ai", "/", "", "", ""],
                "img_src": "",
                "thumbnail": "",
                "priority": "",
                "engines": ["startpage", "duckduckgo"],
                "positions": [4, 2],
                "score": 1.5,
                "category": "general",
                "publishedDate": null
            },
            {
                "title": "GitHub - xorbitsai/inference: Swap GPT for any LLM",
                "url": "https://github.com/xorbitsai/inference",
                "content": "Swap GPT for any LLM by changing a single line of code.",
                "engine": "duckduckgo",
                "template": "default.html",
                "parsed_url": ["https", "github.com", "/xorbitsai/inference", "", "", ""],
                "engines": ["duckduckgo"],
                "positions": [1],
                "score": 1.0,
                "category": "general"
            }
        ],
        "answers": [],
        "corrections": [],
        "infoboxes": [],
        "suggestions": [],
        "unresponsive_engines": [["brave", "too many requests"]]
    });

    let results = response.get("results")
        .and_then(|r| r.as_array())
        .map_or(Vec::new(), |v| v.clone());

    assert_eq!(results.len(), 2);

    // Extract and verify the first result
    let first = &results[0];
    assert_eq!(
        first.get("title").and_then(|t| t.as_str()).unwrap_or(""),
        "XInfer.AI — Your AI Customer Service Agent - Live in Minutes"
    );
    assert_eq!(
        first.get("url").and_then(|u| u.as_str()).unwrap_or(""),
        "https://xinfer.ai/"
    );
    assert_eq!(
        first.get("content").and_then(|c| c.as_str()).unwrap_or(""),
        "Your AI Customer Service Agent - Live in minutes. Turns conversations into sales — automatically."
    );

    // Verify additional fields are accessible (even if we don't use them)
    assert_eq!(
        first.get("engine").and_then(|e| e.as_str()).unwrap_or(""),
        "duckduckgo"
    );
    assert!(first.get("score").and_then(|s| s.as_f64()).is_some());
}

/// Test that the parsing logic correctly handles the filter_map pattern
#[test]
fn test_searxng_filter_map_pattern() {
    let response = json!({
        "query": "test",
        "results": [
            {"title": "Valid 1", "url": "https://1.com", "content": "Content 1"},
            {"title": "", "url": "https://2.com", "content": "Content 2"},
            {"title": "Valid 3", "url": "", "content": ""},
            {"title": "", "url": "", "content": ""},
            {"url": "https://5.com", "content": "Content 5"}
        ]
    });

    let results = response.get("results")
        .and_then(|r| r.as_array())
        .map_or(Vec::new(), |v| v.clone());

    // Apply the same filter_map logic as in the tool
    let items: Vec<(String, String, String)> = results
        .iter()
        .filter_map(|result| {
            let title = result.get("title")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            let url = result.get("url")
                .and_then(|u| u.as_str())
                .unwrap_or("")
                .to_string();
            let snippet = result.get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();

            if !title.is_empty() || !url.is_empty() {
                Some((title, url, snippet))
            } else {
                None
            }
        })
        .collect();

    // Should have 4 results (all except the 4th which has empty title AND url)
    assert_eq!(items.len(), 4);

    // Verify the filtered results
    assert_eq!(items[0], ("Valid 1".to_string(), "https://1.com".to_string(), "Content 1".to_string()));
    assert_eq!(items[1], ("".to_string(), "https://2.com".to_string(), "Content 2".to_string()));
    assert_eq!(items[2], ("Valid 3".to_string(), "".to_string(), "".to_string()));
    assert_eq!(items[3], ("".to_string(), "https://5.com".to_string(), "Content 5".to_string()));
}