use std::cmp;
use std::cmp::max;
use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::time::{Duration, Instant};

use delay_map::HashMapDelay;
use futures::StreamExt;
use tokio::sync::{mpsc, oneshot, Notify};

use crate::cid::{ConnectionId, ConnectionPeer};
use crate::congestion;
use crate::event::{SocketEvent, StreamEvent};
use crate::packet::{Packet, PacketBuilder, PacketType, SelectiveAck};
use crate::recv::ReceiveBuffer;
use crate::send::SendBuffer;
use crate::sent::SentPackets;
use crate::utils::wrap_compare_less;

// rationale from C reference impl:
// Allow a reception window of at least 3 ack_nrs behind seq_nr
// A non-SYN packet with an ack_nr difference greater than this is
// considered suspicious and ignored
const ALLOWED_ACK_WINDOW: u16 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Error {
    EmptyDataPayload,
    InvalidAckNum,
    InvalidFin,
    InvalidSeqNum,
    InvalidSyn,
    Reset,
    SynFromAcceptor,
    TimedOut,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::EmptyDataPayload => "missing payload in DATA packet",
            Self::InvalidAckNum => "received ACK for unsent packet",
            Self::InvalidFin => "received multiple FIN packets with distinct sequence numbers",
            Self::InvalidSeqNum => {
                "received packet with sequence number outside of remote peer's [SYN,FIN] range"
            }
            Self::InvalidSyn => "received multiple SYN packets with distinct sequence numbers",
            Self::Reset => "received RESET packet from remote peer",
            Self::SynFromAcceptor => "received SYN packet from connection acceptor",
            Self::TimedOut => "connection timed out",
        };

        write!(f, "{s}")
    }
}

impl From<Error> for io::ErrorKind {
    fn from(value: Error) -> Self {
        use Error::*;
        match value {
            EmptyDataPayload | InvalidAckNum | InvalidFin | InvalidSeqNum | InvalidSyn
            | SynFromAcceptor => io::ErrorKind::InvalidData,
            Reset => io::ErrorKind::ConnectionReset,
            TimedOut => io::ErrorKind::TimedOut,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Endpoint {
    Initiator((u16, usize)),
    Acceptor((u16, u16)),
}

enum State<const N: usize> {
    Connecting(Option<oneshot::Sender<io::Result<()>>>),
    Established {
        recv_buf: ReceiveBuffer<N>,
        send_buf: SendBuffer<N>,
        sent_packets: SentPackets,
    },
    Closing {
        local_fin: Option<u16>,
        remote_fin: Option<u16>,
        recv_buf: ReceiveBuffer<N>,
        send_buf: SendBuffer<N>,
        sent_packets: SentPackets,
    },
    Closed {
        err: Option<Error>,
    },
}

pub type Write = (Vec<u8>, oneshot::Sender<io::Result<usize>>);
pub type Read = (usize, oneshot::Sender<io::Result<Vec<u8>>>);

#[derive(Clone, Copy, Debug)]
pub struct ConnectionConfig {
    pub max_packet_size: u16,
    pub max_conn_attempts: usize,
    pub max_idle_timeout: Duration,
    pub initial_timeout: Duration,
    pub min_timeout: Duration,
    pub target_delay: Duration,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            max_conn_attempts: 3,
            max_idle_timeout: Duration::from_secs(10),
            max_packet_size: congestion::DEFAULT_MAX_PACKET_SIZE_BYTES as u16,
            initial_timeout: congestion::DEFAULT_INITIAL_TIMEOUT,
            min_timeout: congestion::DEFAULT_MIN_TIMEOUT,
            target_delay: Duration::from_micros(congestion::DEFAULT_TARGET_MICROS.into()),
        }
    }
}

impl From<ConnectionConfig> for congestion::Config {
    fn from(value: ConnectionConfig) -> Self {
        Self {
            max_packet_size_bytes: u32::from(value.max_packet_size),
            initial_timeout: value.initial_timeout,
            min_timeout: value.min_timeout,
            target_delay_micros: value.target_delay.as_micros() as u32,
            ..Default::default()
        }
    }
}

pub struct Connection<const N: usize, P> {
    state: State<N>,
    cid: ConnectionId<P>,
    config: ConnectionConfig,
    endpoint: Endpoint,
    peer_ts_diff: Duration,
    peer_recv_window: u32,
    socket_events: mpsc::UnboundedSender<SocketEvent<P>>,
    unacked: HashMapDelay<u16, Packet>,
    pending_reads: VecDeque<Read>,
    readable: Notify,
    pending_writes: VecDeque<Write>,
    writable: Notify,

    // The number of packets in the send queue. Packets that haven't
    // been sent yet and packets marked as needing to be resend count.
    // The oldest un-acked packet in the send queue is seq_nr - cur_window_packets
    cur_window_packets: u16,

    // counter of duplicate acks
    duplicate_ack: u16,    
}

impl<const N: usize, P: ConnectionPeer> Connection<N, P> {
    pub fn new(
        cid: ConnectionId<P>,
        config: ConnectionConfig,
        syn: Option<Packet>,
        connected: oneshot::Sender<io::Result<()>>,
        socket_events: mpsc::UnboundedSender<SocketEvent<P>>,
    ) -> Self {
        let (endpoint, peer_ts_diff, peer_recv_window) = match syn {
            Some(syn) => {
                let syn_ack = rand::random();
                let endpoint = Endpoint::Acceptor((syn.seq_num(), syn_ack));

                let now = crate::time::now_micros();
                let peer_ts_diff = crate::time::duration_between(syn.ts_micros(), now);

                (endpoint, peer_ts_diff, syn.window_size())
            }
            None => {
                let syn = rand::random();
                let endpoint = Endpoint::Initiator((syn, 0));
                (endpoint, Duration::ZERO, u32::MAX)
            }
        };

        Self {
            state: State::Connecting(Some(connected)),
            cid,
            config,
            endpoint,
            peer_ts_diff,
            peer_recv_window,
            socket_events,
            unacked: HashMapDelay::new(config.initial_timeout),
            pending_reads: VecDeque::new(),
            readable: Notify::new(),
            pending_writes: VecDeque::new(),
            writable: Notify::new(),
            cur_window_packets: 0,
            duplicate_ack: 0,
        }
    }

