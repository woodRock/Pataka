//! Minimal Server-Sent Events reader.

use futures_util::StreamExt;

use crate::error::{Error, Result};

/// Read an SSE body and call `on_event(event_name, data)` for each event.
/// Return `false` from the callback to stop reading.
pub async fn read(resp: reqwest::Response, mut on_event: impl FnMut(&str, &str) -> Result<bool>) -> Result<()> {
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut total = 0usize;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        total += chunk.len();
        if total > super::MAX_REPLY_BYTES * 4 {
            return Err(Error::Provider("the reply was too large".into()));
        }
        buf.extend(chunk.iter().filter(|b| **b != b'\r'));
        while let Some(pos) = buf.windows(2).position(|w| w == b"\n\n") {
            let block: Vec<u8> = buf.drain(..pos + 2).collect();
            let block = String::from_utf8_lossy(&block);
            let mut event = String::new();
            let mut data = String::new();
            for line in block.lines() {
                if let Some(v) = line.strip_prefix("event:") {
                    event = v.trim().to_string();
                } else if let Some(v) = line.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(v.strip_prefix(' ').unwrap_or(v));
                }
            }
            if !data.is_empty() && !on_event(&event, &data)? {
                return Ok(());
            }
        }
    }
    Ok(())
}
