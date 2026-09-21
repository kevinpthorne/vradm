use std::collections::{VecDeque, HashMap};
use crate::framing::{seq_after, seq_after_eq, seq_diff, seq_advance, CanonicalDataFrame};

pub const MAX_IP_FRAGMENTS: usize = 8;
pub const MAX_FRAG_PAYLOAD: usize = 37;

#[derive(Debug, Clone, PartialEq)]
pub struct FragmentHeader {
    pub urgent_flush: bool,
    pub frag_idx: u8,
    pub total_frags_minus_one: u8,
    pub best_effort: bool,
}

impl FragmentHeader {
    pub fn encode(&self) -> u8 {
        ((self.urgent_flush as u8) << 7) |
        ((self.frag_idx & 0x07) << 4) |
        ((self.total_frags_minus_one & 0x07) << 1) |
        (self.best_effort as u8)
    }

    pub fn decode(byte: u8) -> Self {
        Self {
            urgent_flush: (byte & 0x80) != 0,
            frag_idx: (byte >> 4) & 0x07,
            total_frags_minus_one: (byte >> 1) & 0x07,
            best_effort: (byte & 0x01) != 0,
        }
    }
}

pub struct IpPacketSlicer;

impl IpPacketSlicer {
    pub fn slice(
        packet: &[u8],
        urgent_flush: bool,
        best_effort: bool,
        start_seq: u8,
    ) -> Result<Vec<CanonicalDataFrame>, &'static str> {
        if packet.is_empty() {
            return Err("Empty packet");
        }
        let total_frags = (packet.len() + MAX_FRAG_PAYLOAD - 1) / MAX_FRAG_PAYLOAD;
        if total_frags > MAX_IP_FRAGMENTS {
            return Err("Packet too large");
        }

        let mut frames = Vec::with_capacity(total_frags);
        for i in 0..total_frags {
            let offset = i * MAX_FRAG_PAYLOAD;
            let end = (offset + MAX_FRAG_PAYLOAD).min(packet.len());
            let chunk = &packet[offset..end];

            let header = FragmentHeader {
                urgent_flush,
                frag_idx: i as u8,
                total_frags_minus_one: (total_frags - 1) as u8,
                best_effort,
            };

            let mut frame = CanonicalDataFrame::new();
            // Protocol Version = 10 (binary) -> 2
            // FRAME_CLASS = bit 2
            let mut ctrl = 0x02; // Protocol version 3.8 = 0b10
            if best_effort {
                ctrl |= 1 << 2;
            }
            frame.ctrl = ctrl;
            frame.seq = seq_advance(start_seq, i as u8);
            frame.payload_len = (1 + chunk.len()) as u8;
            frame.payload[0] = header.encode();
            frame.payload[1..1 + chunk.len()].copy_from_slice(chunk);
            
            frames.push(frame);
        }

        Ok(frames)
    }
}

#[derive(Debug, Clone)]
pub struct InFlightFrame {
    pub frame: CanonicalDataFrame,
    pub acked: bool,
}

pub struct ArqTransmitter {
    pub next_seq: u8,
    pub next_be_seq: u8,
    pub in_flight: VecDeque<InFlightFrame>,
    pub pending_reliable: VecDeque<CanonicalDataFrame>,
    pub pending_be: VecDeque<CanonicalDataFrame>,
}

impl ArqTransmitter {
    pub fn new() -> Self {
        Self {
            next_seq: 0,
            next_be_seq: 0,
            in_flight: VecDeque::new(),
            pending_reliable: VecDeque::new(),
            pending_be: VecDeque::new(),
        }
    }

