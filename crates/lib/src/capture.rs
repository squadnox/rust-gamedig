use std::io::Write;
use std::net::{IpAddr, SocketAddr};

use crate::GDResult;

use pcap_file::pcapng::{blocks::enhanced_packet::EnhancedPacketOption, PcapNgBlock};
use pnet_packet::{
    ethernet::{EtherType, MutableEthernetPacket},
    ip::{IpNextHeaderProtocol, IpNextHeaderProtocols},
    ipv4::MutableIpv4Packet,
    ipv6::MutableIpv6Packet,
    tcp::{MutableTcpPacket, TcpFlags},
    udp::MutableUdpPacket,
    PacketSize,
};

/// Info about a packet we have sent or recieved.
#[derive(Clone, Debug, PartialEq)]
pub struct PacketInfo<'a> {
    pub direction: PacketDirection,
    pub protocol: PacketProtocol,
    pub remote_address: &'a SocketAddr,
    pub local_address: &'a SocketAddr,
}

/// The direction of a packet.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PacketDirection {
    /// The packet is coming from us, destined for a server.
    Send,
    /// A server has sent this packet to us.
    Receive,
}

/// The protocol of a packet.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PacketProtocol {
    TCP,
    UDP,
}

/// Trait for objects that can write packet captures.
pub trait CaptureWriter {
    fn write(&mut self, packet: &PacketInfo, data: &[u8]) -> crate::GDResult<()>;
    fn new_connect(&mut self, packet: &PacketInfo) -> crate::GDResult<()>;
    // TODO: Tcp FIN when socket ends
}

// Packet size constants
const PACKET_SIZE: usize = 5012;
const HEADER_SIZE_ETHERNET: usize = 14;
const HEADER_SIZE_IP4: usize = 20;
const HEADER_SIZE_IP6: usize = 40;
const HEADER_SIZE_UDP: usize = 4;

/// A writer that does nothing
struct NullWriter;
impl CaptureWriter for NullWriter {
    fn write(&mut self, _: &PacketInfo, _: &[u8]) -> GDResult<()> { Ok(()) }
    fn new_connect(&mut self, _: &PacketInfo) -> GDResult<()> { Ok(()) }
}

/// Writer that writes to pcap file
struct PcapWriter<W: Write> {
    writer: pcap_file::pcapng::PcapNgWriter<W>,
    start_time: std::time::Instant,
    send_seq: u32,
    rec_seq: u32,
    has_sent_handshake: bool,
    stream_count: u32,
}
impl<W: Write> PcapWriter<W> {
    fn new(writer: pcap_file::pcapng::PcapNgWriter<W>) -> Self {
        Self {
            writer,
            start_time: std::time::Instant::now(),
            send_seq: 0,
            rec_seq: 0,
            has_sent_handshake: false,
            stream_count: 0,
        }
    }
}

impl<W: Write> CaptureWriter for PcapWriter<W> {
    fn write(&mut self, info: &PacketInfo, data: &[u8]) -> GDResult<()> {
        self.write_transport_packet(info, data);

        Ok(())
    }

    fn new_connect(&mut self, packet: &PacketInfo) -> GDResult<()> {
        match packet.protocol {
            PacketProtocol::TCP => {
                self.write_tcp_handshake(packet);
            }
            PacketProtocol::UDP => {}
        }

        self.stream_count = self.stream_count.wrapping_add(1);

        Ok(())
    }
}

impl<W: Write> PcapWriter<W> {
    /// Encode the transport layer packet with a payload and write it.
    fn write_transport_packet(&mut self, info: &PacketInfo, payload: &[u8]) {
        let mut buf = vec![0; PACKET_SIZE - usize::max(HEADER_SIZE_IP4, HEADER_SIZE_IP6) - HEADER_SIZE_ETHERNET];

        let (source_port, dest_port) = match info.direction {
            PacketDirection::Send => (info.local_address.port(), info.remote_address.port()),
            PacketDirection::Receive => (info.remote_address.port(), info.local_address.port()),
        };

        match info.protocol {
            PacketProtocol::TCP => {
                let buf_size = {
                    let mut tcp = MutableTcpPacket::new(&mut buf).unwrap();
                    tcp.set_source(source_port);
                    tcp.set_destination(dest_port);
                    tcp.set_payload(payload);
                    tcp.set_data_offset(5);
                    tcp.set_window(43440);
                    match info.direction {
                        PacketDirection::Send => {
                            tcp.set_sequence(self.send_seq);
                            tcp.set_acknowledgement(self.rec_seq);

                            self.send_seq = self.send_seq.wrapping_add(payload.len() as u32);
                        }
                        PacketDirection::Receive => {
                            tcp.set_sequence(self.rec_seq);
                            tcp.set_acknowledgement(self.send_seq);

                            self.rec_seq = self.rec_seq.wrapping_add(payload.len() as u32);
                        }
                    }
                    tcp.set_flags(TcpFlags::PSH | TcpFlags::ACK);

                    tcp.packet_size()
                };

                self.write_transport_payload(
                    info,
                    IpNextHeaderProtocols::Tcp,
                    &buf[.. buf_size + payload.len()],
                    vec![],
                );

                let mut info = info.clone();
                let buf_size = {
                    let mut tcp = MutableTcpPacket::new(&mut buf).unwrap();
                    tcp.set_source(dest_port);
                    tcp.set_destination(source_port);
                    tcp.set_data_offset(5);
                    tcp.set_window(43440);
                    match &info.direction {
                        PacketDirection::Send => {
                            tcp.set_sequence(self.rec_seq);
                            tcp.set_acknowledgement(self.send_seq);

                            info.direction = PacketDirection::Receive;
                        }
                        PacketDirection::Receive => {
                            tcp.set_sequence(self.send_seq);
                            tcp.set_acknowledgement(self.rec_seq);

                            info.direction = PacketDirection::Send;
                        }
                    }
                    tcp.set_flags(TcpFlags::ACK);

                    tcp.packet_size()
                };

                self.write_transport_payload(
                    &info,
                    IpNextHeaderProtocols::Tcp,
                    &buf[.. buf_size],
                    vec![EnhancedPacketOption::Comment("Generated TCP ack".into())],
                );
            }
            PacketProtocol::UDP => {
                let buf_size = {
                    let mut udp = MutableUdpPacket::new(&mut buf).unwrap();
                    udp.set_source(source_port);
                    udp.set_destination(dest_port);
                    udp.set_length((payload.len() + HEADER_SIZE_UDP) as u16);
                    udp.set_payload(payload);

                    udp.packet_size()
                };

                self.write_transport_payload(
                    info,
                    IpNextHeaderProtocols::Udp,
                    &buf[.. buf_size + payload.len()],
                    vec![],
                );
            }
        }
    }

