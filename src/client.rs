use crate::protocol::frame_set::{Datagram, FrameNumberCache, RELIABLE, RELIABLE_ORDERED, UNRELIABLE};
use crate::protocol::game::bedrock_packet_ids::BedrockPacketType;
use crate::protocol::game::play_status::LoginStatus;
use crate::protocol::game::{client_to_server_handshake, disconnect, login, network_settings, play_status, req_network_settings, resource_pack_client_response, resource_packs_info, server_to_client_handshake};
use crate::protocol::game_packet::GamePacket;
use crate::protocol::packet_ids::{PacketType, MAGIC};
use crate::protocol::{acknowledge, conn_req, conn_req_accepted, connected_ping, connected_pong, frame_set, game_packet, incompatible_protocol, new_incoming_conn, open_conn_reply1, open_conn_reply2, open_conn_req1, open_conn_req2};
use crate::utils::address::InternetAddress;
use crate::utils::color_format::COLOR_WHITE;
use crate::utils::encryption::Encryption;
use crate::utils::{address, color_format, encryption};
use binary_utils::binary::Stream;
use chrono::Utc;
use minecraft_auth::bedrock;
use openssl::base64::decode_block;
use openssl::ec::EcKey;
use openssl::pkey::{PKey, Private};
use rand::Rng;
use serde_json::Value;
use std::collections::HashMap;
use std::io::Result;
use std::net::UdpSocket;

//conn_req update
// maybe encryption disabled on server?
// if there is a skipped packet, wait for it, if you don't wait and try to decrypt it, you will get an 'invalid checksum' error
// NACK ACK System handler errors
// fragment packet receiving - sending etc.
// gönderdiğimiz paketleri buna kaydetme: FrameCache { //sequencenumber => framecache eğer nack gelirse birdaha göndeririz

pub struct Client {
    socket: UdpSocket,
    target_address: &'static str,
    target_port: u16,
    client_guid: i64,
    chain: Vec<String>,
    ec_key: EcKey<Private>,
    game: GamePacket,
    frame_number_cache: FrameNumberCache,
    last_received_packets: HashMap<i32, Datagram>, // seq_number: Datagram
    last_received_fragment_packets: HashMap<u16, HashMap<u32, Vec<u8>>>, // split_id: index => buffer
    last_received_sequence_number: i32,
    last_handled_sequence_number: i32,
    debug: bool,
    compression_enabled: bool,
    encryption_enabled: bool
}

pub async fn create(target_address: &'static str, target_port: u16, client_version: &'static str, debug: bool) -> Option<Client> {
    let mut bedrock = bedrock::new(client_version.to_string(), false);
    if !bedrock.auth().await { return None; }
    let mut rng = rand::thread_rng();
    Option::from(Client{
        socket: UdpSocket::bind("0.0.0.0:0").expect("Socket Bind Error"),
        target_address,
        target_port,
        client_guid: rng.gen_range(10000..100000),
        chain: bedrock.get_chain_data(),
        ec_key: bedrock.get_ec_key()?,
        game: GamePacket{encryption: Encryption::fake_gcm(vec![23, 1, 5, 33, 7, 1, 24, 0, 12, 32, 2, 15, 23, 1, 5, 33, 7, 1, 24, 0, 12, 32, 2, 15, 23, 1, 5, 33, 7, 1, 24, 0]).unwrap()},
        frame_number_cache: frame_set::start_number_cache(),
        last_received_packets: HashMap::new(),
        last_received_fragment_packets: HashMap::new(),
        last_received_sequence_number: -1,
        last_handled_sequence_number: -1,
        debug,
        compression_enabled: false,
        encryption_enabled: false
    })
}

impl Client {
    pub fn connect(&mut self) -> Result<()> {
        if self.debug {
            println!("Local socket bound to: {}", self.socket.local_addr()?);
        }
        let address = format!("{}:{}", self.target_address, self.target_port);
        self.socket.connect(address)?;

        self.read_raknet_socket();

        Ok(())
    }