    pub fn enqueue_packet(&mut self, packet: &[u8], urgent_flush: bool, best_effort: bool) -> Result<(), &'static str> {
        let start_seq = if best_effort { self.next_be_seq } else { self.next_seq };
        let frames = IpPacketSlicer::slice(packet, urgent_flush, best_effort, start_seq)?;
        if best_effort {
            self.next_be_seq = seq_advance(self.next_be_seq, frames.len() as u8);
            for frame in frames {
                self.pending_be.push_back(frame);
            }
        } else {
            self.next_seq = seq_advance(self.next_seq, frames.len() as u8);
            for frame in frames {
                self.pending_reliable.push_back(frame);
            }
        }
        Ok(())
    }

    pub fn on_ack_received(&mut self, ack_base: u8, ack_map: u8) {
        // Remove cumulatively acked frames
        while let Some(inflight) = self.in_flight.front() {
            if seq_after_eq(ack_base, inflight.frame.seq) {
                self.in_flight.pop_front();
            } else {
                break;
            }
        }

        // Process selective acks
        for inflight in self.in_flight.iter_mut() {
            let diff = seq_diff(inflight.frame.seq, ack_base);
            if diff > 0 && diff <= 7 {
                let bit_idx = diff - 1;
                if (ack_map & (1 << bit_idx)) != 0 {
                    inflight.acked = true;
                }
            }
        }
    }

    pub fn get_frames_to_transmit(&mut self, max_frames: usize) -> Vec<CanonicalDataFrame> {
        let mut to_transmit = Vec::new();

        // 1. Send best-effort frames first, they don't consume ARQ window
        while let Some(_) = self.pending_be.front() {
            if to_transmit.len() >= max_frames {
                break;
            }
            to_transmit.push(self.pending_be.pop_front().unwrap());
        }

        // Fill window up to W_ARQ = 8
        while self.in_flight.len() < 8 && !self.pending_reliable.is_empty() {
            let frame = self.pending_reliable.pop_front().unwrap();
            self.in_flight.push_back(InFlightFrame {
                frame,
                acked: false,
            });
        }

        for inflight in self.in_flight.iter_mut() {
            if !inflight.acked && to_transmit.len() < max_frames {
                to_transmit.push(inflight.frame.clone());
            }
        }

        to_transmit
    }
}

pub struct ReassemblyContext {
    pub pkt_id: u8,
    pub fragments: [Option<Vec<u8>>; 8],
    pub total_frags: u8,
    pub received_mask: u8,
}

pub struct ArqReceiver {
    pub ack_base: u8,
    pub ack_map: u8,
    pub current_seq_rx: u8,
    pub current_be_seq_rx: u8,
    pub contexts: HashMap<(bool, u8), ReassemblyContext>,
    pub initialized: bool,
    pub be_initialized: bool,
}

impl ArqReceiver {
    pub fn new() -> Self {
        Self {
            ack_base: 0,
            ack_map: 0,
            current_seq_rx: 0,
            current_be_seq_rx: 0,
            contexts: HashMap::new(),
            initialized: false,
            be_initialized: false,
        }
    }

