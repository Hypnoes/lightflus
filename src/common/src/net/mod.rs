use std::{
    net::UdpSocket,
    pin::Pin,
    sync::atomic::{self, AtomicU64},
    task::{self, Poll},
    time::Duration,
};

use futures_util::{ready, Future, FutureExt};
use proto::common::{Ack, ExecutionId, Heartbeat, HostAddr, NodeType};
use tokio::sync::mpsc;

use crate::utils;

use self::gateway::{ReceiveAckRpcGateway, ReceiveHeartbeatRpcGateway};

pub const SUCCESS: i32 = 200;
pub const BAD_REQUEST: i32 = 400;
pub const INTERNAL_SERVER_ERROR: i32 = 500;
pub(crate) const DEFAULT_RPC_TIMEOUT: u64 = 3;
pub(crate) const DEFAULT_CONNECT_TIMEOUT: u64 = 3;
pub mod cluster;
#[cfg(not(tarpaulin_include))]
pub mod gateway;

#[derive(Clone, Debug)]
pub struct ClientConfig {
    // address
    pub address: PersistableHostAddr,
    // timeout
    pub timeout: u32,
    // retry count
    pub retry: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize, Default, Hash)]
pub struct PersistableHostAddr {
    pub host: String,
    pub port: u16,
}

impl PersistableHostAddr {
    pub fn as_uri(&self) -> String {
        format!("http://{}:{}", &self.host, self.port)
    }

    fn is_valid(&self) -> bool {
        !self.host.is_empty() && self.port > 0
    }

    pub fn local(port: usize) -> Self {
        Self {
            host: hostname().unwrap_or_default(),
            port: port as u16,
        }
    }
}

pub fn to_host_addr(hashable: &PersistableHostAddr) -> HostAddr {
    HostAddr {
        host: hashable.host.clone(),
        port: hashable.port as u32,
    }
}

pub fn hostname() -> Option<String> {
    use std::process::Command;
    if cfg!(unix) || cfg!(windows) {
        let output = match Command::new("hostname").output() {
            Ok(o) => o,
            Err(_) => return None,
        };
        let mut s = String::from_utf8(output.stdout).unwrap();
        s.pop(); // pop '\n'
        Some(s)
    } else {
        None
    }
}

pub fn local_ip() -> Option<String> {
    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(_) => return None,
    };

    match socket.connect("8.8.8.8:80") {
        Ok(()) => (),
        Err(_) => return None,
    };

    socket.local_addr().ok().map(|addr| addr.ip().to_string())
}

/// Heartbeat Builder
///
/// How to build heartbeat sender
/// HeartbeatBuilder::build is used to build a heartbeat sender. This method has three arguments:
/// - First Arg: the host address of remote node
/// - Second Arg: rpc connection timeout
/// - Third Arg: rpc request timeout
///
/// [HeartbeatSender] implements [Future] which can be ran by:
/// - Tokio spawning
/// - async/await
///
/// # Example of Tokio spawning
///
/// ```
/// use common::net::{HeartbeatBuilder, gateway:SafeTaskManagerRpcGateway};
///
/// #[tokio::main]
/// async fn main() {
///     let builder = HeartbeatBuilder {
///         node_addrs: vec![PersistableHostAddr {
///             host: "localhost".to_string(),
///             port: 8080
///         }],
///         period: 3,
///         connection_timeout: 3
///         rpc_timeout: 3
///     };
///     
///     let heartbeat = builder.build(|addr, connect_timeout, rpc_timeout| SafeTaskManagerRpcGateway::with_timeout(addr, connect_timeout, rpc_timeout));
///     let _ = tokio::spawn(heartbeat);
/// }
/// ```
///
/// # Example of async/await
///
/// ```
/// use common::net::{HeartbeatBuilder, gateway:SafeTaskManagerRpcGateway};
///
/// #[tokio::main]
/// async fn main() {
///     let builder = HeartbeatBuilder {
///         node_addrs: vec![PersistableHostAddr {
///             host: "localhost".to_string(),
///             port: 8080
///         }],
///         period: 3,
///         connection_timeout: 3
///         rpc_timeout: 3
///     };
///     
///     let heartbeat = builder.build(|addr, connect_timeout, rpc_timeout| SafeTaskManagerRpcGateway::with_timeout(addr, connect_timeout, rpc_timeout));
///     heartbeat.await
/// }
/// ```
#[derive(serde::Deserialize, Clone, Debug)]
pub struct HeartbeatBuilder {
    #[serde(default)]
    pub node_addrs: Vec<PersistableHostAddr>,
    /// period of heartbeat, in seconds
    pub period: u64,
    /// timeout of heartbeat rpc connection, in seconds
    pub connection_timeout: u64,
    /// timeout of heartbeat rpc request, in seconds
    pub rpc_timeout: u64,
}

