use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use tokio::net::UnixDatagram;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

const MAX_DATAGRAM_SIZE: usize = 256 * 1024;

const SPEECH_TYPE_UPWARD: u64 = 0;
const SPEECH_TYPE_DOWNWARD: u64 = 1;

const UP_REGISTER: u64 = 0;
const UP_STREAM_PREPARE: u64 = 1;
const UP_STREAM_CANCEL: u64 = 2;
const UP_STREAM_TRANSMITTING: u64 = 3;

const STREAM_ASR: u64 = 2;
const ACTIVATE_WAKEUP: u64 = 0;

pub enum SpeechProxyEvent {
    Registered {
        vendor: String,
        codec: Option<String>,
    },
    Wake,
    Fatal(String),
}

pub struct SpeechProxy {
    handle: SpeechHandle,
    event_rx: mpsc::Receiver<SpeechProxyEvent>,
    task: JoinHandle<()>,
    public_path: PathBuf,
    proxy_path: PathBuf,
}

impl SpeechProxy {
    pub async fn bind(
        public_path: impl Into<PathBuf>,
        native_path: impl Into<PathBuf>,
    ) -> anyhow::Result<Self> {
        let public_path = public_path.into();
        let native_path = native_path.into();
        if !native_path.exists() {
            bail!(
                "native speech socket {} is missing; start.sh must relocate mico_aivs_lab's socket first",
                native_path.display()
            );
        }

        let proxy_path = public_path
            .parent()
            .unwrap_or_else(|| Path::new("/tmp"))
            .join("xiaoai.proxy.usock");

        prepare_socket_path(&public_path)?;
        prepare_socket_path(&proxy_path)?;
        let public = UnixDatagram::bind(&public_path)
            .with_context(|| format!("bind speech socket {}", public_path.display()))?;
        let native = UnixDatagram::bind(&proxy_path)
            .with_context(|| format!("bind proxy socket {}", proxy_path.display()))?;

        let (event_tx, event_rx) = mpsc::channel(16);
        let (command_tx, command_rx) = mpsc::channel(16);
        let handle = SpeechHandle { command_tx };
        let task_event_tx = event_tx.clone();
        let task = tokio::spawn(async move {
            if let Err(err) = run_proxy(public, native, native_path, event_tx, command_rx).await {
                let _ = task_event_tx
                    .send(SpeechProxyEvent::Fatal(format!("{err:#}")))
                    .await;
            }
        });

        info!(
            socket = %public_path.display(),
            native = %proxy_path.display(),
            "speech.usock proxy listening"
        );
        Ok(Self {
            handle,
            event_rx,
            task,
            public_path,
            proxy_path,
        })
    }

    pub fn handle(&self) -> SpeechHandle {
        self.handle.clone()
    }

    pub async fn recv(&mut self) -> Option<SpeechProxyEvent> {
        self.event_rx.recv().await
    }
}

impl Drop for SpeechProxy {
    fn drop(&mut self) {
        self.task.abort();
        let _ = fs::remove_file(&self.public_path);
        let _ = fs::remove_file(&self.proxy_path);
    }
}

#[derive(Clone)]
pub struct SpeechHandle {
    command_tx: mpsc::Sender<ServiceCommand>,
}

impl SpeechHandle {
    pub async fn mute_native(&self) -> anyhow::Result<()> {
        self.send(CommandKind::Mute).await
    }

    async fn send(&self, kind: CommandKind) -> anyhow::Result<()> {
        let (result_tx, result_rx) = oneshot::channel();
        self.command_tx
            .send(ServiceCommand { kind, result_tx })
            .await
            .context("speech.usock proxy stopped")?;
        result_rx
            .await
            .context("speech.usock command response was dropped")?
    }
}

struct ServiceCommand {
    kind: CommandKind,
    result_tx: oneshot::Sender<anyhow::Result<()>>,
}

enum CommandKind {
    Mute,
}

struct ProxyState {
    mipns_peer: Option<PathBuf>,
    native_path: PathBuf,
    muted: bool,
}

