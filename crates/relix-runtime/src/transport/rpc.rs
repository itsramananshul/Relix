//! libp2p RPC transport — ported from OpenPrem `network/rpc.rs` per
//! `docs/code-reuse-map.md`. Uses `/relix/rpc/1` protocol identifier (replaces
//! `/rpc/1`) but otherwise inherits the transport stack: TCP + Noise XK + Yamux
//! + CBOR `request_response` + Kademlia DHT.
//!
//! The wire payload here is the raw [`WireRpc::Request`] / [`WireRpc::Response`]
//! pair carrying RELIX-1 envelopes (see `super::envelope`). The transport is
//! envelope-agnostic; admission is performed by the dispatch bridge.

use std::collections::HashMap;
use std::time::Duration;

use futures::StreamExt;
use libp2p::{
    StreamProtocol, identity, kad, noise,
    request_response::{self, OutboundRequestId, ProtocolSupport, ResponseChannel},
    swarm::{NetworkBehaviour, Swarm, SwarmEvent},
    tcp, yamux,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

pub use libp2p::{PeerId, core::Multiaddr};

/// CORR PART 4: hard cap on the in-flight `pending_calls`
/// map. A bursty peer (or a stuck responder) used to grow
/// the table without limit and exhaust process memory; the
/// cap returns an `OVERLOADED` error to the caller instead
/// of accepting and dropping silently. 1000 is well above
/// any realistic load — the rate limiter in front of the
/// bridge fires long before this.
pub const MAX_PENDING_CALLS: usize = 1000;

/// Wire payload — opaque bytes carrying a CBOR-encoded RELIX-1 envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireRequest {
    /// Caller's CBOR-encoded `RequestEnvelope`.
    pub envelope: Vec<u8>,
}

/// Wire payload — opaque bytes carrying a CBOR-encoded RELIX-1 response envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireResponse {
    /// Responder's CBOR-encoded `ResponseEnvelope`.
    pub envelope: Vec<u8>,
}

#[derive(NetworkBehaviour)]
struct Behaviour {
    rpc: request_response::cbor::Behaviour<WireRequest, WireResponse>,
    kademlia: kad::Behaviour<kad::store::MemoryStore>,
    /// RELIX-2 streaming substream. Independent of `rpc`: a
    /// single libp2p connection multiplexes both protocols on
    /// separate Yamux substreams. Capabilities marked
    /// `stream_out` open a `/relix/rpc/stream/1` substream
    /// (see [`super::stream`]) while everything else continues
    /// using the unary cbor request_response path.
    stream: libp2p_stream::Behaviour,
}

/// Inbound RPC event surfaced to the application loop.
pub enum Event {
    /// Inbound request — application MUST call `Responder::respond` exactly once.
    Request {
        /// Encoded RELIX-1 envelope from the caller.
        envelope: Vec<u8>,
        /// Source peer (verified at transport layer by Noise).
        from: PeerId,
        /// Reply channel.
        respond: Responder,
    },
    /// A peer connected. Useful for manifest-exchange triggers.
    PeerConnected {
        /// The peer.
        peer_id: PeerId,
        /// The address it was reached at.
        address: Multiaddr,
    },
    /// SEC §18: a peer's last connection closed. Surfaced so a
    /// persistent consumer can promptly drop any trust learned for
    /// that peer (e.g. its knowledge-share source key) — no stale
    /// trust lingering after the connection drops.
    PeerDisconnected {
        /// The peer whose connection closed.
        peer_id: PeerId,
    },
}

/// Reply handle for an inbound request.
pub struct Responder {
    channel: ResponseChannel<WireResponse>,
    cmd_tx: mpsc::Sender<SwarmCommand>,
}

impl Responder {
    /// Send the encoded response envelope. Drops the channel on completion.
    pub async fn respond(self, envelope: Vec<u8>) {
        let _ = self
            .cmd_tx
            .send(SwarmCommand::Respond {
                envelope,
                channel: self.channel,
            })
            .await;
    }
}

/// Cloneable handle for making outbound RPC calls and dialing peers.
#[derive(Clone)]
pub struct Client {
    peer_id: PeerId,
    cmd_tx: mpsc::Sender<SwarmCommand>,
    /// Control handle for the libp2p stream behaviour. Clone-
    /// able and independent of the unary command channel —
    /// stream open / accept calls talk directly to the
    /// behaviour through its own internal channel.
    stream_control: libp2p_stream::Control,
}

impl Client {
    /// Our peer id (= libp2p Ed25519 pubkey hash).
    pub fn peer_id(&self) -> PeerId {
        self.peer_id
    }

    /// Borrow the streaming control for opening / accepting
    /// `/relix/rpc/stream/1` substreams. Each call returns a
    /// fresh clone — controls are cheap to clone and the
    /// caller can pass it across awaits / threads freely.
    pub fn stream_control(&self) -> libp2p_stream::Control {
        self.stream_control.clone()
    }

