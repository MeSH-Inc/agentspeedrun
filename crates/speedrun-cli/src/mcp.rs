//! Minimal MCP server over stdio (newline-delimited JSON-RPC 2.0) that
//! lets agents read the run and propose or claim milestones.
//!
//! The project is found from, in order: `--project-dir`, the client's MCP
//! roots (asked for after initialization when the client supports them),
//! then the working directory. Editors such as Zed may not start servers
//! in the project, so roots matter.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Value, json};

use crate::project::Project;
use crate::tools;

const ROOTS_REQUEST_ID: &str = "speedrun-roots";

struct Session {
    explicit_dir: Option<PathBuf>,
    roots: Vec<PathBuf>,
    client_has_roots: bool,
}

impl Session {
    /// Where tools look for the project.
    fn project_dir(&self) -> PathBuf {
        if let Some(d) = &self.explicit_dir {
            return d.clone();
        }
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        self.roots
            .iter()
            .find(|r| Project::find(r).ok().flatten().is_some())
            .cloned()
            .unwrap_or(cwd)
    }
}

/// `file:///a/b%20c` → `/a/b c`. Non-file URIs are ignored.
fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?, 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(PathBuf::from(String::from_utf8(out).ok()?))
}

fn ask_for_roots(out: &mut impl Write) -> Result<()> {
    write(
        out,
        json!({"jsonrpc":"2.0","id":ROOTS_REQUEST_ID,"method":"roots/list"}),
    )
}

const PROTOCOL_VERSION: &str = "2025-06-18";

pub fn serve(project_dir: Option<&Path>) -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    let mut session = Session {
        explicit_dir: project_dir.map(Path::to_path_buf),
        roots: Vec::new(),
        client_has_roots: false,
    };
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            write(
                &mut stdout,
                json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}),
            )?;
            continue;
        };
        let method = msg.get("method").and_then(Value::as_str);
        // A response to our roots/list request.
        if method.is_none() {
            if msg.get("id").and_then(Value::as_str) == Some(ROOTS_REQUEST_ID) {
                session.roots = msg
                    .pointer("/result/roots")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|r| r.get("uri").and_then(Value::as_str).and_then(file_uri_to_path))
                    .collect();
            }
            continue;
        }
        let method = method.unwrap_or_default();
        // Notifications have no id and get no response.
        let Some(id) = msg.get("id").cloned() else {
            let wants_roots = session.client_has_roots && session.explicit_dir.is_none();
            if wants_roots
                && matches!(
                    method,
                    "notifications/initialized" | "notifications/roots/list_changed"
                )
            {
                ask_for_roots(&mut stdout)?;
            }
            continue;
        };
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let response = match method {
            "initialize" => {
                session.client_has_roots = params.pointer("/capabilities/roots").is_some();
                let version = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(PROTOCOL_VERSION);
                json!({"jsonrpc":"2.0","id":id,"result":{
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "agentspeedrun", "version": env!("CARGO_PKG_VERSION")},
                }})
            }
            "ping" => json!({"jsonrpc":"2.0","id":id,"result":{}}),
            "tools/list" => json!({"jsonrpc":"2.0","id":id,"result":{"tools": tools::definitions()}}),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let (text, is_error) = match tools::call(name, &args, &session.project_dir()) {
                    Ok(t) => (t, false),
                    Err(e) => (format!("{e:#}"), true),
                };
                json!({"jsonrpc":"2.0","id":id,"result":{
                    "content":[{"type":"text","text":text}],
                    "isError": is_error,
                }})
            }
            _ => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":format!("method not found: {method}")}})
            }
        };
        write(&mut stdout, response)?;
    }
    Ok(())
}

fn write(out: &mut impl Write, v: Value) -> Result<()> {
    writeln!(out, "{v}")?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_file_uris() {
        assert_eq!(
            file_uri_to_path("file:///home/a/my%20app"),
            Some(PathBuf::from("/home/a/my app"))
        );
        assert_eq!(
            file_uri_to_path("file:///x/caf%C3%A9"),
            Some(PathBuf::from("/x/café"))
        );
        assert_eq!(file_uri_to_path("https://example.com"), None);
    }
}
