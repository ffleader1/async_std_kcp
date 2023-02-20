use std::{
    collections::{hash_map::Entry, HashMap},
    net::SocketAddr,
    ops::Deref,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration
};


use byte_string::ByteStr;
use kcp::KcpResult;
use log::{error, trace};
use spin::Mutex as SpinMutex;

use futures::{FutureExt, select};
use async_notify::Notify;

use async_std::{
    task,
    net::{UdpSocket}
};

use futures::channel::mpsc::{channel, Sender};
use futures::stream::StreamExt;
use futures::sink::SinkExt;
use crate::{skcp::KcpSocket, KcpConfig};


pub struct KcpSession {
    socket: SpinMutex<KcpSocket>,
    closed: AtomicBool,
    session_expire: Duration,
    session_close_notifier: Option<(Sender<SocketAddr>, SocketAddr)>,
    input_tx: Sender<Vec<u8>>,
    notifier: Notify,
}

impl Drop for KcpSession {
    fn drop(&mut self) {
        trace!(
            "[SESSION] KcpSession conv {} is dropping, closed? {}",
            self.socket.lock().conv(),
            self.closed.load(Ordering::Acquire),
        );
    }
}


impl KcpSession {
    fn new(
        socket: KcpSocket,
        session_expire: Duration,
        session_close_notifier: Option<(Sender<SocketAddr>, SocketAddr)>,
        input_tx: Sender<Vec<u8>>,
    ) -> KcpSession {
        KcpSession {
            socket: SpinMutex::new(socket),
            closed: AtomicBool::new(false),
            session_expire,
            session_close_notifier,
            input_tx,
            notifier: Notify::new(),
        }
    }

