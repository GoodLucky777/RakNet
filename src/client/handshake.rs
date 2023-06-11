use std::sync::Arc;
use std::sync::Mutex;

#[cfg(feature = "async_std")]
use async_std::{
    future::Future,
    net::UdpSocket,
    task::Poll,
    task::Waker,
    task::{self},
};

use binary_utils::Streamable;
#[cfg(feature = "async_tokio")]
use tokio::{
    future::Future,
    net::UdpSocket,
    task::Poll,
    task::Waker,
    task::{self},
};

use crate::connection::queue::send::SendQueue;
use crate::connection::queue::RecvQueue;
use crate::protocol::frame::FramePacket;
use crate::protocol::packet::offline::{
    IncompatibleProtocolVersion, OpenConnectReply, OpenConnectRequest, SessionInfoReply,
    SessionInfoRequest,
};
use crate::protocol::packet::online::{ConnectionRequest, NewConnection, OnlinePacket};
use crate::protocol::packet::Packet;
use crate::protocol::packet::PacketId;
use crate::protocol::reliability::Reliability;
use crate::protocol::Magic;
use crate::rakrs_debug;
use crate::server::current_epoch;

macro_rules! match_ids {
    ($socket: expr, $($ids: expr),*) => {
        {
            let mut recv_buf: [u8; 2048] = [0; 2048];
            let mut tries: u8 = 0;
            let ids = vec![$($ids),*];
            let mut pk: Option<Vec<u8>> = None;

            loop {
                if (tries >= 5) {
                    break;
                }

                let len: usize;
                let rc = $socket.recv(&mut recv_buf).await;

                match rc {
                    Err(_) => {
                        tries += 1;
                        continue;
                    },
                    Ok(l) => len = l
                };

                if ids.contains(&recv_buf[0]) {
                    pk = Some(recv_buf[..len].to_vec());
                    break;
                }
            }

            pk
        }
    };
}

macro_rules! expect_reply {
    ($socket: expr, $reply: ty) => {{
        let mut recv_buf: [u8; 2048] = [0; 2048];
        let mut tries: u8 = 0;
        let mut pk: Option<$reply> = None;

        loop {
            if (tries >= 5) {
                break;
            }

            let len: usize;
            let rc = $socket.recv(&mut recv_buf).await;

            match rc {
                Err(_) => {
                    tries += 1;
                    continue;
                }
                Ok(l) => len = l,
            };

            if let Ok(packet) = <$reply>::compose(&mut recv_buf[1..len], &mut 0) {
                pk = Some(packet);
                break;
            }
        }

        pk
    }};
}

