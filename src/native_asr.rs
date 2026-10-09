use std::fs::{metadata, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::recognize_result::RecognizeResult;

#[derive(Debug, Clone)]
pub struct NativeAsrEvent {
    pub id: String,
    pub text: String,
    pub is_final: bool,
}

pub fn spawn_instruction_log_watcher(
    path: PathBuf,
    interval: Duration,
) -> mpsc::Receiver<NativeAsrEvent> {
    let (tx, rx) = mpsc::channel(32);
    tokio::spawn(async move {
        let mut size: u64 = 0;
        let mut initialized = false;
        loop {
            if !initialized {
                if path.exists() {
                    if let Ok(meta) = metadata(&path) {
                        size = meta.len();
                    }
                }
                initialized = true;
                tokio::time::sleep(interval).await;
                continue;
            }
            match read_new_events(&path, &mut size) {
                Ok(events) => {
                    for event in events {
                        if tx.send(event).await.is_err() {
                            return;
                        }
                    }
                }
                Err(err) => {
                    debug!(error = %err, path = %path.display(), "instruction log not ready")
                }
            }
            tokio::time::sleep(interval).await;
        }
    });
    rx
}

fn read_new_events(path: &PathBuf, size: &mut u64) -> std::io::Result<Vec<NativeAsrEvent>> {
    if !path.exists() {
        *size = 0;
        return Ok(Vec::new());
    }
    let new_size = metadata(path)?.len();
    if new_size == *size {
        return Ok(Vec::new());
    }
    let mut file = File::open(path)?;
    if new_size > *size {
        file.seek(SeekFrom::Start(*size))?;
    }
    *size = new_size;

    let mut events = Vec::new();
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let parsed: RecognizeResult = match serde_json::from_str(&line) {
            Ok(parsed) => parsed,
            Err(err) => {
                warn!(error = %err, "ignored malformed instruction log line");
                continue;
            }
        };
        let Some(text) = parsed.asr_text() else {
            continue;
        };
        events.push(NativeAsrEvent {
            id: parsed.header.id,
            text: text.to_string(),
            is_final: parsed.payload.is_final,
        });
    }
    Ok(events)
}