    /// Encode a network layer (IP) packet with a payload.
    fn encode_ip_packet(
        &self,
        buf: &mut [u8],
        info: &PacketInfo,
        protocol: IpNextHeaderProtocol,
        payload: &[u8],
    ) -> (usize, EtherType) {
        match (info.local_address.ip(), info.remote_address.ip()) {
            (IpAddr::V4(local_address), IpAddr::V4(remote_address)) => {
                let (source, destination) = if info.direction == PacketDirection::Send {
                    (local_address, remote_address)
                } else {
                    (remote_address, local_address)
                };

                let header_size = HEADER_SIZE_IP4 + (32 / 8);

                let mut ip = MutableIpv4Packet::new(buf).unwrap();
                ip.set_version(4);
                ip.set_total_length((payload.len() + header_size) as u16);
                ip.set_next_level_protocol(protocol);
                // https://en.wikipedia.org/wiki/Internet_Protocol_version_4#Total_Length

                ip.set_header_length((header_size / 4) as u8);
                ip.set_source(source);
                ip.set_destination(destination);
                ip.set_payload(payload);
                ip.set_ttl(64);
                ip.set_flags(pnet_packet::ipv4::Ipv4Flags::DontFragment);

                let mut options_writer =
                    pnet_packet::ipv4::MutableIpv4OptionPacket::new(ip.get_options_raw_mut()).unwrap();
                options_writer.set_copied(1);
                options_writer.set_class(0);
                options_writer.set_number(pnet_packet::ipv4::Ipv4OptionNumbers::SID);
                options_writer.set_length(&[4]);
                options_writer.set_data(&(self.stream_count as u16).to_be_bytes());

                ip.set_checksum(pnet_packet::ipv4::checksum(&ip.to_immutable()));

                (ip.packet_size(), pnet_packet::ethernet::EtherTypes::Ipv4)
            }
            (IpAddr::V6(local_address), IpAddr::V6(remote_address)) => {
                let (source, destination) = match info.direction {
                    PacketDirection::Send => (local_address, remote_address),
                    PacketDirection::Receive => (remote_address, local_address),
                };

                let mut ip = MutableIpv6Packet::new(buf).unwrap();
                ip.set_version(6);
                ip.set_payload_length(payload.len() as u16);
                ip.set_next_header(protocol);
                ip.set_source(source);
                ip.set_destination(destination);
                ip.set_hop_limit(64);
                ip.set_payload(payload);
                ip.set_flow_label(self.stream_count);

                (ip.packet_size(), pnet_packet::ethernet::EtherTypes::Ipv6)
            }
            _ => unreachable!(),
        }
    }

    /// Encode a physical layer (ethernet) packet with a payload.
    fn encode_ethernet_packet(
        &self,
        buf: &mut [u8],
        ethertype: pnet_packet::ethernet::EtherType,
        payload: &[u8],
    ) -> usize {
        let mut ethernet = MutableEthernetPacket::new(buf).unwrap();
        ethernet.set_ethertype(ethertype);
        ethernet.set_payload(payload);

        ethernet.packet_size()
    }

