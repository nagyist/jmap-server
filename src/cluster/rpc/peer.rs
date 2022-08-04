use futures::{stream::StreamExt, SinkExt};
use std::time::Instant;
use std::{net::SocketAddr, time::Duration};
use store::rand::Rng;
use store::tracing::{debug, error};
use tokio::sync::watch;
use tokio::{
    net::TcpStream,
    sync::mpsc,
    time::{self},
};
use tokio_util::codec::Framed;

use crate::cluster::rpc::{
    Request, Response, RPC_INACTIVITY_TIMEOUT, RPC_MAX_BACKOFF_MS, RPC_MAX_CONNECT_ATTEMPTS,
};
use crate::cluster::{Event, PeerId, IPC_CHANNEL_BUFFER};

use super::serialize::RpcEncoder;
use super::{Protocol, RpcEvent, RPC_TIMEOUT_MS};

pub fn spawn_peer_rpc(
    main_tx: mpsc::Sender<Event>,
    local_peer_id: PeerId,
    key: String,
    peer_id: PeerId,
    peer_addr: SocketAddr,
) -> (mpsc::Sender<RpcEvent>, watch::Receiver<bool>) {
    let (event_tx, mut event_rx) = mpsc::channel::<RpcEvent>(IPC_CHANNEL_BUFFER);
    let (online_tx, online_rx) = watch::channel(false);

    tokio::spawn(async move {
        let mut conn_ = None;
        let mut is_online = false;

        'main: loop {
            let mut message = match time::timeout(
                Duration::from_millis(RPC_INACTIVITY_TIMEOUT),
                event_rx.recv(),
            )
            .await
            {
                Ok(Some(message)) => message,
                Ok(None) => {
                    debug!("Peer RPC process with {} exiting.", peer_addr);
                    break;
                }
                Err(_) => {
                    // Close connection after the configured inactivity timeout.
                    if conn_.is_some() {
                        debug!("Closing inactive connection to peer {}.", peer_addr);
                        conn_ = None;
                    }
                    continue;
                }
            };

            // Connect to peer if we are not already connected.
            let conn = if let Some(conn) = &mut conn_ {
                conn
            } else {
                let mut connection_attempts = 0;

                'retry: loop {
                    // Connect and authenticate with peer.
                    match connect_peer(
                        peer_addr,
                        Request::Auth {
                            peer_id: local_peer_id,
                            key: key.clone(),
                        },
                    )
                    .await
                    {
                        Ok(conn) => {
                            conn_ = conn.into();

                            // Notify processes that the peer is online.
                            if !is_online {
                                is_online = true;
                                if online_tx.send(true).is_err() {
                                    debug!("Failed to send online status.");
                                }
                            }

                            if connection_attempts < RPC_MAX_CONNECT_ATTEMPTS {
                                // Connection established, send message.
                                break 'retry;
                            } else {
                                // Connection established, but we have already notified the task the current
                                // message was undeliverable. Continue with the next message on the queue.
                                continue 'main;
                            }
                        }
                        Err(err) => {
                            // Keep retrying.
                            connection_attempts += 1;

                            if connection_attempts == RPC_MAX_CONNECT_ATTEMPTS {
                                // Give up trying to deliver the message,
                                // notify task that the message could not be sent.
                                message.failed();
                                message = RpcEvent::FireAndForget {
                                    request: Request::None,
                                };
                            }

                            // Truncated exponential backoff
                            let mut next_attempt_ms = std::cmp::min(
                                2u64.pow(connection_attempts)
                                    + store::rand::thread_rng().gen_range(0..1000),
                                RPC_MAX_BACKOFF_MS,
                            );

                            error!(
                                "Failed to connect to peer {} ({}), retrying in {} ms.",
                                peer_addr, err, next_attempt_ms
                            );

                            // Reject messages while we wait to reconnect.
                            'wait: loop {
                                let timer = Instant::now();

                                match time::timeout(
                                    Duration::from_millis(next_attempt_ms),
                                    event_rx.recv(),
                                )
                                .await
                                {
                                    Ok(Some(new_message)) => {
                                        match new_message {
                                            new_message @ RpcEvent::FireAndForget {
                                                request: Request::UpdatePeers { .. } | Request::Ping,
                                            } => {
                                                // Peer requested to update peer list via gossip, which means that
                                                // it is probably back online, attempt to reconnect.
                                                message = new_message;
                                                connection_attempts = 0;
                                                continue 'retry;
                                            }
                                            _ => {
                                                // Do not accept new messages until we are able to reconnect.
                                                new_message.failed();
                                            }
                                        }
                                    }
                                    Ok(None) => {
                                        // RPC process was ended.
                                        debug!("Peer RPC process with {} exiting.", peer_addr);
                                        break 'main;
                                    }
                                    Err(_) => {
                                        // Timeout reached, attempt to reconnect.
                                        break 'wait;
                                    }
                                }

                                // Continue waiting to reconnect.
                                let elapsed_ms = timer.elapsed().as_millis() as u64;
                                if next_attempt_ms > elapsed_ms {
                                    next_attempt_ms -= elapsed_ms;
                                } else {
                                    break 'wait;
                                }
                            }

                            continue 'retry;
                        }
                    }
                }

                conn_.as_mut().unwrap()
            };

            let err = match message {
                RpcEvent::NeedResponse {
                    response_tx,
                    request,
                } => match send_rpc(conn, request).await {
                    Ok(response) => {
                        // Send response via oneshot channel
                        if response_tx.send(response).is_err() {
                            error!("Channel failed while sending message.");
                        }
                        continue;
                    }
                    Err(err) => {
                        if response_tx.send(Response::None).is_err() {
                            error!("Channel failed while sending message.");
                        }
                        err
                    }
                },
                RpcEvent::FireAndForget { request } => match send_rpc(conn, request).await {
                    Ok(response) => {
                        // Send response via the main channel
                        if let Err(err) =
                            main_tx.send(Event::RpcResponse { peer_id, response }).await
                        {
                            error!("Channel failed while sending message: {}", err);
                        }
                        continue;
                    }
                    Err(err) => err,
                },
            };

            debug!("Failed to send RPC request to peer {}: {}", peer_addr, err);
            conn_ = None;

            // Notify processes that the peer is offline.
            is_online = false;
            if online_tx.send(false).is_err() {
                debug!("Failed to send online status.");
            }
        }
    });

    (event_tx, online_rx)
}

async fn connect_peer(
    addr: SocketAddr,
    auth_frame: Request,
) -> std::io::Result<Framed<TcpStream, RpcEncoder>> {
    time::timeout(Duration::from_millis(RPC_TIMEOUT_MS), async {
        let mut conn = Framed::new(TcpStream::connect(&addr).await?, RpcEncoder::default());
        if let Response::Pong = send_rpc(&mut conn, auth_frame).await? {
            Ok(conn)
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Failed to authenticate peer.",
            ))
        }
    })
    .await
    .map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("RPC connection to {} timed out.", addr),
        )
    })?
}

async fn send_rpc(
    conn: &mut Framed<TcpStream, RpcEncoder>,
    request: Request,
) -> std::io::Result<Response> {
    conn.send(Protocol::Request(request)).await?;
    match conn.next().await {
        Some(Ok(Protocol::Response(response))) => Ok(response),
        Some(Ok(invalid)) => Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("Received invalid RPC response: {:?}", invalid),
        )),
        Some(Err(err)) => Err(err),
        None => Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "RPC connection unexpectedly closed.",
        )),
    }
}