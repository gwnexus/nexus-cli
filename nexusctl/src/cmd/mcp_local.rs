//! Local stdio MCP server (`nexus mcp-local`).
//!
//! Exposes tools that need machine-local filesystem/session state and have
//! no Claude Code custom-tool equivalent (Claude Code has no custom-tool
//! registration hook; MCP is the only path to a new agent-callable tool).
//! NEXUS-APP dispatch af407643: nexus-mcp correctly declined to own these
//! (Dispatch d22de820) since a hosted server has no access to project-local
//! or session-local machine state.
//!
//! - `nexus_headroom_intercept_retrieve`: reads the original uncompressed
//!   content behind a Headroom-compressed tool result, by hash, from the
//!   project-local `.nexus/headroom-cache/<project_id>/<hash>.json` cache
//!   (nexus-oc-plugins' `OriginalStore` format).
//! - `nexus_cost_summary`: registered for tool discovery, but not yet
//!   implemented -- see the doc comment on `cost_summary()` below for why.
//!
//! Speaks MCP's stdio transport: newline-delimited JSON-RPC 2.0 messages,
//! one per line, on stdin/stdout. Must never write anything else to
//! stdout -- any stray byte corrupts the protocol stream for the host
//! (Claude Code) that spawned this process.

use serde_json::{json, Value};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const SERVER_NAME: &str = "nexus-local-tools";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Run the local MCP server loop until stdin closes (i.e. until the host
/// process terminates this child process).
pub async fn run() -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // malformed line: ignore, keep the connection alive
        };

        // Notifications (no "id") never get a response.
        let Some(id) = request.get("id").cloned() else {
            continue;
        };

        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");

        let response = match method {
            "initialize" => handle_initialize(id),
            "tools/list" => handle_tools_list(id),
            "tools/call" => handle_tools_call(id, request.get("params")).await,
            other => error_response(id, -32601, &format!("method not found: {}", other)),
        };

        let mut out = serde_json::to_string(&response)?;
        out.push('\n');
        stdout.write_all(out.as_bytes()).await?;
        stdout.flush().await?;
    }

    Ok(())
}

fn handle_initialize(id: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION }
        }
    })
}

fn handle_tools_list(id: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "tools": [
                {
                    "name": "nexus_headroom_intercept_retrieve",
                    "description": "Retrieve the original uncompressed content behind a Headroom-compressed tool result, by content hash. Optionally filter to lines matching a query.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "hash": {
                                "type": "string",
                                "description": "The content hash from the [HEADROOM:v1] header."
                            },
                            "query": {
                                "type": "string",
                                "description": "Optional search query to filter to relevant lines."
                            }
                        },
                        "required": ["hash"]
                    }
                },
                {
                    "name": "nexus_cost_summary",
                    "description": "On-demand markdown cost/spend summary for the current session.",
                    "inputSchema": { "type": "object", "properties": {} }
                }
            ]
        }
    })
}

async fn handle_tools_call(id: Value, params: Option<&Value>) -> Value {
    let Some(params) = params else {
        return error_response(id, -32602, "missing params");
    };
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

    let result = match name {
        "nexus_headroom_intercept_retrieve" => {
            let workspace = std::env::current_dir().unwrap_or_default();
            headroom_intercept_retrieve(&workspace, &arguments).await
        }
        "nexus_cost_summary" => cost_summary().await,
        other => Err(format!("unknown tool: {}", other)),
    };

    match result {
        Ok(text) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "content": [{ "type": "text", "text": text }], "isError": false }
        }),
        Err(msg) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "content": [{ "type": "text", "text": msg }], "isError": true }
        }),
    }
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

/// `.nexus/headroom-cache/<project_id>/<hash>.json` envelope, per
/// nexus-oc-plugins' `core/headroom-intercept/store.ts` (`OriginalStore`).
#[derive(serde::Deserialize)]
struct HeadroomCacheEntry {
    content: String,
}

/// Validate a content hash: non-empty, bounded length, hex characters only.
/// Guards against path traversal / malformed lookups into the cache
/// directory (mirrors the validation the OpenCode `headroom-intercept`
/// plugin already applies before a cache-file read).
fn valid_hash(hash: &str) -> bool {
    !hash.is_empty() && hash.len() <= 64 && hash.chars().all(|c| c.is_ascii_hexdigit())
}

async fn headroom_intercept_retrieve(
    workspace: &std::path::Path,
    args: &Value,
) -> Result<String, String> {
    let hash = args
        .get("hash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required argument: hash".to_string())?;

    if !valid_hash(hash) {
        return Err(format!(
            "invalid hash '{}': expected a hex content hash",
            hash
        ));
    }

    let project_id = current_project_id(workspace)?;

    let cache_path: PathBuf = workspace
        .join(".nexus")
        .join("headroom-cache")
        .join(&project_id)
        .join(format!("{}.json", hash));

    let raw = tokio::fs::read_to_string(&cache_path)
        .await
        .map_err(|e| format!("no cached content for hash '{}': {}", hash, e))?;

    let entry: HeadroomCacheEntry = serde_json::from_str(&raw)
        .map_err(|e| format!("cache entry for hash '{}' is malformed: {}", hash, e))?;

    let content = match args.get("query").and_then(|v| v.as_str()) {
        Some(query) if !query.is_empty() => {
            let needle = query.to_lowercase();
            let filtered: Vec<&str> = entry
                .content
                .lines()
                .filter(|line| line.to_lowercase().contains(&needle))
                .collect();
            if filtered.is_empty() {
                entry.content
            } else {
                filtered.join("\n")
            }
        }
        _ => entry.content,
    };

    Ok(content)
}

