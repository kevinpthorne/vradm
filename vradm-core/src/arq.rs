use std::collections::VecDeque;
use crate::framing::{seq_after, seq_after_eq, seq_diff, seq_advance, CanonicalDataFrame};

pub const MAX_IP_FRAGMENTS: usize = 8;
pub const MAX_FRAG_PAYLOAD: usize = 37;
pub const MAX_REASSEMBLY_CONTEXTS: usize = 16;
pub const MAX_IP_PACKET_LEN: usize = 296;

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
    pub fn slice_into(
        packet: &[u8],
        urgent_flush: bool,
        best_effort: bool,
        start_seq: u8,
        out_frames: &mut [CanonicalDataFrame; MAX_IP_FRAGMENTS],
    ) -> Result<usize, &'static str> {
        if packet.is_empty() {
            return Err("Empty packet");
        }
        let total_frags = (packet.len() + MAX_FRAG_PAYLOAD - 1) / MAX_FRAG_PAYLOAD;
        if total_frags > MAX_IP_FRAGMENTS {
            return Err("Packet too large");
        }

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
            let mut ctrl = 0x02; // Protocol version 3.8 = 0b10
            if best_effort {
                ctrl |= 1 << 2;
            }
            frame.ctrl = ctrl;
            frame.seq = seq_advance(start_seq, i as u8);
            frame.payload_len = (1 + chunk.len()) as u8;
            frame.payload[0] = header.encode();
            frame.payload[1..1 + chunk.len()].copy_from_slice(chunk);

            out_frames[i] = frame;
        }

        Ok(total_frags)
    }

    pub fn slice(
        packet: &[u8],
        urgent_flush: bool,
        best_effort: bool,
        start_seq: u8,
    ) -> Result<Vec<CanonicalDataFrame>, &'static str> {
        let mut frames = [CanonicalDataFrame::new(); MAX_IP_FRAGMENTS];
        let total_frags = Self::slice_into(packet, urgent_flush, best_effort, start_seq, &mut frames)?;
        Ok(frames[..total_frags].to_vec())
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
            in_flight: VecDeque::with_capacity(16),
            // Pre-allocate to accommodate bursts: 1024 frames
            // SPEC §1.2 & §10 Zero-Allocation Invariant on real-time audio thread
            pending_reliable: VecDeque::with_capacity(1024),
            pending_be: VecDeque::with_capacity(1024),
        }
    }

    pub fn reset(&mut self) {
        self.next_seq = 0;
        self.next_be_seq = 0;
        self.in_flight.clear();
        self.pending_reliable.clear();
        self.pending_be.clear();
    }

    pub fn enqueue_packet(&mut self, packet: &[u8], urgent_flush: bool, best_effort: bool) -> Result<(), &'static str> {
        let start_seq = if best_effort { self.next_be_seq } else { self.next_seq };
        let mut frames = [CanonicalDataFrame::new(); MAX_IP_FRAGMENTS];
        let total_frags = IpPacketSlicer::slice_into(packet, urgent_flush, best_effort, start_seq, &mut frames)?;
        if best_effort {
            if self.pending_be.len() + total_frags > self.pending_be.capacity() {
                return Err("Pending best-effort queue full");
            }
            self.next_be_seq = seq_advance(self.next_be_seq, total_frags as u8);
            for i in 0..total_frags {
                self.pending_be.push_back(frames[i]);
            }
        } else {
            if self.pending_reliable.len() + total_frags > self.pending_reliable.capacity() {
                return Err("Pending reliable queue full");
            }
            self.next_seq = seq_advance(self.next_seq, total_frags as u8);
            for i in 0..total_frags {
                self.pending_reliable.push_back(frames[i]);
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

    pub fn get_frames_to_transmit_into(
        &mut self,
        max_frames: usize,
        out_frames: &mut [CanonicalDataFrame; 8],
    ) -> usize {
        let mut count = 0;
        let limit = max_frames.min(8);

        // 1. Send best-effort frames first, they don't consume ARQ window
        while let Some(_) = self.pending_be.front() {
            if count >= limit {
                break;
            }
            out_frames[count] = self.pending_be.pop_front().unwrap();
            count += 1;
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
            if !inflight.acked && count < limit {
                out_frames[count] = inflight.frame;
                count += 1;
            }
        }

        count
    }

    pub fn get_frames_to_transmit(&mut self, max_frames: usize) -> Vec<CanonicalDataFrame> {
        let mut frames = [CanonicalDataFrame::new(); 8];
        let n = self.get_frames_to_transmit_into(max_frames, &mut frames);
        frames[..n].to_vec()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReassemblyContext {
    pub is_best_effort: bool,
    pub pkt_id: u8,
    pub total_frags: u8,
    pub received_mask: u8,
    pub fragments: [[u8; MAX_FRAG_PAYLOAD]; 8],
    pub frag_lens: [u8; 8],
    pub delivered: bool,
}

impl ReassemblyContext {
    pub fn new(is_best_effort: bool, pkt_id: u8, total_frags: u8) -> Self {
        Self {
            is_best_effort,
            pkt_id,
            total_frags,
            received_mask: 0,
            fragments: [[0u8; MAX_FRAG_PAYLOAD]; 8],
            frag_lens: [0u8; 8],
            delivered: false,
        }
    }
}

/// Structural validation before interpreting an IP frame or its feedback.
/// SOTP frames must be routed to their own protocol, not the IP reassembler.
pub fn valid_ip_frame(frame: &CanonicalDataFrame) -> bool {
    if frame.ctrl & 0x80 != 0 || frame.ctrl & 0x03 != 0x02
        || (frame.ctrl >> 4) & 0x07 > 4
        || frame.payload_len as usize > frame.payload.len()
    {
        return false;
    }
    if frame.payload_len == 0 {
        return true;
    }
    let header = FragmentHeader::decode(frame.payload[0]);
    header.frag_idx <= header.total_frags_minus_one
        && header.best_effort == (frame.ctrl & 0x04 != 0)
}

pub struct ArqReceiver {
    pub ack_base: u8,
    pub ack_map: u8,
    pub recv_bitmap: u16,
    pub current_seq_rx: u8,
    pub current_be_seq_rx: u8,
    pub contexts: [Option<ReassemblyContext>; MAX_REASSEMBLY_CONTEXTS],
    pub next_delivery_seq: u8,
    pub initialized: bool,
    pub be_initialized: bool,
    pub next_evict_idx: usize,
}

impl ArqReceiver {
    pub fn new() -> Self {
        Self::with_initial_seq(0)
    }

    /// Start a fresh receiver at an explicitly agreed reliable sequence.
    /// The engine uses sequence zero. Nonzero starts are for explicit session
    /// setup or test harnesses; never derive this value from an incoming frame.
    pub fn with_initial_seq(initial_seq: u8) -> Self {
        let base = initial_seq.wrapping_sub(1);
        Self {
            ack_base: base,
            ack_map: 0,
            recv_bitmap: 0,
            current_seq_rx: base,
            current_be_seq_rx: 0,
            contexts: [None; MAX_REASSEMBLY_CONTEXTS],
            next_delivery_seq: initial_seq,
            initialized: false,
            be_initialized: false,
            next_evict_idx: 0,
        }
    }

    /// Reset to the engine's default reliable sequence zero.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Ingest without releasing a completed packet. ACKs promise retention in
    /// the fixed context pool, not that the host has consumed the packet.
    pub fn ingest_frame(&mut self, frame: &CanonicalDataFrame) {
        if valid_ip_frame(frame) && frame.payload_len == 0 && frame.ctrl & 0x04 == 0 {
            // A reliable keepalive occupies one sequence but delivers no IP
            // bytes. Represent it as an empty one-fragment context so ordered
            // polling can skip it once preceding packets have been delivered.
            let mut keepalive = *frame;
            keepalive.payload_len = 1;
            keepalive.payload[0] = 0;
            self.ingest_frame_inner(&keepalive);
        } else {
            self.ingest_frame_inner(frame);
        }
    }

    fn ingest_frame_inner(&mut self, frame: &CanonicalDataFrame) -> Option<()> {
        if !valid_ip_frame(frame) {
            return None;
        }
        // Admission precedes ACK advancement: never acknowledge a fragment
        // that cannot be retained, or one that conflicts with an active packet.
        if frame.payload_len > 0 {
            let header = FragmentHeader::decode(frame.payload[0]);
            let pkt_id = frame.seq.wrapping_sub(header.frag_idx);
            if !header.best_effort {
                // An already-delivered packet cannot acquire new fragments,
                // even if its old deduplication context has been evicted.
                if seq_diff(pkt_id, self.next_delivery_seq) >= 128 {
                    return None;
                }
                let count = header.total_frags_minus_one + 1;
                if self.contexts.iter().flatten().any(|ctx|
                    !ctx.is_best_effort && !ctx.delivered && ctx.pkt_id != pkt_id
                    && (seq_diff(pkt_id, ctx.pkt_id) < ctx.total_frags
                        || seq_diff(ctx.pkt_id, pkt_id) < count)) {
                    return None;
                }
            }
            let existing = self.contexts.iter().flatten().find(|ctx|
                ctx.is_best_effort == header.best_effort && ctx.pkt_id == pkt_id);
            if let Some(ctx) = existing {
                let current = if header.best_effort { self.current_be_seq_rx } else { self.current_seq_rx };
                let stale = seq_after(current, ctx.pkt_id) && seq_diff(current, ctx.pkt_id) > 64;
                if !stale && ctx.total_frags != header.total_frags_minus_one + 1 {
                    return None;
                }
            } else if self.contexts.iter().all(|slot| match slot {
                Some(ctx) => !ctx.delivered && !ctx.is_best_effort,
                None => false,
            }) {
                return None;
            }
        }
        // Empty best-effort feedback has no fragment or sequence-space state.
        if frame.payload_len == 0 && frame.ctrl & 0x04 != 0 {
            return None;
        }
        let is_best_effort = (frame.ctrl & 0x04) != 0;
        let seq = frame.seq;

        if !is_best_effort {
            // This window is established locally, including before the first
            // received frame. Inferring it from that frame would cumulatively
            // acknowledge earlier, lost packets without ever receiving them.
            let diff = seq_diff(seq, self.ack_base);
            if diff == 0 || diff > 8 {
                return None;
            }
            self.initialized = true;

            // Deduplication Check 1 (Cumulative): If seq <= self.ack_base, frame was already acknowledged
            if seq_after_eq(self.ack_base, seq) {
                return None;
            }

            // Deduplication Check 2 (Selective): If seq is already recorded in recv_bitmap
            let diff = seq_diff(seq, self.ack_base);
            if diff > 0 && diff <= 16 {
                let bit_idx = (diff - 1) as usize;
                if (self.recv_bitmap & (1 << bit_idx)) != 0 {
                    return None;
                }
            }

            if seq_after(seq, self.current_seq_rx) {
                self.current_seq_rx = seq;
            }

            if diff == 1 {
                self.ack_base = seq;
                self.recv_bitmap >>= 1;
                while (self.recv_bitmap & 1) != 0 {
                    self.ack_base = seq_advance(self.ack_base, 1);
                    self.recv_bitmap >>= 1;
                }
            } else if diff <= 16 {
                let bit_idx = (diff - 1) as usize;
                self.recv_bitmap |= 1 << bit_idx;
            }
            self.ack_map = (self.recv_bitmap as u8) & 0x7F;
        } else {
            if !self.be_initialized {
                let expected_be_base = if frame.payload_len > 0 {
                    let header = FragmentHeader::decode(frame.payload[0]);
                    let pkt_id = seq.wrapping_sub(header.frag_idx);
                    pkt_id.wrapping_sub(1)
                } else {
                    seq.wrapping_sub(1)
                };
                self.current_be_seq_rx = expected_be_base;
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
        let current_max_seq = if is_best_effort { self.current_be_seq_rx } else { self.current_seq_rx };

        // Reject ancient incoming frames older than 64 sequences
        if seq_after(current_max_seq, pkt_id) && seq_diff(current_max_seq, pkt_id) > 64 {
            return None;
        }

        // Find existing context or empty slot, proactively invalidating stale slots on the fly
        let mut found_idx = None;
        let mut empty_idx = None;
        for (i, slot) in self.contexts.iter_mut().enumerate() {
            if let Some(ctx) = slot {
                // Invalidate stale contexts (> 64 sequences behind current_max_seq)
                if ctx.is_best_effort == is_best_effort
                    && (ctx.is_best_effort || ctx.delivered)
                    && seq_after(current_max_seq, ctx.pkt_id)
                    && seq_diff(current_max_seq, ctx.pkt_id) > 64 {
                    *slot = None;
                    if empty_idx.is_none() {
                        empty_idx = Some(i);
                    }
                    continue;
                }
                if ctx.is_best_effort == is_best_effort && ctx.pkt_id == pkt_id {
                    found_idx = Some(i);
                    break;
                }
            } else if empty_idx.is_none() {
                empty_idx = Some(i);
            }
        }

        let ctx_idx = if let Some(i) = found_idx {
            i
        } else if let Some(i) = empty_idx {
            let total_frags = header.total_frags_minus_one + 1;
            self.contexts[i] = Some(ReassemblyContext::new(is_best_effort, pkt_id, total_frags));
            i
        } else {
            // Evict a delivered context using round-robin, or an incomplete best-effort context
            let mut evict_idx = None;
            for offset in 0..MAX_REASSEMBLY_CONTEXTS {
                let i = (self.next_evict_idx + offset) % MAX_REASSEMBLY_CONTEXTS;
                if let Some(ctx) = &self.contexts[i] {
                    if ctx.delivered {
                        evict_idx = Some(i);
                        break;
                    }
                }
            }
            let chosen_idx = if let Some(i) = evict_idx {
                i
            } else {
                // Best-effort traffic may lose incomplete datagrams under
                // pressure. Never evict acknowledged reliable fragments, and
                // compare ages only within the best-effort sequence space.
                self.contexts.iter().enumerate()
                    .filter_map(|(i, slot)| slot.as_ref()
                        .filter(|ctx| ctx.is_best_effort)
                        .map(|ctx| (i, seq_diff(self.current_be_seq_rx, ctx.pkt_id))))
                    .max_by_key(|&(_, age)| age)
                    .map(|(i, _)| i)?
            };
            self.next_evict_idx = (chosen_idx + 1) % MAX_REASSEMBLY_CONTEXTS;
            let total_frags = header.total_frags_minus_one + 1;
            self.contexts[chosen_idx] = Some(ReassemblyContext::new(is_best_effort, pkt_id, total_frags));
            chosen_idx
        };

        let ctx = self.contexts[ctx_idx].as_mut().unwrap();

        // Deduplication Check 3 (Packet-level): Do not process frames for already delivered packets
        if ctx.delivered {
            return None;
        }

        let frag_idx = header.frag_idx as usize;
        let total_frags = ctx.total_frags as usize;
        if frag_idx < total_frags && frag_idx < 8 {
            if (ctx.received_mask & (1 << frag_idx)) == 0 {
                let chunk_len = data.len().min(MAX_FRAG_PAYLOAD);
                ctx.fragments[frag_idx][..chunk_len].copy_from_slice(&data[..chunk_len]);
                ctx.frag_lens[frag_idx] = chunk_len as u8;
                ctx.received_mask |= 1 << frag_idx;
            }
        }

        Some(())
    }

    /// Consume one eligible packet into caller-owned storage. Reliable packets
    /// follow their original sequence order; complete best-effort datagrams may
    /// bypass a reliable gap. Returns (byte length, best-effort class).
    /// Call only when the downstream queue has capacity for a whole packet.
    pub fn poll_packet_into(&mut self, out: &mut [u8; MAX_IP_PACKET_LEN]) -> Option<(usize, bool)> {
        // At most 16 contexts can be visited, including zero-byte keepalives.
        for _ in 0..MAX_REASSEMBLY_CONTEXTS {
            let complete = |ctx: &ReassemblyContext| {
                let mask = ((1u16 << ctx.total_frags) - 1) as u8;
                !ctx.delivered && ctx.received_mask == mask
            };
            let reliable = self.contexts.iter().position(|slot| slot.as_ref().map_or(false, |ctx|
                !ctx.is_best_effort && ctx.pkt_id == self.next_delivery_seq && complete(ctx)));
            let idx = reliable.or_else(|| self.contexts.iter().position(|slot|
                slot.as_ref().map_or(false, |ctx| ctx.is_best_effort && complete(ctx))))?;
            let ctx = self.contexts[idx].as_mut().unwrap();
            let mut len = 0;
            for i in 0..ctx.total_frags as usize {
                let n = ctx.frag_lens[i] as usize;
                out[len..len + n].copy_from_slice(&ctx.fragments[i][..n]);
                len += n;
            }
            ctx.delivered = true;
            if !ctx.is_best_effort {
                self.next_delivery_seq = seq_advance(ctx.pkt_id, ctx.total_frags);
            }
            if len > 0 {
                return Some((len, ctx.is_best_effort));
            }
        }
        None
    }

    /// Convenience API: ingest a frame and consume at most one eligible packet.
    /// A repaired gap can release several packets; drain poll_packet_into too.
    pub fn receive_frame_into(
        &mut self,
        frame: &CanonicalDataFrame,
        out_packet: &mut [u8; MAX_IP_PACKET_LEN],
    ) -> Option<usize> {
        self.ingest_frame(frame);
        self.poll_packet_into(out_packet).map(|(len, _)| len)
    }

    pub fn receive_frame(&mut self, frame: &CanonicalDataFrame) -> Option<Vec<u8>> {
        let mut buf = [0u8; MAX_IP_PACKET_LEN];
        self.receive_frame_into(frame, &mut buf).map(|len| buf[..len].to_vec())
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
        f0.ctrl = 0x02; // v3.8 IP frame
        f0.seq = 0;
        f0.payload_len = 1;
        f0.payload[0] = 0; // frag_idx 0, total 0
        rx.receive_frame(&f0);
        assert_eq!(rx.ack_base, 0);

        // Now we receive seq = 8 (diff = 8).
        let mut f8 = CanonicalDataFrame::new();
        f8.ctrl = 0x02; // v3.8 IP frame
        f8.seq = 8;
        f8.payload_len = 1;
        f8.payload[0] = 0;
        rx.receive_frame(&f8);
        
        // This should not set bit 7 of ack_map!
        assert_eq!(rx.ack_map & 0x80, 0, "Bit 7 (Feedback Type) was overwritten!");
    }
}


