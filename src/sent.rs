use std::collections::BTreeSet;
use std::fmt::{self, Formatter};
use std::time::{Duration, Instant};

use delay_map::HashMapDelay;
use tracing::{error, info};

use crate::circular_buffer::SizableCircularBuffer;
use crate::congestion;
use crate::packet::{Packet, PacketType, SelectiveAck};
use crate::seq::CircularRangeInclusive;

const LOSS_THRESHOLD: usize = 3;

type Bytes = Vec<u8>;

#[derive(Clone, Debug)]
pub struct SentPacket {
    pub seq_num: u16,
    pub packet_type: PacketType,
    pub data: Option<Bytes>,
    pub transmission: Instant,
    pub retransmissions: Vec<Instant>,
    pub acks: Vec<Instant>,
    pub need_resend: bool,
}

impl SentPacket {
    fn rtt(&self, now: Instant) -> Duration {
        let last_transmission = self.retransmissions.first().unwrap_or(&self.transmission);
        now.duration_since(*last_transmission)
    }
}

#[derive(Clone, Debug)]
pub struct SentPackets {
    /// The unacked packets in flight.
    pub outgoing_packets: SizableCircularBuffer<SentPacket>,

    /// The sequence number of the next packet to send.
    pub next_sequence_number: u16,

    /// The amount of packets in flight
    pub current_packet_window: u16,
    congestion_ctrl: congestion::Controller,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SentPacketsError {
    InvalidAckNum,
}

impl fmt::Display for SentPacketsError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAckNum => write!(f, "invalid ack number"),
        }
    }
}

impl std::error::Error for SentPacketsError {}

impl SentPackets {
    /// Note: `next_sequence_number` corresponds to the sequence number of the next packet to send.
    pub fn new(next_sequence_number: u16, congestion_ctrl: congestion::Controller) -> Self {
        Self {
            outgoing_packets: SizableCircularBuffer::new(),
            next_sequence_number: next_sequence_number.wrapping_add(1),
            current_packet_window: 0,
            congestion_ctrl,
        }
    }

    pub fn next_seq_num(&self) -> u16 {
        self.next_sequence_number
    }

    pub fn ack_num(&self) -> u16 {
        self.last_ack_num().unwrap_or(0)
    }

    pub fn seq_num_range(&self) -> CircularRangeInclusive {
        CircularRangeInclusive::new(
            self.next_sequence_number
                .wrapping_sub(self.current_packet_window)
                .wrapping_sub(2),
            self.next_sequence_number,
        )
    }

    pub fn timeout(&self) -> Duration {
        self.congestion_ctrl.timeout()
    }

    pub fn on_timeout(&mut self) {
        self.congestion_ctrl.on_timeout()
    }

    pub fn window(&self) -> u32 {
        self.congestion_ctrl.bytes_available_in_window()
    }

    pub fn has_unacked_packets(&self) -> bool {
        self.first_unacked_seq_num().is_some()
    }

    /// # Panics
    ///
    /// Panics if `seq_num` does not correspond to the next expected packet or a previously sent
    /// packet.
    ///
    /// Panics if the transmit is not a retransmission and `len` is greater than the amount of
    /// available space in the window.
    pub fn on_transmit(
        &mut self,
        seq_num: u16,
        packet_type: PacketType,
        data: Option<Bytes>,
        len: u32,
        now: Instant,
    ) {
        let is_retransmission = self.next_sequence_number != seq_num;
        // info!(
        //     "transmitting seq_num= {:?} {:?}, packet_type={:?}, len={:?}, is_retransmission={:?}",
        //     self.next_sequence_number, seq_num, packet_type, len, is_retransmission
        // );

        // If the packet sequence number is beyond the next sequence number, then panic.
        if !self.seq_num_range().contains(seq_num) {
            panic!(
                "out of order transmit {:?} {:?}",
                self.seq_num_range(),
                seq_num
            );
        }

        // If this is not a retransmission and the length of the packet is greater than the amount
        // of available space in the window, then panic.
        if !is_retransmission && len > self.window() {
            panic!("transmit exceeds available send window");
        }

        match self.outgoing_packets.get_mut(seq_num as usize) {
            Some(sent) => {
                sent.retransmissions.push(now);
            }
            None => {
                let sent = SentPacket {
                    seq_num,
                    packet_type,
                    data,
                    transmission: now,
                    retransmissions: Vec::new(),
                    acks: Vec::new(),
                    need_resend: false,
                };
                self.current_packet_window += 1;
                self.outgoing_packets
                    .ensure_size(seq_num as usize, self.current_packet_window as usize);
                self.outgoing_packets.put(seq_num as usize, sent);
                self.next_sequence_number = self.next_sequence_number.wrapping_add(1);
            }
        }

        let transmit = if is_retransmission {
            congestion::Transmit::Retransmission
        } else {
            congestion::Transmit::Initial { bytes: len }
        };

        // The unwrap is safe given the check above on the available window.
        self.congestion_ctrl.on_transmit(seq_num, transmit).unwrap();
    }

