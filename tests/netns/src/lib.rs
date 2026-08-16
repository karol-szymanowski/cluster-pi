#[cfg(test)]
mod tests {
    use cluster_netboot::ProxyDhcpServer;
    use dhcproto::v4::{Message, Opcode, OptionCode};
    use std::net::Ipv4Addr;

    #[tokio::test]
    async fn test_proxydhcp_rfc4578_yiaddr_invariance() {
        let server = ProxyDhcpServer::new(
            Ipv4Addr::new(192, 168, 1, 200),
            67,
            None,
            "default",
            "bootcode.bin",
        );

        let mut discover = Message::default();
        discover.set_opcode(Opcode::BootRequest);
        discover.set_xid(0xabcdef01);
        discover.set_chaddr(&[
            0xd8, 0x3a, 0xdd, 0x12, 0x34, 0x56, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]);
        discover
            .opts_mut()
            .insert(dhcproto::v4::DhcpOption::MessageType(
                dhcproto::v4::MessageType::Discover,
            ));
        discover
            .opts_mut()
            .insert(dhcproto::v4::DhcpOption::ClassIdentifier(
                b"PXEClient:Arch:00011:UNDI:003001".to_vec(),
            ));

        let reply = server
            .process_packet(&discover)
            .await
            .expect("Must produce ProxyDHCP offer");

        // 1. yiaddr MUST BE 0.0.0.0
        assert_eq!(reply.yiaddr(), Ipv4Addr::UNSPECIFIED);

        // 2. siaddr must be VIP
        assert_eq!(reply.siaddr(), Ipv4Addr::new(192, 168, 1, 200));

        // 3. Option 60 must equal PXEClient
        let opt60 = reply
            .opts()
            .get(OptionCode::ClassIdentifier)
            .expect("Must contain Option 60");
        match opt60 {
            dhcproto::v4::DhcpOption::ClassIdentifier(s) => assert_eq!(s.as_slice(), b"PXEClient"),
            _ => panic!("Expected ClassIdentifier option"),
        }

        // 4. Option 43 must be present
        assert!(reply.opts().get(OptionCode::VendorExtensions).is_some());
    }

    #[tokio::test]
    async fn test_proxydhcp_drops_standard_dhcp_requests() {
        let server = ProxyDhcpServer::new(
            Ipv4Addr::new(192, 168, 1, 200),
            67,
            None,
            "default",
            "bootcode.bin",
        );

        let mut discover = Message::default();
        discover.set_opcode(Opcode::BootRequest);
        discover.set_xid(0x99887766);
        discover
            .opts_mut()
            .insert(dhcproto::v4::DhcpOption::MessageType(
                dhcproto::v4::MessageType::Discover,
            ));
        // No Option 60 PXEClient

        let reply = server.process_packet(&discover).await;
        assert!(reply.is_none(), "Must ignore non-PXE DHCP requests");
    }
}
