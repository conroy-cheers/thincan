use crate::frame::UnixFrame;
use crate::wire::{
    ACK_LEN, ACK_NO_ACK, ACK_OK, ACK_SERVER_ERR, FRAME_HDR_LEN, MAX_DATA_LEN, MSG_FILTERS_ACK,
    MSG_FRAME, MSG_SEND_FRAME, MSG_SET_FILTERS, decode_filters, decode_send_frame, encode_ack_into,
    encode_frame_into, read_msg_into, write_msg,
};
use embedded_can::Id;
use embedded_can_interface::{Id as IfaceId, IdMask, IdMaskFilter};
use std::collections::HashMap;
use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

/// A Unix-domain CAN bus server hosting a shared simulated bus.
pub struct BusServer {
    path: PathBuf,
    cmd_tx: Sender<BusCommand>,
    shutdown_tx: Sender<()>,
    accept_thread: Option<thread::JoinHandle<()>>,
    bus_thread: Option<thread::JoinHandle<()>>,
}

impl BusServer {
    /// Start a new bus server bound to the provided socket path.
    pub fn start(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;

        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();

        let bus_thread = thread::spawn(move || bus_loop(cmd_rx));

        let accept_cmd = cmd_tx.clone();
        let accept_thread = thread::spawn(move || accept_loop(listener, accept_cmd, shutdown_rx));

        Ok(Self {
            path,
            cmd_tx,
            shutdown_tx,
            accept_thread: Some(accept_thread),
            bus_thread: Some(bus_thread),
        })
    }

    /// Shut down the server and remove the socket path.
    pub fn shutdown(&mut self) -> io::Result<()> {
        let _ = self.shutdown_tx.send(());
        let _ = self.cmd_tx.send(BusCommand::Shutdown);
        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.bus_thread.take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_file(&self.path);
        Ok(())
    }
}