    pub async fn event_loop(
        &mut self,
        mut stream_events: mpsc::UnboundedReceiver<StreamEvent>,
        mut writes: mpsc::UnboundedReceiver<Write>,
        mut reads: mpsc::UnboundedReceiver<Read>,
        mut shutdown: oneshot::Receiver<()>,
    ) {
        tracing::debug!("uTP conn starting...");

        // If we are the initiating endpoint, then send the SYN. If we are the accepting endpoint,
        // then send the SYN-ACK.
        match self.endpoint {
            Endpoint::Initiator((syn_seq_num, ..)) => {
                let syn = self.syn_packet(syn_seq_num);
                self.socket_events
                    .send(SocketEvent::Outgoing((syn.clone(), self.cid.peer.clone())))
                    .unwrap();
                self.unacked
                    .insert_at(syn_seq_num, syn, self.config.initial_timeout);

                self.endpoint = Endpoint::Initiator((syn_seq_num, 1));
            }
            Endpoint::Acceptor((syn, syn_ack)) => {
                let state = self.state_packet().unwrap();
                self.socket_events
                    .send(SocketEvent::Outgoing((state, self.cid.peer.clone())))
                    .unwrap();

                let recv_buf = ReceiveBuffer::new(syn);
                let send_buf = SendBuffer::new();

                let congestion_ctrl = congestion::Controller::new(self.config.into());

                // NOTE: We initialize with the sequence number of the SYN-ACK minus 1 because the
                // SYN-ACK contains the incremented sequence number (i.e. the next sequence
                // number). This is consistent with the reference implementation and the libtorrent
                // implementation where STATE packets set the sequence number to the next sequence
                // number.
                let sent_packets = SentPackets::new(syn_ack.wrapping_sub(1), syn_ack.wrapping_sub(1), congestion_ctrl);

                // The connection must be in the `Connecting` state. We optimistically mark the
                // connection `Established` here. This enables the accepting endpoint to send DATA
                // packets before receiving any from the initiator.
                //
                // TODO: Find a more elegant way to emit the connected message.
                if let State::Connecting(connected) = &mut self.state {
                    let _ = connected.take().unwrap().send(Ok(()));
                } else {
                    panic!("connection in invalid state prior to event loop beginning");
                }

                self.state = State::Established {
                    recv_buf,
                    send_buf,
                    sent_packets,
                };
            }
        }

        let mut shutting_down = false;
        let idle_timeout = tokio::time::sleep(self.config.max_idle_timeout);
        tokio::pin!(idle_timeout);
        loop {
            tokio::select! {
                Some(event) = stream_events.recv() => {
                    match event {
                        StreamEvent::Incoming(packet) => {
                            // Reset the idle timeout on any incoming packet.
                            let idle_deadline = tokio::time::Instant::now() + self.config.max_idle_timeout;
                            idle_timeout.as_mut().reset(idle_deadline);

                            self.on_packet(&packet, Instant::now());
                        }
                        StreamEvent::Shutdown => {
                            shutting_down = true;
                        }
                    }
                }
                Some(Ok(timeout)) = self.unacked.next() => {
                    let (seq, packet) = timeout;
                    tracing::debug!(seq, ack = %packet.ack_num(), packet = ?packet.packet_type(), "timeout");

                    self.on_timeout(packet, Instant::now());
                }
                Some(write) = writes.recv(), if !shutting_down => {
                    // Reset the idle timeout on any new write.
                    let idle_deadline = tokio::time::Instant::now() + self.config.max_idle_timeout;
                    idle_timeout.as_mut().reset(idle_deadline);

                    self.on_write(write);
                }
                Some(read) = reads.recv() => {
                    self.on_read(read);
                }
                _ = self.readable.notified() => {
                    self.process_reads();
                }
                _ = self.writable.notified() => {
                    self.process_writes(Instant::now());
                }
                () = &mut idle_timeout => {
                    if !std::matches!(self.state, State::Closed { .. }) {
                        let unacked: Vec<u16> = self.unacked.keys().copied().collect();
                        tracing::warn!(?unacked, "idle timeout expired, closing...");

                        self.state = State::Closed { err: Some(Error::TimedOut) };
                    }
                }
                _ = &mut shutdown, if !shutting_down => {
                    tracing::debug!("uTP conn initiating shutdown...");
                    shutting_down = true;
                }
            }

            if shutting_down && !std::matches!(self.state, State::Closed { .. }) {
                self.shutdown();
            }

            if let State::Closed { err } = self.state {
                tracing::debug!(?err, "uTP conn closing...");

                self.process_reads();
                self.process_writes(Instant::now());

                if let Err(..) = self
                    .socket_events
                    .send(SocketEvent::Shutdown(self.cid.clone()))
                {
                    tracing::warn!("unable to send shutdown signal to uTP socket");
                }

                break;
            }
        }
    }