    fn read_raknet_socket(&mut self) {
        let req1: Vec<u8> = open_conn_req1::new(MAGIC, 11, 1492).encode();
        self.socket.send(&req1).expect("Packet could not be sent");

        let mut buffer = vec![0; 2048];

        let mut should_stop = false;

        loop {
            if should_stop {
                break;
            }
            match self.socket.recv_from(&mut buffer) {
                Ok((amt, _src)) => {
                    let mut stream = Stream::new(Vec::from(&buffer[..amt]), 0);

                    let packet_id = stream.get_byte();

                    let packet_type = PacketType::from_byte(packet_id);
                    match packet_type {
                        PacketType::OpenConnReply1 => {
                            let open_conn_reply1 = open_conn_reply1::decode(stream.get_buffer());
                            if self.debug {
                                println!("--- {}OpenConnReply1{} ---", color_format::COLOR_GOLD, COLOR_WHITE);
                                println!("Magic: {:?}", open_conn_reply1.magic);
                                let guid_format = format!("{:x}", open_conn_reply1.server_guid);
                                println!("Server GUID (Format DecToHex): {}", guid_format);
                                println!("Server Security: {}", open_conn_reply1.server_security);
                                println!("Cookie: {:?}", open_conn_reply1.cookie);
                                println!("MTU: {}", open_conn_reply1.mtu);
                            }
                            let req2 = open_conn_req2::new(MAGIC, address::new(4, self.target_address.to_string(), self.target_port), open_conn_reply1.cookie, false, open_conn_reply1.mtu, self.client_guid).encode();
                            self.socket.send(&req2).expect("OpenConnectionRequest2 Packet could not be sent");
                        },
                        PacketType::OpenConnReply2 => {
                            let open_conn_reply2 = open_conn_reply2::decode(stream.get_buffer());
                            if self.debug {
                                println!("--- {}OpenConnReply2{} ---", color_format::COLOR_GOLD, COLOR_WHITE);
                                println!("Magic: {:?}", open_conn_reply2.magic);
                                let guid_format = format!("{:x}", open_conn_reply2.server_guid);
                                println!("Server GUID (Format DecToHex): {}", guid_format);
                                println!("Client Address: {}:{}", open_conn_reply2.client_address.address, open_conn_reply2.client_address.port);
                                println!("MTU: {}", open_conn_reply2.mtu);
                                println!("Encryption Enabled: {}", open_conn_reply2.encryption_enabled);
                            }

                            let body = conn_req::new(self.client_guid, Utc::now().timestamp(), false).encode();

                            let frame = Datagram::create_frame(body, RELIABLE, &self.frame_number_cache, None);
                            let datagram = Datagram::create(vec![frame], &self.frame_number_cache).to_binary();
                            self.frame_number_cache.sequence_number += 1;
                            self.frame_number_cache.reliable_frame_index += 1;

                            self.socket.send(&datagram).expect("ConnectionRequest Packet could not be sent");
                        },
                        PacketType::IncompatibleProtocol => {
                            let incompatible_protocol = incompatible_protocol::decode(stream.get_buffer());
                            println!("{}Incompatible Protocol Version, Server Protocol Version: {}{}", color_format::COLOR_RED, incompatible_protocol.server_protocol, COLOR_WHITE);
                            should_stop = true;
                        },
                        PacketType::DisconnectionNotification => {
                            println!("{}Disconnection Notification Packet Received From Server.{}", color_format::COLOR_RED, COLOR_WHITE);
                            should_stop = true;
                        }
                        _ => { /*vec![]*/ }
                    };

                    if frame_set::is_datagram(packet_id) {
                        let datagram = Datagram::from_binary(stream.get_buffer());

                        let ack = acknowledge::create(PacketType::ACK, 1, true, Option::from(datagram.sequence_number.clone()), None, None);
                        self.socket.send(&ack.encode()).expect("ACK Send Error");

                        let seq = datagram.sequence_number;
                        self.last_received_packets.insert(seq, datagram);


                        if (self.last_received_sequence_number + 1) != seq {
                            for seq_num in (self.last_received_sequence_number+1)..seq {
                                let nack = acknowledge::create(PacketType::NACK, 1, true, Option::from(seq_num), None, None);
                                self.socket.send(&nack.encode()).expect("NACK Send Error");
                            }
                        }
                        if seq > self.last_received_sequence_number {
                            self.last_received_sequence_number = seq;
                        }

                        let mut sorted_sequence_numbers: Vec<i32> = self.last_received_packets
                            .keys()
                            .cloned()
                            .collect();
                        sorted_sequence_numbers.sort();


                        //fragment suspect
                        for sequence_number in sorted_sequence_numbers {
                            if sequence_number == self.last_handled_sequence_number + 1 {
                                if let Some(datagram) = self.last_received_packets.get(&sequence_number) {
                                    for frame in &datagram.frames {

                                        let mut real_body = frame.body.clone();

                                        if let Some(fragment) = &frame.fragment {
                                            self.last_received_fragment_packets.entry(fragment.compound_id).or_insert_with(HashMap::new).insert(fragment.index, frame.body.clone());
                                            if let Some(fragment_data) = self.last_received_fragment_packets.get(&fragment.compound_id) {
                                                if (fragment_data.len() as u32) == fragment.compound_size {

                                                    let mut keys: Vec<u32> = fragment_data.keys().cloned().collect();
                                                    keys.sort();

                                                    let mut result = Vec::new();
                                                    for key in keys {
                                                        if let Some(value) = fragment_data.get(&key) {
                                                            result.extend_from_slice(value);
                                                        }
                                                    }
                                                    real_body = result;
                                                } else { continue; }
                                            } else { continue; }
                                        }


                                        let mut stream = Stream::new(real_body, 0);
                                        let packet_id = stream.get_byte();
                                        let packet_type = PacketType::from_byte(packet_id);

                                        match packet_type {
                                            PacketType::NACK => {
                                                let nack = acknowledge::decode(stream.get_buffer());
                                                //if self.debug {
                                                    println!("--- {}NACK{} ---", color_format::COLOR_RED, COLOR_WHITE);
                                                    println!("Record Count: Record Type {}", if nack.record_count == 0 { "Range" } else { "Single" });
                                                    println!("Single Sequence Number: {}", nack.single_sequence_number);
                                                    println!("Sequence Number: {:?}", nack.sequence_number);
                                                    println!("Start Sequence Number: {:?}", nack.start_sequence_number);
                                                    println!("End Sequence Number: {:?}", nack.end_sequence_number);
                                                //}
                                            }
                                            PacketType::ConnectedPing => {
                                                let connected_ping = connected_ping::decode(stream.get_buffer());
                                                if self.debug {
                                                    println!("--- {}ConnectedPing{} ---", color_format::COLOR_GOLD, COLOR_WHITE);
                                                    println!("Ping Time: {:?}", connected_ping.ping_time);
                                                }
                                                let connected_pong = connected_pong::create(connected_ping.ping_time, Utc::now().timestamp()).encode();
                                                let frame = Datagram::create_frame(connected_pong, UNRELIABLE, &self.frame_number_cache, None);
                                                let datagram = Datagram::create(vec![frame], &self.frame_number_cache).to_binary();
                                                self.frame_number_cache.sequence_number += 1;
                                                self.socket.send(&datagram).expect("ConnectedPong Packet could not be sent");
                                            },
                                            PacketType::ConnectedPong => {
                                                let connected_pong = connected_pong::decode(stream.get_buffer());
                                                if self.debug {
                                                    println!("--- {}ConnectedPong{} ---", color_format::COLOR_GOLD, COLOR_WHITE);
                                                    println!("Ping Time: {:?}", connected_pong.ping_time);
                                                    println!("Pong Time: {:?}", connected_pong.pong_time);
                                                }
                                                /*let connected_ping = connected_ping::create(Utc::now().timestamp()).encode();
                                                let frame = Datagram::create_frame(connected_ping, UNRELIABLE, &self.frame_number_cache, None);
                                                let datagram = Datagram::create(vec![frame], &self.frame_number_cache).to_binary();
                                                self.frame_number_cache.sequence_number += 1;
                                                self.socket.send(&datagram).expect("ConnectedPing Packet could not be sent");*/
                                            },
                                            PacketType::ConnReqAccepted => {
                                                let conn_req_accepted = conn_req_accepted::decode(stream.get_buffer());
                                                if self.debug {
                                                    println!("--- {}ConnectionRequestAccepted{} ---", color_format::COLOR_GOLD, COLOR_WHITE);
                                                    println!("Client Address: {}:{}", conn_req_accepted.client_address.address, conn_req_accepted.client_address.port);
                                                    println!("System Index: {}", conn_req_accepted.system_index);
                                                    for index in 0..20 {
                                                        println!("System Address {}: {}:{}", index + 1, conn_req_accepted.system_addresses[index].address, conn_req_accepted.system_addresses[index].port);
                                                    }
                                                    println!("Ping Time: {}", conn_req_accepted.ping_time);
                                                    println!("Pong Time: {}", conn_req_accepted.ping_time);
                                                }

                                                // New Incoming Connection
                                                let addresses: [InternetAddress; 20] = core::array::from_fn(|_| address::new(4, "0.0.0.0".to_string(), 0));
                                                let new_incoming_conn = new_incoming_conn::new(address::new(4, self.target_address.to_string(), self.target_port), addresses, Utc::now().timestamp(), Utc::now().timestamp() + 1).encode();
                                                let frame = Datagram::create_frame(new_incoming_conn, RELIABLE_ORDERED, &self.frame_number_cache, None);
                                                self.frame_number_cache.reliable_frame_index += 1;
                                                self.frame_number_cache.ordered_frame_index += 1;

                                                // Connected Ping
                                                let connected_ping = connected_ping::create(Utc::now().timestamp()).encode();
                                                let frame_two = Datagram::create_frame(connected_ping, UNRELIABLE, &self.frame_number_cache, None);

                                                // Request Network Settings Packet
                                                let request_network_settings = req_network_settings::new(712).encode();
                                                let frame_three = Datagram::create_frame(request_network_settings, RELIABLE_ORDERED, &self.frame_number_cache, None);

                                                let datagram = Datagram::create(vec![frame, frame_two, frame_three], &self.frame_number_cache).to_binary();
                                                self.frame_number_cache.sequence_number += 1;
                                                self.frame_number_cache.reliable_frame_index += 1;
                                                self.frame_number_cache.ordered_frame_index += 1;

                                                self.socket.send(&datagram).expect("NewIncomingConnection & RequestNetworkSettings Packet could not be sent");
                                                //should_stop = true;
                                            },
                                            PacketType::Game => { // birden fazla bedrock paketi olabilir while döngüsü gerek gibi
                                                if !self.encryption_enabled {
                                                    if !self.compression_enabled { // Compression Not Enabled AND Encryption Not Enabled
                                                        let _length = stream.get_unsigned_var_int(); // unsignedvarint fonksiyonu belli ki yanlış
                                                        let packet_id = stream.get_unsigned_var_int();
                                                        let packet_type = BedrockPacketType::from_byte(packet_id as u16);

                                                        println!("--- {}{}{} ---", color_format::COLOR_GOLD, BedrockPacketType::get_packet_name(packet_id as u16), COLOR_WHITE);
                                                        match packet_type {
                                                            BedrockPacketType::NetworkSettings => {
                                                                let network_settings = network_settings::decode(stream.get_remaining().unwrap());
                                                                println!("Compression Threshold: {}", if network_settings.compression_threshold == 1 { "COMPRESS_EVERYTHING" } else { "COMPRESS_NOTHING" });
                                                                println!("Compression Algorithm: {}", if network_settings.compression_algorithm == 0 { "ZLIB" } else if network_settings.compression_algorithm == 1 { "SNAPPY" } else { "NONE" });
                                                                println!("Enable Client Throttling: {}", network_settings.enable_client_throttling);
                                                                println!("Client Throttle Threshold: {}", network_settings.client_throttle_threshold);
                                                                println!("Client Throttle Scalar: {}", network_settings.client_throttle_scalar);
                                                                self.compression_enabled = true;


                                                                // DATA SENT AS FRAGMENT
                                                                let pkey = PKey::from_ec_key(self.ec_key.clone()).expect("PKey Error");
                                                                let login_data_detail = login::convert_login_chain(&mut self.chain, pkey, self.target_address, self.target_port, self.client_guid);
                                                                let login = login::new(712, login_data_detail[0].clone(), login_data_detail[1].clone()).encode();

                                                                let datagrams = Datagram::split_packet(login, &mut self.frame_number_cache);

                                                                for datagram in datagrams {
                                                                    self.socket.send(&datagram.to_binary()).expect("Login Packet Fragment could not be sent");
                                                                }



                                                            },
                                                            BedrockPacketType::Disconnect => {
                                                                let disconnect = disconnect::decode(stream.get_remaining().unwrap());
                                                                println!("Disconnect (string): {}", String::from_utf8(stream.get_remaining().unwrap()).unwrap());
                                                                println!("Reason: {}", disconnect.reason);
                                                                if disconnect.skip_message {
                                                                    println!("Message: {}", disconnect.message.unwrap());
                                                                    println!("Filtered Message: {}", disconnect.filtered_message.unwrap());
                                                                }
                                                                should_stop = true;
                                                            }
                                                            _ => {}
                                                        }
                                                    } else { // Compression Enabled AND Encryption Not Enabled
                                                        let compression_type = stream.get_byte();

                                                        println!("Compression Type: {}", if compression_type == 0 { format!("{}ZLIB{}", color_format::COLOR_AQUA, COLOR_WHITE) } else if compression_type == 1 { format!("{}SNAPPY{}", color_format::COLOR_AQUA, COLOR_WHITE) } else { format!("{}NONE{}", color_format::COLOR_AQUA, COLOR_WHITE) });
                                                        let mut decompressed = stream.get_remaining().unwrap();
                                                        if compression_type == 0 {
                                                            decompressed = game_packet::decompress(&stream.get_remaining().unwrap());
                                                        }


                                                        let mut decompressed_stream = Stream::new(decompressed, 0);
                                                        let _length = decompressed_stream.get_unsigned_var_int();
                                                        let packet_id = decompressed_stream.get_unsigned_var_int();
                                                        let packet_type = BedrockPacketType::from_byte(packet_id as u16);

                                                        println!("--- {}{}{} ---", color_format::COLOR_GOLD, BedrockPacketType::get_packet_name(packet_id as u16), COLOR_WHITE);
                                                        match packet_type {
                                                            BedrockPacketType::ServerToClientHandshake => {
                                                                let s_to_c_handshake = server_to_client_handshake::decode(decompressed_stream.get_remaining().unwrap());
                                                                let jwt = String::from_utf8(s_to_c_handshake.jwt).unwrap();
                                                                println!("JWT: {:?}", jwt);
                                                                let jwt_split: Vec<&str> = jwt.split('.').collect();

                                                                let jwt_header = Encryption::b64_url_decode(jwt_split[0]).unwrap();
                                                                let jwt_header_value: Value = serde_json::from_str(jwt_header.as_str()).expect("JWT Header can not decoded.");

                                                                let jwt_payload = Encryption::b64_url_decode(jwt_split[1]).unwrap();
                                                                let jwt_payload_value: Value = serde_json::from_str(jwt_payload.as_str()).expect("JWT Payload can not decoded.");

                                                                let x5u = jwt_header_value.get("x5u").and_then(Value::as_str).unwrap().to_string();
                                                                let server_private = encryption::parse_der_public_key(decode_block(x5u.as_str()).unwrap().as_slice());
                                                                let salt = decode_block(jwt_payload_value.get("salt").and_then(Value::as_str).unwrap()).unwrap();

                                                                let local_pkey = PKey::from_ec_key(self.ec_key.clone()).expect("Local PKey Error");
                                                                let shared_secret = encryption::generate_shared_secret(local_pkey, server_private);
                                                                let encryption_key = encryption::generate_key(&shared_secret, salt);
                                                                let encryption = Encryption::fake_gcm(encryption_key).expect("Encryption Fake GCM Error");

                                                                self.game = game_packet::new(encryption);
                                                                self.encryption_enabled = true;

                                                                // DATA SENT AS FRAGMENT
                                                                let c_to_s_handshake = client_to_server_handshake::new().encode();

                                                                let game_packet = self.game.encrypt(&c_to_s_handshake);

                                                                let datagrams = Datagram::split_packet(game_packet, &mut self.frame_number_cache);

                                                                for datagram in datagrams {
                                                                    self.socket.send(&datagram.to_binary()).expect("ClientToServerHandshake Packet Fragment could not be sent");
                                                                }

                                                            },
                                                            _ => {}
                                                        }
                                                    }
                                                } else { // Encryption Enabled
                                                    let mut decrypted_stream = Stream::new(self.game.decrypt(&stream.get_remaining().unwrap()), 0);
                                                    let compression_type = decrypted_stream.get_byte();
                                                    println!("Compression Type: {}", if compression_type == 0 { format!("{}ZLIB{}", color_format::COLOR_LIGHT_PURPLE, COLOR_WHITE) } else if compression_type == 1 { format!("{}SNAPPY{}", color_format::COLOR_LIGHT_PURPLE, COLOR_WHITE) } else { format!("{}NONE{}", color_format::COLOR_LIGHT_PURPLE, COLOR_WHITE) });
                                                    let mut decompressed = decrypted_stream.get_remaining().unwrap();
                                                    if compression_type == 0 {
                                                        decompressed = game_packet::decompress(&decrypted_stream.get_remaining().unwrap());
                                                    }

                                                    let mut decompressed_stream = Stream::new(decompressed, 0);
                                                    while !decompressed_stream.feof() {
                                                        let length = decompressed_stream.get_unsigned_var_int();

                                                        let packet = decompressed_stream.get(length).unwrap();
                                                        let mut packet_stream = Stream::new(packet, 0);

                                                        let packet_id = packet_stream.get_unsigned_var_int();
                                                        let packet_type = BedrockPacketType::from_byte(packet_id as u16);

                                                        println!("--- {}{}{} ---", color_format::COLOR_GOLD, BedrockPacketType::get_packet_name(packet_id as u16), COLOR_WHITE);
                                                        match packet_type {
                                                            BedrockPacketType::ResourcePacksInfo => {
                                                                let resource_packs_info = resource_packs_info::decode(packet_stream.get_remaining().unwrap());
                                                                let mut rp_uuids = Vec::new();
                                                                println!("Must Accept: {}", resource_packs_info.must_accept);
                                                                println!("Has Addons: {}", resource_packs_info.has_addons);
                                                                println!("Has Scripts: {}", resource_packs_info.has_scripts);
                                                                println!("Force Server Packs: {}", resource_packs_info.force_server_packs);
                                                                let behaviour_pack_count = resource_packs_info.behaviour_packs.len();
                                                                println!("Behaviour Pack Count: {}", behaviour_pack_count);
                                                                for (i, behaviour_pack) in resource_packs_info.behaviour_packs.iter().enumerate() {
                                                                    println!("- Behaviour Pack {} -", i + 1);
                                                                    println!(" - UUID: {}", behaviour_pack.uuid);
                                                                    println!(" - Version: {}", behaviour_pack.version);
                                                                    println!(" - Size Bytes: {}", behaviour_pack.size_bytes);
                                                                    println!(" - Encryption Key: {}", behaviour_pack.encryption_key);
                                                                    println!(" - Sub Pack Name: {}", behaviour_pack.sub_pack_name);
                                                                    println!(" - Content ID: {}", behaviour_pack.content_id);
                                                                    println!(" - Has Scripts: {}", behaviour_pack.has_scripts);
                                                                    println!(" - Is Addon Pack: {}", behaviour_pack.is_addon_pack);
                                                                    println!("--------------------");
                                                                }
                                                                let resource_pack_count = resource_packs_info.resource_packs.len();
                                                                println!("Resource Pack Count: {}", resource_pack_count);
                                                                for (i, resource_pack) in resource_packs_info.resource_packs.iter().enumerate() {
                                                                    rp_uuids.push(resource_pack.uuid.clone());
                                                                    println!("- Resource Pack {} -", i + 1);
                                                                    println!(" - UUID: {}", resource_pack.uuid);
                                                                    println!(" - Version: {}", resource_pack.version);
                                                                    println!(" - Size Bytes: {}", resource_pack.size_bytes);
                                                                    println!(" - Encryption Key: {}", resource_pack.encryption_key);
                                                                    println!(" - Sub Pack Name: {}", resource_pack.sub_pack_name);
                                                                    println!(" - Content ID: {}", resource_pack.content_id);
                                                                    println!(" - Has Scripts: {}", resource_pack.has_scripts);
                                                                    println!(" - Is Addon Pack: {}", resource_pack.is_addon_pack);
                                                                    println!(" - Is RTX Capable: {}", resource_pack.is_rtx_capable);
                                                                    println!("-------------------");
                                                                }
                                                                let cdn_url_count = resource_packs_info.cdn_urls.len();
                                                                println!("CDN URL Count: {}", cdn_url_count);
                                                                for (i, cdn_url) in resource_packs_info.cdn_urls.iter().enumerate() {
                                                                    println!("- CDN URL {} -", i);
                                                                    println!(" - Pack ID: {}", cdn_url.pack_id);
                                                                    println!(" - CDN URL: {}", cdn_url.cdn_url);
                                                                    println!("-------------");
                                                                }

                                                                // RESOURCE PACK CLIENT RESPONSE PACKET {COMPLETED}
                                                                let rp_client_response = resource_pack_client_response::new(resource_pack_client_response::COMPLETED, rp_uuids).encode();

                                                                let game_packet = self.game.encrypt(&rp_client_response);

                                                                let datagrams = Datagram::split_packet(game_packet, &mut self.frame_number_cache);

                                                                for datagram in datagrams {
                                                                    self.socket.send(&datagram.to_binary()).expect("ResourcePackClientResponse Packet Fragment could not be sent");
                                                                }

                                                            },
                                                            BedrockPacketType::ResourcePackStack => {
                                                            },
                                                            BedrockPacketType::StartGame => {
                                                                println!("--- {}Start Game{} ---", color_format::COLOR_GOLD, COLOR_WHITE);
                                                            },
                                                            BedrockPacketType::CreativeContent => {
                                                            },
                                                            BedrockPacketType::BiomeDefinitionList => {
                                                            },
                                                            BedrockPacketType::LevelChunk => {
                                                            },
                                                            BedrockPacketType::PlayStatus => {
                                                                let play_status = play_status::decode(packet_stream.get_remaining().unwrap());
                                                                let status = LoginStatus::try_from(play_status.status).unwrap();
                                                                match status {
                                                                    LoginStatus::LoginSuccess => println!("Status: {}Login Success{}", color_format::COLOR_GREEN, COLOR_WHITE),
                                                                    LoginStatus::LoginFailedClient => println!("Status: {}Login Failed Client{}", color_format::COLOR_RED, COLOR_WHITE),
                                                                    LoginStatus::LoginFailedServer => println!("Status: {}Login Failed Server{}", color_format::COLOR_RED, COLOR_WHITE),
                                                                    LoginStatus::PlayerSpawn => println!("Status: {}Player Spawn{}", color_format::COLOR_GREEN, COLOR_WHITE),
                                                                    LoginStatus::LoginFailedInvalidTenant => println!("Status: {}Login Failed Invalid Tenant{}", color_format::COLOR_RED, COLOR_WHITE),
                                                                    LoginStatus::LoginFailedVanillaEdu => println!("Status: {}Login Failed Vanilla Edu{}", color_format::COLOR_RED, COLOR_WHITE),
                                                                    LoginStatus::LoginFailedEduVanilla => println!("Status: {}Login Failed Edu Vanilla{}", color_format::COLOR_RED, COLOR_WHITE),
                                                                    LoginStatus::LoginFailedServerFull => println!("Status: {}Login Failed Server Full{}", color_format::COLOR_RED, COLOR_WHITE),
                                                                    LoginStatus::LoginFailedEditorVanilla => println!("Status: {}Login Failed Editor Vanilla{}", color_format::COLOR_RED, COLOR_WHITE),
                                                                    LoginStatus::LoginFailedVanillaEditor => println!("Status: {}Login Failed Vanilla Editor{}", color_format::COLOR_RED, COLOR_WHITE),
                                                                }
                                                            }
                                                            _ => {}
                                                        }
                                                    }
                                                }
                                            },
                                            PacketType::DisconnectionNotification => {
                                                println!("{}Disconnect Notification Packet Received{}", color_format::COLOR_RED, COLOR_WHITE);
                                            }
                                            _ => {}
                                        }
                                    }
                                    self.last_handled_sequence_number = sequence_number;
                                    self.last_received_packets.remove(&sequence_number);
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Error receiving data: {}", e);
                }
            }
        }
    }
}