    /// Open a new outbound streaming substream to `peer` using
    /// the RELIX-2 protocol. Returns the raw libp2p stream;
    /// callers wrap it with [`super::stream::StreamReader`] and
    /// drive the framing helpers.
    pub async fn open_stream(
        &self,
        peer: PeerId,
    ) -> Result<libp2p::Stream, libp2p_stream::OpenStreamError> {
        self.stream_control
            .clone()
            .open_stream(peer, super::stream::PROTOCOL)
            .await
    }

    /// Register the [`super::stream::PROTOCOL`] for inbound
    /// streams. Returns an `IncomingStreams` handle (a
    /// `Stream<Item = (PeerId, libp2p::Stream)>`) the caller
    /// drives forward. Registering twice for the same
    /// protocol returns `AlreadyRegistered`.
    pub fn accept_streams(
        &self,
    ) -> Result<libp2p_stream::IncomingStreams, libp2p_stream::AlreadyRegistered> {
        self.stream_control.clone().accept(super::stream::PROTOCOL)
    }

    /// Issue an outbound RPC. Returns the encoded response envelope.
    pub async fn call(&self, peer: PeerId, envelope: Vec<u8>) -> Result<Vec<u8>, String> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SwarmCommand::Call {
                peer,
                envelope,
                reply: tx,
            })
            .await
            .map_err(|_| "RPC channel closed".to_string())?;
        rx.await.map_err(|_| "reply cancelled".to_string())?
    }

    /// Dial a known peer address (e.g. a bootstrap or peer listed in config).
    pub async fn dial(&self, addr: Multiaddr) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SwarmCommand::Dial { addr, reply: tx })
            .await
            .map_err(|_| "RPC channel closed".to_string())?;
        rx.await.map_err(|_| "dial cancelled".to_string())?
    }

    /// Trigger Kademlia bootstrap.
    pub async fn bootstrap_kademlia(&self) {
        let _ = self.cmd_tx.send(SwarmCommand::BootstrapKademlia).await;
    }
}

#[allow(dead_code)]
enum SwarmCommand {
    Dial {
        addr: Multiaddr,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Call {
        peer: PeerId,
        envelope: Vec<u8>,
        reply: oneshot::Sender<Result<Vec<u8>, String>>,
    },
    Respond {
        envelope: Vec<u8>,
        channel: ResponseChannel<WireResponse>,
    },
    BootstrapKademlia,
}

/// Swarm event loop. Spawn `.run()` as a Tokio task.
pub struct EventLoop {
    swarm: Swarm<Behaviour>,
    cmd_tx: mpsc::Sender<SwarmCommand>,
    command_receiver: mpsc::Receiver<SwarmCommand>,
    event_sender: mpsc::Sender<Event>,
    pending_calls: HashMap<OutboundRequestId, oneshot::Sender<Result<Vec<u8>, String>>>,
}

impl EventLoop {
    /// Run the event loop until both the command channel and event sender close.
    pub async fn run(mut self) {
        loop {
            tokio::select! {
                event = self.swarm.select_next_some() => {
                    self.handle_swarm_event(event).await;
                }
                command = self.command_receiver.recv() => {
                    match command {
                        Some(cmd) => self.handle_command(cmd).await,
                        None => break,
                    }
                }
            }
        }
    }

    async fn handle_swarm_event(&mut self, event: SwarmEvent<BehaviourEvent>) {
        match event {
            SwarmEvent::Behaviour(BehaviourEvent::Rpc(rpc_event)) => {
                use request_response::Event as RrEvent;
                match rpc_event {
                    RrEvent::Message { message, peer, .. } => match message {
                        request_response::Message::Request {
                            request, channel, ..
                        } => {
                            let respond = Responder {
                                channel,
                                cmd_tx: self.cmd_tx.clone(),
                            };
                            let _ = self
                                .event_sender
                                .send(Event::Request {
                                    envelope: request.envelope,
                                    from: peer,
                                    respond,
                                })
                                .await;
                        }
                        request_response::Message::Response {
                            request_id,
                            response,
                            ..
                        } => {
                            if let Some(sender) = self.pending_calls.remove(&request_id) {
                                let _ = sender.send(Ok(response.envelope));
                            }
                        }
                    },
                    RrEvent::OutboundFailure {
                        request_id, error, ..
                    } => {
                        if let Some(sender) = self.pending_calls.remove(&request_id) {
                            let _ = sender.send(Err(format!("{error:?}")));
                        }
                    }
                    RrEvent::ResponseSent { .. } => {}
                    RrEvent::InboundFailure { .. } => {}
                }
            }
            SwarmEvent::ConnectionEstablished {
                peer_id, endpoint, ..
            } => {
                let addr = endpoint.get_remote_address().clone();
                self.swarm
                    .behaviour_mut()
                    .kademlia
                    .add_address(&peer_id, addr.clone());
                let _ = self
                    .event_sender
                    .send(Event::PeerConnected {
                        peer_id,
                        address: addr,
                    })
                    .await;
            }
            SwarmEvent::Behaviour(BehaviourEvent::Kademlia(_)) => {}
            SwarmEvent::NewListenAddr { .. } => {}
            SwarmEvent::IncomingConnection { .. } => {}
            // SEC §18: surface the disconnect so a persistent consumer
            // can drop trust learned for this peer. libp2p emits one
            // `ConnectionClosed` per closed connection; a peer may
            // briefly have multiple, so consumers must tolerate a
            // disconnect for a peer that still has another live
            // connection (the source-key registry treats unregister
            // idempotently, and a still-live peer re-registers on its
            // next manifest exchange).
            SwarmEvent::ConnectionClosed { peer_id, .. } => {
                let _ = self
                    .event_sender
                    .send(Event::PeerDisconnected { peer_id })
                    .await;
            }
            SwarmEvent::OutgoingConnectionError { .. } => {}
            _ => {}
        }
    }