    fn shutdown(&mut self) {
        match &mut self.state {
            State::Connecting(..) => {
                self.state = State::Closed { err: None };
            }
            State::Established {
                send_buf,
                sent_packets,
                recv_buf,
            } => {
                let mut local_fin = None;
                if self.pending_writes.is_empty() && send_buf.is_empty() {
                    // TODO: Helper for construction of FIN.
                    let recv_window = recv_buf.available() as u32;
                    let seq_num = sent_packets.next_seq_num();
                    let ack_num = recv_buf.ack_num();
                    let selective_ack = recv_buf.selective_ack();
                    let fin = PacketBuilder::new(
                        PacketType::Fin,
                        self.cid.send,
                        crate::time::now_micros(),
                        recv_window,
                        seq_num,
                    )
                    .ack_num(ack_num)
                    .selective_ack(selective_ack)
                    .build();

                    local_fin = Some(seq_num);

                    tracing::debug!(seq = %seq_num, "transmitting FIN");
                    Self::transmit(
                        &mut self.cur_window_packets,
                        sent_packets,
                        &mut self.unacked,
                        &mut self.socket_events,
                        fin,
                        &self.cid.peer,
                        Instant::now(),
                    );
                }

                self.state = State::Closing {
                    send_buf: send_buf.clone(),
                    sent_packets: sent_packets.clone(),
                    recv_buf: recv_buf.clone(),
                    local_fin,
                    remote_fin: None,
                }
            }
            State::Closing {
                send_buf,
                sent_packets,
                recv_buf,
                local_fin,
                ..
            } => {
                // If we have not sent our FIN, and there are no pending writes, and there is no
                // pending data in the send buffer, then send our FIN. If there were still data to
                // send, then we would not know which sequence number to assign to our FIN.
                if local_fin.is_none() && self.pending_writes.is_empty() && send_buf.is_empty() {
                    // TODO: Helper for construction of FIN.
                    let recv_window = recv_buf.available() as u32;
                    let seq_num = sent_packets.next_seq_num();
                    let ack_num = recv_buf.ack_num();
                    let selective_ack = recv_buf.selective_ack();
                    let fin = PacketBuilder::new(
                        PacketType::Fin,
                        self.cid.send,
                        crate::time::now_micros(),
                        recv_window,
                        seq_num,
                    )
                    .ack_num(ack_num)
                    .selective_ack(selective_ack)
                    .build();

                    *local_fin = Some(seq_num);

                    tracing::debug!(seq = %seq_num, "transmitting FIN");
                    Self::transmit(
                        &mut self.cur_window_packets,
                        sent_packets,
                        &mut self.unacked,
                        &mut self.socket_events,
                        fin,
                        &self.cid.peer,
                        Instant::now(),
                    );
                }
            }
            State::Closed { .. } => {}
        }
    }

    fn process_writes(&mut self, now: Instant) {
        let (send_buf, sent_packets, recv_buf) = match &mut self.state {
            State::Connecting(..) => return,
            State::Established {
                send_buf,
                sent_packets,
                recv_buf,
                ..
            } => (send_buf, sent_packets, recv_buf),
            State::Closing {
                send_buf,
                sent_packets,
                recv_buf,
                ..
            } => (send_buf, sent_packets, recv_buf),
            State::Closed { err, .. } => {
                let result = match err {
                    Some(err) => Err(io::ErrorKind::from(*err)),
                    None => Ok(0),
                };
                while let Some(pending) = self.pending_writes.pop_front() {
                    let (.., tx) = pending;
                    let _ = tx.send(result.map_err(io::Error::from));
                }
                return;
            }
        };

        // Compose as many data packets as possible, consuming data from the send buffer.
        let now_micros = crate::time::now_micros();
        let mut window = cmp::min(sent_packets.window(), self.peer_recv_window) as usize;
        let mut payloads = Vec::new();
        while window > 0 {
            // TODO: Do not rely on approximation here. Account for header and extensions.
            let max_data_size = cmp::min(window, usize::from(self.config.max_packet_size - 64));
            let mut data = vec![0; max_data_size];
            let n = send_buf.read(&mut data).unwrap();
            if n == 0 {
                break;
            }
            data.truncate(n);
            payloads.push(data);

            window -= n;
        }

        // Write as much data as possible into send buffer.
        while let Some((data, ..)) = self.pending_writes.front() {
            if data.len() <= send_buf.available() {
                let (data, tx) = self.pending_writes.pop_front().unwrap();
                send_buf.write(&data).unwrap();
                let _ = tx.send(Ok(data.len()));
                self.writable.notify_one();
            } else {
                break;
            }
        }

        // Transmit data packets.
        // TODO: Helper for construction of DATA packet.
        let mut seq_num = sent_packets.next_seq_num();
        let recv_window = recv_buf.available() as u32;
        let ack_num = recv_buf.ack_num();
        let selective_ack = recv_buf.selective_ack();
        for payload in payloads.into_iter() {
            let packet = PacketBuilder::new(
                PacketType::Data,
                self.cid.send,
                now_micros,
                recv_window,
                seq_num,
            )
            .payload(payload)
            .ts_diff_micros(self.peer_ts_diff.as_micros() as u32)
            .ack_num(ack_num)
            .selective_ack(selective_ack.clone())
            .build();
            Self::transmit(
                &mut self.cur_window_packets,
                sent_packets,
                &mut self.unacked,
                &mut self.socket_events,
                packet,
                &self.cid.peer,
                now,
            );
            seq_num = seq_num.wrapping_add(1);
        }
    }