impl Drop for BusServer {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

type ClientId = u64;

enum BusCommand {
    AddClient {
        id: ClientId,
        tx: Sender<ServerMsg>,
    },
    RemoveClient {
        id: ClientId,
    },
    SendFrame {
        id: ClientId,
        seq: u64,
        expect_ack: bool,
        frame: UnixFrame,
    },
    SetFilters {
        id: ClientId,
        seq: u64,
        filters: Vec<IdMaskFilter>,
    },
    Shutdown,
}

enum ServerMsg {
    Frame(UnixFrame),
    Ack { seq: u64, status: u8 },
    Hello,
    FiltersAck { seq: u64, status: u8 },
}

struct ClientState {
    tx: Sender<ServerMsg>,
    filters: Vec<IdMaskFilter>,
}

struct PendingFilters {
    seq: u64,
    filters: Vec<IdMaskFilter>,
}

fn accept_loop(listener: UnixListener, cmd_tx: Sender<BusCommand>, shutdown_rx: Receiver<()>) {
    let next_id = Arc::new(std::sync::atomic::AtomicU64::new(1));
    loop {
        if shutdown_rx.try_recv().is_ok() {
            break;
        }
        match listener.accept() {
            Ok((stream, _addr)) => {
                let id = next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if let Err(err) = handle_client(stream, id, &cmd_tx) {
                    eprintln!("embedded-can-unix-socket: accept failed: {err}");
                }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(err) => {
                eprintln!("embedded-can-unix-socket: listener error: {err}");
                break;
            }
        }
    }
}

fn handle_client(stream: UnixStream, id: ClientId, cmd_tx: &Sender<BusCommand>) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    let read_stream = stream.try_clone()?;
    let (client_tx, client_rx) = mpsc::channel();
    cmd_tx
        .send(BusCommand::AddClient {
            id,
            tx: client_tx.clone(),
        })
        .ok();

    let cmd_tx_reader = cmd_tx.clone();
    thread::spawn(move || client_reader_loop(read_stream, id, cmd_tx_reader));

    let cmd_tx_writer = cmd_tx.clone();
    thread::spawn(move || client_writer_loop(stream, id, client_rx, cmd_tx_writer));

    Ok(())
}

fn client_reader_loop(mut stream: UnixStream, id: ClientId, cmd_tx: Sender<BusCommand>) {
    let mut payload = Vec::new();
    loop {
        let msg_type = match read_msg_into(&mut stream, &mut payload) {
            Ok(msg_type) => msg_type,
            Err(_) => {
                let _ = cmd_tx.send(BusCommand::RemoveClient { id });
                break;
            }
        };

        match msg_type {
            MSG_SEND_FRAME => match decode_send_frame(&payload) {
                Ok(send) => {
                    let _ = cmd_tx.send(BusCommand::SendFrame {
                        id,
                        seq: send.seq,
                        expect_ack: send.expect_ack,
                        frame: send.frame,
                    });
                }
                Err(_) => {
                    let _ = cmd_tx.send(BusCommand::RemoveClient { id });
                    break;
                }
            },
            MSG_SET_FILTERS => match decode_filters(&payload) {
                Ok((seq, filters)) => {
                    let _ = cmd_tx.send(BusCommand::SetFilters { id, seq, filters });
                }
                Err(_) => {
                    let _ = cmd_tx.send(BusCommand::SetFilters {
                        id,
                        seq: 0,
                        filters: vec![],
                    });
                }
            },
            _ => {}
        }
    }
}

fn client_writer_loop(
    mut stream: UnixStream,
    id: ClientId,
    rx: Receiver<ServerMsg>,
    cmd_tx: Sender<BusCommand>,
) {
    let mut frame_buf = [0u8; FRAME_HDR_LEN + MAX_DATA_LEN];
    let mut ack_buf = [0u8; ACK_LEN];
    while let Ok(msg) = rx.recv() {
        let result = match msg {
            ServerMsg::Frame(frame) => {
                let len = encode_frame_into(&mut frame_buf, &frame);
                write_msg(&mut stream, MSG_FRAME, &frame_buf[..len])
            }
            ServerMsg::Ack { seq, status } => {
                encode_ack_into(&mut ack_buf, seq, status);
                write_msg(&mut stream, crate::wire::MSG_ACK, &ack_buf)
            }
            ServerMsg::Hello => write_msg(&mut stream, crate::wire::MSG_HELLO, &[]),
            ServerMsg::FiltersAck { seq, status } => {
                encode_ack_into(&mut ack_buf, seq, status);
                write_msg(&mut stream, MSG_FILTERS_ACK, &ack_buf)
            }
        };
        if result.is_err() {
            let _ = cmd_tx.send(BusCommand::RemoveClient { id });
            break;
        }
    }
}

fn bus_loop(cmd_rx: Receiver<BusCommand>) {
    let mut clients: HashMap<ClientId, ClientState> = HashMap::new();
    let mut pending_filters: HashMap<ClientId, PendingFilters> = HashMap::new();

    loop {
        let cmd = match cmd_rx.recv() {
            Ok(cmd) => cmd,
            Err(_) => break,
        };
        if matches!(cmd, BusCommand::Shutdown) {
            break;
        }
        handle_command(cmd, &mut clients, &mut pending_filters);
    }
}

fn handle_command(
    cmd: BusCommand,
    clients: &mut HashMap<ClientId, ClientState>,
    pending_filters: &mut HashMap<ClientId, PendingFilters>,
) {
    match cmd {
        BusCommand::AddClient { id, tx } => {
            let pending = pending_filters.remove(&id);
            let filters = pending
                .as_ref()
                .map(|entry| entry.filters.clone())
                .unwrap_or_default();
            clients.insert(
                id,
                ClientState {
                    tx: tx.clone(),
                    filters,
                },
            );
            let _ = tx.send(ServerMsg::Hello);
            if let Some(pending) = pending {
                let _ = tx.send(ServerMsg::FiltersAck {
                    seq: pending.seq,
                    status: ACK_OK,
                });
            }
        }
        BusCommand::RemoveClient { id } => {
            clients.remove(&id);
            pending_filters.remove(&id);
        }
        BusCommand::SendFrame {
            id,
            seq,
            expect_ack,
            frame,
        } => {
            let ack_status = if expect_ack && clients.len() <= 1 {
                ACK_NO_ACK
            } else {
                ACK_OK
            };

            broadcast_frame(clients, &frame);

            if expect_ack {
                if let Some(client) = clients.get(&id) {
                    let _ = client.tx.send(ServerMsg::Ack {
                        seq,
                        status: ack_status,
                    });
                }
            }
        }
        BusCommand::SetFilters { id, seq, filters } => {
            if seq == 0 {
                if let Some(client) = clients.get_mut(&id) {
                    let _ = client.tx.send(ServerMsg::FiltersAck {
                        seq,
                        status: ACK_SERVER_ERR,
                    });
                }
                return;
            }
            if let Some(client) = clients.get_mut(&id) {
                client.filters = filters;
                let _ = client.tx.send(ServerMsg::FiltersAck {
                    seq,
                    status: ACK_OK,
                });
            } else {
                pending_filters.insert(id, PendingFilters { seq, filters });
            }
        }
        BusCommand::Shutdown => {}
    }
}

fn broadcast_frame(clients: &HashMap<ClientId, ClientState>, frame: &UnixFrame) {
    for client in clients.values() {
        if filters_match(&client.filters, frame) {
            let _ = client.tx.send(ServerMsg::Frame(*frame));
        }
    }
}

fn filters_match(filters: &[IdMaskFilter], frame: &UnixFrame) -> bool {
    if filters.is_empty() {
        return true;
    }
    filters
        .iter()
        .any(|filter| filter_match(filter, frame.id()))
}

fn filter_match(filter: &IdMaskFilter, id: Id) -> bool {
    match (filter.id, filter.mask, id) {
        (IfaceId::Standard(fid), IdMask::Standard(mask), Id::Standard(id)) => {
            (id.as_raw() & mask) == (fid.as_raw() & mask)
        }
        (IfaceId::Extended(fid), IdMask::Extended(mask), Id::Extended(id)) => {
            (id.as_raw() & mask) == (fid.as_raw() & mask)
        }
        _ => false,
    }
}
