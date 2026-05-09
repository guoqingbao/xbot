use serde_json::json;
use tempfile::tempdir;
use xbot::tools::{EditFileTool, ListDirTool, ReadFileTool, Tool, ToolOutput, find_match};

#[tokio::test]
async fn read_file_has_line_numbers() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sample.txt");
    std::fs::write(
        &path,
        (1..=20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    let tool = ReadFileTool::new(Some(dir.path().to_path_buf()), None, vec![]);
    let result = tool
        .execute(json!({"path": path.display().to_string()}))
        .await;
    match result {
        ToolOutput::Text(text) => {
            assert!(text.contains("1| line 1"));
            assert!(text.contains("20| line 20"));
        }
        other => panic!("unexpected output: {other:?}"),
    }
}

#[tokio::test]
async fn read_file_offset_and_limit() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sample.txt");
    std::fs::write(
        &path,
        (1..=20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    let tool = ReadFileTool::new(Some(dir.path().to_path_buf()), None, vec![]);
    let result = tool
        .execute(json!({"path": path.display().to_string(), "offset": 5, "limit": 3}))
        .await;
    match result {
        ToolOutput::Text(text) => {
            assert!(text.contains("5| line 5"));
            assert!(text.contains("7| line 7"));
            assert!(!text.contains("8| line 8"));
            assert!(text.contains("Use offset=8 to continue"));
        }
        other => panic!("unexpected output: {other:?}"),
    }
}

#[tokio::test]
async fn read_file_image_returns_blocks() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("pixel.png");
    std::fs::write(&path, b"\x89PNG\r\n\x1a\nfake-png-data").unwrap();
    let tool = ReadFileTool::new(Some(dir.path().to_path_buf()), None, vec![]);
    let result = tool
        .execute(json!({"path": path.display().to_string()}))
        .await;
    match result {
        ToolOutput::Blocks(blocks) => {
            assert_eq!(blocks[0]["type"], json!("image_url"));
            assert!(
                blocks[0]["image_url"]["url"]
                    .as_str()
                    .unwrap()
                    .starts_with("data:image/png;base64,")
            );
            let actual = std::path::PathBuf::from(blocks[0]["_meta"]["path"].as_str().unwrap())
                .canonicalize()
                .unwrap();
            let expected = path.canonicalize().unwrap();
            assert_eq!(actual, expected);
        }
        other => panic!("unexpected output: {other:?}"),
    }
}

#[test]
fn find_match_exact_and_trimmed() {
    let (exact, count) = find_match("hello world", "world");
    assert_eq!(exact.unwrap(), "world");
    assert_eq!(count, 1);

    let (trimmed, count) = find_match("    def foo():\n        pass\n", "def foo():\n    pass");
    assert!(trimmed.unwrap().contains("    def foo():"));
    assert_eq!(count, 1);
}

#[tokio::test]
async fn edit_file_exact_match() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("a.py");
    std::fs::write(&path, "hello world").unwrap();
    let tool = EditFileTool::new(Some(dir.path().to_path_buf()), None);
    let result = tool
        .execute(
            json!({"path": path.display().to_string(), "old_text": "world", "new_text": "earth"}),
        )
        .await;
    match result {
        ToolOutput::Text(text) => {
            assert!(text.contains("Successfully"));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello earth");
        }
        other => panic!("unexpected output: {other:?}"),
    }
}

#[tokio::test]
async fn edit_file_crlf_preserved() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("crlf.txt");
    std::fs::write(&path, b"line1\r\nline2\r\nline3").unwrap();
    let tool = EditFileTool::new(Some(dir.path().to_path_buf()), None);
    let result = tool
        .execute(json!({"path": path.display().to_string(), "old_text": "line1\nline2", "new_text": "LINE1\nLINE2"}))
        .await;
    match result {
        ToolOutput::Text(text) => {
            assert!(text.contains("Successfully"));
            let raw = std::fs::read(&path).unwrap();
            assert!(raw.windows(2).any(|window| window == b"\r\n"));
        }
        other => panic!("unexpected output: {other:?}"),
    }
}

#[tokio::test]
async fn list_dir_recursive_ignores_noise() {
    let dir = tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    std::fs::create_dir_all(dir.path().join("node_modules/pkg")).unwrap();
    std::fs::write(dir.path().join("src").join("main.rs"), "fn main() {}").unwrap();
    std::fs::write(dir.path().join("README.md"), "hi").unwrap();
    let tool = ListDirTool::new(Some(dir.path().to_path_buf()), None);
    let result = tool
        .execute(json!({"path": dir.path().display().to_string(), "recursive": true}))
        .await;
    match result {
        ToolOutput::Text(text) => {
            let normalized = text.replace('\\', "/");
            assert!(normalized.contains("src/main.rs"));
            assert!(normalized.contains("README.md"));
            assert!(!normalized.contains(".git"));
            assert!(!normalized.contains("node_modules"));
        }
        other => panic!("unexpected output: {other:?}"),
    }
}
