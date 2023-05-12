use ambient_ecs::{
    query, Component, ComponentValue, EntityId, Networked, Serializable, Store, World,
};
use serde::de::DeserializeOwned;
use std::{
    io::ErrorKind,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use stream::FrameError;

use ambient_rpc::RpcError;
use ambient_std::log_error;
use quinn::{
    ClientConfig, ConnectionClose, ConnectionError::ConnectionClosed, Endpoint, ServerConfig,
    TransportConfig,
};
use rand::Rng;
use rustls::{Certificate, PrivateKey, RootCertStore};
use thiserror::Error;

pub use ambient_ecs::generated::components::core::network::{
    is_remote_entity, persistent_resources, synced_resources,
};

pub type AsyncMutex<T> = tokio::sync::Mutex<T>;
pub mod client;
pub mod client_connection;
pub mod client_game_state;
pub mod codec;
pub mod hooks;
pub mod native;
pub mod proto;
pub mod rpc;
pub mod server;
pub mod stream;

pub const RPC_BISTREAM_ID: u32 = 2;

pub const WASM_BISTREAM_ID: u32 = 10;

pub const WASM_UNISTREAM_ID: u32 = 11;

pub const PLAYER_INPUT_DATAGRAM_ID: u32 = 12;
pub const WASM_DATAGRAM_ID: u32 = 13;

const MAX_FRAME_SIZE: usize = 1024 * 1024 * 1024;

pub fn init_all_components() {
    client::init_components();
    server::init_components();
    client_game_state::init_components();
}

pub trait ServerWorldExt {
    fn persisted_resource_entity(&self) -> Option<EntityId>;
    fn persisted_resource<T: ComponentValue>(&self, component: Component<T>) -> Option<&T>;
    fn persisted_resource_mut<T: ComponentValue>(
        &mut self,
        component: Component<T>,
    ) -> Option<&mut T>;
    fn synced_resource_entity(&self) -> Option<EntityId>;
    fn synced_resource<T: ComponentValue>(&self, component: Component<T>) -> Option<&T>;
    fn synced_resource_mut<T: ComponentValue>(&mut self, component: Component<T>)
        -> Option<&mut T>;
}
impl ServerWorldExt for World {
    fn persisted_resource_entity(&self) -> Option<EntityId> {
        query(())
            .incl(persistent_resources())
            .iter(self, None)
            .map(|(id, _)| id)
            .next()
    }
    fn persisted_resource<T: ComponentValue>(&self, component: Component<T>) -> Option<&T> {
        assert_persisted(*component);
        self.persisted_resource_entity()
            .and_then(|id| self.get_ref(id, component).ok())
    }
    fn persisted_resource_mut<T: ComponentValue>(
        &mut self,
        component: Component<T>,
    ) -> Option<&mut T> {
        assert_persisted(*component);
        self.persisted_resource_entity()
            .and_then(|id| self.get_mut(id, component).ok())
    }

    fn synced_resource_entity(&self) -> Option<EntityId> {
        query(())
            .incl(synced_resources())
            .iter(self, None)
            .map(|(id, _)| id)
            .next()
    }
    fn synced_resource<T: ComponentValue>(&self, component: Component<T>) -> Option<&T> {
        assert_networked(*component);
        self.synced_resource_entity()
            .and_then(|id| self.get_ref(id, component).ok())
    }
    fn synced_resource_mut<T: ComponentValue>(
        &mut self,
        component: Component<T>,
    ) -> Option<&mut T> {
        self.synced_resource_entity()
            .and_then(|id| self.get_mut(id, component).ok())
    }
}

pub fn assert_networked(desc: ambient_ecs::ComponentDesc) {
    if !desc.has_attribute::<Networked>() {
        panic!(
            "Attempt to access sync {desc:#?} which is not marked as `Networked`. Attributes: {:?}",
            desc.attributes()
        );
    }

    if !desc.has_attribute::<Serializable>() {
        panic!(
            "Sync component {desc:#?} is not serializable. Attributes: {:?}",
            desc.attributes()
        );
    }
}

fn assert_persisted(desc: ambient_ecs::ComponentDesc) {
    assert_networked(desc);

    if !desc.has_attribute::<Store>() {
        panic!("Attempt to access persisted resource {desc:?} which is not `Store`");
    }
}

#[derive(Debug, Error)]
pub enum NetworkError {
    #[error("No more data to be read from stream")]
    EndOfStream,
    #[error("Connection closed by peer")]
    ConnectionClosed,
    #[error("Bad bincode message format: {0:?}")]
    BadMsgFormat(#[from] bincode::Error),
    #[error("IO Error")]
    IOError(#[from] std::io::Error),
    #[error("Quinn connection failed")]
    ConnectionError(#[from] quinn::ConnectionError),
    #[error(transparent)]
    ReadToEndError(#[from] quinn::ReadToEndError),
    #[error(transparent)]
    WriteError(#[from] quinn::WriteError),
    #[error(transparent)]
    SendDatagramError(#[from] quinn::SendDatagramError),
    #[error(transparent)]
    RpcError(#[from] RpcError),
    #[error(transparent)]
    ProxyError(#[from] ambient_proxy::Error),
    #[error("Bad frame")]
    FrameError(#[from] FrameError),
}

impl NetworkError {
    /// Returns true if the connection was properly closed.
    ///
    /// Does not return true if the stream was closed as the connection may
    /// still be alive.
    pub fn is_closed(&self) -> bool {
        match self {
            Self::ConnectionClosed => true,
            // The connection was closed automatically,
            // for example by dropping the [`quinn::Connection`]
            Self::ConnectionError(ConnectionClosed(ConnectionClose { error_code, .. }))
                if u64::from(*error_code) == 0 =>
            {
                true
            }
            Self::IOError(err) if matches!(err.kind(), ErrorKind::ConnectionReset) => true,
            _ => false,
        }
    }

    /// Returns `true` if the network error is [`EndOfStream`].
    ///
    /// [`EndOfStream`]: NetworkError::EndOfStream
    #[must_use]
    pub fn is_end_of_stream(&self) -> bool {
        matches!(self, Self::EndOfStream)
    }
}

pub fn create_client_endpoint_random_port() -> Option<Endpoint> {
    for _ in 0..10 {
        let client_port = {
            let mut rng = rand::thread_rng();
            rng.gen_range(15000..25000)
        };
        let client_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), client_port);

        if let Ok(mut endpoint) = Endpoint::client(client_addr) {
            let cert = Certificate(CERT.to_vec());
            let mut roots = RootCertStore::empty();
            roots.add(&cert).unwrap();
            let crypto = rustls::ClientConfig::builder()
                .with_safe_defaults()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let mut transport = TransportConfig::default();
            transport.keep_alive_interval(Some(Duration::from_secs_f32(1.)));
            if std::env::var("AMBIENT_DISABLE_TIMEOUT").is_ok() {
                transport.max_idle_timeout(None);
            } else {
                transport.max_idle_timeout(Some(Duration::from_secs_f32(60.).try_into().unwrap()));
            }
            let mut client_config = ClientConfig::new(Arc::new(crypto));
            client_config.transport_config(Arc::new(transport));

            endpoint.set_default_client_config(client_config);
            return Some(endpoint);
        }
    }
    None
}

fn create_server(server_addr: SocketAddr) -> anyhow::Result<Endpoint> {
    let cert = Certificate(CERT.to_vec());
    let cert_key = PrivateKey(CERT_KEY.to_vec());
    let mut server_conf = ServerConfig::with_single_cert(vec![cert.clone()], cert_key)?;
    let mut transport = TransportConfig::default();
    if std::env::var("AMBIENT_DISABLE_TIMEOUT").is_ok() {
        transport.max_idle_timeout(None);
    } else {
        transport.max_idle_timeout(Some(Duration::from_secs_f32(60.).try_into()?));
    }
    transport.keep_alive_interval(Some(Duration::from_secs_f32(1.)));
    let transport = Arc::new(transport);
    server_conf.transport = transport.clone();

    let mut endpoint = Endpoint::server(server_conf, server_addr)?;

    // Create client config for the server endpoint for proxying and hole punching
    let mut roots = RootCertStore::empty();
    roots.add(&cert).unwrap();
    let crypto = rustls::ClientConfig::builder()
        .with_safe_defaults()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let mut client_config = ClientConfig::new(Arc::new(crypto));
    client_config.transport_config(transport);
    endpoint.set_default_client_config(client_config);

    Ok(endpoint)
}

pub const CERT: &[u8] = include_bytes!("./cert.der");
pub const CERT_KEY: &[u8] = include_bytes!("./cert.key.der");

#[macro_export]
macro_rules! log_network_result {
    ( $x:expr ) => {
        if let Err(err) = $x {
            $crate::log_network_error(&err.into());
        }
    };
}

pub fn log_network_error(err: &anyhow::Error) {
    if let Some(quinn::WriteError::ConnectionLost(err)) = err.downcast_ref::<quinn::WriteError>() {
        log::info!("Connection lost: {:#}", err);
    } else if let Some(err) = err.downcast_ref::<quinn::ConnectionError>() {
        log::info!("Connection error: {:#}", err);
    } else if let Some(err) = err.downcast_ref::<quinn::WriteError>() {
        log::info!("Write error: {:#}", err);
    } else {
        log_error(err);
    }
}

#[macro_export]
macro_rules! unwrap_log_network_err {
    ( $x:expr ) => {
        match $x {
            Ok(val) => val,
            Err(err) => {
                $crate::log_network_error(&err.into());
                return Default::default();
            }
        }
    };
}