async fn run_proxy(
    public: UnixDatagram,
    native: UnixDatagram,
    native_path: PathBuf,
    event_tx: mpsc::Sender<SpeechProxyEvent>,
    mut command_rx: mpsc::Receiver<ServiceCommand>,
) -> anyhow::Result<()> {
    let mut state = ProxyState {
        mipns_peer: None,
        native_path,
        muted: false,
    };
    let mut public_buf = vec![0_u8; MAX_DATAGRAM_SIZE];
    let mut native_buf = vec![0_u8; MAX_DATAGRAM_SIZE];

    loop {
        tokio::select! {
            packet = public.recv_from(&mut public_buf) => {
                let (size, peer) = packet.context("receive mipns datagram")?;
                let peer = peer
                    .as_pathname()
                    .map(Path::to_path_buf)
                    .context("mipns used an unnamed Unix datagram socket")?;
                handle_mipns_packet(
                    &public_buf[..size],
                    peer,
                    &native,
                    &event_tx,
                    &mut state,
                )
                .await?;
            }
            packet = native.recv_from(&mut native_buf) => {
                let (size, _) = packet.context("receive native speech datagram")?;
                let Some(mipns_peer) = state.mipns_peer.as_deref() else {
                    debug!("dropped native speech packet before mipns registered");
                    continue;
                };
                send_to(&public, mipns_peer, &native_buf[..size]).await?;
            }
            command = command_rx.recv() => {
                let Some(command) = command else {
                    return Ok(());
                };
                let result = handle_command(&native, &mut state, command.kind).await;
                let _ = command.result_tx.send(result);
            }
        }
    }
}

async fn handle_mipns_packet(
    packet: &[u8],
    peer: PathBuf,
    native: &UnixDatagram,
    event_tx: &mpsc::Sender<SpeechProxyEvent>,
    state: &mut ProxyState,
) -> anyhow::Result<()> {
    let mut outgoing = packet.to_vec();
    match decode_speech_upward(packet) {
        Ok(Some(event)) => match event.up_type {
            UP_REGISTER => {
                state.mipns_peer = Some(peer);
                state.muted = false;
                let vendor = event.speech_vendor.unwrap_or_else(|| "unknown".to_string());
                let codec = event.speech_codec.filter(|value| !value.trim().is_empty());
                info!(
                    vendor,
                    codec = codec.as_deref().unwrap_or("pcm16"),
                    "mipns registered; forwarding to native ASR"
                );
                event_tx
                    .send(SpeechProxyEvent::Registered { vendor, codec })
                    .await
                    .ok();
            }
            UP_STREAM_PREPARE => {
                state.mipns_peer = Some(peer);
                state.muted = false;
                if event.activate_mode.unwrap_or_default() == ACTIVATE_WAKEUP {
                    info!("WAKE native mipns; native microphone input enabled");
                    event_tx.send(SpeechProxyEvent::Wake).await.ok();
                }
            }
            UP_STREAM_TRANSMITTING => {
                if state.muted && event.transmit_type == Some(STREAM_ASR) {
                    outgoing = silence_asr_packet(event.data.len());
                }
            }
            _ => {}
        },
        Ok(None) => {}
        Err(err) => warn!(error = %err, "forwarding malformed mipns packet"),
    }
    send_to(native, &state.native_path, &outgoing).await
}

async fn handle_command(
    native: &UnixDatagram,
    state: &mut ProxyState,
    kind: CommandKind,
) -> anyhow::Result<()> {
    match kind {
        CommandKind::Mute => {
            if state.muted {
                return Ok(());
            }
            state.muted = true;
            send_to(native, &state.native_path, &stream_cancel_packet()).await?;
            info!("native microphone input muted; native dialog cancelled");
        }
    }
    Ok(())
}

async fn send_to(socket: &UnixDatagram, peer: &Path, packet: &[u8]) -> anyhow::Result<()> {
    let written = socket
        .send_to(packet, peer)
        .await
        .with_context(|| format!("send speech datagram to {}", peer.display()))?;
    if written != packet.len() {
        bail!(
            "short speech datagram write: wrote {written}, wanted {}",
            packet.len()
        );
    }
    Ok(())
}

fn prepare_socket_path(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create speech socket directory {}", parent.display()))?;
    }
    if path.exists() {
        fs::remove_file(path)
            .with_context(|| format!("remove stale speech socket {}", path.display()))?;
    }
    Ok(())
}

fn silence_asr_packet(pcm_len: usize) -> Vec<u8> {
    transmitting_packet(&vec![0_u8; pcm_len])
}

fn transmitting_packet(data: &[u8]) -> Vec<u8> {
    let body = message([varint_field(1, STREAM_ASR), bytes_field(2, data)]);
    upward(message([
        varint_field(1, UP_STREAM_TRANSMITTING),
        bytes_field(4, &body),
    ]))
}