macro_rules! update_state {
    ($done: expr, $shared_state: expr, $state: expr) => {{
        let mut state = $shared_state.lock().unwrap();
        state.status = $state;
        state.done = true;
        if let Some(waker) = state.waker.take() {
            waker.wake();
        }
        return;
    }};
    ($shared_state: expr, $state: expr) => {{
        let mut state = $shared_state.lock().unwrap();
        state.status = $state;
        state.done = false;
        if let Some(waker) = state.waker.take() {
            waker.wake();
        }
    }};
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HandshakeStatus {
    Created,
    Opening,
    SessionOpen,
    Failed,
    IncompatibleVersion,
    Completed,
}

struct HandshakeState {
    status: HandshakeStatus,
    done: bool,
    waker: Option<Waker>,
}

pub struct ClientHandshake {
    status: Arc<Mutex<HandshakeState>>,
}

impl ClientHandshake {
    pub fn new(socket: Arc<UdpSocket>, id: i64, version: u8, mtu: u16, attempts: u8) -> Self {
        let state = Arc::new(Mutex::new(HandshakeState {
            done: false,
            status: HandshakeStatus::Created,
            waker: None,
        }));

        let shared_state = state.clone();

        task::spawn(async move {
            let connect_request = OpenConnectRequest {
                magic: Magic::new(),
                protocol: version,
                mtu_size: mtu,
            };

            update_state!(shared_state, HandshakeStatus::Opening);

            send_packet(&socket, connect_request.into()).await;
            let reply = match_ids!(
                socket.clone(),
                OpenConnectReply::id(),
                IncompatibleProtocolVersion::id()
            );

            if reply.is_none() {
                update_state!(true, shared_state, HandshakeStatus::Failed);
            }

            if let Ok(_) =
                IncompatibleProtocolVersion::compose(&mut reply.clone().unwrap()[1..], &mut 0)
            {
                update_state!(true, shared_state, HandshakeStatus::IncompatibleVersion);
            }

            let open_reply = OpenConnectReply::compose(&mut reply.unwrap()[1..], &mut 0);

            if open_reply.is_err() {
                let mut state = shared_state.lock().unwrap();
                state.status = HandshakeStatus::Failed;
                state.done = true;
                if let Some(waker) = state.waker.take() {
                    waker.wake();
                }
                return;
            }

            let session_info = SessionInfoRequest {
                magic: Magic::new(),
                address: socket.peer_addr().unwrap(),
                mtu_size: mtu,
                client_id: id,
            };

            update_state!(shared_state, HandshakeStatus::SessionOpen);

            send_packet(&socket, session_info.into()).await;

            let session_reply = expect_reply!(socket, SessionInfoReply);

            if session_reply.is_none() {
                update_state!(true, shared_state, HandshakeStatus::Failed);
            }

            let session_reply = session_reply.unwrap();

            if session_reply.mtu_size != mtu {
                update_state!(true, shared_state, HandshakeStatus::Failed);
            }

            // create a temporary sendq
            let mut send_q = SendQueue::new(
                mtu,
                5000,
                attempts.clone().into(),
                socket.clone(),
                socket.peer_addr().unwrap(),
            );
            let mut recv_q = RecvQueue::new();

            let connect_request = ConnectionRequest {
                time: current_epoch() as i64,
                client_id: id,
            };

            if let Err(_) = send_q
                .insert(
                    Packet::from(connect_request).parse().unwrap(),
                    Reliability::ReliableOrd,
                    true,
                    None,
                )
                .await
            {
                update_state!(true, shared_state, HandshakeStatus::Failed);
            }

            let mut buf: [u8; 2048] = [0; 2048];

            loop {
                let len: usize;
                let rec = socket.recv_from(&mut buf).await;

                match rec {
                    Err(_) => {
                        continue;
                    }
                    Ok((l, _)) => len = l,
                };

                // proccess frame packet
                match buf[0] {
                    0x80..=0x8d => {
                        if let Ok(pk) = FramePacket::compose(&mut buf[..len], &mut 0) {
                            recv_q.insert(pk).unwrap();

                            let raw_packets = recv_q.flush();

                            for mut raw_pk in raw_packets {
                                let pk = Packet::compose(&mut raw_pk[..], &mut 0);

                                if let Ok(pk) = pk {
                                    if pk.is_online() {
                                        match pk.get_online() {
                                            OnlinePacket::ConnectionAccept(pk) => {
                                                // send new incoming connection
                                                let new_incoming = NewConnection {
                                                    server_address: socket.peer_addr().unwrap(),
                                                    system_address: socket.local_addr().unwrap(),
                                                    request_time: pk.request_time,
                                                    timestamp: pk.timestamp,
                                                };
                                                if let Err(_) = send_q
                                                    .insert(
                                                        Packet::from(new_incoming).parse().unwrap(),
                                                        Reliability::ReliableOrd,
                                                        true,
                                                        None,
                                                    )
                                                    .await
                                                {
                                                    update_state!(
                                                        true,
                                                        shared_state,
                                                        HandshakeStatus::Failed
                                                    );
                                                } else {
                                                    update_state!(
                                                        true,
                                                        shared_state,
                                                        HandshakeStatus::Completed
                                                    );
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        });

        Self { status: state }
    }
}

impl Future for ClientHandshake {
    type Output = HandshakeStatus;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Self::Output> {
        // see if we can finish
        let mut state = self.status.lock().unwrap();

        if state.done {
            return Poll::Ready(state.status);
        } else {
            state.waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
    }
}

async fn send_packet(socket: &Arc<UdpSocket>, packet: Packet) {
    if let Err(e) = socket.send(&mut packet.parse().unwrap()[..]).await {
        rakrs_debug!("[CLIENT] Failed sending payload to server! {}", e);
    }
}
