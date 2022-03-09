// Copyright (C) 2019-2022 Aleo Systems Inc.
// This file is part of the snarkOS library.

// The snarkOS library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The snarkOS library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the snarkOS library. If not, see <https://www.gnu.org/licenses/>.

use crate::{constants::*, known_network::KnownNetwork};
use snarkos_environment::CurrentNetwork;
use snarkos_network::Data;
use snarkos_storage::BlockLocators;
use snarkos_synthetic_node::{ClientMessage, SynthNode, MESSAGE_LENGTH_PREFIX_SIZE, MESSAGE_VERSION};
use snarkvm::traits::Network;

use pea2pea::{
    protocols::{Disconnect, Handshake, Reading, Writing},
    Config,
    Node as Pea2PeaNode,
    Pea2Pea,
};
use rand::{rngs::SmallRng, seq::IteratorRandom, SeedableRng};
use std::{convert::TryInto, io, net::SocketAddr, ops::Deref, sync::Arc, time::Duration};
use structopt::StructOpt;
use tokio::task;
use tracing::*;

// CLI
// TODO: investigate using clap instead.
#[derive(Debug, StructOpt)]
pub struct Opts {
    /// Specify the IP address and port for the node server.
    /// Naming and defaults kept consistent with snarkOS.
    #[structopt(parse(try_from_str), default_value = "0.0.0.0:4132", long = "node")]
    pub node: SocketAddr,
}

#[derive(Clone)]
pub struct Crawler {
    synth_node: SynthNode,
    pub known_network: Arc<KnownNetwork>,
}

impl Pea2Pea for Crawler {
    fn node(&self) -> &Pea2PeaNode {
        &self.synth_node.node()
    }
}

impl Deref for Crawler {
    type Target = SynthNode;

    fn deref(&self) -> &Self::Target {
        &self.synth_node
    }
}

impl Crawler {
    /// Creates a crawler node with the most basic network protocols enabled.
    pub async fn new(opts: Opts) -> Self {
        let config = Config {
            name: Some("snarkOS crawler".into()),
            listener_ip: Some(opts.node.ip()),
            desired_listening_port: Some(opts.node.port()),
            max_connections: MAXIMUM_NUMBER_OF_PEERS as u16,
            max_handshake_time_ms: 5_000,
            ..Default::default()
        };

        let pea2pea_node = Pea2PeaNode::new(Some(config)).await.unwrap();
        let client_state = Default::default();
        let node = Self {
            synth_node: SynthNode::new(pea2pea_node, client_state),
            known_network: Arc::new(KnownNetwork::default()),
        };

        node.enable_disconnect().await;
        node.enable_handshake().await;
        node.enable_reading().await;
        node.enable_writing().await;

        node
    }

    fn rng(&self) -> SmallRng {
        SmallRng::from_entropy()
    }

    pub async fn is_connected(&self, addr: SocketAddr) -> bool {
        // Handshakes can take a while.
        if self.node().is_connecting(addr) {
            return true;
        }

        let connected_addrs = self.node().connected_addrs();

        self.state
            .peers
            .lock()
            .await
            .iter()
            .filter_map(|peer| {
                if peer.listening_addr == addr || peer.connected_addr == addr {
                    Some(addr)
                } else {
                    None
                }
            })
            .any(|addr| connected_addrs.contains(&addr))
    }

    /// Spawns a task dedicated to peer maintenance.
    pub fn update_peers(&self) {
        let node = self.clone();
        task::spawn(async move {
            loop {
                // Disconnect from peers we have just crawled.
                for addr in node.known_network.addrs_to_disconnect() {
                    if let Some(addr) = node.get_peer_connected_addr(addr).await {
                        node.node().disconnect(addr).await;
                    }
                }

                // Connect to peers we haven't crawled in a while.
                for addr in node
                    .known_network
                    .addrs_to_connect()
                    .into_iter()
                    .choose_multiple(&mut node.rng(), NUM_CONCURRENT_CONNECTION_ATTEMPTS as usize)
                {
                    if !node.is_connected(addr).await {
                        let node_clone = node.clone();
                        task::spawn(async move {
                            if node_clone.node().connect(addr).await.is_ok() {
                                let _ = node_clone.send_direct_message(addr, ClientMessage::PeerRequest);
                            } else {
                                node_clone.known_network.update_timestamp(addr);
                            }
                        });
                    }
                }

                debug!(parent: node.node().span(), "crawling the network for more peers; asking peers for their peers");
                node.send_broadcast(ClientMessage::PeerRequest).unwrap();
                tokio::time::sleep(Duration::from_secs(PEER_INTERVAL_SECS)).await;
            }
        });
    }

    fn log_known_network(&self) {
        let node = self.clone();
        tokio::spawn(async move {
            loop {
                info!(parent: node.node().span(), "current peers: {}", node.node().num_connected());
                info!(parent: node.node().span(), "known addresses: {}", node.known_network.nodes().len());
                info!(parent: node.node().span(), "known connections: {}", node.known_network.connections().len());
                tokio::time::sleep(Duration::from_secs(LOG_INTERVAL_SECS)).await;
            }
        });
    }

    /// Starts the usual periodic activities of a crawler node.
    pub fn run_periodic_tasks(&self) {
        self.log_known_network();
        self.update_peers();
    }
}

pub enum InboundMessage {
    Handled(ClientMessage),
    Unhandled,
}

/// Inbound message processing logic for the crawler nodes.
#[async_trait::async_trait]
impl Reading for Crawler {
    type Message = InboundMessage;

