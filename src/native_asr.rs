use std::ffi::CString;
use std::fs::{metadata, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::recognize_result::RecognizeResult;

const INOTIFY_HEARTBEAT: Duration = Duration::from_secs(2);

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
        if let Err(err) = watch_with_inotify(&path, interval, &tx).await {
            warn!(error = %err, "inotify unavailable; falling back to polling");
            watch_with_poll(&path, interval, tx).await;
        }
    });
    rx
}

async fn watch_with_inotify(
    path: &Path,
    fallback: Duration,
    tx: &mpsc::Sender<NativeAsrEvent>,
) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "instruction log path has no parent",
            )
        })?;

    while !parent.exists() {
        tokio::time::sleep(fallback).await;
    }

    let mut watcher = InotifyWatch::new(path)?;
    info!(path = %path.display(), "watching instruction.log with inotify");

    let mut size: u64 = current_size(path);
    let mut leftover = String::new();
    let heartbeat = fallback.max(INOTIFY_HEARTBEAT);
    loop {
        tokio::select! {
            readable = watcher.readable() => {
                readable?;
                watcher.drain_and_refresh()?;
                if send_new_events(path, &mut size, &mut leftover, tx).await {
                    return Ok(());
                }
            }
            _ = tokio::time::sleep(heartbeat) => {
                if send_new_events(path, &mut size, &mut leftover, tx).await {
                    return Ok(());
                }
            }
        }
    }
}

async fn watch_with_poll(path: &Path, interval: Duration, tx: mpsc::Sender<NativeAsrEvent>) {
    let mut size: u64 = current_size(path);
    let mut leftover = String::new();
    loop {
        if send_new_events(path, &mut size, &mut leftover, &tx).await {
            return;
        }
        tokio::time::sleep(interval).await;
    }
}

async fn send_new_events(
    path: &Path,
    size: &mut u64,
    leftover: &mut String,
    tx: &mpsc::Sender<NativeAsrEvent>,
) -> bool {
    match read_new_events(path, size, leftover) {
        Ok(events) => {
            for event in events {
                if tx.send(event).await.is_err() {
                    return true;
                }
            }
        }
        Err(err) => {
            debug!(error = %err, path = %path.display(), "instruction log not ready")
        }
    }
    false
}

fn current_size(path: &Path) -> u64 {
    metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

fn read_new_events(
    path: &Path,
    size: &mut u64,
    leftover: &mut String,
) -> io::Result<Vec<NativeAsrEvent>> {
    if !path.exists() {
        *size = 0;
        leftover.clear();
        return Ok(Vec::new());
    }
    let new_size = metadata(path)?.len();
    if new_size == *size && leftover.is_empty() {
        return Ok(Vec::new());
    }
    let mut file = File::open(path)?;
    if new_size > *size {
        file.seek(SeekFrom::Start(*size))?;
    } else {
        leftover.clear();
    }
    *size = new_size;

    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    leftover.push_str(&buf);

    let mut events = Vec::new();
    while let Some(idx) = leftover.find('\n') {
        let line: String = leftover.drain(..=idx).collect();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<RecognizeResult>(trimmed) {
            Ok(parsed) => {
                if let Some(text) = parsed.asr_text().map(str::to_string) {
                    events.push(NativeAsrEvent {
                        id: parsed.header.id,
                        text,
                        is_final: parsed.payload.is_final,
                    });
                }
            }
            Err(err) => {
                warn!(error = %err, "ignored malformed instruction log line");
            }
        }
    }
    Ok(events)
}

struct InotifyWatch {
    async_fd: AsyncFd<OwnedFd>,
    path: PathBuf,
    file_wd: Option<i32>,
}

impl InotifyWatch {
    fn new(path: &Path) -> io::Result<Self> {
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        let parent = path.parent().unwrap_or(Path::new("."));
        add_watch(
            owned.as_raw_fd(),
            parent,
            libc::IN_CREATE | libc::IN_MOVED_TO | libc::IN_DELETE | libc::IN_MOVED_FROM,
        )?;
        let mut watcher = Self {
            async_fd: AsyncFd::new(owned)?,
            path: path.to_path_buf(),
            file_wd: None,
        };
        watcher.refresh_file_watch()?;
        Ok(watcher)
    }

    async fn readable(&self) -> io::Result<()> {
        let mut guard = self.async_fd.readable().await?;
        guard.clear_ready();
        Ok(())
    }

    fn drain_and_refresh(&mut self) -> io::Result<()> {
        let mut buf = [0_u8; 4096];
        loop {
            let n = unsafe {
                libc::read(
                    self.async_fd.as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::WouldBlock {
                    break;
                }
                return Err(err);
            }
            if n == 0 {
                break;
            }
        }
        self.refresh_file_watch()
    }

    fn refresh_file_watch(&mut self) -> io::Result<()> {
        if self.path.exists() {
            if self.file_wd.is_none() {
                self.file_wd = Some(add_watch(
                    self.async_fd.as_raw_fd(),
                    &self.path,
                    libc::IN_MODIFY | libc::IN_MOVE_SELF | libc::IN_DELETE_SELF,
                )?);
            }
        } else {
            self.file_wd = None;
        }
        Ok(())
    }
}

fn add_watch(fd: RawFd, path: &Path, mask: u32) -> io::Result<i32> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains interior nul"))?;
    let wd = unsafe { libc::inotify_add_watch(fd, c_path.as_ptr(), mask) };
    if wd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(wd)
    }
}

#[cfg(test)]
mod tests {
    use super::read_new_events;
    use std::io::Write;

    #[test]
    fn buffers_partial_json_line_until_newline() {
        let dir = std::env::temp_dir().join(format!(
            "xiaoai-inotify-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("instruction.log");
        let mut file = std::fs::File::create(&path).unwrap();
        let mut size = 0;
        let mut leftover = String::new();

        write!(
            file,
            r#"{{"header":{{"dialog_id":"d","id":"1","name":"RecognizeResult","namespace":"SpeechRecognizer"}},"payload":{{"is_final":true,"results":[{{"text":"打开PC"}}]}}}"#
        )
        .unwrap();
        file.flush().unwrap();
        assert!(read_new_events(&path, &mut size, &mut leftover)
            .unwrap()
            .is_empty());
        assert!(!leftover.is_empty());

        writeln!(file).unwrap();
        file.flush().unwrap();
        let events = read_new_events(&path, &mut size, &mut leftover).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].text, "打开PC");
        assert!(leftover.is_empty());

        std::fs::remove_dir_all(dir).unwrap();
    }
}
