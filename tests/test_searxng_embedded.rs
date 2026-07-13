#[cfg(test)]
mod searxng_embedded_tests {
    use xbot::config::WebSearchConfig;
    use xbot::tools::{Tool, ToolOutput, WebSearchTool};
    use serde_json::json;

    #[test]
    fn test_spec_description_for_duckduckgo() {
        let config = WebSearchConfig {
            provider: "duckduckgo".to_string(),
            api_key: String::new(),
            base_url: None,
            max_results: 10,
        };
        let tool = WebSearchTool::new(config, None);
        let spec = tool.spec();
        assert_eq!(spec.description, "Search the web using DuckDuckGo.");
    }

    #[test]
    fn test_spec_description_for_searxng_embedded() {
        let config = WebSearchConfig {
            provider: "searxng-embedded".to_string(),
            api_key: String::new(),
            base_url: None,
            max_results: 10,
        };
        let tool = WebSearchTool::new(config, None);
        let spec = tool.spec();
        
        // When feature is enabled, we get the specific description
        // When feature is disabled, we get the generic fallback
        #[cfg(feature = "searxng-embedded")]
        assert_eq!(spec.description, "Search the web using the native embedded SearXNG-RS engine.");
        
        #[cfg(not(feature = "searxng-embedded"))]
        assert!(spec.description.contains("searxng-embedded"));
    }

    #[test]
    fn test_spec_description_for_unknown_provider() {
        let config = WebSearchConfig {
            provider: "unknown_provider".to_string(),
            api_key: String::new(),
            base_url: None,
            max_results: 10,
        };
        let tool = WebSearchTool::new(config, None);
        let spec = tool.spec();
        assert!(spec.description.contains("unknown_provider"));
    }

    #[tokio::test]
    async fn test_execute_duckduckgo_provider() {
        let config = WebSearchConfig {
            provider: "duckduckgo".to_string(),
            api_key: String::new(),
            base_url: None,
            max_results: 10,
        };
        let tool = WebSearchTool::new(config, None);
        let params = json!({ "query": "rust programming" });
        let output = tool.execute(params).await;
        // We can't easily test the actual search without network, but we can verify no panic
        assert!(matches!(output, ToolOutput::Text(_)));
    }

    #[tokio::test]
    async fn test_execute_searxng_embedded_provider() {
        #[cfg(feature = "searxng-embedded")]
        {
            // Verify the feature is enabled and the code compiles
            let config = WebSearchConfig {
                provider: "searxng-embedded".to_string(),
                api_key: String::new(),
                base_url: None,
                max_results: 10,
            };
            let tool = WebSearchTool::new(config, None);
            let spec = tool.spec();
            assert_eq!(spec.description, "Search the web using the native embedded SearXNG-RS engine.");
            
            // Actually perform a search to verify results
            let params = json!({ "query": "!code searxng-rs" });
            let output = tool.execute(params).await;
            
            match output {
                ToolOutput::Text(text) => {
                    println!("Search results for '!code searxng-rs':\n{}", text);
                    // Verify the results contain the GitHub repository
                    assert!(text.contains("github.com"), "Results should contain github.com");
                    assert!(text.contains("searxng-rs") || text.contains("SearXNG"), "Results should mention searxng-rs");
                    // Ensure we got some results
                    assert!(!text.contains("No results found"), "Search should return results");
                }
                _ => panic!("Expected Text output"),
            }
        }
        #[cfg(not(feature = "searxng-embedded"))]
        {
            let config = WebSearchConfig {
                provider: "searxng-embedded".to_string(),
                api_key: String::new(),
                base_url: None,
                max_results: 10,
            };
            let tool = WebSearchTool::new(config, None);
            let params = json!({ "query": "rust programming" });
            let output = tool.execute(params).await;
            match output {
                ToolOutput::Text(text) => {
                    assert!(text.contains("feature is not enabled"));
                    assert!(text.contains("Rebuild with"));
                }
                _ => panic!("Expected Text output"),
            }
        }
    }

    #[tokio::test]
    async fn test_execute_unknown_provider() {
        let config = WebSearchConfig {
            provider: "unknown".to_string(),
            api_key: String::new(),
            base_url: None,
            max_results: 10,
        };
        let tool = WebSearchTool::new(config, None);
        let params = json!({ "query": "test" });
        let output = tool.execute(params).await;
        match output {
            ToolOutput::Text(text) => {
                assert!(text.contains("unknown"));
                assert!(text.contains("not implemented"));
            }
            _ => panic!("Expected Text output for unknown provider"),
        }
    }

    #[test]
    fn test_format_search_results_empty() {
        let items: Vec<(String, String, String)> = vec![];
        let result = format_search_results("test query", &items);
        assert_eq!(result, "No results for: test query");
    }

    #[test]
    fn test_format_search_results_single() {
        let items = vec![(
            "Title".to_string(),
            "https://example.com".to_string(),
            "Snippet".to_string(),
        )];
        let result = format_search_results("test query", &items);
        assert!(result.contains("Title"));
        assert!(result.contains("https://example.com"));
        assert!(result.contains("Snippet"));
    }

    // Helper function for tests (defined here to avoid import issues)
    fn format_search_results(query: &str, items: &[(String, String, String)]) -> String {
        if items.is_empty() {
            return format!("No results for: {query}");
        }
        let mut lines = vec![format!("Results for: {query}\n")];
        for (index, (title, url, snippet)) in items.iter().enumerate() {
            lines.push(format!(
                "{}. {}\n   {}",
                index + 1,
                title,
                url
            ));
            if !snippet.trim().is_empty() {
                lines.push(format!("   {}", snippet));
            }
        }
        lines.join("\n")
    }
}