    fn on_write(&mut self, write: Write) {
        let (data, tx) = write;

        match &mut self.state {
            State::Connecting(..) | State::Established { .. } => {
                self.pending_writes.push_back((data, tx));
            }
            State::Closing {
                local_fin,
                remote_fin,
                ..
            } => {
                if local_fin.is_none() && remote_fin.is_some() {
                    self.pending_writes.push_back((data, tx));
                } else {
                    let _ = tx.send(Ok(0));
                }
            }
            State::Closed { err, .. } => {
                let result = match err {
                    Some(err) => Err(io::Error::from(io::ErrorKind::from(*err))),
                    None => Ok(0),
                };
                let _ = tx.send(result);
            }
        }

        self.process_writes(Instant::now());

        self.writable.notify_waiters();
    }

    fn process_reads(&mut self) {
        let recv_buf = match &mut self.state {
            State::Connecting(..) => return,
            State::Established { recv_buf, .. } | State::Closing { recv_buf, .. } => recv_buf,
            State::Closed { err } => {
                let result = match err {
                    Some(err) => Err(io::ErrorKind::from(*err)),
                    None => Ok(vec![]),
                };
                while let Some(pending) = self.pending_reads.pop_front() {
                    let (.., tx) = pending;
                    let _ = tx.send(result.clone().map_err(io::Error::from));
                }
                return;
            }
        };

        while let Some((len, ..)) = self.pending_reads.front() {
            let mut buf = vec![0; *len];
            let n = recv_buf.read(&mut buf).unwrap();
            if n == 0 {
                break;
            } else {
                let (.., tx) = self.pending_reads.pop_front().unwrap();
                buf.truncate(n);
                let _ = tx.send(Ok(buf));
            }
        }

        // If we have reached EOF, then send an empty result to all pending reads.
        if self.eof() {
            while let Some((.., tx)) = self.pending_reads.pop_front() {
                let _ = tx.send(Ok(vec![]));
            }
        }
    }

    fn on_read(&mut self, read: Read) {
        let (len, tx) = read;
        match &mut self.state {
            State::Connecting(..) | State::Established { .. } | State::Closing { .. } => {
                self.pending_reads.push_back((len, tx))
            }
            State::Closed { err } => {
                let result = match err {
                    Some(err) => Err(io::Error::from(io::ErrorKind::from(*err))),
                    None => Ok(vec![]),
                };
                let _ = tx.send(result);
            }
        }

        self.process_reads();

        self.readable.notify_waiters();
    }

    fn eof(&self) -> bool {
        match &self.state {
            State::Connecting(..) | State::Established { .. } => false,
            State::Closing {
                recv_buf,
                remote_fin,
                ..
            } => match remote_fin {
                Some(fin) => recv_buf.ack_num() == *fin,
                None => false,
            },
            State::Closed { .. } => true,
        }
    }

    fn on_timeout(&mut self, packet: Packet, now: Instant) {
        match &mut self.state {
            State::Connecting(connected) => match &mut self.endpoint {
                Endpoint::Initiator((syn, attempts)) => {
                    if *attempts >= self.config.max_conn_attempts {
                        let err = Error::TimedOut;
                        if let Some(connected) = connected.take() {
                            let err = io::Error::from(io::ErrorKind::from(err));
                            let _ = connected.send(Err(err));
                        }
                        self.state = State::Closed { err: Some(err) };
                    } else {
                        let seq = *syn;
                        *attempts += 1;
                        let packet = self.syn_packet(seq);
                        let _ = self.socket_events.send(SocketEvent::Outgoing((
                            packet.clone(),
                            self.cid.peer.clone(),
                        )));
                        self.unacked
                            .insert_at(seq, packet, self.config.initial_timeout);
                    }
                }
                Endpoint::Acceptor(..) => {}
            },
            State::Established {
                sent_packets,
                recv_buf,
                ..
            }
            | State::Closing {
                sent_packets,
                recv_buf,
                ..
            } => {
                // If the timed out packet is a SYN, then do nothing since the connection has
                // already been established.
                if std::matches!(packet.packet_type(), PacketType::Syn) {
                    return;
                }

                sent_packets.on_timeout();

                // TODO: Limit number of retransmissions.
                let recv_window = recv_buf.available() as u32;
                let now_micros = crate::time::now_micros();
                let ts_diff_micros = self.peer_ts_diff.as_micros() as u32;
                let packet = PacketBuilder::from(packet)
                    .window_size(recv_window)
                    .ack_num(recv_buf.ack_num())
                    .selective_ack(recv_buf.selective_ack())
                    .ts_micros(now_micros)
                    .ts_diff_micros(ts_diff_micros)
                    .build();
                Self::transmit(
                    &mut self.cur_window_packets,
                    sent_packets,
                    &mut self.unacked,
                    &mut self.socket_events,
                    packet,
                    &self.cid.peer,
                    now,
                );
            }
            State::Closed { .. } => {}
        }
    }

