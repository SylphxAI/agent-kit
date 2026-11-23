//! Read a bounded inline preview without allocating oversized spool records.

use super::protocol::ConsoleLogArtifact;
use std::io::{self, BufRead, BufReader};
use tempfile::NamedTempFile;

pub(super) struct ConsoleCapture {
    pub logs: Vec<String>,
    pub artifact: Option<ConsoleLogArtifact>,
    pub partial_record: bool,
}

pub(super) fn collect_console(
    mut spool: NamedTempFile,
    limit: usize,
) -> io::Result<ConsoleCapture> {
    let mut logs = Vec::new();
    let mut reader = BufReader::new(spool.as_file_mut());
    let mut preview_record = Vec::new();
    let mut include_inline = true;
    let mut budget = limit.saturating_sub(2); // JSON array brackets
    let (mut entries, mut bytes, mut current_bytes) = (0u64, 0u64, 0u64);
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            break;
        }
        let newline = buf.iter().position(|&b| b == b'\n');
        let payload_len = newline.unwrap_or(buf.len());
        let consumed = payload_len + usize::from(newline.is_some());
        current_bytes += consumed as u64;
        if include_inline {
            if payload_len <= budget.saturating_sub(preview_record.len()) {
                preview_record.extend_from_slice(&buf[..payload_len]);
            } else {
                include_inline = false;
            }
        }
        if newline.is_some() {
            entries += 1;
            bytes += current_bytes;
            current_bytes = 0;
            if include_inline {
                // JSON.stringify can emit escaped lone UTF-16 surrogates that Rust
                // String cannot represent. Preserve the exact record in the spool.
                if let Ok(text) = serde_json::from_slice::<String>(&preview_record) {
                    logs.push(text);
                    // Reserve a separator for the next entry.
                    budget = budget.saturating_sub(preview_record.len() + 1);
                    preview_record.clear();
                } else {
                    include_inline = false;
                }
            }
        }
        reader.consume(consumed);
    }

    let partial_record = current_bytes != 0;
    let artifact = if entries > logs.len() as u64 {
        if partial_record {
            // Only newline-terminated records are part of the retrieval format.
            spool.as_file().set_len(bytes)?;
        }
        let path = spool.into_temp_path().keep().map_err(|err| err.error)?;
        Some(ConsoleLogArtifact {
            path,
            entries,
            bytes,
        })
    } else {
        None
    };
    Ok(ConsoleCapture {
        logs,
        artifact,
        partial_record,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, Write};

    fn collect_bytes(bytes: &[u8], limit: usize) -> ConsoleCapture {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(bytes).unwrap();
        file.rewind().unwrap();
        collect_console(file, limit).unwrap()
    }

    #[test]
    fn exact_serialized_budget_and_no_empty_artifact() {
        let capture = collect_bytes(b"\"a\"\n\"b\"\n", 9);
        assert_eq!(capture.logs, ["a", "b"]);
        assert!(capture.artifact.is_none());
        assert!(!capture.partial_record);
    }

    #[test]
    fn artifact_contains_complete_records_and_flags_partial_tail() {
        let capture = collect_bytes(b"\"a\"\n\"b\"\n\"unterminated", 8);
        assert_eq!(capture.logs, ["a"]);
        assert!(capture.partial_record);
        let artifact = capture.artifact.unwrap();
        assert_eq!((artifact.entries, artifact.bytes), (2, 8));
        assert_eq!(std::fs::read(&artifact.path).unwrap(), b"\"a\"\n\"b\"\n");
        std::fs::remove_file(artifact.path).unwrap();
    }
}