    async fn handle_command(&mut self, cmd: SwarmCommand) {
        match cmd {
            SwarmCommand::Dial { addr, reply } => {
                let result = match self.swarm.dial(addr) {
                    Ok(()) => Ok(()),
                    Err(e) => Err(format!("{e:?}")),
                };
                let _ = reply.send(result);
            }
            SwarmCommand::Call {
                peer,
                envelope,
                reply,
            } => {
                // CORR PART 4: enforce a max pending-calls
                // bound BEFORE send_request so a bursty peer
                // cannot grow `pending_calls` without limit
                // and exhaust the responder slot table.
                if self.pending_calls.len() >= MAX_PENDING_CALLS {
                    let _ = reply.send(Err(format!(
                        "OVERLOADED: pending_calls at cap {MAX_PENDING_CALLS}"
                    )));
                    return;
                }
                // CORR PART 3: the unary RPC reply path is
                // race-free because `handle_command` and
                // `handle_swarm_event` run in the same
                // `tokio::select!` body — `swarm.send_request`
                // synthesises and returns an `OutboundRequestId`
                // synchronously, and the response handler can
                // only fire on a later poll of
                // `swarm.select_next_some()`. The `insert`
                // therefore lands strictly before any
                // matching response is ever surfaced. (A
                // pre-spec read suggested we needed to
                // insert before `send_request`; with the
                // single-task select loop the ordering is
                // load-bearing equivalent.) The OVERLOADED
                // gate above is PART 4's bound.
                let request_id = self
                    .swarm
                    .behaviour_mut()
                    .rpc
                    .send_request(&peer, WireRequest { envelope });
                self.pending_calls.insert(request_id, reply);
            }
            SwarmCommand::Respond { envelope, channel } => {
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .rpc
                    .send_response(channel, WireResponse { envelope });
            }
            SwarmCommand::BootstrapKademlia => {
                let _ = self.swarm.behaviour_mut().kademlia.bootstrap();
            }
        }
    }
}

/// Bring up a transport instance.
///
/// `keypair_bytes` — caller-provided Ed25519 secret (32 raw bytes) so the
/// transport's libp2p `PeerId` matches `relix_core::types::NodeId` derived
/// from the same key. The alpha controller loads this from its identity file.
pub async fn new(
    keypair_bytes: [u8; 32],
    port: u16,
) -> Result<(Client, mpsc::Receiver<Event>, EventLoop), Box<dyn std::error::Error>> {
    let id_keys: identity::Keypair = identity::Keypair::ed25519_from_bytes(keypair_bytes)?;
    let peer_id = id_keys.public().to_peer_id();

    let mut swarm: Swarm<Behaviour> = libp2p::SwarmBuilder::with_existing_identity(id_keys)
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_behaviour(|key| Behaviour {
            rpc: request_response::cbor::Behaviour::new(
                [(StreamProtocol::new("/relix/rpc/1"), ProtocolSupport::Full)],
                request_response::Config::default(),
            ),
            kademlia: kad::Behaviour::new(
                key.public().to_peer_id(),
                kad::store::MemoryStore::new(key.public().to_peer_id()),
            ),
            // RELIX-2 streaming substream. The behaviour stays
            // dormant until a caller registers the
            // `/relix/rpc/stream/1` protocol via the Control
            // (responder side) or opens a stream via the
            // Control (caller side).
            stream: libp2p_stream::Behaviour::new(),
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(u64::MAX)))
        .build();
    // Extract a Control handle from the freshly-built stream
    // behaviour BEFORE the swarm starts. The Control talks to
    // the behaviour through an internal channel and stays
    // valid for the swarm's lifetime; clones are cheap.
    let stream_control = swarm.behaviour_mut().stream.new_control();

    let addr: Multiaddr = format!("/ip4/127.0.0.1/tcp/{port}").parse()?;
    swarm.listen_on(addr.clone())?;
    tracing::info!(addr = %addr, peer = %peer_id, "transport listening");

    let (cmd_tx, cmd_rx) = mpsc::channel::<SwarmCommand>(64);
    let (event_tx, event_rx) = mpsc::channel::<Event>(64);

    let client = Client {
        peer_id,
        cmd_tx: cmd_tx.clone(),
        stream_control,
    };
    let event_loop = EventLoop {
        swarm,
        cmd_tx,
        command_receiver: cmd_rx,
        event_sender: event_tx,
        pending_calls: HashMap::new(),
    };

    Ok((client, event_rx, event_loop))
}