    fn on_packet(&mut self, packet: &Packet, now: Instant) {
        let now_micros = crate::time::now_micros();
        self.peer_recv_window = packet.window_size();

        // Cap the diff. If the clock on the remote machine is behind the clock on the local
        // machine, then we could end up with large (inaccurate) diffs. Use the max idle timeout as
        // an upper bound on the possible diff. If the diff exceeds the bound, then assume the
        // remote clock is behind the local clock and use a diff of 1s.
        let peer_ts_diff = crate::time::duration_between(packet.ts_micros(), now_micros);
        if peer_ts_diff > self.config.max_idle_timeout {
            self.peer_ts_diff = Duration::from_secs(1);
        } else {
            self.peer_ts_diff = peer_ts_diff;
        }

        match packet.packet_type() {
            PacketType::Syn => self.on_syn(packet.seq_num()),
            PacketType::State => {
                let delay = Duration::from_micros(packet.ts_diff_micros().into());
                self.on_state(
                    packet.seq_num(),
                    packet.ack_num(),
                    packet.selective_ack(),
                    delay,
                    now,
                );
            }
            PacketType::Data => self.on_data(packet.seq_num(), packet.payload()),
            PacketType::Fin => {
                if packet.payload().is_empty() {
                    self.on_fin(packet.seq_num(), None);
                } else {
                    self.on_fin(packet.seq_num(), Some(packet.payload()));
                }
            }
            PacketType::Reset => {
                self.on_reset();
            }
        }

        // If there are any lost packets, then queue retransmissions.
        self.retransmit_lost_packets(now);

        // Send a STATE packet if appropriate packet and connection in appropriate state.
        //
        // NOTE: We don't call `transmit` because we do not wish to track this packet.
        // NOTE: We should probably move this so that multiple incoming packets can be processed
        // before we send a STATE.
        match packet.packet_type() {
            PacketType::Syn | PacketType::Fin | PacketType::Data => {
                if let Some(state) = self.state_packet() {
                    self.socket_events
                        .send(SocketEvent::Outgoing((state, self.cid.peer.clone())))
                        .expect("outgoing channel should be open if connection is not closed");
                }
            }
            PacketType::State | PacketType::Reset => {}
        };

        // If the packet is a STATE packet, then notify writable because the send window may have
        // increased.
        if std::matches!(packet.packet_type(), PacketType::State) {
            self.writable.notify_one();
        }

        // If the packet contains a data payload, or the packet was a FIN, then notify readable
        // because there may be new data available in the receive buffer and/or reads to complete.
        if !packet.payload().is_empty() || std::matches!(packet.packet_type(), PacketType::Fin) {
            self.readable.notify_one();
        }

        // If both endpoints have exchanged FINs and all data has been received/acknowledged, then
        // close the connection.
        if let State::Closing {
            local_fin: Some(..),
            remote_fin: Some(remote),
            sent_packets,
            recv_buf,
            ..
        } = &mut self.state
        {
            if !sent_packets.has_unacked_packets() && recv_buf.ack_num() == *remote {
                self.state = State::Closed { err: None };
            }
        }
    }

    fn on_syn(&mut self, seq_num: u16) {
        let err = match self.endpoint {
            // If we are the accepting endpoint, then check whether the SYN is a retransmission. A
            // non-matching sequence number is incorrect behavior.
            Endpoint::Acceptor((syn, ..)) => {
                if seq_num != syn {
                    Some(Error::InvalidSyn)
                } else {
                    None
                }
            }
            // If we are the initiating endpoint, then an incoming SYN is incorrect behavior.
            Endpoint::Initiator(..) => Some(Error::SynFromAcceptor),
        };

        if let Some(err) = err {
            if !std::matches!(self.state, State::Closed { .. }) {
                self.reset(err);
            }
        }
    }

    fn on_state(
        &mut self,
        seq_num: u16,
        ack_num: u16,
        selective_ack: Option<&SelectiveAck>,
        delay: Duration,
        now: Instant,
    ) {
        match &mut self.state {
            State::Connecting(connected) => match self.endpoint {
                // If the STATE acknowledges our SYN, then mark the connection established.
                Endpoint::Initiator((syn, ..)) => {
                    if ack_num == syn {
                        // NOTE: In a deviation from the specification, we initialize the ACK num
                        // to the sequence number of the SYN-ACK minus 1. This is consistent with
                        // the reference implementation and the libtorrent implementation.
                        let recv_buf = ReceiveBuffer::new(seq_num.wrapping_sub(1));

                        let send_buf = SendBuffer::new();

                        let congestion_ctrl = congestion::Controller::new(self.config.into());
                        let sent_packets = SentPackets::new(syn, syn, congestion_ctrl);

                        let _ = connected.take().unwrap().send(Ok(()));
                        self.state = State::Established {
                            recv_buf,
                            send_buf,
                            sent_packets,
                        };
                    }
                }
                Endpoint::Acceptor(..) => {}
            },
            State::Established { sent_packets, .. } | State::Closing { sent_packets, .. } => {
                // invalid ack check from libtorrent
                let ack_window = max(self.cur_window_packets + ALLOWED_ACK_WINDOW, ALLOWED_ACK_WINDOW);
                if wrap_compare_less(sent_packets.seq_num().wrapping_sub(1), ack_num) ||
                    wrap_compare_less(ack_num, sent_packets.seq_num().wrapping_sub(1).wrapping_sub(ack_window)) {
                    // this reset is weird libtorrent just skips the packet instead of resetting
                    // Hopefully jacob can spot me with a reason for this
                    self.reset(Error::InvalidAckNum);
                    return;
                }

                // number of packets past the expected
                // ack_nr is the last acked, seq_nr is the
                // current. Subtracting 1 makes 0 mean "this is the next expected packet"
                let past_expected = seq_num - sent_packets.ack_num() - 1;

                // acks is the number of packets that was acked, in normal case - no selective
                // acks, no losses, no resends, it will usually be equal to 1
                // we can calculate it here and not only for ST_STATE packet, as each utp
                // packet has info about remote side last acked packet.
                let mut acks = ack_num - (sent_packets.seq_num() - 1 - self.cur_window_packets);

                if acks > self.cur_window_packets {
                    // this case happens if the we already received this ack nr
                    acks = 0;
                }

                // rationale from c reference impl:
                // if we get the same ack_nr as in the last packet
                // increase the duplicate_ack counter, otherwise reset
                // it to 0.
                // It's important to only count ACKs in ST_STATE packets. Any other
                // packet (primarily ST_DATA) is likely to have been sent because of the
                // other end having new outgoing data, not in response to incoming data.
                // For instance, if we're receiving a steady stream of payload with no
                // outgoing data, and we suddently have a few bytes of payload to send (say,
                // a bittorrent HAVE message), we're very likely to see 3 duplicate ACKs
                // immediately after sending our payload packet. This effectively disables
                // the fast-resend on duplicate-ack logic for bi-directional connections
                // (except in the case of a selective ACK). This is in line with BSD4.4 TCP
                // implementation.
                if self.cur_window_packets > 0 && ack_num == sent_packets.seq_num() - self.cur_window_packets - 1 {
                    self.duplicate_ack += 1;
                } else {
                    self.duplicate_ack = 0
                }

                let mut i: u16 = 0;
                while i < acks {
                    sent_packets.ack(ack_num, delay, now);
                    i += 1;
                }
                sent_packets.on_ack(ack_num, selective_ack, delay, now);
                    // }
                    //
                    // // Mark all packets up to `ack_num` acknowledged by retaining all packets in
                    // // the range beyond `ack_num`.
                    // let acked = CircularRangeInclusive::new(range.start(), ack_num);
                    // self.unacked.retain(|seq, _| !acked.contains(*seq));

                // TODO: Helper for selective ACK.
                if let Some(selective_ack) = selective_ack {
                    for (i, acked) in selective_ack.acked().iter().enumerate() {
                        let seq_num = usize::from(ack_num).wrapping_add(2 + i) as u16;
                        if *acked {
                            self.unacked.remove(&seq_num);
                        }
                    }
                }
                // } else {
                //     self.reset(Error::InvalidAckNum);
                // }
            }
            State::Closed { .. } => {}
        }
    }