fn stream_cancel_packet() -> Vec<u8> {
    upward(message([varint_field(1, UP_STREAM_CANCEL)]))
}

fn upward(upward: Vec<u8>) -> Vec<u8> {
    message([varint_field(1, SPEECH_TYPE_UPWARD), bytes_field(2, &upward)])
}

#[derive(Debug, Default)]
struct UpEvent {
    up_type: u64,
    speech_vendor: Option<String>,
    speech_codec: Option<String>,
    activate_mode: Option<u64>,
    transmit_type: Option<u64>,
    data: Vec<u8>,
}

fn decode_speech_upward(buf: &[u8]) -> anyhow::Result<Option<UpEvent>> {
    let fields = parse_fields(buf)?;
    if find_varint(&fields, 1) != Some(SPEECH_TYPE_UPWARD) {
        return Ok(None);
    }
    let upward = find_bytes(&fields, 2).context("upward message has no body")?;
    let fields = parse_fields(upward)?;
    let up_type = find_varint(&fields, 1).context("upward body has no type")?;
    let mut event = UpEvent {
        up_type,
        ..UpEvent::default()
    };
    match up_type {
        UP_REGISTER => {
            if let Some(body) = find_bytes(&fields, 2) {
                let body = parse_fields(body)?;
                event.speech_vendor = find_string(&body, 1);
                event.speech_codec = find_string(&body, 2);
            }
        }
        UP_STREAM_PREPARE => {
            if let Some(body) = find_bytes(&fields, 3) {
                let body = parse_fields(body)?;
                event.activate_mode = find_varint(&body, 1);
            }
        }
        UP_STREAM_TRANSMITTING => {
            if let Some(body) = find_bytes(&fields, 4) {
                let body = parse_fields(body)?;
                event.transmit_type = find_varint(&body, 1);
                event.data = find_bytes(&body, 2).unwrap_or_default().to_vec();
            }
        }
        _ => {}
    }
    Ok(Some(event))
}

fn find_varint(fields: &[Field], number: u64) -> Option<u64> {
    fields.iter().find_map(|field| match field {
        Field::Varint {
            number: field_number,
            value,
        } if *field_number == number => Some(*value),
        _ => None,
    })
}

fn find_bytes(fields: &[Field], number: u64) -> Option<&[u8]> {
    fields.iter().find_map(|field| match field {
        Field::Bytes {
            number: field_number,
            value,
        } if *field_number == number => Some(value.as_slice()),
        _ => None,
    })
}

fn find_string(fields: &[Field], number: u64) -> Option<String> {
    String::from_utf8(find_bytes(fields, number)?.to_vec()).ok()
}

fn message(parts: impl IntoIterator<Item = Vec<u8>>) -> Vec<u8> {
    parts.into_iter().flatten().collect()
}

fn varint_field(number: u64, value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    encode_varint(number << 3, &mut out);
    encode_varint(value, &mut out);
    out
}

fn bytes_field(number: u64, value: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_varint((number << 3) | 2, &mut out);
    encode_varint(value.len() as u64, &mut out);
    out.extend_from_slice(value);
    out
}

fn encode_varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

#[derive(Debug, PartialEq)]
enum Field {
    Varint { number: u64, value: u64 },
    Fixed64 { number: u64, value: [u8; 8] },
    Bytes { number: u64, value: Vec<u8> },
    Fixed32 { number: u64, value: [u8; 4] },
}

fn parse_fields(mut buf: &[u8]) -> anyhow::Result<Vec<Field>> {
    let mut fields = Vec::new();
    while !buf.is_empty() {
        let key = decode_varint(&mut buf)?;
        let number = key >> 3;
        if number == 0 {
            bail!("protobuf field number 0 is invalid");
        }
        match key & 7 {
            0 => fields.push(Field::Varint {
                number,
                value: decode_varint(&mut buf)?,
            }),
            1 => {
                if buf.len() < 8 {
                    bail!("truncated fixed64");
                }
                let mut value = [0_u8; 8];
                value.copy_from_slice(&buf[..8]);
                buf = &buf[8..];
                fields.push(Field::Fixed64 { number, value });
            }
            2 => {
                let len = usize::try_from(decode_varint(&mut buf)?)
                    .context("protobuf byte field is too large")?;
                if buf.len() < len {
                    bail!("truncated bytes");
                }
                fields.push(Field::Bytes {
                    number,
                    value: buf[..len].to_vec(),
                });
                buf = &buf[len..];
            }
            5 => {
                if buf.len() < 4 {
                    bail!("truncated fixed32");
                }
                let mut value = [0_u8; 4];
                value.copy_from_slice(&buf[..4]);
                buf = &buf[4..];
                fields.push(Field::Fixed32 { number, value });
            }
            wire => bail!("unsupported protobuf wire type {wire}"),
        }
    }
    Ok(fields)
}