impl HeartbeatBuilder {
    pub fn build<F: Fn(&HostAddr, u64, u64) -> T, T: ReceiveHeartbeatRpcGateway>(
        &self,
        f: F,
    ) -> HeartbeatSender<T> {
        HeartbeatSender {
            gateways: self
                .node_addrs
                .iter()
                .map(|addr| to_host_addr(addr))
                .map(|host_addr| f(&host_addr, self.connection_timeout, self.rpc_timeout))
                .collect(),
            interval: tokio::time::interval(Duration::from_secs(self.period)),
            execution_id: None,
            current_heartbeat_id: AtomicU64::default(),
        }
    }
}

pub struct HeartbeatSender<T: ReceiveHeartbeatRpcGateway> {
    gateways: Vec<T>,
    interval: tokio::time::Interval,
    execution_id: Option<ExecutionId>,
    current_heartbeat_id: AtomicU64,
}
impl<T: ReceiveHeartbeatRpcGateway> HeartbeatSender<T> {
    pub fn update_execution_id(&mut self, execution_id: ExecutionId) {
        self.execution_id = Some(execution_id)
    }
}

impl<T: ReceiveHeartbeatRpcGateway> Future for HeartbeatSender<T> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        loop {
            ready!(Pin::new(&mut this.interval).poll_tick(cx));
            let now = utils::times::now();
            tracing::debug!("heartbeat sent at time {:?}", now);

            while let Some(true) = this
                .gateways
                .iter()
                .map(|gateway| {
                    gateway.receive_heartbeat(Heartbeat {
                        heartbeat_id: this
                            .current_heartbeat_id
                            .fetch_add(1, atomic::Ordering::SeqCst),
                        timestamp: Some(prost_types::Timestamp {
                            seconds: now.timestamp(),
                            nanos: now.timestamp_subsec_nanos() as i32,
                        }),
                        node_type: NodeType::JobManager as i32,
                        execution_id: this.execution_id.clone(),
                    })
                })
                .into_iter()
                .map(|mut future| future.poll_unpin(cx).is_ready())
                .reduce(|a, b| a && b)
            {
                break;
            }
        }
    }
}

/// The builder of [AckResponder] which is also the configuration of ACK
///
/// AckResponderBuilder::build has three arguments:
/// - First Arg: the host address of remote node
/// - Second Arg: rpc connection timeout
/// - Third Arg: rpc request timeout
///
/// It will return two values:
/// - a new [AckResponder]
/// - a [mpsc::Sender] channel for [Ack] messages. Users can trigger ack by send an [Ack] message into it.
///
/// [AckResponder] implements [Future]. Users can run an [AckResponder] by:
/// - Tokio spawn
/// - async/await
///
/// # Example of Tokio spwan
/// ```
/// use common::net::{AckResponderBuilder, gateway:SafeTaskManagerRpcGateway};
///
/// #[tokio::main]
/// async fn main() {
///     let builder = AckResponderBuilder {
///         delay: 3,
///         buf_size: 10,
///         nodes: vec![PersistableHostAddr {
///             host: "localhost".to_string(),
///             port: 8080
///         }],
///         connection_timeout: 3,
///         rpc_timeout: 3
///     };
///     
///     let (responder, _) = builder.build(|addr, connect_timeout, rpc_timeout| SafeTaskManagerRpcGateway::with_timeout(addr, connect_timeout, rpc_timeout));
///     let _ = tokio::spawn(responder);
/// }
/// ```
///
/// # Example of Tokio spwan
/// ```
/// use common::net::{AckResponderBuilder, gateway:SafeTaskManagerRpcGateway};
///
/// #[tokio::main]
/// async fn main() {
///     let builder = AckResponderBuilder {
///         delay: 3,
///         buf_size: 10,
///         nodes: vec![PersistableHostAddr {
///             host: "localhost".to_string(),
///             port: 8080
///         }],
///         connection_timeout: 3,
///         rpc_timeout: 3
///     };
///     
///     let (responder, _) = builder.build(|addr, connect_timeout, rpc_timeout| SafeTaskManagerRpcGateway::with_timeout(addr, connect_timeout, rpc_timeout));
///     responder.await
/// }
/// ```
#[derive(serde::Deserialize, Clone, Debug)]
pub struct AckResponderBuilder {
    // deplay duration, in seconds
    pub delay: u64,
    // buffer ack queue size
    pub buf_size: usize,
    // ack nodes
    #[serde(default)]
    pub nodes: Vec<PersistableHostAddr>,
    /// timeout of ack rpc connection, in seconds
    pub connection_timeout: u64,
    /// timeout of ack rpc request, in seconds
    pub rpc_timeout: u64,
}

impl AckResponderBuilder {
    pub fn build<F: Fn(&PersistableHostAddr, u64, u64) -> T, T: ReceiveAckRpcGateway>(
        &self,
        f: F,
    ) -> (AckResponder<T>, mpsc::Sender<Ack>) {
        let (tx, rx) = mpsc::channel(self.buf_size);
        (
            AckResponder {
                delay_interval: tokio::time::interval(Duration::from_secs(self.delay)),
                recv: rx,
                gateway: self
                    .nodes
                    .iter()
                    .map(|addr| f(addr, self.connection_timeout, self.rpc_timeout))
                    .collect(),
            },
            tx,
        )
    }
}

