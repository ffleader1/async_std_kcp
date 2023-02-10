
use std::{
    io::{self, ErrorKind},
    net::SocketAddr,
    sync::Arc,
    time::Duration,
    mem::take
};

use byte_string::ByteStr;
use kcp::{Error as KcpError, KcpResult};
use log::{debug, error, trace};

use futures::{select, FutureExt};
use async_std::task;
use async_std::{
    net::{ToSocketAddrs, UdpSocket},
    task::JoinHandle
};

use futures::channel::mpsc::{channel, Receiver};
use futures::executor::block_on;
use futures::stream::StreamExt;


use crate::{config::KcpConfig, session::KcpSessionManager, stream::KcpStream};
// use crate::session::KcpSession;

pub struct KcpListener {
    udp: Arc<UdpSocket>,
    accept_rx: Receiver<(KcpStream, SocketAddr)>,
    task_watcher: Option<JoinHandle<()>>,
}

impl Drop for KcpListener {
    fn drop(&mut self) {
        let old_task_watcher = take( &mut self.task_watcher);
        block_on(old_task_watcher.unwrap().cancel());
    }
}

impl KcpListener {
    /// Create an `KcpListener` bound to `addr`
    pub async fn bind<A: ToSocketAddrs>(config: KcpConfig, addr: A) -> KcpResult<KcpListener> {
        let udp = UdpSocket::bind(addr).await?;
        KcpListener::from_socket(config, udp).await
    }

    /// Create a `KcpListener` from an existed `UdpSocket`
    pub async fn from_socket(config: KcpConfig, udp: UdpSocket) -> KcpResult<KcpListener> {
        let udp = Arc::new(udp);
        let server_udp = udp.clone();

        let (mut accept_tx, accept_rx) = channel(1024 /* backlogs */);
        let task_watcher = task::spawn(async move {
            let (close_tx, mut close_rx) = channel(64);

            let mut sessions = KcpSessionManager::new();
            let mut packet_buffer = [0u8; 2048];
            loop {
                select! {
                    peer_addr = close_rx.next().fuse() => {
                        let peer_addr = peer_addr.expect("close_tx closed unexpectly");
                        sessions.close_peer(peer_addr);
                        trace!("session peer_addr: {} removed", peer_addr);
                    }

                    recv_res = udp.recv_from(&mut packet_buffer).fuse() => {
                        match recv_res {
                            Err(err) => {
                                error!("udp.recv_from failed, error: {}", err);
                                task::sleep(Duration::from_secs(1)).await;
                            }
                            Ok((n, peer_addr)) => {
                                let packet = &mut packet_buffer[..n];

                                log::trace!("received peer: {}, {:?}", peer_addr, ByteStr::new(packet));

                                let mut conv = kcp::get_conv(packet);
                                if conv == 0 {
                                    // Allocate a conv for client.
                                    conv = sessions.alloc_conv();
                                    debug!("allocate {} conv for peer: {}", conv, peer_addr);

                                    kcp::set_conv(packet, conv);
                                }

                                let sn = kcp::get_sn(packet);

                                let session = match sessions.get_or_create(&config, conv, sn, &udp, peer_addr, &close_tx).await {
                                    Ok((s, created)) => {
                                        if created {
                                            // Created a new session, constructed a new accepted client
                                            let stream = KcpStream::with_session(s.clone());
                                            if let Err(..) = accept_tx.try_send((stream, peer_addr)) {
                                                debug!("failed to create accepted stream due to channel failure");

                                                // remove it from session
                                                sessions.close_peer(peer_addr);
                                                continue;
                                            }
                                        } else {
                                            let session_conv = s.conv().await;
                                            if session_conv != conv {
                                                debug!("received peer: {} with conv: {} not match with session conv: {}",
                                                       peer_addr,
                                                       conv,
                                                       session_conv);
                                                continue;
                                            }
                                        }

                                        s
                                    },
                                    Err(err) => {
                                        error!("failed to create session, error: {:?}, peer: {}, conv: {}", err, peer_addr, conv);
                                        continue;
                                    }
                                };

                                // let mut kcp = session.kcp_socket().lock().await;
                                // if let Err(err) = kcp.input(packet) {
                                //     error!("kcp.input failed, peer: {}, conv: {}, error: {}, packet: {:?}", peer_addr, conv, err, ByteStr::new(packet));
                                // }
                                session.input(packet).await;
                            }
                        }
                    }
                }
            }
        }
        );

        Ok(KcpListener {
            udp: server_udp,
            accept_rx,
            task_watcher: Some(task_watcher),
        })
    }

    /// Accept a new connected `KcpStream`
    pub async fn accept(&mut self) -> KcpResult<(KcpStream, SocketAddr)> {
        loop {
            match self.accept_rx.try_next() {
                Err(_) =>{
                    continue
                }
                Ok(v) => {
                    return match v {
                        Some(s) => Ok(s),
                        None => Err(KcpError::IoError(io::Error::new(
                            ErrorKind::Other,
                            "accept channel closed unexpectly",
                        ))),
                    }
                }

            }
        }

    }

    /// Get the local address of the underlying socket
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.udp.local_addr()
    }
}

#[cfg(unix)]
impl std::os::unix::io::AsRawFd for KcpListener {
    fn as_raw_fd(&self) -> std::os::unix::prelude::RawFd {
        self.udp.unwrap().as_raw_fd()
    }
}

#[cfg(windows)]
impl std::os::windows::io::AsRawSocket for KcpListener {
    fn as_raw_socket(&self) -> std::os::windows::prelude::RawSocket {
        self.udp.as_raw_socket()
    }
}

#[cfg(test)]
mod test {
    use super::KcpListener;
    use crate::{config::KcpConfig, stream::KcpStream};
    use futures::future;
    use futures::io::{AsyncReadExt, AsyncWriteExt};

    #[async_std::test]
    async fn multi_echo() {
        let _ = env_logger::try_init();

        let config = KcpConfig::default();

        let mut listener = KcpListener::bind(config, "127.0.0.1:0").await.unwrap();
        let server_addr = listener.local_addr().unwrap();
        async_std::task::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                async_std::task::spawn(async move {
                    let mut buffer = [0u8; 8192];
                    while let Ok(n) = stream.read(&mut buffer).await {
                        if n == 0 {
                            break;
                        }
                        let data = &buffer[..n];
                        stream.write_all(data).await.unwrap();
                        stream.flush().await.unwrap();
                    }
                });
            }
        });

        let mut vfut = Vec::new();

        for _ in 0..3 {
            vfut.push(async move {
                let mut stream = KcpStream::connect(&config, server_addr).await.unwrap();
                for _ in 0..2 {

                    const SEND_BUFFER: &[u8] = b"HELLO WORLD";
                    stream.write_all(SEND_BUFFER).await.unwrap();
                    stream.flush().await.unwrap();
                    let mut buffer = [0u8; 1024];
                    let n = stream.recv(&mut buffer).await.unwrap();
                    assert_eq!(SEND_BUFFER, &buffer[..n]);


                }
            });
        }

        future::join_all(vfut).await;
    }
}