    /// Handle an ACK for a packet with sequence number `ack_num`, and an optional selective_ack.
    ///
    /// Returns Error, if `ack_num` is not within the sequence number range of the sent packets.
    pub fn on_ack(
        &mut self,
        ack_num: u16,
        selective_ack: Option<&SelectiveAck>,
        delay: Duration,
        now: Instant,
        unacked: &mut HashMapDelay<u16, Packet>,
    ) -> Result<(), SentPacketsError> {
        let range = self.seq_num_range();
        if !CircularRangeInclusive::new(
            self.next_sequence_number
                .wrapping_sub(self.current_packet_window)
                .wrapping_sub(2),
            self.next_sequence_number.wrapping_sub(1),
        )
        .contains(ack_num)
        {
            return Err(SentPacketsError::InvalidAckNum);
        }

        // Do not ACK if ACK num corresponds to initial packet.
        if self.current_packet_window != 0 {
            self.on_ack_num(ack_num, selective_ack, delay, now, unacked);
        } else {
            unacked.remove(&ack_num);
        }

        while self.current_packet_window > 0
            && self
                .outgoing_packets
                .get(
                    self.next_sequence_number
                        .wrapping_sub(self.current_packet_window) as usize,
                )
                .is_none()
        {
            self.current_packet_window -= 1;
        }

        Ok(())
    }

    /// # Panics
    ///
    /// Panics if `ack_num` does not correspond to a previously sent packet.
    fn on_ack_num(
        &mut self,
        ack_num: u16,
        selective_ack: Option<&SelectiveAck>,
        delay: Duration,
        now: Instant,
        unacked: &mut HashMapDelay<u16, Packet>,
    ) {
        if let Some(sack) = selective_ack {
            self.on_selective_ack(ack_num, sack, delay, now, unacked);
        } else {
            self.ack(ack_num, delay, now, unacked);
        }

        // An ACK for `ack_num` implicitly ACKs all sequence numbers that precede `ack_num`.
        // Account for any preceding unacked packets.
        self.ack_prior_unacked(ack_num, delay, now, unacked);
    }

    /// # Panics
    ///
    /// Panics if `ack_num` does not correspond to a previously sent packet.
    fn on_selective_ack(
        &mut self,
        ack_num: u16,
        selective_ack: &SelectiveAck,
        delay: Duration,
        now: Instant,
        unacked: &mut HashMapDelay<u16, Packet>,
    ) {
        self.ack(ack_num, delay, now, unacked);

        let range = self.seq_num_range();

        // The first bit of the selective ACK corresponds to `ack_num.wrapping_add(2)`, where
        // `ack_num.wrapping_add(1)` is assumed to have been dropped.
        let mut sack_num = ack_num.wrapping_add(2);
        for ack in selective_ack.acked() {
            // Break once we exhaust all sent sequence numbers. The selective ACK length is a
            // multiple of 32, so it may be padded beyond the actual range of sequence numbers.
            if !range.contains(sack_num) {
                break;
            }

            if ack {
                self.ack(sack_num, delay, now, unacked);
            }

            sack_num = sack_num.wrapping_add(1);
        }
    }