    /// Write a TCP handshake.
    fn write_tcp_handshake(&mut self, info: &PacketInfo) {
        let (source_port, dest_port) = (info.local_address.port(), info.remote_address.port());

        let mut info = info.clone();
        info.direction = PacketDirection::Send;
        let mut buf = vec![0; PACKET_SIZE];
        // Add a generated comment to all packets
        let options = vec![
            pcap_file::pcapng::blocks::enhanced_packet::EnhancedPacketOption::Comment("Generated TCP handshake".into()),
        ];

        // SYN
        let buf_size = {
            let mut tcp = MutableTcpPacket::new(&mut buf).unwrap();
            self.send_seq = 500;
            tcp.set_sequence(self.send_seq);
            tcp.set_flags(TcpFlags::SYN);
            tcp.set_source(source_port);
            tcp.set_destination(dest_port);
            tcp.set_window(43440);
            tcp.set_data_offset(5);

            tcp.packet_size()
        };
        self.write_transport_payload(
            &info,
            IpNextHeaderProtocols::Tcp,
            &buf[.. buf_size],
            options.clone(),
        );

        // SYN + ACK
        info.direction = PacketDirection::Receive;
        let buf_size = {
            let mut tcp = MutableTcpPacket::new(&mut buf).unwrap();
            self.send_seq = self.send_seq.wrapping_add(1);
            tcp.set_acknowledgement(self.send_seq);
            self.rec_seq = 1000;
            tcp.set_sequence(self.rec_seq);
            tcp.set_flags(TcpFlags::SYN | TcpFlags::ACK);
            tcp.set_source(dest_port);
            tcp.set_destination(source_port);
            tcp.set_window(43440);
            tcp.set_data_offset(5);

            tcp.packet_size()
        };
        self.write_transport_payload(
            &info,
            IpNextHeaderProtocols::Tcp,
            &buf[.. buf_size],
            options.clone(),
        );

        // ACK
        info.direction = PacketDirection::Send;
        let buf_size = {
            let mut tcp = MutableTcpPacket::new(&mut buf).unwrap();
            tcp.set_sequence(self.send_seq);
            self.rec_seq = self.rec_seq.wrapping_add(1);
            tcp.set_acknowledgement(self.rec_seq);
            tcp.set_flags(TcpFlags::ACK);
            tcp.set_source(source_port);
            tcp.set_destination(dest_port);
            tcp.set_window(43440);
            tcp.set_data_offset(5);

            tcp.packet_size()
        };
        self.write_transport_payload(
            &info,
            IpNextHeaderProtocols::Tcp,
            &buf[.. buf_size],
            options,
        );

        self.has_sent_handshake = true;
    }

    /// Take a transport layer packet as a buffer and write it after encoding
    /// all the layers under it.
    fn write_transport_payload(
        &mut self,
        info: &PacketInfo,
        protocol: IpNextHeaderProtocol,
        payload: &[u8],
        options: Vec<pcap_file::pcapng::blocks::enhanced_packet::EnhancedPacketOption>,
    ) {
        let mut network_packet = vec![0; PACKET_SIZE - HEADER_SIZE_ETHERNET];
        let (network_size, ethertype) = self.encode_ip_packet(&mut network_packet, info, protocol, payload);
        let network_size = network_size + payload.len();
        network_packet.truncate(network_size);

        let mut physical_packet = vec![0; PACKET_SIZE];
        let physical_size =
            self.encode_ethernet_packet(&mut physical_packet, ethertype, &network_packet) + network_size;

        physical_packet.truncate(physical_size);

        self.writer
            .write_block(
                &pcap_file::pcapng::blocks::enhanced_packet::EnhancedPacketBlock {
                    original_len: physical_size as u32,
                    data: physical_packet.into(),
                    interface_id: 0,
                    timestamp: self.start_time.elapsed(),
                    options,
                }
                .into_block(),
            )
            .unwrap();
    }
}

/// Setup the static capture into a file or to nowhere.
///
/// This leaks the writer.
///
/// # Panics
/// - If this is called more than once (OnceLock used internally).
///
/// # Safety
/// The safety of this function has not been evaluated yet, and
/// testing has only been done with limited CLI use cases.
pub unsafe fn simple_setup_capture(file_name: Option<String>) {
    let writer: Box<dyn CaptureWriter + Send + Sync> = if let Some(file_name) = file_name {
        let file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(file_name)
            .unwrap();
        let mut writer = pcap_file::pcapng::PcapNgWriter::new(file).unwrap();

        // Write headers
        writer
            .write_block(
                &pcap_file::pcapng::blocks::interface_description::InterfaceDescriptionBlock {
                    linktype: pcap_file::DataLink::ETHERNET,
                    snaplen: 0xFFFF,
                    options: vec![],
                }
                .into_block(),
            )
            .unwrap();

        let writer = PcapWriter::new(writer);
        Box::new(writer)
    } else {
        Box::new(NullWriter)
    };
    setup_capture(writer);
}

/// Set a capture writer to handle packet send/recieve data.
///
/// This leaks the writer.
///
/// # Panics
/// - If this is called more than once (OnceLock used internally).
///
/// # Safety
/// The safety of this function has not been evaluated yet, and
/// testing has only been done with limited CLI use cases.
pub unsafe fn setup_capture(writer: Box<dyn CaptureWriter + Send + Sync>) {
    // TODO: safety
    unsafe {
        crate::socket::capture::set_writer(writer);
    }
}
