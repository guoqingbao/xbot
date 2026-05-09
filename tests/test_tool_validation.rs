use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use xbot::tools::{
    ExecTool, Tool, ToolOutput, ToolRegistry, ToolSpec, cast_params, validate_params,
};

struct SampleTool;

#[async_trait]
impl Tool for SampleTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "sample".to_string(),
            description: "sample tool".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "minLength": 2},
                    "count": {"type": "integer", "minimum": 1, "maximum": 10},
                    "mode": {"type": "string", "enum": ["fast", "full"]},
                    "meta": {
                        "type": "object",
                        "properties": {
                            "tag": {"type": "string"},
                            "flags": {"type": "array", "items": {"type": "string"}}
                        },
                        "required": ["tag"]
                    }
                },
                "required": ["query", "count"]
            }),
        }
    }

    async fn execute(&self, _params: Value) -> ToolOutput {
        ToolOutput::Text("ok".to_string())
    }
}

#[test]
fn validate_params_missing_required() {
    let spec = SampleTool.spec();
    let errors = validate_params(&spec.parameters, &json!({"query": "hi"}));
    assert!(
        errors
            .iter()
            .any(|error| error.contains("missing required count"))
    );
}

#[test]
fn validate_params_type_and_range() {
    let spec = SampleTool.spec();
    let errors = validate_params(&spec.parameters, &json!({"query": "hi", "count": 0}));
    assert!(
        errors
            .iter()
            .any(|error| error.contains("count must be >= 1"))
    );

    let errors = validate_params(&spec.parameters, &json!({"query": "hi", "count": "2"}));
    assert!(
        errors
            .iter()
            .any(|error| error.contains("count should be integer"))
    );
}

#[test]
fn validate_params_enum_and_min_length() {
    let spec = SampleTool.spec();
    let errors = validate_params(
        &spec.parameters,
        &json!({"query": "h", "count": 2, "mode": "slow"}),
    );
    assert!(
        errors
            .iter()
            .any(|error| error.contains("query must be at least 2 chars"))
    );
    assert!(
        errors
            .iter()
            .any(|error| error.contains("mode must be one of"))
    );
}

#[test]
fn validate_params_nested_object_and_array() {
    let spec = SampleTool.spec();
    let errors = validate_params(
        &spec.parameters,
        &json!({"query": "hi", "count": 2, "meta": {"flags": [1, "ok"]}}),
    );
    assert!(
        errors
            .iter()
            .any(|error| error.contains("missing required meta.tag"))
    );
    assert!(
        errors
            .iter()
            .any(|error| error.contains("meta.flags[0] should be string"))
    );
}

#[test]
fn validate_params_ignores_unknown_fields() {
    let spec = SampleTool.spec();
    let errors = validate_params(
        &spec.parameters,
        &json!({"query": "hi", "count": 2, "extra": "x"}),
    );
    assert!(errors.is_empty());
}

#[tokio::test]
async fn registry_returns_validation_error() {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(SampleTool));
    let result = registry.execute("sample", json!({"query": "hi"})).await;
    match result {
        ToolOutput::Text(text) => assert!(text.contains("Invalid parameters")),
        other => panic!("unexpected output: {other:?}"),
    }
}

#[test]
fn cast_params_string_to_int() {
    let schema = json!({
        "type": "object",
        "properties": {"count": {"type": "integer"}}
    });
    let cast = cast_params(&schema, &json!({"count": "42"}));
    assert_eq!(cast["count"], json!(42));
}

#[test]
fn cast_params_string_to_number() {
    let schema = json!({
        "type": "object",
        "properties": {"rate": {"type": "number"}}
    });
    let cast = cast_params(&schema, &json!({"rate": "2.5"}));
    assert_eq!(cast["rate"], json!(2.5));
}

#[test]
fn cast_params_string_to_bool() {
    let schema = json!({
        "type": "object",
        "properties": {"enabled": {"type": "boolean"}}
    });
    assert_eq!(
        cast_params(&schema, &json!({"enabled": "true"}))["enabled"],
        json!(true)
    );
    assert_eq!(
        cast_params(&schema, &json!({"enabled": "false"}))["enabled"],
        json!(false)
    );
    assert_eq!(
        cast_params(&schema, &json!({"enabled": "1"}))["enabled"],
        json!(true)
    );
}

#[test]
fn cast_params_array_items() {
    let schema = json!({
        "type": "object",
        "properties": {"nums": {"type": "array", "items": {"type": "integer"}}}
    });
    let cast = cast_params(&schema, &json!({"nums": ["1", "2", "3"]}));
    assert_eq!(cast["nums"], json!([1, 2, 3]));
}

#[test]
fn cast_params_nested_object() {
    let schema = json!({
        "type": "object",
        "properties": {
            "config": {
                "type": "object",
                "properties": {
                    "port": {"type": "integer"},
                    "debug": {"type": "boolean"}
                }
            }
        }
    });
    let cast = cast_params(
        &schema,
        &json!({"config": {"port": "8080", "debug": "true"}}),
    );
    assert_eq!(cast["config"]["port"], json!(8080));
    assert_eq!(cast["config"]["debug"], json!(true));
}

#[test]
fn exec_extract_absolute_paths_keeps_full_windows_path() {
    let cmd = r"type C:\user\workspace\txt";
    let paths = ExecTool::extract_absolute_paths(cmd);
    assert_eq!(paths, vec![r"C:\user\workspace\txt"]);
}

#[test]
fn exec_extract_absolute_paths_ignores_relative_posix_segments() {
    let cmd = ".venv/bin/python script.py";
    let paths = ExecTool::extract_absolute_paths(cmd);
    assert!(!paths.iter().any(|path| path == "/bin/python"));
}

#[test]
fn exec_extract_absolute_paths_captures_posix_absolute_paths() {
    let cmd = "cat /tmp/data.txt > /tmp/out.txt";
    let paths = ExecTool::extract_absolute_paths(cmd);
    assert!(paths.contains(&"/tmp/data.txt".to_string()));
    assert!(paths.contains(&"/tmp/out.txt".to_string()));
}

#[test]
fn exec_extract_absolute_paths_captures_home_paths() {
    let cmd = "cat ~/.xbot/config.json > ~/out.txt";
    let paths = ExecTool::extract_absolute_paths(cmd);
    assert!(paths.contains(&"~/.xbot/config.json".to_string()));
    assert!(paths.contains(&"~/out.txt".to_string()));
}
