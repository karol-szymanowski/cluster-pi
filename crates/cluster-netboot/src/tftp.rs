use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

const TFTP_OP_RRQ: u16 = 1;
const TFTP_OP_DATA: u16 = 3;
const TFTP_OP_ACK: u16 = 4;
const TFTP_OP_ERROR: u16 = 5;
const TFTP_OP_OACK: u16 = 6;

const DEFAULT_BLOCK_SIZE: usize = 512;
const TFTP_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_RETRIES: usize = 5;

pub struct TftpServer {
    root_dir: PathBuf,
    listen_port: u16,
}

impl TftpServer {
    pub fn new(root_dir: impl AsRef<Path>, listen_port: u16) -> Self {
        Self {
            root_dir: root_dir.as_ref().to_path_buf(),
            listen_port,
        }
    }

    pub async fn run(
        self: Arc<Self>,
        cancel_token: CancellationToken,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let socket = UdpSocket::bind(("0.0.0.0", self.listen_port)).await?;
        tracing::info!(
            port = self.listen_port,
            root = %self.root_dir.display(),
            "TFTP server listening"
        );

        let mut buf = vec![0u8; 1024];

        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    tracing::info!("TFTP listener shutting down");
                    break;
                }
                recv_res = socket.recv_from(&mut buf) => {
                    match recv_res {
                        Ok((len, peer)) => {
                            let packet = buf[..len].to_vec();
                            let server_clone = self.clone();
                            tokio::spawn(async move {
                                if let Err(e) = server_clone.handle_initial_packet(packet, peer).await {
                                    tracing::warn!(peer = %peer, error = %e, "TFTP transfer session failed");
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("TFTP recv error: {}", e);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn handle_initial_packet(
        &self,
        packet: Vec<u8>,
        peer: SocketAddr,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if packet.len() < 4 {
            return Ok(());
        }

        let opcode = u16::from_be_bytes([packet[0], packet[1]]);
        if opcode != TFTP_OP_RRQ {
            return Ok(());
        }

        // Parse RRQ filename and options
        let (filename, mode, options) = parse_rrq(&packet[2..])?;
        tracing::debug!(
            peer = %peer,
            file = %filename,
            mode = %mode,
            "Received TFTP RRQ"
        );

        // Sanitize and resolve file path
        let sanitized = filename.trim_start_matches('/');
        let target_path = self.root_dir.join(sanitized);

        // Spawn a dedicated ephemeral UDP socket for this client session to avoid HOL blocking
        let session_socket = UdpSocket::bind("0.0.0.0:0").await?;

        if !target_path.is_file() {
            tracing::warn!(file = %target_path.display(), "TFTP requested file not found");
            let err_pkt = build_error_packet(1, "File not found");
            let _ = session_socket.send_to(&err_pkt, peer).await;
            return Ok(());
        }

        let block_size = options
            .get("blksize")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(DEFAULT_BLOCK_SIZE)
            .min(1428); // MTU safe

        let mut file = File::open(&target_path)?;
        let file_len = file.metadata()?.len();

        let mut block_num: u16 = 1;

        if !options.is_empty() {
            // Send OACK
            let mut oack_opts = HashMap::new();
            if options.contains_key("blksize") {
                oack_opts.insert("blksize", block_size.to_string());
            }
            if options.contains_key("tsize") {
                oack_opts.insert("tsize", file_len.to_string());
            }

            let oack_pkt = build_oack_packet(&oack_opts);
            session_socket.send_to(&oack_pkt, peer).await?;

            // Wait for ACK 0
            if !wait_for_ack(&session_socket, peer, 0).await? {
                return Ok(());
            }
        }

        // Stream DATA blocks
        let mut read_buf = vec![0u8; block_size];
        loop {
            let n = file.read(&mut read_buf)?;
            let data_pkt = build_data_packet(block_num, &read_buf[..n]);

            let mut retries = 0;
            let mut acked = false;

            while retries < MAX_RETRIES {
                session_socket.send_to(&data_pkt, peer).await?;
                if wait_for_ack(&session_socket, peer, block_num).await? {
                    acked = true;
                    break;
                }
                retries += 1;
            }

            if !acked {
                tracing::warn!(peer = %peer, block = block_num, "TFTP transfer timed out waiting for ACK");
                break;
            }

            block_num = block_num.wrapping_add(1);

            if n < block_size {
                // Final packet sent and acked
                break;
            }
        }

        tracing::debug!(file = %filename, peer = %peer, "TFTP file transfer complete");
        Ok(())
    }
}

async fn wait_for_ack(
    socket: &UdpSocket,
    peer: SocketAddr,
    expected_block: u16,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let mut buf = [0u8; 64];
    let start = tokio::time::Instant::now();

    while start.elapsed() < TFTP_TIMEOUT {
        let timeout_left = TFTP_TIMEOUT.saturating_sub(start.elapsed());
        if let Ok(Ok((len, from))) =
            tokio::time::timeout(timeout_left, socket.recv_from(&mut buf)).await
        {
            if from == peer && len >= 4 {
                let opcode = u16::from_be_bytes([buf[0], buf[1]]);
                let block = u16::from_be_bytes([buf[2], buf[3]]);
                if opcode == TFTP_OP_ACK && block == expected_block {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

type RrqParsed =
    Result<(String, String, HashMap<String, String>), Box<dyn std::error::Error + Send + Sync>>;

fn parse_rrq(payload: &[u8]) -> RrqParsed {
    let mut parts = Vec::new();
    let mut current = Vec::new();

    for &b in payload {
        if b == 0 {
            parts.push(String::from_utf8_lossy(&current).to_string());
            current.clear();
        } else {
            current.push(b);
        }
    }

    if parts.len() < 2 {
        return Err("Malformed RRQ payload".into());
    }

    let filename = parts[0].clone();
    let mode = parts[1].to_lowercase();
    let mut options = HashMap::new();

    let mut i = 2;
    while i + 1 < parts.len() {
        options.insert(parts[i].to_lowercase(), parts[i + 1].clone());
        i += 2;
    }

    Ok((filename, mode, options))
}

fn build_data_packet(block: u16, data: &[u8]) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(4 + data.len());
    pkt.extend_from_slice(&TFTP_OP_DATA.to_be_bytes());
    pkt.extend_from_slice(&block.to_be_bytes());
    pkt.extend_from_slice(data);
    pkt
}

fn build_error_packet(code: u16, msg: &str) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(5 + msg.len());
    pkt.extend_from_slice(&TFTP_OP_ERROR.to_be_bytes());
    pkt.extend_from_slice(&code.to_be_bytes());
    pkt.extend_from_slice(msg.as_bytes());
    pkt.push(0);
    pkt
}

fn build_oack_packet(options: &HashMap<&str, String>) -> Vec<u8> {
    let mut pkt = Vec::new();
    pkt.extend_from_slice(&TFTP_OP_OACK.to_be_bytes());
    for (k, v) in options {
        pkt.extend_from_slice(k.as_bytes());
        pkt.push(0);
        pkt.extend_from_slice(v.as_bytes());
        pkt.push(0);
    }
    pkt
}