    fn on_data(&mut self, seq_num: u16, data: &[u8]) {
        // If the data payload is empty, then reset the connection. A DATA packet must contain a
        // non-empty payload.
        if data.is_empty() {
            self.reset(Error::EmptyDataPayload);
        }

        match &mut self.state {
            State::Connecting(..) => match self.endpoint {
                // If we are the accepting endpoint, then the connection should have been marked
                // `Established` at the beginning of the event loop.
                // TODO: Clean this up so that we do not have to rely on `unreachable`.
                Endpoint::Acceptor(..) => unreachable!(),
                // If we are the initiating endpoint, then we cannot mark the connection
                // established, because we are waiting on the STATE for the SYN. Without the STATE,
                // we do not know how to order this data.
                //
                // TODO: Save the data so that it may be written to the receive buffer upon the
                // connection being established.
                Endpoint::Initiator(..) => {}
            },
            // If the connection is established, and there is sufficient capacity, then incorporate
            // the data into the receive buffer.
            State::Established { recv_buf, .. } => {
                if data.len() <= recv_buf.available() && !recv_buf.was_written(seq_num) {
                    recv_buf.write(data, seq_num);
                }
            }
            // If the connection is closing and we have a remote FIN, then check whether the
            // sequence number falls within the appropriate range. If it does, and there is
            // sufficient capacity, then incorporate the data into the receive buffer. If it does
            // not, then reset the connection.
            State::Closing {
                recv_buf,
                remote_fin,
                ..
            } => {
                if let Some(remote_fin) = remote_fin {
                    let start = recv_buf.init_seq_num();
                    let range = crate::seq::CircularRangeInclusive::new(start, *remote_fin);
                    if !range.contains(seq_num) {
                        self.state = State::Closed {
                            err: Some(Error::InvalidSeqNum),
                        };
                        return;
                    }
                }

                if data.len() <= recv_buf.available() && !recv_buf.was_written(seq_num) {
                    recv_buf.write(data, seq_num);
                }
            }
            State::Closed { .. } => {}
        }
    }

    fn on_fin(&mut self, seq_num: u16, data: Option<&[u8]>) {
        match &mut self.state {
            State::Connecting(..) => {}
            State::Established {
                recv_buf,
                sent_packets,
                send_buf,
            } => {
                // Register the FIN with the receive buffer.
                if let Some(data) = data {
                    recv_buf.write(data, seq_num);
                } else {
                    recv_buf.write(&[], seq_num);
                }

                tracing::debug!(seq = %seq_num, "received FIN");

                self.state = State::Closing {
                    recv_buf: recv_buf.clone(),
                    send_buf: send_buf.clone(),
                    sent_packets: sent_packets.clone(),
                    remote_fin: Some(seq_num),
                    local_fin: None,
                };
            }
            State::Closing {
                recv_buf,
                remote_fin,
                ..
            } => {
                match remote_fin {
                    // If we have already received a FIN, a subsequent FIN with a different
                    // sequence number is incorrect behavior.
                    Some(fin) => {
                        if seq_num != *fin {
                            self.reset(Error::InvalidFin);
                        }
                    }
                    None => {
                        tracing::debug!(seq = %seq_num, "received FIN");

                        *remote_fin = Some(seq_num);
                        if let Some(data) = data {
                            recv_buf.write(data, seq_num);
                        } else {
                            recv_buf.write(&[], seq_num);
                        }
                    }
                }
            }
            State::Closed { .. } => {}
        }
    }

    fn on_reset(&mut self) {
        tracing::warn!("RESET from remote");

        // If the connection is not already closed or reset, then reset the connection.
        if !std::matches!(self.state, State::Closed { .. }) {
            self.reset(Error::Reset);
        }
    }

    fn reset(&mut self, err: Error) {
        tracing::warn!(?err, "resetting connection: {err}");
        self.state = State::Closed { err: Some(err) }
    }