    /// Returns a set containing the sequence numbers of lost packets.
    ///
    /// A packet is lost if it has not been acknowledged and some threshold number of packets sent
    /// after it have been acknowledged.
    fn detect_lost_packets(&self) -> BTreeSet<u16> {
        let mut acked = 0;
        let mut lost = BTreeSet::new();

        for i in 0..self.current_packet_window {
            let seq_num = self.next_sequence_number.wrapping_sub(1).wrapping_sub(i);
            if let Some(packet) = self.outgoing_packets.get(seq_num as usize) {
                if packet.acks.is_empty() && acked >= LOSS_THRESHOLD {
                    lost.insert(packet.seq_num);
                }

                if !packet.acks.is_empty() {
                    acked += 1;
                }
            }
        }

        lost
    }

    fn ack(
        &mut self,
        seq_num: u16,
        delay: Duration,
        now: Instant,
        unacked: &mut HashMapDelay<u16, Packet>,
    ) {
        if let Some(packet) = self.outgoing_packets.get(seq_num as usize).cloned() {
            let ack = congestion::Ack {
                delay,
                rtt: packet.rtt(now),
                received_at: now,
            };
            self.congestion_ctrl.on_ack(packet.seq_num, ack).unwrap();

            self.outgoing_packets.delete(seq_num as usize);
            unacked.remove(&packet.seq_num);
        } else {
            // panic!("cannot ack unsent packet");
        }
    }

    /// Acknowledges any unacknowledged packets that precede `seq_num`.
    fn ack_prior_unacked(
        &mut self,
        seq_num: u16,
        delay: Duration,
        now: Instant,
        unacked: &mut HashMapDelay<u16, Packet>,
    ) {
        if let Some(first_unacked) = self.first_unacked_seq_num() {
            let start = first_unacked;
            let end = seq_num;
            if start >= end {
                return;
            }

            for i in start..end {
                if let Some(packet) = self.outgoing_packets.get(i as usize) {
                    self.ack(packet.seq_num, delay, now, unacked);
                }
            }
        }
    }

    /// # Panics
    ///
    /// Panics if `seq_num` does not correspond to a previously sent packet.
    fn on_lost(&mut self, seq_num: u16, retransmitting: bool) {
        if !self.seq_num_range().contains(seq_num) {
            panic!("cannot mark unsent packet lost");
        }

        // The unwrap is safe assuming that we do not panic above.
        self.congestion_ctrl
            .on_lost_packet(seq_num, retransmitting)
            .expect("lost packet was previously sent");
    }

    /// Returns the sequence number of the last (i.e. latest) packet in a contiguous sequence of
    /// acknowledged packets.
    ///
    /// Returns `None` if none of the (possibly zero) packets have been acknowledged.
    pub fn last_ack_num(&self) -> Option<u16> {
        Some(
            self.next_sequence_number
                .wrapping_sub(self.current_packet_window)
                .wrapping_sub(1),
        )
    }

