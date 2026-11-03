//! Single-prompt text access and erasure through the full-token client.
use std::io::{self, Write};

use anyhow::{Result, bail};

use crate::client::Client;

fn path(event_id: &str) -> Result<String> {
    if event_id.is_empty() || event_id == "." || event_id == ".." || event_id.chars().any(char::is_control) {
        bail!("invalid prompt event id");
    }
    let mut encoded = String::new();
    for byte in event_id.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(encoded, "%{byte:02X}")?;
        }
    }
    Ok(format!("/v1/prompts/{encoded}"))
}

fn write_content(out: &mut impl Write, content: Option<&str>) -> io::Result<()> {
    if let Some(content) = content {
        out.write_all(content.as_bytes())?;
    }
    Ok(())
}

pub fn show(client: &Client, event_id: &str) -> Result<()> {
    let content: Option<String> = client.get(&path(event_id)?)?;
    write_content(&mut io::stdout().lock(), content.as_deref())?;
    Ok(())
}

pub fn erase(client: &Client, event_id: &str) -> Result<()> {
    let erased: bool = client.delete(&path(event_id)?)?;
    println!("{erased}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_output_is_exact_and_absence_is_silent() {
        let mut output = Vec::new();
        write_content(&mut output, None).unwrap();
        assert!(output.is_empty());
        write_content(&mut output, Some("line one\nline two")).unwrap();
        assert_eq!(output, b"line one\nline two");
    }

    #[test]
    fn event_id_cannot_modify_the_route() {
        assert_eq!(path("event/one?x=1").unwrap(), "/v1/prompts/event%2Fone%3Fx%3D1");
        assert!(path("..").is_err());
        assert!(path("bad\nvalue").is_err());
    }
}