    pub fn receive_frame(&mut self, frame: &CanonicalDataFrame) -> Option<Vec<u8>> {
        let is_best_effort = (frame.ctrl & 0x04) != 0;
        let seq = frame.seq;

        if !is_best_effort {
            if !self.initialized {
                self.ack_base = seq.wrapping_sub(1);
                self.current_seq_rx = self.ack_base;
                self.initialized = true;
            }

            if seq_after(seq, self.current_seq_rx) {
                self.current_seq_rx = seq;
            }

            if seq_after(seq, self.ack_base) {
                let diff = seq_diff(seq, self.ack_base);
                if diff == 1 {
                    self.ack_base = seq;
                    self.ack_map >>= 1;
                    while (self.ack_map & 1) != 0 {
                        self.ack_base = seq_advance(self.ack_base, 1);
                        self.ack_map >>= 1;
                    }
                } else if diff <= 7 {
                    let bit_idx = diff - 1;
                    self.ack_map |= 1 << bit_idx;
                }
            }
        } else {
            if !self.be_initialized {
                self.current_be_seq_rx = seq.wrapping_sub(1);
                self.be_initialized = true;
            }
            if seq_after(seq, self.current_be_seq_rx) {
                self.current_be_seq_rx = seq;
            }
        }

        if frame.payload_len == 0 {
            return None;
        }

        let payload = &frame.payload[0..(frame.payload_len as usize)];
        let header = FragmentHeader::decode(payload[0]);
        let data = &payload[1..];

        let pkt_id = seq.wrapping_sub(header.frag_idx);
        let ctx_key = (is_best_effort, pkt_id);

        let current_max_seq = if is_best_effort { self.current_be_seq_rx } else { self.current_seq_rx };

        if seq_after(current_max_seq, pkt_id) {
            if seq_diff(current_max_seq, pkt_id) > 64 {
                self.contexts.remove(&ctx_key);
                // Also eagerly purge any other stale contexts of the same stream type
                self.contexts.retain(|&(be, k), _| {
                    if be != is_best_effort {
                        true
                    } else {
                        seq_diff(current_max_seq, k) <= 64
                    }
                });
                return None;
            }
        }

        let total_frags = header.total_frags_minus_one + 1;
        let ctx = self.contexts.entry(ctx_key).or_insert(ReassemblyContext {
            pkt_id,
            fragments: Default::default(),
            total_frags,
            received_mask: 0,
        });

        if header.frag_idx < total_frags {
            if (ctx.received_mask & (1 << header.frag_idx)) == 0 {
                ctx.fragments[header.frag_idx as usize] = Some(data.to_vec());
                ctx.received_mask |= 1 << header.frag_idx;
            }
        }

        if ctx.received_mask == (1 << total_frags) - 1 {
            let mut packet = Vec::new();
            for i in 0..total_frags {
                packet.extend_from_slice(ctx.fragments[i as usize].as_ref().unwrap());
            }
            self.contexts.remove(&ctx_key);
            Some(packet)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fragment_header() {
        let h = FragmentHeader {
            urgent_flush: true,
            frag_idx: 3,
            total_frags_minus_one: 7,
            best_effort: false,
        };
        let b = h.encode();
        let h2 = FragmentHeader::decode(b);
        assert_eq!(h, h2);
    }

    #[test]
    fn test_slicer_and_reassembly() {
        let mut tx = ArqTransmitter::new();
        let mut rx = ArqReceiver::new();

        let packet = vec![0xAB; 100]; // 100 bytes -> 3 fragments (37, 37, 26)
        tx.enqueue_packet(&packet, false, false).unwrap();

        let frames = tx.get_frames_to_transmit(10);
        assert_eq!(frames.len(), 3);

        assert_eq!(rx.receive_frame(&frames[0]), None);
        assert_eq!(rx.receive_frame(&frames[1]), None);
        let reassembled = rx.receive_frame(&frames[2]).unwrap();

        assert_eq!(reassembled, packet);

        // Acknowledge all frames cumulatively
        tx.on_ack_received(2, 0); // ack_base = 2 (meaning 0, 1, 2 are received)
        assert_eq!(tx.in_flight.len(), 0);
    }

    #[test]
    fn test_selective_repeat_arq() {
        let mut tx = ArqTransmitter::new();
        let mut rx = ArqReceiver::new();

        // Enqueue 3 packets of 1 fragment each
        let p1 = vec![1; 20];
        let p2 = vec![2; 20];
        let p3 = vec![3; 20];

        tx.enqueue_packet(&p1, false, false).unwrap();
        tx.enqueue_packet(&p2, false, false).unwrap();
        tx.enqueue_packet(&p3, false, false).unwrap();

        let frames = tx.get_frames_to_transmit(10);
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].seq, 0);
        assert_eq!(frames[1].seq, 1);
        assert_eq!(frames[2].seq, 2);

        // Receiver receives frame 0 and frame 2 (frame 1 dropped)
        let _ = rx.receive_frame(&frames[0]);
        let _ = rx.receive_frame(&frames[2]);

        // Rx state: ack_base should be 0.
        // ack_map should have bit 0 set (representing seq 2, because seq 2 is ack_base + 2, which is bit index 1?
        // Wait, diff = 2, so bit_idx = 1. Let's check.)
        assert_eq!(rx.ack_base, 0);
        assert_eq!(rx.ack_map, 1 << 1); 

        tx.on_ack_received(rx.ack_base, rx.ack_map);

        // Now frame 0 is acked cumulatively. Frame 2 is acked selectively.
        // Frame 1 is unacked.
        let retransmit = tx.get_frames_to_transmit(10);
        assert_eq!(retransmit.len(), 1);
        assert_eq!(retransmit[0].seq, 1);

        // Receive frame 1
        let p2_rx = rx.receive_frame(&retransmit[0]).unwrap();
        assert_eq!(p2_rx, p2);

        // Now rx ack_base should jump to 2.
        assert_eq!(rx.ack_base, 2);
        assert_eq!(rx.ack_map, 0);
    }

    #[test]
    fn test_fatal_ack_map_wrap() {
        let mut rx = ArqReceiver::new();
        // Initialize rx with ack_base = 0.
        // We will receive seq = 0 first.
        let mut f0 = CanonicalDataFrame::new();
        f0.seq = 0;
        f0.payload_len = 1;
        f0.payload[0] = 0; // frag_idx 0, total 0
        rx.receive_frame(&f0);
        assert_eq!(rx.ack_base, 0);

        // Now we receive seq = 8 (diff = 8).
        let mut f8 = CanonicalDataFrame::new();
        f8.seq = 8;
        f8.payload_len = 1;
        f8.payload[0] = 0;
        rx.receive_frame(&f8);
        
        // This should not set bit 7 of ack_map!
        assert_eq!(rx.ack_map & 0x80, 0, "Bit 7 (Feedback Type) was overwritten!");
    }
}