    fn syn_packet(&self, seq_num: u16) -> Packet {
        let now_micros = crate::time::now_micros();
        PacketBuilder::new(
            PacketType::Syn,
            self.cid.recv,
            now_micros,
            N as u32,
            seq_num,
        )
        .build()
    }

    fn state_packet(&self) -> Option<Packet> {
        let now = crate::time::now_micros();
        let conn_id = self.cid.send;
        let ts_diff_micros = self.peer_ts_diff.as_micros() as u32;

        match &self.state {
            State::Connecting(..) => match self.endpoint {
                Endpoint::Initiator(..) => None,
                Endpoint::Acceptor((syn, syn_ack)) => {
                    let packet =
                        PacketBuilder::new(PacketType::State, conn_id, now, N as u32, syn_ack)
                            .ts_diff_micros(ts_diff_micros)
                            .ack_num(syn)
                            .build();
                    Some(packet)
                }
            },
            State::Established {
                recv_buf,
                sent_packets,
                ..
            }
            | State::Closing {
                recv_buf,
                sent_packets,
                ..
            } => {
                // NOTE: Consistent with the reference implementation and the libtorrent
                // implementation, STATE packets always include the next sequence number.
                let seq_num = sent_packets.next_seq_num();
                let ack_num = recv_buf.ack_num();
                let recv_window = recv_buf.available() as u32;
                let selective_ack = recv_buf.selective_ack();
                let packet =
                    PacketBuilder::new(PacketType::State, conn_id, now, recv_window, seq_num)
                        .ts_diff_micros(ts_diff_micros)
                        .ack_num(ack_num)
                        .selective_ack(selective_ack)
                        .build();
                Some(packet)
            }
            State::Closed { .. } => None,
        }
    }

    fn retransmit_lost_packets(&mut self, now: Instant) {
        let (sent_packets, recv_buf) = match &mut self.state {
            State::Connecting(..) | State::Closed { .. } => return,
            State::Established {
                sent_packets,
                recv_buf,
                ..
            }
            | State::Closing {
                sent_packets,
                recv_buf,
                ..
            } => (sent_packets, recv_buf),
        };

        if !sent_packets.has_lost_packets() {
            return;
        }

        let conn_id = self.cid.send;
        let now_micros = crate::time::now_micros();
        let recv_window = recv_buf.available() as u32;
        let ts_diff_micros = self.peer_ts_diff.as_micros() as u32;
        for (seq_num, packet_type, payload) in sent_packets.lost_packets() {
            let mut packet =
                PacketBuilder::new(packet_type, conn_id, now_micros, recv_window, seq_num);
            if let Some(payload) = payload {
                packet = packet.payload(payload);
            }
            let packet = packet
                .ts_diff_micros(ts_diff_micros)
                .ack_num(recv_buf.ack_num())
                .selective_ack(recv_buf.selective_ack())
                .build();

            Self::transmit(
                &mut self.cur_window_packets,
                sent_packets,
                &mut self.unacked,
                &mut self.socket_events,
                packet,
                &self.cid.peer,
                now,
            );
        }
    }

