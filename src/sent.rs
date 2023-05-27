use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use crate::congestion;
use crate::packet::{PacketType, SelectiveAck};
use crate::seq::CircularRangeInclusive;
use crate::utils::SizableCircularBuffer;

const LOSS_THRESHOLD: usize = 3;

type Bytes = Vec<u8>;

#[derive(Clone, Debug)]
struct SentPacket {
    pub seq_num: u16,
    pub packet_type: PacketType,
    pub data: Option<Bytes>,
    pub transmission: Instant,
    pub retransmissions: Vec<Instant>,
    pub acks: Vec<Instant>,
}

impl SentPacket {
    fn rtt(&self, now: Instant) -> Duration {
        let last_transmission = self.retransmissions.first().unwrap_or(&self.transmission);
        now.duration_since(*last_transmission)
    }
}

#[derive(Clone, Debug)]
pub struct SentPackets {
    packets: SizableCircularBuffer<SentPacket>,
    init_seq_num: u16,
    seq_num: u16,
    ack_num: u16,
    pub lost_packets: BTreeSet<u16>,
    congestion_ctrl: congestion::Controller,
}

impl SentPackets {
    /// Note: `init_seq_num` corresponds to the sequence number just before the sequence number of
    /// the first packet to track.
    pub fn new(init_seq_num: u16, seq_num: u16, congestion_ctrl: congestion::Controller) -> Self {
        Self {
            packets: SizableCircularBuffer::new(),
            init_seq_num,
            seq_num,
            ack_num: 0,
            lost_packets: BTreeSet::new(),
            congestion_ctrl,
        }
    }

    pub fn inc_seq_num(&mut self) {
        self.seq_num = self.seq_num.wrapping_add(1);
    }

    pub fn next_seq_num(&self) -> u16 {
        self.seq_num.wrapping_add(1)
    }

    pub fn seq_num(&self) -> u16 {
        self.seq_num
    }

    pub fn inc_ack_num(&mut self) {
        self.ack_num = self.ack_num.wrapping_add(1);
    }

    pub fn ack_num(&self) -> u16 {
        self.ack_num

    }

