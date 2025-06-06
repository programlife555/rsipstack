use crate::{
    transport::{
        connection::{TransportSender, KEEPALIVE_REQUEST, KEEPALIVE_RESPONSE},
        sip_addr::SipAddr,
        stream::{send_raw_to_stream, send_to_stream, StreamConnection},
        SipConnection, TransportEvent,
    },
    Result,
};
use rsip::SipMessage;
use std::{fmt, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Mutex,
};
use tracing::{debug, error, info};
pub struct TcpInner {
    pub local_addr: SipAddr,
    pub remote_addr: Option<SipAddr>,
    pub read_half: Arc<Mutex<tokio::io::ReadHalf<TcpStream>>>,
    pub write_half: Arc<Mutex<tokio::io::WriteHalf<TcpStream>>>,
}

#[derive(Clone)]
pub struct TcpConnection {
    pub inner: Arc<TcpInner>,
}

impl TcpConnection {
    pub async fn connect(remote: &SipAddr) -> Result<Self> {
        let socket_addr = remote.get_socketaddr()?;
        let stream = TcpStream::connect(socket_addr).await?;

        let local_addr = SipAddr {
            r#type: Some(rsip::transport::Transport::Tcp),
            addr: remote.addr.clone(),
        };

        let (read_half, write_half) = tokio::io::split(stream);

        let connection = TcpConnection {
            inner: Arc::new(TcpInner {
                local_addr,
                remote_addr: Some(remote.clone()),
                read_half: Arc::new(Mutex::new(read_half)),
                write_half: Arc::new(Mutex::new(write_half)),
            }),
        };

        info!(
            "Created TCP client connection: {} -> {}",
            connection.get_addr(),
            remote
        );

        Ok(connection)
    }

    pub async fn from_stream(stream: TcpStream, local_addr: SipAddr) -> Result<Self> {
        let remote_addr = stream.peer_addr()?;
        let remote_sip_addr = SipAddr {
            r#type: Some(rsip::transport::Transport::Tcp),
            addr: remote_addr.into(),
        };

        let (read_half, write_half) = tokio::io::split(stream);

        let connection = TcpConnection {
            inner: Arc::new(TcpInner {
                local_addr,
                remote_addr: Some(remote_sip_addr),
                read_half: Arc::new(Mutex::new(read_half)),
                write_half: Arc::new(Mutex::new(write_half)),
            }),
        };

        info!(
            "Created TCP server connection: {} <- {}",
            connection.get_addr(),
            remote_addr
        );

        Ok(connection)
    }

    pub async fn create_listener(local: std::net::SocketAddr) -> Result<(TcpListener, SipAddr)> {
        let listener = TcpListener::bind(local).await?;
        let local_addr = listener.local_addr()?;

        let sip_addr = SipAddr {
            r#type: Some(rsip::transport::Transport::Tcp),
            addr: local_addr.into(),
        };

        info!("Created TCP listener on {}", local_addr);

        Ok((listener, sip_addr))
    }

    pub async fn serve_listener(
        listener: TcpListener,
        local_addr: SipAddr,
        sender: TransportSender,
    ) -> Result<()> {
        info!("Starting TCP listener on {}", local_addr);

        loop {
            match listener.accept().await {
                Ok((stream, remote_addr)) => {
                    debug!("New TCP connection from {}", remote_addr);

                    let tcp_connection =
                        TcpConnection::from_stream(stream, local_addr.clone()).await?;
                    let sip_connection = SipConnection::Tcp(tcp_connection.clone());

                    let sender_clone = sender.clone();

                    tokio::spawn(async move {
                        if let Err(e) = tcp_connection.serve_loop(sender_clone).await {
                            error!("Error handling TCP connection: {:?}", e);
                        }
                    });

                    if let Err(e) = sender.send(TransportEvent::New(sip_connection)) {
                        error!("Error sending new connection event: {:?}", e);
                    }
                }
                Err(e) => {
                    error!("Error accepting TCP connection: {}", e);
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl StreamConnection for TcpConnection {
    fn get_addr(&self) -> &SipAddr {
        &self.inner.local_addr
    }

    async fn send_message(&self, msg: SipMessage) -> Result<()> {
        info!("TcpConnection send:{}", msg);
        send_to_stream(&self.inner.write_half, msg).await
    }

    async fn send_raw(&self, data: &[u8]) -> Result<()> {
        send_raw_to_stream(&self.inner.write_half, data).await
    }

    async fn serve_loop(&self, sender: TransportSender) -> Result<()> {
        let mut buf = vec![0u8; 2048];
        let sip_connection = SipConnection::Tcp(self.clone());
        let remote_addr = self.inner.remote_addr.clone().unwrap().clone();
        let mut read_half = self.inner.read_half.lock().await;
        loop {
            let len = read_half.read(&mut buf).await?;
            if len <= 0 {
                continue;
            }

            match &buf[..len] {
                KEEPALIVE_REQUEST => {
                    self.send_raw(KEEPALIVE_RESPONSE).await?;
                    continue;
                }
                KEEPALIVE_RESPONSE => continue,
                _ => {
                    if buf.iter().all(|&b| b.is_ascii_whitespace()) {
                        continue;
                    }
                }
            }

            let undecoded = match std::str::from_utf8(&buf[..len]) {
                Ok(s) => s,
                Err(e) => {
                    info!("decoding text ferror: {} buf: {:?}", e, &buf[..len]);
                    continue;
                }
            };

            let sip_msg = match rsip::SipMessage::try_from(undecoded) {
                Ok(msg) => msg,
                Err(e) => {
                    info!("error parsing SIP message error: {} buf: {}", e, undecoded);
                    continue;
                }
            };

            if let Err(e) = sender.send(TransportEvent::Incoming(
                sip_msg,
                sip_connection.clone(),
                remote_addr.clone(),
            )) {
                error!("Error sending incoming message: {:?}", e);
                break;
            }
        }
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        let mut write_half = self.inner.write_half.lock().await;
        write_half.shutdown().await?;
        Ok(())
    }
}

impl fmt::Display for TcpConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner.remote_addr {
            Some(remote) => write!(f, "TCP {} -> {}", self.inner.local_addr, remote),
            None => write!(f, "TCP {}", self.inner.local_addr),
        }
    }
}

impl fmt::Debug for TcpConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}
