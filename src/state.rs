use crate::channel::{Channel, ChannelRef};
use crate::client::{Client, ClientRef};
use crate::crypt::CryptState;
use crate::error::MumbleError;
use crate::message::ClientMessage;
use crate::proto::mumble::{Authenticate, ChannelRemove, ChannelState, CodecVersion, UserRemove, Version};
use crate::proto::{message_to_bytes, MessageKind};
use crate::server::constants::MAX_CLIENTS;
use crate::voice::{ServerBound, VoicePacket};
use bytes::BytesMut;
use protobuf::Message;
use scc::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::WriteHalf;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc::Sender;
use tokio::sync::RwLock;
use tokio_rustls::server::TlsStream;

pub struct CodecState {
    pub opus: bool,
    pub alpha: i32,
    pub beta: i32,
    pub prefer_alpha: bool,
}

impl Default for CodecState {
    fn default() -> Self {
        Self {
            opus: true,
            alpha: 0,
            beta: 0,
            prefer_alpha: false,
        }
    }
}

impl CodecState {
    pub fn get_version(&self) -> i32 {
        if self.prefer_alpha {
            return self.alpha;
        }

        self.beta
    }

    pub fn get_codec_version(&self) -> CodecVersion {
        let mut codec_version = CodecVersion::default();
        codec_version.set_alpha(self.alpha);
        codec_version.set_beta(self.beta);
        codec_version.set_opus(self.opus);
        codec_version.set_prefer_alpha(self.prefer_alpha);

        codec_version
    }
}

pub type ServerStateRef = Arc<ServerState>;

pub struct ServerState {
    pub clients: HashMap<u32, ClientRef>,
    pub clients_by_socket: HashMap<SocketAddr, ClientRef>,
    pub channels: HashMap<u32, Arc<Channel>>,
    pub codec_state: Arc<RwLock<CodecState>>,
    pub socket: Arc<UdpSocket>,
}

impl ServerState {
    pub fn new(socket: Arc<UdpSocket>) -> Self {
        let channels = HashMap::new();
        channels.upsert(
            0,
            Arc::new(Channel::new(0, Some(0), "Root".to_string(), "Root channel".to_string(), false)),
        );

        Self {
            // we preallocate the maximum amount of clients to prevent the possibility of resizes
            // later, which will prevent double-sends in certain situations
            clients: HashMap::with_capacity(MAX_CLIENTS),
            clients_by_socket: HashMap::with_capacity(MAX_CLIENTS),
            channels,
            codec_state: Arc::new(RwLock::new(CodecState::default())),
            socket,
        }
    }

    pub fn add_client(
        &self,
        version: Version,
        authenticate: Authenticate,
        crypt_state: CryptState,
        write: WriteHalf<TlsStream<TcpStream>>,
        publisher: Sender<ClientMessage>,
    ) -> ClientRef {
        let session_id = self.get_free_session_id();

        let client = Arc::new(Client::new(
            version,
            authenticate,
            session_id,
            0,
            crypt_state,
            write,
            self.socket.clone(),
            publisher,
        ));

        crate::metrics::CLIENTS_TOTAL.inc();
        self.clients.upsert(session_id, client.clone());

        client
    }

    pub fn add_channel(&self, state: &ChannelState) -> ChannelRef {
        let channel_id = self.get_free_channel_id();
        let channel = Arc::new(Channel::new(
            channel_id,
            Some(state.get_parent()),
            state.get_name().to_string(),
            state.get_description().to_string(),
            state.get_temporary(),
        ));

        tracing::debug!("Created channel {} with name {}", channel_id, state.get_name().to_string());

        self.channels.upsert(channel_id, channel.clone());

        channel
    }

    pub fn get_client_by_name(&self, name: &str) -> Option<ClientRef> {
        let client = self.clients.any_entry(|_k, client| client.authenticate.get_username() == name);

        if let Some(cl) = client {
            return Some(cl.clone());
        }

        None
    }

    pub async fn set_client_socket(&self, client: ClientRef, addr: SocketAddr) {
        let socket_lock = client.udp_socket_addr.swap(Some(Arc::new(addr)));
        if let Some(exiting_addr) = socket_lock {
            self.clients_by_socket.remove(exiting_addr.as_ref());
        }

        self.clients_by_socket.upsert(addr, client);
    }

    pub fn broadcast_message<T: Message>(&self, kind: MessageKind, message: &T) -> Result<(), MumbleError> {
        tracing::trace!("broadcast message: {:?}, {:?}", std::any::type_name::<T>(), message);

        let bytes = message_to_bytes(kind, message)?;

        self.clients.scan(|_k, client| {
            match client.publisher.try_send(ClientMessage::SendMessage {
                kind,
                payload: bytes.clone(),
            }) {
                Ok(_) => {}
                Err(err) => {
                    tracing::error!("failed to send message to {}: {}", client.authenticate.get_username(), err);
                }
            };
        });

        Ok(())
    }

    fn handle_client_left_channel(&self, client_session: u32, leave_channel_id: u32) -> Option<u32> {
        if let Some(channel) = self.channels.get(&leave_channel_id) {
            // remove the client from the channel
            channel.clients.remove(&client_session);

            if channel.parent_id.is_none() {
                println!("channel {} had no parent, could be root", channel.id);
                return None;
            };

            // if the channel isn't temporary then we want to keep it
            if !channel.temporary || !channel.get_clients().is_empty() {
                return None;
            };
        }

        // Broadcast channel remove
        let mut channel_remove = ChannelRemove::new();
        channel_remove.set_channel_id(leave_channel_id);

        self.channels.remove(&leave_channel_id);

        match self.broadcast_message(MessageKind::ChannelRemove, &channel_remove) {
            Ok(_) => (),
            Err(e) => tracing::error!("failed to send channel remove: {:?}", e),
        }

        Some(leave_channel_id)
    }