pub struct AckResponder<T: ReceiveAckRpcGateway> {
    delay_interval: tokio::time::Interval,
    recv: mpsc::Receiver<Ack>,
    gateway: Vec<T>,
}

impl<T: ReceiveAckRpcGateway> Future for AckResponder<T> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        loop {
            ready!(Pin::new(&mut this.delay_interval).poll_tick(cx));
            this.delay_interval.reset();

            match this.recv.poll_recv(cx) {
                Poll::Ready(ack) => {
                    ack.into_iter().for_each(|ack| {
                        while let Some(true) = this
                            .gateway
                            .iter()
                            .map(|gateway| gateway.receive_ack(ack.clone()))
                            .into_iter()
                            .map(|mut future| future.poll_unpin(cx).is_ready())
                            .reduce(|a, b| a && b)
                        {
                            break;
                        }
                    });
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {

    use chrono::Duration;
    use proto::common::{ack::AckType, Ack, NodeType};

    use crate::net::gateway::MockRpcGateway;

    use super::{HeartbeatBuilder, PersistableHostAddr};

    #[test]
    pub fn test_local_ip() {
        use super::local_ip;
        let option = local_ip();
        assert!(option.is_some());
        println!("{}", option.unwrap())
    }

    #[test]
    pub fn test_to_host_addr() {
        let mut addr = super::PersistableHostAddr {
            host: "198.0.0.1".to_string(),
            port: 8970,
        };

        let host_addr = super::to_host_addr(&addr);
        assert_eq!(host_addr.host.as_str(), "198.0.0.1");
        assert_eq!(host_addr.port, 8970);

        assert_eq!(addr.as_uri().as_str(), "http://198.0.0.1:8970");
        assert!(addr.is_valid());

        addr.host = "".to_string();
        assert!(!addr.is_valid());

        addr.host = "198.0.0.1".to_string();
        addr.port = 0;
        assert!(!addr.is_valid());
    }

    #[test]
    pub fn test_hostname() {
        let host = super::hostname();
        assert!(host.is_some());
    }

    #[tokio::test]
    async fn test_ack_success() {
        use super::AckResponderBuilder;

        let builder = AckResponderBuilder {
            delay: 3,
            buf_size: 10,
            nodes: vec![],
            connection_timeout: 3,
            rpc_timeout: 3,
        };

        let (gateway, mut rx, _) = MockRpcGateway::new(builder.buf_size, 0);

        let (responder, tx) = builder.build(|_, _, _| gateway.clone());

        let handler = tokio::spawn(responder);
        // send first time
        {
            let result = tx
                .send(Ack {
                    timestamp: None,
                    ack_type: AckType::Heartbeat as i32,
                    node_type: NodeType::JobManager as i32,
                    execution_id: None,
                    request_id: None,
                })
                .await;
            let start = chrono::Utc::now();
            assert!(result.is_ok());

            let result = rx.recv().await;
            let end = chrono::Utc::now();
            assert_eq!(
                result,
                Some(Ack {
                    timestamp: None,
                    ack_type: AckType::Heartbeat as i32,
                    node_type: NodeType::JobManager as i32,
                    execution_id: None,
                    request_id: None,
                })
            );

            let duration = end - start;
            assert!(duration <= Duration::seconds(1))
        }

        // send second time
        {
            let result = tx
                .send(Ack {
                    timestamp: None,
                    ack_type: AckType::Heartbeat as i32,
                    node_type: NodeType::JobManager as i32,
                    execution_id: None,
                    request_id: None,
                })
                .await;
            assert!(result.is_ok());
            let start = chrono::Utc::now();

            let result = rx.recv().await;
            let end = chrono::Utc::now();
            assert_eq!(
                result,
                Some(Ack {
                    timestamp: None,
                    ack_type: AckType::Heartbeat as i32,
                    node_type: NodeType::JobManager as i32,
                    execution_id: None,
                    request_id: None,
                })
            );

            let duration = end - start;
            assert!(duration >= Duration::seconds(3))
        }

        handler.abort();
    }

    #[tokio::test]
    async fn test_heartbeat_success() {
        let builder = HeartbeatBuilder {
            node_addrs: vec![PersistableHostAddr {
                host: "11".to_string(),
                port: 11,
            }],
            period: 3,
            connection_timeout: 3,
            rpc_timeout: 3,
        };

        let (gateway, _, mut rx) = MockRpcGateway::new(0, 10);

        let heartbeat = builder.build(|_, _, _| gateway.clone());
        let handler = tokio::spawn(heartbeat);

        {
            let start = chrono::Utc::now();
            let result = rx.recv().await;
            let end = chrono::Utc::now();
            assert!(result.is_some());
            assert!(end - start <= Duration::seconds(1));
        }

        {
            let start = chrono::Utc::now();
            let result = rx.recv().await;
            let end = chrono::Utc::now();
            assert!(result.is_some());
            assert!(end - start >= Duration::seconds(3));
        }

        handler.abort()
    }
}