fn current_project_id(workspace: &std::path::Path) -> Result<String, String> {
    nexus_core::config::load_linked_project(Some(workspace))
        .map_err(|e| format!("could not resolve linked project: {}", e))?
        .map(|p| p.id)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            "no project linked in this workspace (.nexus/config.toml has no [project] section)"
                .to_string()
        })
}

/// `nexus_cost_summary` is registered for tool discovery, but has no real
/// implementation yet: the actual cost/spend computation
/// (`core/cost-control`, Helicone-backed) lives in nexus-oc-plugins
/// (TypeScript) and nexus-cli has no local data source to read it from
/// today (no API endpoint, no local telemetry file). Returns an honest
/// "not available" result rather than fabricated numbers -- see NEXUS-APP
/// dispatch af407643 follow-up for the data-source handoff this needs
/// before it can be implemented for real.
async fn cost_summary() -> Result<String, String> {
    Ok(
        "Cost summary is not yet available from nexus-cli's local MCP server: \
the underlying cost/spend computation depends on a data source \
(Helicone-backed telemetry, currently owned by nexus-oc-plugins' \
core/cost-control) that nexus-cli does not have access to today. This \
tool is registered for discovery but intentionally returns no fabricated \
numbers. See NEXUS-APP dispatch af407643 for the follow-up needed (a \
concrete local data file or API endpoint nexus-cli can read from) before \
this can be implemented."
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_hash_accepts_hex() {
        assert!(valid_hash("24f77d70750d3ce843871aaf"));
        assert!(valid_hash("abcdef0123456789"));
    }

    #[test]
    fn test_valid_hash_rejects_empty() {
        assert!(!valid_hash(""));
    }

    #[test]
    fn test_valid_hash_rejects_non_hex() {
        assert!(!valid_hash("not-a-hash"));
        assert!(!valid_hash("../../../etc/passwd"));
        assert!(!valid_hash("abc/def"));
    }

    #[test]
    fn test_valid_hash_rejects_overlong() {
        let too_long = "a".repeat(65);
        assert!(!valid_hash(&too_long));
    }

    #[test]
    fn test_handle_initialize_shape() {
        let resp = handle_initialize(json!(1));
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["serverInfo"]["name"], SERVER_NAME);
        assert!(resp["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn test_handle_tools_list_exposes_both_tools() {
        let resp = handle_tools_list(json!(1));
        let tools = resp["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"nexus_headroom_intercept_retrieve"));
        assert!(names.contains(&"nexus_cost_summary"));
    }

    #[tokio::test]
    async fn test_headroom_intercept_retrieve_reads_cache_entry() {
        let tmp = std::env::temp_dir().join(format!(
            "nexus-mcp-local-test-{}-{}",
            std::process::id(),
            "retrieve"
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let project_id = "test-project-id";
        let cache_dir = tmp.join(".nexus").join("headroom-cache").join(project_id);
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(
            tmp.join(".nexus").join("config.toml"),
            format!(
                "[project]\nid = \"{}\"\nname = \"Test\"\nslug = \"test\"\n",
                project_id
            ),
        )
        .unwrap();
        std::fs::write(
            cache_dir.join("abc123.json"),
            r#"{"hash":"abc123","length":10,"estimatedTokens":5,"storedAt":"2026-01-01T00:00:00Z","content":"line one\nline two matches\nline three"}"#,
        )
        .unwrap();

        let result = headroom_intercept_retrieve(&tmp, &json!({ "hash": "abc123" })).await;

        let _ = std::fs::remove_dir_all(&tmp);

        let content = result.unwrap();
        assert!(content.contains("line one"));
        assert!(content.contains("line two matches"));
    }

    #[tokio::test]
    async fn test_headroom_intercept_retrieve_filters_by_query() {
        let tmp = std::env::temp_dir().join(format!(
            "nexus-mcp-local-test-{}-{}",
            std::process::id(),
            "query"
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let project_id = "test-project-id";
        let cache_dir = tmp.join(".nexus").join("headroom-cache").join(project_id);
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(
            tmp.join(".nexus").join("config.toml"),
            format!("[project]\nid = \"{}\"\n", project_id),
        )
        .unwrap();
        std::fs::write(
            cache_dir.join("def456.json"),
            r#"{"hash":"def456","content":"alpha line\nbeta line\ngamma line"}"#,
        )
        .unwrap();

        let result =
            headroom_intercept_retrieve(&tmp, &json!({ "hash": "def456", "query": "beta" })).await;

        let _ = std::fs::remove_dir_all(&tmp);

        let content = result.unwrap();
        assert_eq!(content, "beta line");
    }

    #[tokio::test]
    async fn test_headroom_intercept_retrieve_rejects_invalid_hash() {
        let tmp = std::env::temp_dir();
        let result =
            headroom_intercept_retrieve(&tmp, &json!({ "hash": "../../etc/passwd" })).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid hash"));
    }

    #[tokio::test]
    async fn test_headroom_intercept_retrieve_requires_hash_argument() {
        let tmp = std::env::temp_dir();
        let result = headroom_intercept_retrieve(&tmp, &json!({})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing required argument"));
    }

    #[tokio::test]
    async fn test_cost_summary_returns_honest_not_available_message() {
        let result = cost_summary().await.unwrap();
        assert!(result.contains("not yet available"));
        assert!(result.contains("af407643"));
    }
}