    fn transmit(
        cur_window_packets: &mut u16,
        sent_packets: &mut SentPackets,
        unacked: &mut HashMapDelay<u16, Packet>,
        socket_events: &mut mpsc::UnboundedSender<SocketEvent<P>>,
        packet: Packet,
        dest: &P,
        now: Instant,
    ) {
        let (payload, len) = if packet.payload().is_empty() {
            (None, 0)
        } else {
            (
                Some(packet.payload().clone()),
                packet.payload().len() as u32,
            )
        };

        sent_packets.on_transmit(packet.seq_num(), packet.packet_type(), payload, len, now);
        unacked.insert_at(packet.seq_num(), packet.clone(), sent_packets.timeout());
        *cur_window_packets = cur_window_packets.wrapping_add(1);
        socket_events
            .send(SocketEvent::Outgoing((packet, dest.clone())))
            .expect("outgoing channel should be open if connection is not closed");
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use std::net::SocketAddr;

    const BUF: usize = 2048;
    const DELAY: Duration = Duration::from_millis(100);

    fn conn(endpoint: Endpoint) -> Connection<BUF, SocketAddr> {
        let (connected, _) = oneshot::channel();
        let (socket_events, _) = mpsc::unbounded_channel();

        let peer = SocketAddr::from(([127, 0, 0, 1], 3400));
        let cid = ConnectionId {
            send: 101,
            recv: 100,
            peer,
        };

        Connection {
            state: State::Connecting(Some(connected)),
            cid,
            config: ConnectionConfig::default(),
            endpoint,
            peer_ts_diff: Duration::from_millis(100),
            peer_recv_window: u32::MAX,
            socket_events,
            unacked: HashMapDelay::new(DELAY),
            pending_reads: VecDeque::new(),
            readable: Notify::new(),
            pending_writes: VecDeque::new(),
            writable: Notify::new(),
            cur_window_packets: 0,
            duplicate_ack: 0,
        }
    }

    #[test]
    fn on_syn_initiator() {
        let syn = 100;
        let endpoint = Endpoint::Initiator((syn, 1));
        let mut conn = conn(endpoint);

        conn.on_syn(syn);
        assert!(std::matches!(
            conn.state,
            State::Closed {
                err: Some(Error::SynFromAcceptor),
            }
        ));
    }

    #[test]
    fn on_syn_acceptor() {
        let syn = 100;
        let syn_ack = 101;
        let endpoint = Endpoint::Acceptor((syn, syn_ack));
        let mut conn = conn(endpoint);

        conn.on_syn(syn);
        assert!(std::matches!(conn.state, State::Connecting { .. }));
    }

    #[test]
    fn on_syn_acceptor_non_matching_syn() {
        let syn = 100;
        let syn_ack = 101;
        let endpoint = Endpoint::Acceptor((syn, syn_ack));
        let mut conn = conn(endpoint);

        let alt_syn = 128;
        conn.on_syn(alt_syn);
        assert!(std::matches!(
            conn.state,
            State::Closed {
                err: Some(Error::InvalidSyn),
            }
        ));
    }

    #[test]
    fn on_state_connecting_initiator() {
        let syn = 100;
        let endpoint = Endpoint::Initiator((syn, 1));
        let mut conn = conn(endpoint);

        let seq_num = 1;
        let now = Instant::now();
        conn.on_state(seq_num, syn, None, DELAY, now);

        assert!(std::matches!(conn.state, State::Established { .. }));
    }

    #[test]
    fn on_state_established_invalid_ack_num() {
        let syn = 100;
        let syn_ack = 101;
        let endpoint = Endpoint::Acceptor((syn, syn_ack));
        let mut conn = conn(endpoint);

        let congestion_ctrl = congestion::Controller::new(conn.config.into());
        let mut sent_packets = SentPackets::new(syn_ack, syn_ack, congestion_ctrl);

        let data = vec![0xef];
        let len = 64;
        let now = Instant::now();
        sent_packets.on_transmit(
            syn_ack.wrapping_add(1),
            PacketType::Data,
            Some(data),
            len,
            now,
        );

        let send_buf = SendBuffer::new();
        let recv_buf = ReceiveBuffer::new(syn);

        conn.state = State::Established {
            sent_packets,
            send_buf,
            recv_buf,
        };

        let now = Instant::now();
        conn.on_state(
            syn.wrapping_add(1),
            syn_ack.wrapping_add(2),
            None,
            DELAY,
            now,
        );

        assert!(std::matches!(
            conn.state,
            State::Closed {
                err: Some(Error::InvalidAckNum),
            }
        ));
    }

    #[test]
    fn on_fin_established() {
        let syn = 100;
        let syn_ack = 101;
        let endpoint = Endpoint::Acceptor((syn, syn_ack));
        let mut conn = conn(endpoint);

        let send_buf = SendBuffer::new();
        let congestion_ctrl = congestion::Controller::new(conn.config.into());
        let sent_packets = SentPackets::new(syn_ack, syn_ack, congestion_ctrl);
        let recv_buf = ReceiveBuffer::new(syn);

        conn.state = State::Established {
            send_buf,
            sent_packets,
            recv_buf,
        };

        let fin = syn.wrapping_add(3);
        conn.on_fin(fin, None);
        match conn.state {
            State::Closing {
                local_fin,
                remote_fin,
                ..
            } => {
                assert!(local_fin.is_none());
                assert_eq!(remote_fin, Some(fin));
            }
            _ => panic!("connection should be closing"),
        }
    }

    #[test]
    fn on_fin_closing() {
        let syn = 100;
        let syn_ack = 101;
        let endpoint = Endpoint::Acceptor((syn, syn_ack));
        let mut conn = conn(endpoint);

        let send_buf = SendBuffer::new();
        let congestion_ctrl = congestion::Controller::new(conn.config.into());
        let sent_packets = SentPackets::new(syn_ack, syn_ack, congestion_ctrl);
        let recv_buf = ReceiveBuffer::new(syn);

        let local_fin = syn_ack.wrapping_add(3);
        conn.state = State::Closing {
            send_buf,
            sent_packets,
            recv_buf,
            local_fin: Some(local_fin),
            remote_fin: None,
        };

        let remote_fin = syn.wrapping_add(3);
        conn.on_fin(remote_fin, None);
        match conn.state {
            State::Closing {
                local_fin: local,
                remote_fin: remote,
                ..
            } => {
                assert_eq!(local, Some(local_fin));
                assert_eq!(remote, Some(remote_fin));
            }
            _ => panic!("connection should be closing"),
        }
    }

    #[test]
    fn on_fin_closing_non_matching_fin() {
        let syn = 100;
        let syn_ack = 101;
        let endpoint = Endpoint::Acceptor((syn, syn_ack));
        let mut conn = conn(endpoint);

        let send_buf = SendBuffer::new();
        let congestion_ctrl = congestion::Controller::new(conn.config.into());
        let sent_packets = SentPackets::new(syn_ack, syn_ack, congestion_ctrl);
        let recv_buf = ReceiveBuffer::new(syn);

        let fin = syn.wrapping_add(3);
        conn.state = State::Closing {
            send_buf,
            sent_packets,
            recv_buf,
            remote_fin: Some(fin),
            local_fin: None,
        };

        let alt_fin = fin.wrapping_add(1);
        conn.on_fin(alt_fin, None);
        assert!(std::matches!(
            conn.state,
            State::Closed {
                err: Some(Error::InvalidFin),
            }
        ));
    }

    #[test]
    fn on_reset_non_closed() {
        let syn = 100;
        let endpoint = Endpoint::Initiator((syn, 1));
        let mut conn = conn(endpoint);

        conn.on_reset();
        assert!(std::matches!(
            conn.state,
            State::Closed {
                err: Some(Error::Reset),
            }
        ));
    }

    #[test]
    fn on_reset_closed() {
        let syn = 100;
        let endpoint = Endpoint::Initiator((syn, 1));
        let mut conn = conn(endpoint);

        conn.state = State::Closed { err: None };

        conn.on_reset();
        assert!(std::matches!(conn.state, State::Closed { err: None }));
    }
}