    /// Returns the sequence number of the first (i.e. earliest) packet that has not been
    /// acknowledged.
    ///
    /// Returns `None` if all (possibly zero) sent packets have been acknowledged.
    fn first_unacked_seq_num(&self) -> Option<u16> {
        Some(
            self.next_sequence_number
                .wrapping_sub(self.current_packet_window),
        )
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use quickcheck::{quickcheck, TestResult};

    const DELAY: Duration = Duration::from_millis(100);

    // TODO: Bolster tests.

    #[test]
    fn next_seq_num() {
        fn prop(next_seq_num: u16, len: u8) -> TestResult {
            let congestion_ctrl = congestion::Controller::new(congestion::Config::default());
            let mut sent_packets = SentPackets::new(next_seq_num, congestion_ctrl);
            if len == 0 {
                return TestResult::from_bool(
                    sent_packets.next_seq_num() == next_seq_num.wrapping_add(1),
                );
            }

            let final_seq_num = next_seq_num.wrapping_add(u16::from(len));
            let range = CircularRangeInclusive::new(next_seq_num.wrapping_add(1), final_seq_num);
            let transmission = Instant::now();
            for seq_num in range {
                sent_packets.outgoing_packets.put(
                    seq_num as usize,
                    SentPacket {
                        seq_num,
                        packet_type: PacketType::Data,
                        data: None,
                        transmission,
                        acks: Default::default(),
                        retransmissions: Default::default(),
                        need_resend: false,
                    },
                );
                sent_packets.next_sequence_number = seq_num.wrapping_add(1);
                sent_packets.current_packet_window =
                    sent_packets.current_packet_window.wrapping_add(1);
            }

            TestResult::from_bool(sent_packets.next_seq_num() == final_seq_num.wrapping_add(1))
        }
        quickcheck(prop as fn(u16, u8) -> TestResult)
    }

    #[test]
    fn on_transmit_initial() {
        let next_seq_num = u16::MAX;
        let congestion_ctrl = congestion::Controller::new(congestion::Config::default());
        let mut sent_packets = SentPackets::new(next_seq_num, congestion_ctrl);

        let seq_num = sent_packets.next_seq_num();
        let data = vec![0];
        let len = data.len() as u32;
        let now = Instant::now();
        sent_packets.on_transmit(seq_num, PacketType::Data, Some(data), len, now);

        assert_eq!(sent_packets.current_packet_window, 1);

        let packet = &sent_packets
            .outgoing_packets
            .get(sent_packets.next_seq_num().wrapping_sub(1) as usize)
            .unwrap();
        assert_eq!(packet.seq_num, seq_num);
        assert_eq!(packet.transmission, now);
        assert!(packet.acks.is_empty());
        assert!(packet.retransmissions.is_empty());
    }

    #[test]
    fn on_transmit_retransmit() {
        let init_seq_num = u16::MAX;
        let congestion_ctrl = congestion::Controller::new(congestion::Config::default());
        let mut sent_packets = SentPackets::new(init_seq_num, congestion_ctrl);

        let seq_num = sent_packets.next_seq_num();
        let data = vec![0];
        let len = data.len() as u32;
        let first = Instant::now();
        let second = Instant::now();
        sent_packets.on_transmit(seq_num, PacketType::Data, Some(data.clone()), len, first);
        sent_packets.on_transmit(seq_num, PacketType::Data, Some(data), len, second);

        assert_eq!(sent_packets.current_packet_window, 1);

        let packet = &sent_packets
            .outgoing_packets
            .get(sent_packets.next_seq_num().wrapping_sub(1) as usize)
            .unwrap();
        assert_eq!(packet.seq_num, seq_num);
        assert_eq!(packet.transmission, first);
        assert!(packet.acks.is_empty());
        assert_eq!(packet.retransmissions.len(), 1);
        assert_eq!(packet.retransmissions[0], second);
    }

    #[test]
    #[should_panic]
    fn on_transmit_out_of_order() {
        let init_seq_num = u16::MAX;
        let congestion_ctrl = congestion::Controller::new(congestion::Config::default());
        let mut sent_packets = SentPackets::new(init_seq_num, congestion_ctrl);

        let out_of_order_seq_num = init_seq_num.wrapping_add(2);
        let data = vec![0];
        let len = data.len() as u32;
        let now = Instant::now();

        sent_packets.on_transmit(out_of_order_seq_num, PacketType::Data, Some(data), len, now);
    }

    #[test]
    fn on_selective_ack() {
        let next_seq_num = u16::MAX;
        let congestion_ctrl = congestion::Controller::new(congestion::Config::default());
        let mut sent_packets = SentPackets::new(next_seq_num, congestion_ctrl);
        let mut unacked = HashMapDelay::new(Duration::from_secs(1));

        let data = vec![0];
        let len = data.len() as u32;

        const COUNT: usize = 10;
        for _ in 0..COUNT {
            let now = Instant::now();
            let seq_num = sent_packets.next_seq_num();
            sent_packets.on_transmit(seq_num, PacketType::Data, Some(data.clone()), len, now);
        }

        const SACK_LEN: usize = COUNT - 2;
        let mut acked = vec![false; SACK_LEN];
        for (i, ack) in acked.iter_mut().enumerate() {
            if i % 2 == 0 {
                *ack = true;
            }
        }
        let selective_ack = SelectiveAck::new(acked);

        let now = Instant::now();
        sent_packets
            .on_ack(
                next_seq_num.wrapping_add(1),
                Some(&selective_ack),
                DELAY,
                now,
                &mut unacked,
            )
            .unwrap();
        for i in 2..COUNT {
            let is_empty = i % 2 == 0;
            assert_eq!(
                sent_packets
                    .outgoing_packets
                    .get(next_seq_num.wrapping_add(i as u16) as usize)
                    .unwrap()
                    .acks
                    .is_empty(),
                is_empty
            );
        }
    }

    #[test]
    fn detect_lost_packets() {
        let next_seq_num = u16::MAX;
        let congestion_ctrl = congestion::Controller::new(congestion::Config::default());
        let mut sent_packets = SentPackets::new(next_seq_num, congestion_ctrl);
        let mut unacked = HashMapDelay::new(Duration::from_secs(1));

        let data = vec![0];
        let len = data.len() as u32;

        const COUNT: usize = 10;
        const START: usize = COUNT - LOSS_THRESHOLD;
        for i in 0..COUNT {
            let now = Instant::now();
            let seq_num = sent_packets.next_seq_num();
            sent_packets.on_transmit(seq_num, PacketType::Data, Some(data.clone()), len, now);

            if i >= START {
                sent_packets.ack(seq_num, DELAY, now, &mut unacked);
            }
        }

        let lost = sent_packets.detect_lost_packets();
        panic!("{:?}", lost);
        for i in [65535, 0, 1, 2, 3, 4, 5] {
            let packet = &sent_packets.outgoing_packets.get(i).unwrap();
            assert!(lost.contains(&packet.seq_num));
        }
    }

    #[test]
    fn ack() {
        let next_seq_num = u16::MAX;
        let congestion_ctrl = congestion::Controller::new(congestion::Config::default());
        let mut sent_packets = SentPackets::new(next_seq_num, congestion_ctrl);
        let mut unacked = HashMapDelay::new(Duration::from_secs(1));

        let seq_num = sent_packets.next_seq_num();
        let data = vec![0];
        let len = data.len() as u32;
        let now = Instant::now();
        sent_packets.on_transmit(seq_num, PacketType::Data, Some(data), len, now);

        let now = Instant::now();
        sent_packets.ack(seq_num, DELAY, now, &mut unacked);

        let packet = sent_packets.outgoing_packets.get(seq_num as usize).unwrap();

        assert_eq!(packet.acks.len(), 1);
        assert_eq!(packet.acks[0], now);
    }

    #[test]
    fn ack_prior_unacked() {
        let next_seq_num = u16::MAX;
        let congestion_ctrl = congestion::Controller::new(congestion::Config::default());
        let mut sent_packets = SentPackets::new(next_seq_num, congestion_ctrl);
        let mut unacked = HashMapDelay::new(Duration::from_secs(1));

        let data = vec![0];
        let len = data.len() as u32;

        const COUNT: usize = 10;
        for _ in 0..COUNT {
            let now = Instant::now();
            let seq_num = sent_packets.next_seq_num();
            sent_packets.on_transmit(seq_num, PacketType::Data, Some(data.clone()), len, now);
        }

        const ACK_NUM: u16 = 3;
        assert!(usize::from(ACK_NUM) < COUNT);
        assert!(COUNT - usize::from(ACK_NUM) > 2);

        let now = Instant::now();
        sent_packets.ack_prior_unacked(ACK_NUM, DELAY, now, &mut unacked);
        panic!("{:?}", sent_packets.outgoing_packets);
        for i in 0..usize::from(ACK_NUM) {
            assert_eq!(sent_packets.outgoing_packets.get(i).unwrap().acks.len(), 1);
        }
    }

    #[test]
    #[should_panic]
    fn ack_unsent() {
        let init_seq_num = u16::MAX;
        let congestion_ctrl = congestion::Controller::new(congestion::Config::default());
        let mut sent_packets = SentPackets::new(init_seq_num, congestion_ctrl);
        let mut unacked = HashMapDelay::new(Duration::from_secs(1));

        let unsent_ack_num = init_seq_num.wrapping_add(2);
        let now = Instant::now();
        sent_packets.ack(unsent_ack_num, DELAY, now, &mut unacked);
    }
}
