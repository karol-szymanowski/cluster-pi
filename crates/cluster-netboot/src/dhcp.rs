use cluster_common::crd::PiNode;
use dhcproto::v4::{Decodable, Encodable, Message, Opcode, OptionCode};
use dhcproto::{Decoder, Encoder};
use kube::api::ListParams;
use kube::{Api, Client};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

/// RFC 4578 PXE ProxyDHCP Server
pub struct ProxyDhcpServer {
    vip: Ipv4Addr,
    listen_port: u16,
    kube_client: Option<Client>,
    namespace: String,
    boot_file: String,
}

impl ProxyDhcpServer {
    pub fn new(
        vip: Ipv4Addr,
        listen_port: u16,
        kube_client: Option<Client>,
        namespace: impl Into<String>,
        boot_file: impl Into<String>,
    ) -> Self {
        Self {
            vip,
            listen_port,
            kube_client,
            namespace: namespace.into(),
            boot_file: boot_file.into(),
        }
    }

    /// Performs passive network sniffing for existing DHCPOFFER traffic on the local segment.
    pub async fn passive_sniff_self_check(interface_addr: Ipv4Addr, sniff_duration: Duration) {
        tracing::info!(
            duration = ?sniff_duration,
            "Starting passive DHCP self-check to detect existing DHCP servers"
        );

        let bind_addr = SocketAddrV4::new(interface_addr, 68);
        if let Ok(socket) = UdpSocket::bind(bind_addr).await {
            let mut buf = vec![0u8; 1500];
            let start = tokio::time::Instant::now();

            while start.elapsed() < sniff_duration {
                let timeout_left = sniff_duration.saturating_sub(start.elapsed());
                if let Ok(Ok((len, peer))) =
                    tokio::time::timeout(timeout_left, socket.recv_from(&mut buf)).await
                {
                    let mut decoder = Decoder::new(&buf[..len]);
                    if let Ok(msg) = Message::decode(&mut decoder) {
                        if msg.opcode() == Opcode::BootReply {
                            tracing::info!(
                                server_ip = %peer.ip(),
                                "Passive check detected active DHCP server offer on network"
                            );
                        }
                    }
                }
            }
        } else {
            tracing::debug!("Could not bind port 68 for passive sniffing (port in use or permissions restricted); proceeding");
        }
    }

    /// Runs the ProxyDHCP UDP listener.
    pub async fn run(
        self: Arc<Self>,
        cancel_token: CancellationToken,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, self.listen_port);
        let socket = UdpSocket::bind(bind_addr).await?;
        socket.set_broadcast(true)?;

        tracing::info!(
            port = self.listen_port,
            vip = %self.vip,
            "ProxyDHCP listener bound and active"
        );