    pub fn new_shared(
        socket: KcpSocket,
        session_expire: Duration,
        session_close_notifier: Option<(Sender<SocketAddr>, SocketAddr)>,
    ) -> Arc<KcpSession> {
        let is_client = session_close_notifier.is_none();

        let (input_tx, mut input_rx) = channel(64);

        let udp_socket = socket.udp_socket().clone();

        let session = Arc::new(KcpSession::new(
            socket,
            session_expire,
            session_close_notifier,
            input_tx,
        ));
        let io_task_handle = {
            let session = session.clone();
            task::spawn(async move {
                let mut input_buffer = [0u8; 5536];

                loop {
                    futures::select! {
                        // recv() then input()
                        // Drives the KCP machine forward

                        recv_result = udp_socket.recv(&mut input_buffer).fuse() => {
                            if is_client {
                                match recv_result {
                                    Err(err) => {
                                        error!("[SESSION] UDP recv failed, error: {}", err);
                                    }
                                    Ok(n) => {
                                        let input_buffer = &input_buffer[..n];
                                        let input_conv = kcp::get_conv(input_buffer);
                                        trace!("[SESSION] UDP recv {} bytes, conv: {}, going to input {:?}",
                                               n, input_conv, ByteStr::new(input_buffer));

                                        let mut socket = session.socket.lock();

                                        // Server may allocate another conv for this client.
                                        if !socket.waiting_conv() && socket.conv() != input_conv {
                                            trace!("[SESSION] UDP input conv: {} replaces session conv: {}", input_conv, socket.conv());
                                            socket.set_conv(input_conv);
                                        }

                                        match socket.input(input_buffer) {
                                            Ok(true) => {
                                                trace!("[SESSION] UDP input {} bytes and waked sender/receiver", n);
                                            }
                                            Ok(false) => {}
                                            Err(err) => {
                                                error!("[SESSION] UDP input {} bytes error: {:?}, input buffer {:?}",
                                                       n, err, ByteStr::new(input_buffer));
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // bytes received from listener socket

                        input_opt = input_rx.next().fuse() => {
                            if let Some(input_buffer) = input_opt {
                                let mut socket = session.socket.lock();
                                match socket.input(&input_buffer) {
                                    Ok(waked) => {
                                        // trace!("[SESSION] UDP input {} bytes from channel {:?}",
                                        //        input_buffer.len(), ByteStr::new(&input_buffer));
                                        trace!("[SESSION] UDP input {} bytes from channel, waked? {} sender/receiver",
                                               input_buffer.len(), waked);
                                    }
                                    Err(err) => {
                                        error!("[SESSION] UDP input {} bytes from channel failed, error: {:?}, input buffer {:?}",
                                               input_buffer.len(), err, ByteStr::new(&input_buffer));
                                    }
                                }
                            }
                        }
                    }
                }
            })
        };

        // Per-session updater
        {
            let session = session.clone();
            task::spawn(async move {
                while !session.closed.load(Ordering::Relaxed) {
                    let next = {
                        let mut socket = session.socket.lock();

                        let is_closed = session.closed.load(Ordering::Acquire);
                        if is_closed && socket.can_close() {
                            trace!("[SESSION] KCP session closing");
                            break;
                        }

                        // server socket expires
                        if !is_client {
                            // If this is a server stream, close it automatically after a period of time
                            let last_update_time = socket.last_update_time();
                            let elapsed = last_update_time.elapsed();

                            if elapsed > session.session_expire {
                                if elapsed > session.session_expire * 2 {
                                    // Force close. Client may have already gone.
                                    trace!(
                                        "[SESSION] force close inactive session, conv: {}, last_update: {}s ago",
                                        socket.conv(),
                                        elapsed.as_secs()
                                    );
                                    break;
                                }

                                if !is_closed {
                                    trace!(
                                        "[SESSION] closing inactive session, conv: {}, last_update: {}s ago",
                                        socket.conv(),
                                        elapsed.as_secs()
                                    );
                                    session.closed.store(true, Ordering::Release);
                                }
                            }
                        }

                        // If window is full, flush it immediately
                        if socket.need_flush() {
                            let _ = socket.flush();
                        }

                        match socket.update() {
                            Ok(next_next) => next_next,
                            Err(err) => {
                                error!("[SESSION] KCP update failed, error: {:?}", err);
                                Duration::from_millis(10)
                            }
                        }
                    };
                    select! {
                        _ = {task::sleep(next).await;futures::future::ready(4)} => {},
                        _ = session.notifier.notified().fuse() => {},
                    }
                }

                {
                    // Close the socket.
                    // Wake all pending tasks and let all send/recv return EOF

                    let mut socket = session.socket.lock();
                    socket.close();
                }

                if let Some((ref notifier, peer_addr)) = session.session_close_notifier {
                    let mut notifier_clone = notifier.clone();
                    let _ = notifier_clone.send(peer_addr).await;
                }

                session.closed.store(true, Ordering::Release);
                io_task_handle.cancel().await;

                trace!("[SESSION] KCP session closed");
            });
        }

        session
    }

    pub fn kcp_socket(&self) -> &SpinMutex<KcpSocket> {
        &self.socket
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.notify();
    }


    pub async fn input(&self, buf: &[u8]) {
        let mut input_tx = self.input_tx.clone();
        input_tx.send(buf.to_owned()).await.expect("input channel closed")
    }

    pub async fn conv(&self) -> u32 {
        let socket = self.socket.lock();
        socket.conv()
    }

    pub fn notify(&self) {
        self.notifier.notify();
    }
}

struct KcpSessionUniq(Arc<KcpSession>);

impl Drop for KcpSessionUniq {
    fn drop(&mut self) {
        self.0.close();
    }
}

impl Deref for KcpSessionUniq {
    type Target = KcpSession;

    fn deref(&self) -> &KcpSession {
        &*self.0
    }
}

pub struct KcpSessionManager {
    sessions: HashMap<SocketAddr, KcpSessionUniq>,
}

impl KcpSessionManager {
    pub fn new() -> KcpSessionManager {
        KcpSessionManager {
            sessions: HashMap::new(),
        }
    }

    #[inline]
    pub fn alloc_conv(&mut self) -> u32 {
        rand::random()
    }

    pub fn close_peer(&mut self, peer_addr: SocketAddr) {
        self.sessions.remove(&peer_addr);
    }

    pub async fn get_or_create(
        &mut self,
        config: &KcpConfig,
        conv: u32,
        sn: u32,
        udp: &Arc<UdpSocket>,
        peer_addr: SocketAddr,
        session_close_notifier: &Sender<SocketAddr>,
    ) -> KcpResult<(Arc<KcpSession>, bool)> {
        match self.sessions.entry(peer_addr) {
            Entry::Occupied(mut occ) => {
                let session = occ.get();

                if sn == 0 && session.conv().await != conv {
                    // This is the first packet received from this peer.
                    // Recreate a new session for this specific client.

                    let socket = KcpSocket::new(config, conv, udp.clone(), peer_addr, config.stream)?;
                    let session = KcpSession::new_shared(
                        socket,
                        config.session_expire,
                        Some((session_close_notifier.clone(), peer_addr)),
                    );

                    let old_session = occ.insert(KcpSessionUniq(session.clone()));
                    let old_conv = old_session.conv().await;
                    trace!(
                        "replaced session with conv: {} (old: {}), peer: {}",
                        conv,
                        old_conv,
                        peer_addr
                    );

                    Ok((session, true))
                } else {
                    Ok((session.0.clone(), false))
                }
            }
            Entry::Vacant(vac) => {
                let socket = KcpSocket::new(config, conv, udp.clone(), peer_addr, config.stream)?;
                let session = KcpSession::new_shared(
                    socket,
                    config.session_expire,
                    Some((session_close_notifier.clone(), peer_addr)),
                );
                trace!("created session for conv: {}, peer: {}", conv, peer_addr);
                vac.insert(KcpSessionUniq(session.clone()));
                Ok((session, true))
            }
        }
    }
}