    pub fn set_client_channel(&self, client: ClientRef, channel: &Channel) {
        let leave_channel_id = { client.join_channel(channel.id) };

        channel.get_clients().upsert(client.session_id, client.clone());

        if let Some(leave_channel_id) = leave_channel_id {
            // Broadcast new user state
            let user_state = client.get_user_state();

            match self.broadcast_message(MessageKind::UserState, &user_state) {
                Ok(_) => (),
                Err(e) => tracing::error!("failed to send user state: {:?}", e),
            }

            self.handle_client_left_channel(client.session_id, leave_channel_id);
        }
    }

    pub fn get_channel_by_name(&self, name: &str) -> Option<ChannelRef> {
        let client = self.channels.any_entry(|_k, channel| channel.name == name);

        if let Some(cl) = client {
            return Some(cl.clone());
        }

        None
    }

    // TODO: Check what this does or if this is even needed (we should always use opus)
    pub async fn check_codec(&self) -> Result<Option<CodecVersion>, MumbleError> {
        let current_version = { self.codec_state.read().await.get_version() };
        let mut new_version = current_version;
        let mut versions = std::collections::HashMap::new();

        self.clients.scan(|_, client| {
            for version in &client.codecs {
                *versions.entry(*version).or_insert(0) += 1;
            }
        });

        let mut max = 0;

        for (version, count) in versions {
            if count > max {
                new_version = version;
                max = count;
            }
        }

        if new_version == current_version {
            return Ok(Some(self.codec_state.read().await.get_codec_version()));
        }

        let codec_version = {
            let mut codec_state = self.codec_state.write().await;
            codec_state.prefer_alpha = !codec_state.prefer_alpha;

            if codec_state.prefer_alpha {
                codec_state.alpha = new_version;
            } else {
                codec_state.beta = new_version;
            }

            codec_state.get_codec_version()
        };

        match self.broadcast_message(MessageKind::CodecVersion, &codec_version) {
            Ok(_) => (),
            Err(e) => {
                tracing::error!("failed to broadcast codec version: {:?}", e);
            }
        }

        Ok(None)
    }

    pub fn get_client_by_socket(&self, socket_addr: &SocketAddr) -> Option<ClientRef> {
        self.clients_by_socket.get(socket_addr).map(|client| client.clone())
    }

    pub fn remove_client_by_socket(&self, socket_addr: &SocketAddr) {
        self.clients_by_socket.remove(socket_addr);
    }

    pub async fn find_client_with_decrypt(
        &self,
        bytes: &mut BytesMut,
    ) -> Result<(Option<ClientRef>, Option<VoicePacket<ServerBound>>), MumbleError> {

        let mut iter = self.clients.first_entry();
        while let Some(c) = iter {
            let mut try_buf = bytes.clone();
            let (decrypt_result, last_good) = {
                let mut crypt_state = c.crypt_state.lock();
                (crypt_state.decrypt(&mut try_buf), crypt_state.last_good)
            };

            match decrypt_result {
                Ok(p) => {
                    return Ok((Some(c.clone()), Some(p)));
                }
                Err(err) => {
                    let duration = { Instant::now().duration_since(last_good).as_millis() };

                    // last good packet was more than 5sec ago, reset
                    if duration > 5000 {
                        let send_crypt_setup = c.send_crypt_setup(true);

                        if let Err(e) = send_crypt_setup.await {
                            tracing::error!("failed to send crypt setup: {:?}", e);
                        }

                        c.remove_client_udp_socket(self);
                    }

                    tracing::debug!("failed to decrypt packet: {:?}, continue to next client", err);
                }
            }
            iter = c.next();
        }

        Ok((None, None))
    }

    pub fn disconnect(&self, client: ClientRef) -> Result<(u32, u32), MumbleError> {
        crate::metrics::CLIENTS_TOTAL.dec();

        let client_id = client.session_id;

        self.clients.remove(&client_id);

        let socket = client.udp_socket_addr.swap(None);

        if let Some(socket_addr) = socket {
            self.remove_client_by_socket(&socket_addr);
        }

        let channel_id = client.channel_id.load(Ordering::Relaxed);

        self.broadcast_client_delete(client_id, channel_id)?;

        Ok((client_id, channel_id))
    }

    fn broadcast_client_delete(&self, client_id: u32, channel_id: u32) -> Result<(), MumbleError> {
        let mut remove = UserRemove::new();
        remove.set_session(client_id);
        remove.set_reason("disconnected".to_string());

        self.broadcast_message(MessageKind::UserRemove, &remove)?;

        self.handle_client_left_channel(client_id, channel_id);

        Ok(())
    }

    fn get_free_session_id(&self) -> u32 {
        let mut session_id = 1;

        loop {
            if self.clients.contains(&session_id) {
                session_id += 1;
            } else {
                break;
            }
        }

        session_id
    }

    fn get_free_channel_id(&self) -> u32 {
        let mut channel_id = 1;

        loop {
            if self.channels.contains(&channel_id) {
                channel_id += 1;
            } else {
                break;
            }
        }

        channel_id
    }
}
