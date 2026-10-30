//! Minimal MCP server over stdio (newline-delimited JSON-RPC 2.0) that
//! lets agents read the run and propose or claim milestones.

use std::io::{BufRead, Write};

use anyhow::Result;
use serde_json::{Value, json};

use crate::tools;

const PROTOCOL_VERSION: &str = "2025-06-18";

pub fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
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
        // Notifications have no id and get no response.
        let Some(id) = msg.get("id").cloned() else {
            continue;
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or_default();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let response = match method {
            "initialize" => {
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
                let (text, is_error) = match tools::call(name, &args) {
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