fn decode_varint(buf: &mut &[u8]) -> anyhow::Result<u64> {
    let mut value = 0_u64;
    for shift in (0..64).step_by(7) {
        let Some((&byte, rest)) = buf.split_first() else {
            bail!("truncated varint");
        };
        *buf = rest;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(anyhow!("varint too long"))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use tokio::time::timeout;

    use super::*;

    fn register() -> Vec<u8> {
        let body = message([bytes_field(1, b"xiaomi"), bytes_field(2, b"pcm16")]);
        upward(message([
            varint_field(1, UP_REGISTER),
            bytes_field(2, &body),
        ]))
    }

    fn prepare_wakeup() -> Vec<u8> {
        let body = message([varint_field(1, ACTIVATE_WAKEUP), varint_field(2, 0)]);
        upward(message([
            varint_field(1, UP_STREAM_PREPARE),
            bytes_field(3, &body),
        ]))
    }

    fn downward_register_ok() -> Vec<u8> {
        message([
            varint_field(1, SPEECH_TYPE_DOWNWARD),
            bytes_field(3, &message([varint_field(1, 0)])),
        ])
    }

    #[test]
    fn silence_packet_keeps_asr_type_and_zeros_pcm() {
        let silent = silence_asr_packet(4);
        let event = decode_speech_upward(&silent).unwrap().unwrap();
        assert_eq!(event.up_type, UP_STREAM_TRANSMITTING);
        assert_eq!(event.transmit_type, Some(STREAM_ASR));
        assert_eq!(event.data, [0, 0, 0, 0]);
    }

    #[tokio::test]
    async fn proxies_to_native_and_mutes_asr_pcm() {
        let temp = std::env::temp_dir().join(format!(
            "xiaoai-proxy-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&temp).unwrap();
        let public_path = temp.join("speech.usock");
        let native_path = temp.join("speech.native.usock");
        let mipns_path = temp.join("mipns.usock");

        let native = UnixDatagram::bind(&native_path).unwrap();
        let mut proxy = SpeechProxy::bind(&public_path, &native_path).await.unwrap();
        let mipns = UnixDatagram::bind(&mipns_path).unwrap();

        mipns.send_to(&register(), &public_path).await.unwrap();
        let mut buf = vec![0_u8; 4096];
        let (size, _) = timeout(Duration::from_secs(1), native.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        let event = decode_speech_upward(&buf[..size]).unwrap().unwrap();
        assert_eq!(event.up_type, UP_REGISTER);
        assert!(matches!(
            proxy.recv().await,
            Some(SpeechProxyEvent::Registered { .. })
        ));

        native
            .send_to(&downward_register_ok(), temp.join("xiaoai.proxy.usock"))
            .await
            .unwrap();
        let size = timeout(Duration::from_secs(1), mipns.recv(&mut buf))
            .await
            .unwrap()
            .unwrap();
        let outer = parse_fields(&buf[..size]).unwrap();
        assert_eq!(find_varint(&outer, 1), Some(SPEECH_TYPE_DOWNWARD));

        mipns
            .send_to(&prepare_wakeup(), &public_path)
            .await
            .unwrap();
        assert!(matches!(proxy.recv().await, Some(SpeechProxyEvent::Wake)));
        timeout(Duration::from_secs(1), native.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();

        proxy.handle().mute_native().await.unwrap();
        let (size, _) = timeout(Duration::from_secs(1), native.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        let event = decode_speech_upward(&buf[..size]).unwrap().unwrap();
        assert_eq!(event.up_type, UP_STREAM_CANCEL);

        mipns
            .send_to(&transmitting_packet(&[1, 2, 3, 4]), &public_path)
            .await
            .unwrap();
        let (size, _) = timeout(Duration::from_secs(1), native.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        let event = decode_speech_upward(&buf[..size]).unwrap().unwrap();
        assert_eq!(event.data, [0, 0, 0, 0]);

        drop(proxy);
        drop(native);
        drop(mipns);
        fs::remove_dir_all(temp).unwrap();
    }
}