    pub fn seq_num_range(&self) -> CircularRangeInclusive {
        let end = self.next_seq_num().wrapping_sub(1);
        CircularRangeInclusive::new(self.init_seq_num, end)
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

    pub fn has_lost_packets(&self) -> bool {
        !self.lost_packets.is_empty()
    }

    pub fn lost_packets(&self) -> Vec<(u16, PacketType, Option<Bytes>)> {
        self.lost_packets
            .iter()
            .map(|seq| {
                // The unwrap is safe because only sent packets may be lost.
                let packet = self.packets.get(*seq as usize).clone().unwrap();

                (packet.seq_num, packet.packet_type, packet.data.clone())
            })
            .collect()
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
        cur_window_packets: u16,
    ) {
        // If the packet sequence number is beyond the next sequence number, then panic.
        if seq_num > self.next_seq_num() {
            panic!("out of order transmit");
        }

        let is_retransmission = match self.packets.get_mut(seq_num as usize) {
            Some(sent) => {
                sent.retransmissions.push(now);
                true
            }
            None => {
                // If this is not a retransmission and the length of the packet is greater than the amount
                // of available space in the window, then panic.
                if len > self.window() {
                    panic!("transmit exceeds available send window");
                }
                let sent = SentPacket {
                    seq_num,
                    packet_type,
                    data,
                    transmission: now,
                    retransmissions: Vec::new(),
                    acks: Vec::new(),
                };
                self.packets.ensure_size(self.seq_num as usize, cur_window_packets as usize);
                self.packets.put(seq_num as usize, sent);
                false
            }
        };

        let transmit = if is_retransmission {
            congestion::Transmit::Retransmission
        } else {
            congestion::Transmit::Initial { bytes: len }
        };

        // The unwrap is safe given the check above on the available window.
        self.congestion_ctrl.on_transmit(seq_num, transmit).unwrap();

        // increment sequence number on transmit
        self.inc_seq_num();
    }

    /// # Panics
    ///
    /// Panics if `ack_num` does not correspond to a previously sent packet.
    pub fn on_ack(
        &mut self,
        ack_num: u16,
        selective_ack: Option<&SelectiveAck>,
        delay: Duration,
        now: Instant,
    ) {
        if let Some(sack) = selective_ack {
            self.on_selective_ack(ack_num, sack, delay, now);
        } else {
            self.ack(ack_num, delay, now);
        }

        // // An ACK for `ack_num` implicitly ACKs all sequence numbers that precede `ack_num`.
        // // Account for any preceding unacked packets.
        // self.ack_prior_unacked(ack_num, delay, now);
        //
        // // Account for (newly) lost packets.
        // let lost = self.detect_lost_packets();
        // for packet in lost {
        //     if self.lost_packets.insert(packet) {
        //         self.on_lost(packet, true);
        //     }
        // }
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
    ) {
        self.ack(ack_num, delay, now);

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
                self.ack(sack_num, delay, now);
            }

            sack_num = sack_num.wrapping_add(1);
        }
    }

    /// Returns a set containing the sequence numbers of lost packets.
    ///
    /// A packet is lost if it has not been acknowledged and some threshold number of packets sent
    /// after it have been acknowledged.
    pub fn detect_lost_packets(&self, cur_window_packets: u16) -> BTreeSet<u16> {
        let mut lost = BTreeSet::new();

        for i in 0..cur_window_packets {
            let packet_seq_num = self.seq_num.wrapping_sub(1).wrapping_sub(i);
            if let Some(packet) = self.packets.get(packet_seq_num as usize) {
                if packet.retransmissions.is_empty() {
                    lost.insert(packet.seq_num);
                }
            }
        }

        lost
    }

    /// Returns false if `seq_num` does not correspond to a previously sent packet.
    pub fn ack(&mut self, seq_num: u16, delay: Duration, now: Instant) -> bool {
        let binding = self.packets.clone();
        let packet_option = binding.get(seq_num as usize);
        if let Some(packet) = packet_option {
            self.packets.delete(seq_num as usize);

            let ack = congestion::Ack {
                delay,
                rtt: packet.rtt(now),
                received_at: now,
            };
            self.congestion_ctrl.on_ack(packet.seq_num, ack).unwrap();

            // we should stop using it like this
            //packet.acks.push(now);

            self.lost_packets.remove(&packet.seq_num);
            true
        } else {
            false
        }
    }

    /// Acknowledges any unacknowledged packets that precede `ack_num`.
    pub fn ack_prior_unacked(&mut self, ack_num: u16, cur_window_packets: &mut u16, delay: Duration, now: Instant) {
        for _ in 0..ack_num {
            self.ack(ack_num.wrapping_sub(*cur_window_packets), delay, now);
            *cur_window_packets -= 1;
        }
    }

    /// # Panics
    ///
    /// Panics if `seq_num` does not correspond to a previously sent packet.
    pub fn on_lost(&mut self, seq_num: u16, retransmitting: bool) {
        if !self.seq_num_range().contains(seq_num) {
            panic!("cannot mark unsent packet lost");
        }

        // The unwrap is safe assuming that we do not panic above.
        self.congestion_ctrl
            .on_lost_packet(seq_num, retransmitting)
            .expect("lost packet was previously sent");
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
        fn prop(seq_num: u16, len: u8) -> TestResult {
            let congestion_ctrl = congestion::Controller::new(congestion::Config::default());
            let mut sent_packets = SentPackets::new(seq_num, seq_num, congestion_ctrl);
            if len == 0 {
                return TestResult::from_bool(
                    sent_packets.next_seq_num() == seq_num.wrapping_add(1),
                );
            }

            let final_seq_num = seq_num.wrapping_add(u16::from(len));
            let range = CircularRangeInclusive::new(seq_num.wrapping_add(1), final_seq_num);
            let transmission = Instant::now();
            for seq_num in range {
                sent_packets.packets.put(seq_num as usize, SentPacket {
                    seq_num,
                    packet_type: PacketType::Data,
                    data: None,
                    transmission,
                    acks: Default::default(),
                    retransmissions: Default::default(),
                });
                sent_packets.inc_seq_num();
            }

            TestResult::from_bool(sent_packets.next_seq_num() == final_seq_num.wrapping_add(1))
        }
        quickcheck(prop as fn(u16, u8) -> TestResult)
    }

    #[test]
    fn on_transmit_initial() {
        let init_seq_num = u16::MAX;
        let congestion_ctrl = congestion::Controller::new(congestion::Config::default());
        let mut sent_packets = SentPackets::new(init_seq_num, init_seq_num, congestion_ctrl);

        let seq_num = sent_packets.next_seq_num();
        let data = vec![0];
        let len = data.len() as u32;
        let now = Instant::now();
        sent_packets.on_transmit(seq_num, PacketType::Data, Some(data), len, now, 10);

        let packet = sent_packets.packets.get(seq_num as usize).clone().unwrap();
        assert_eq!(packet.seq_num, seq_num);
        assert_eq!(packet.transmission, now);
        assert!(packet.acks.is_empty());
        assert!(packet.retransmissions.is_empty());
    }

    #[test]
    fn on_transmit_retransmit() {
        let init_seq_num = u16::MAX;
        let congestion_ctrl = congestion::Controller::new(congestion::Config::default());
        let mut sent_packets = SentPackets::new(init_seq_num, init_seq_num, congestion_ctrl);

        let seq_num = sent_packets.next_seq_num();
        let data = vec![0];
        let len = data.len() as u32;
        let first = Instant::now();
        let second = Instant::now();
        sent_packets.on_transmit(seq_num, PacketType::Data, Some(data.clone()), len, first, 10);
        sent_packets.on_transmit(seq_num, PacketType::Data, Some(data), len, second, 10);

        let packet = sent_packets.packets.get(seq_num as usize).clone().unwrap();
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
        let mut sent_packets = SentPackets::new(init_seq_num, init_seq_num, congestion_ctrl);

        let out_of_order_seq_num = init_seq_num.wrapping_add(2);
        let data = vec![0];
        let len = data.len() as u32;
        let now = Instant::now();

        sent_packets.on_transmit(out_of_order_seq_num, PacketType::Data, Some(data), len, now, 10);
    }

    #[test]
    fn on_selective_ack() {
        let init_seq_num = u16::MAX;
        let congestion_ctrl = congestion::Controller::new(congestion::Config::default());
        let mut sent_packets = SentPackets::new(init_seq_num, init_seq_num, congestion_ctrl);

        let data = vec![0];
        let len = data.len() as u32;

        const COUNT: usize = 10;
        let mut seq_nums = Vec::new();
        for _ in 0..COUNT {
            let now = Instant::now();
            let seq_num = sent_packets.next_seq_num();
            seq_nums.push(seq_num);
            sent_packets.on_transmit(seq_num, PacketType::Data, Some(data.clone()), len, now, 10);
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
        sent_packets.on_ack(
            init_seq_num.wrapping_add(1),
            Some(&selective_ack),
            DELAY,
            now,
        );
        assert_eq!(sent_packets.packets.get(init_seq_num.wrapping_add(1) as usize).clone().is_none(), true);
        assert!(sent_packets.packets.get(*seq_nums.get(1).unwrap() as usize).clone().unwrap().acks.is_empty());
        for i in seq_nums[2..COUNT].into_iter() {
            let is_packet_acked = i % 2 == 0;
            assert_eq!(sent_packets.packets.get(*i as usize).clone().is_none(), is_packet_acked);
        }
    }

    #[test]
    fn detect_lost_packets() {
        let init_seq_num = u16::MAX;
        let congestion_ctrl = congestion::Controller::new(congestion::Config::default());
        let mut sent_packets = SentPackets::new(init_seq_num, init_seq_num, congestion_ctrl);

        let data = vec![0];
        let len = data.len() as u32;

        const COUNT: usize = 10;
        const START: usize = COUNT - LOSS_THRESHOLD;
        let mut seq_nums = Vec::new();
        for i in 0..COUNT {
            let now = Instant::now();
            let seq_num = sent_packets.next_seq_num();
            seq_nums.push(seq_num);
            sent_packets.on_transmit(seq_num, PacketType::Data, Some(data.clone()), len, now, 10);

            if i >= START {
                sent_packets.ack(seq_num, DELAY, now);
            }
        }

        let lost = sent_packets.detect_lost_packets(20 as u16);
        let hi = lost.len();
        for i in 0..START {
            let packet = &sent_packets.packets.get(*seq_nums.get(i).unwrap() as usize).clone().unwrap();
            assert!(lost.contains(&packet.seq_num));
        }
    }

    #[test]
    fn ack() {
        let init_seq_num = u16::MAX;
        let congestion_ctrl = congestion::Controller::new(congestion::Config::default());
        let mut sent_packets = SentPackets::new(init_seq_num, init_seq_num, congestion_ctrl);

        let seq_num = sent_packets.next_seq_num();
        let data = vec![0];
        let len = data.len() as u32;
        let now = Instant::now();
        sent_packets.on_transmit(seq_num, PacketType::Data, Some(data), len, now, 10);

        // Artificially insert packet into lost packets.
        sent_packets.lost_packets.insert(seq_num);
        assert!(sent_packets.lost_packets.contains(&seq_num));

        let now = Instant::now();
        sent_packets.ack(seq_num, DELAY, now);

        let packet = sent_packets.packets.get(seq_num as usize).clone();

        // If a packet was acked, the receiver got it so we no longer store it
        assert_eq!(packet.is_none(), true);
    }

    #[test]
    fn ack_prior_unacked() {
        let init_seq_num = u16::MAX;
        let congestion_ctrl = congestion::Controller::new(congestion::Config::default());
        let mut sent_packets = SentPackets::new(init_seq_num, init_seq_num, congestion_ctrl);

        let data = vec![0];
        let len = data.len() as u32;

        const COUNT: usize = 10;
        let mut seq_nums = Vec::new();
        for _ in 0..COUNT {
            let now = Instant::now();
            let seq_num = sent_packets.next_seq_num();
            seq_nums.push(seq_num);
            sent_packets.on_transmit(seq_num, PacketType::Data, Some(data.clone()), len, now, 10);
        }

        const ACK_NUM: u16 = 3;
        assert!(usize::from(ACK_NUM) < COUNT);
        assert!(COUNT - usize::from(ACK_NUM) > 2);

        let now = Instant::now();
        sent_packets.ack_prior_unacked(ACK_NUM, &mut 10, DELAY, now);
        for i in seq_nums[0..usize::from(ACK_NUM)].into_iter() {
            assert_eq!(sent_packets.packets.get(*i as usize).clone().unwrap().acks.len(), 0);
        }
    }

    #[test]
    fn ack_unsent() {
        let init_seq_num = u16::MAX;
        let congestion_ctrl = congestion::Controller::new(congestion::Config::default());
        let mut sent_packets = SentPackets::new(init_seq_num, init_seq_num, congestion_ctrl);

        let unsent_ack_num = init_seq_num.wrapping_add(2);
        let now = Instant::now();
        let result = sent_packets.ack(unsent_ack_num, DELAY, now);
        assert_eq!(result, false);
    }
}