    fn read_message<R: io::Read>(&self, source: SocketAddr, reader: &mut R) -> io::Result<Option<Self::Message>> {
        // FIXME: use the maximum message size allowed by the protocol or (better) use streaming deserialization.
        let mut buf = [0u8; 64 * 1024];

        reader.read_exact(&mut buf[..MESSAGE_LENGTH_PREFIX_SIZE])?;
        let len = u32::from_le_bytes(buf[..MESSAGE_LENGTH_PREFIX_SIZE].try_into().unwrap()) as usize;

        if len > buf.len() {
            error!(parent: self.node().span(), "a message from {} is too large ({}B)", source, len);
            return Err(io::ErrorKind::InvalidData.into());
        }

        if reader.read_exact(&mut buf[..len]).is_err() {
            return Ok(None);
        }

        // Read the message ID to filter out undesirable messages.
        let message_id: u16 = bincode::deserialize(&buf[..2]).map_err(|_| io::ErrorKind::InvalidData)?;

        if !ACCEPTED_MESSAGE_IDS.contains(&message_id) {
            return Ok(Some(InboundMessage::Unhandled));
        }

        match ClientMessage::deserialize(&mut io::Cursor::new(&buf[..len])) {
            Ok(msg) => {
                debug!(parent: self.node().span(), "received a {} from {}", msg.name(), source);
                Ok(Some(InboundMessage::Handled(msg)))
            }
            Err(e) => {
                error!(parent: self.node().span(), "a message from {} failed to deserialize: {}", source, e);
                Err(io::ErrorKind::InvalidData.into())
            }
        }
    }

    async fn process_message(&self, source: SocketAddr, message: Self::Message) -> io::Result<()> {
        if let InboundMessage::Handled(message) = message {
            match message {
                ClientMessage::Disconnect(reason) => {
                    debug!(parent: self.node().span(), "peer {} disconnected for the following reason: {:?}", source, reason);
                    Ok(())
                }
                ClientMessage::PeerRequest => {
                    self.process_peer_request(source).await?;
                    Ok(())
                }
                ClientMessage::PeerResponse(peer_ips) => {
                    self.process_peer_response(source, peer_ips).await?;
                    Ok(())
                }
                ClientMessage::Ping(version, _fork_depth, _peer_type, _peer_state, _block_hash, block_header) => {
                    // TODO: we should probably manually deserialize the header, as we only need the
                    // height, and we need to be able to quickly handle any number of such messages
                    let block_header = block_header.deserialize().await.map_err(|_| io::ErrorKind::InvalidData)?;
                    self.process_ping(source, version, block_header.height()).await
                }
                _ => {
                    unreachable!();
                }
            }
        } else {
            Ok(())
        }
    }
}

// Helper methods.
impl Crawler {
    async fn process_peer_request(&self, source: SocketAddr) -> io::Result<()> {
        let peers = self
            .known_network
            .nodes()
            .into_iter()
            .map(|(addr, _)| addr)
            .choose_multiple(&mut self.rng(), 10);
        let msg = ClientMessage::PeerResponse(peers);
        debug!(parent: self.node().span(), "sending a PeerResponse to {}", source);

        self.send_direct_message(source, msg)?;

        Ok(())
    }

    async fn process_peer_response(&self, source: SocketAddr, mut peer_addrs: Vec<SocketAddr>) -> io::Result<()> {
        let node = self.clone();
        task::spawn(async move {
            peer_addrs.retain(|addr| node.node().listening_addr().unwrap() != *addr);

            // Insert the address into the known network and update the crawl state.
            if let Some(listening_addr) = node.get_peer_listening_addr(source).await {
                node.known_network.update_connections(listening_addr, peer_addrs.clone());
                node.known_network.received_peers(listening_addr);
            }

            for addr in peer_addrs {
                if !node.is_connected(addr).await {
                    debug!(parent: node.node().span(), "trying to connect to {}'s peer {}", source, addr);

                    // Only connect if this address needs to be crawled.
                    if node.known_network.should_be_connected_to(addr) {
                        let node_clone = node.clone();
                        task::spawn(async move {
                            if node_clone.node().connect(addr).await.is_ok() {
                                let _ = node_clone.send_direct_message(addr, ClientMessage::PeerRequest);
                            } else {
                                node_clone.known_network.update_timestamp(addr);
                            }
                        });
                    }
                }
            }
        });

        Ok(())
    }

    async fn process_ping(&self, source: SocketAddr, version: u32, block_height: u32) -> io::Result<()> {
        // Ensure the message protocol version is not outdated.
        // TODO: we should probably maintain a detailed list of non-compliant peers so we can
        // report their numbers and reasons for non-compliance with the protocol.
        if version < MESSAGE_VERSION {
            warn!(parent: self.node().span(), "dropping {} due to outdated version ({})", source, version);
            return Err(io::ErrorKind::InvalidData.into());
        }

        debug!(parent: self.node().span(), "peer {} is at height {}", source, block_height);

        // Update the known network nodes and update the crawl state.
        if let Some(listening_addr) = self.get_peer_listening_addr(source).await {
            self.known_network.update_height(listening_addr, block_height);
        }

        let genesis = CurrentNetwork::genesis_block();
        let msg = ClientMessage::Pong(
            None,
            // TODO: we'll be sending this out very often, so we might as well create this
            // object just once and copy it over whenever needed.
            Data::Object(
                BlockLocators::<CurrentNetwork>::from(vec![(genesis.height(), (genesis.hash(), None))].into_iter().collect()).unwrap(),
            ),
        );

        debug!(parent: self.node().span(), "sending a Pong to {}", source);

        self.send_direct_message(source, msg)?;

        Ok(())
    }
}