        let mut buf = vec![0u8; 2048];

        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    tracing::info!("ProxyDHCP listener shutting down on cancellation");
                    break;
                }
                recv_res = socket.recv_from(&mut buf) => {
                    match recv_res {
                        Ok((len, _peer)) => {
                            let mut decoder = Decoder::new(&buf[..len]);
                            if let Ok(request) = Message::decode(&mut decoder) {
                                if let Some(reply) = self.process_packet(&request).await {
                                    let mut out_bytes = Vec::with_capacity(1500);
                                    let mut encoder = Encoder::new(&mut out_bytes);
                                    if reply.encode(&mut encoder).is_ok() {
                                        let dest = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, 68));
                                        let _ = socket.send_to(&out_bytes, dest).await;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!("Error receiving UDP packet: {}", e);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Evaluates incoming DHCP packet and creates strict RFC 4578 ProxyDHCP response if applicable.
    pub async fn process_packet(&self, request: &Message) -> Option<Message> {
        // Must be a BOOTREQUEST (client discovery/request)
        if request.opcode() != Opcode::BootRequest {
            return None;
        }

        // Must carry Option 60 with "PXEClient"
        let mut is_pxe = false;
        if let Some(dhcproto::v4::DhcpOption::ClassIdentifier(ref s)) =
            request.opts().get(OptionCode::ClassIdentifier)
        {
            if s.windows(b"PXEClient".len()).any(|w| w == b"PXEClient") {
                is_pxe = true;
            }
        }

        if !is_pxe {
            tracing::trace!("Ignoring non-PXEClient DHCP request");
            return None;
        }

        // Format MAC address
        let chaddr = request.chaddr();
        let mac_str = if chaddr.len() >= 6 {
            format!(
                "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                chaddr[0], chaddr[1], chaddr[2], chaddr[3], chaddr[4], chaddr[5]
            )
        } else {
            "00:00:00:00:00:00".to_string()
        };

        tracing::info!(
            mac = %mac_str,
            xid = request.xid(),
            "Received PXEClient discovery from node"
        );

        // Resolve boot file target (look up PiNode CR if k8s client is present)
        let target_file = self.resolve_boot_file(&mac_str).await;

        let mut reply = Message::default();
        reply.set_opcode(Opcode::BootReply);
        reply.set_htype(request.htype());
        reply.set_hops(0);
        reply.set_xid(request.xid());
        reply.set_secs(0);
        reply.set_flags(request.flags());
        reply.set_ciaddr(request.ciaddr());

        // CRITICAL INVARIANCE: yiaddr MUST BE 0.0.0.0 (ProxyDHCP does not lease IPs)
        reply.set_yiaddr(Ipv4Addr::UNSPECIFIED);

        // Next server address = our VIP
        reply.set_siaddr(self.vip);
        reply.set_giaddr(request.giaddr());
        reply.set_chaddr(request.chaddr());
        reply.set_fname_str(&target_file);

        // Options
        // 1. DHCP Message Type = DHCPOFFER (2)
        reply
            .opts_mut()
            .insert(dhcproto::v4::DhcpOption::MessageType(
                dhcproto::v4::MessageType::Offer,
            ));

        // 2. Server Identifier = VIP
        reply
            .opts_mut()
            .insert(dhcproto::v4::DhcpOption::ServerIdentifier(self.vip));

        // 3. Option 60 = "PXEClient"
        reply
            .opts_mut()
            .insert(dhcproto::v4::DhcpOption::ClassIdentifier(
                b"PXEClient".to_vec(),
            ));

        // 4. Option 43 = Vendor Specific (PXE discovery control)
        let pxe_opt43 = vec![6, 1, 8, 255];
        reply
            .opts_mut()
            .insert(dhcproto::v4::DhcpOption::VendorExtensions(pxe_opt43));

        Some(reply)
    }

    async fn resolve_boot_file(&self, mac: &str) -> String {
        if let Some(ref client) = self.kube_client {
            let api: Api<PiNode> = Api::namespaced(client.clone(), &self.namespace);
            if let Ok(list) = api.list(&ListParams::default()).await {
                for node in list.items {
                    if node.spec.mac_address.eq_ignore_ascii_case(mac) {
                        tracing::debug!(
                            mac = %mac,
                            serial = %node.spec.hardware_serial,
                            "Found registered PiNode for netboot"
                        );
                        return format!("{}/bootcode.bin", node.spec.hardware_serial);
                    }
                }
            }
        }

        self.boot_file.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_proxydhcp_ignores_non_pxe() {
        let server = ProxyDhcpServer::new(
            Ipv4Addr::new(192, 168, 1, 200),
            67,
            None,
            "default",
            "bootcode.bin",
        );

        let mut req = Message::default();
        req.set_opcode(Opcode::BootRequest);
        req.opts_mut().insert(dhcproto::v4::DhcpOption::MessageType(
            dhcproto::v4::MessageType::Discover,
        ));

        // No Option 60
        let reply = server.process_packet(&req).await;
        assert!(reply.is_none());
    }

    #[tokio::test]
    async fn test_proxydhcp_reply_invariance() {
        let server = ProxyDhcpServer::new(
            Ipv4Addr::new(192, 168, 1, 200),
            67,
            None,
            "default",
            "bootcode.bin",
        );

        let mut req = Message::default();
        req.set_opcode(Opcode::BootRequest);
        req.set_xid(0x12345678);
        req.set_chaddr(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
        req.opts_mut().insert(dhcproto::v4::DhcpOption::MessageType(
            dhcproto::v4::MessageType::Discover,
        ));
        req.opts_mut()
            .insert(dhcproto::v4::DhcpOption::ClassIdentifier(
                b"PXEClient".to_vec(),
            ));

        let reply = server
            .process_packet(&req)
            .await
            .expect("Must produce reply");

        // yiaddr must be 0.0.0.0
        assert_eq!(reply.yiaddr(), Ipv4Addr::UNSPECIFIED);
        // siaddr must be VIP
        assert_eq!(reply.siaddr(), Ipv4Addr::new(192, 168, 1, 200));
        assert_eq!(reply.xid(), 0x12345678);
        assert_eq!(reply.fname_str().unwrap().unwrap(), "bootcode.bin");

        // Verify Option 60 echoed
        let opt60 = reply.opts().get(OptionCode::ClassIdentifier).unwrap();
        match opt60 {
            dhcproto::v4::DhcpOption::ClassIdentifier(s) => assert_eq!(s.as_slice(), b"PXEClient"),
            _ => panic!("Expected ClassIdentifier option"),
        }
    }
}
