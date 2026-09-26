//! Host/media-scheduler MCS upshift policy for SPEC §4.3. This module does not
//! modulate CCF PCM or apply a rate to payload samples. It holds exclusive control
//! owners while negotiating, so unrelated requests cannot replace its counter.

use crate::security::{
    Beacon, ControlCommand, ControlRequest, ControlResponse, ControlRx, ControlTx, SecurityError,
};

pub const MCS_UPSHIFT_ATTEMPTS: u8 = 3;
pub const MCS_COOLDOWN_MS: u64 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McsControlError {
    InvalidMcs,
    InvalidTiming,
    Busy,
    CoolingDown,
    NoActiveRequest,
    NoCommit,
    Security(SecurityError),
}
impl From<SecurityError> for McsControlError {
    fn from(error: SecurityError) -> Self {
        Self::Security(error)
    }
}

/// Authenticated plan. The caller must apply this at the indicated reliable
/// sequence boundary, not immediately on receiving the CCF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McsCommit {
    pub previous_mcs: u8,
    pub target_mcs: u8,
    pub first_sequence: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McsControlEvent {
    None,
    Transmit(Beacon),
    Aborted,
}

#[derive(Clone, Copy)]
struct Attempt {
    target: u8,
    tx_power: u8,
    yield_turn: bool,
    count: u8,
    rto_ms: u64,
    deadline_ms: u64,
}
#[derive(Clone, Copy)]
enum State {
    Idle,
    Awaiting(Attempt),
    Accepted(McsCommit),
}

/// No allocations, locks, or waveform work. Keep these Tx/Rx owners paired with
/// the same endpoint/session. Dropping this policy object does not reset either
/// owner or clear an outstanding request; cancel or expire it explicitly.
pub struct McsNegotiator<'a> {
    tx: &'a mut ControlTx,
    rx: &'a mut ControlRx,
    current: u8,
    state: State,
    cooldown_started_ms: Option<u64>,
    last_time_ms: u64,
}
impl<'a> McsNegotiator<'a> {
    pub fn new(
        tx: &'a mut ControlTx,
        rx: &'a mut ControlRx,
        current_mcs: u8,
        now_ms: u64,
    ) -> Result<Self, McsControlError> {
        if current_mcs > 4 {
            return Err(McsControlError::InvalidMcs);
        }
        tx.expire_control(now_ms)?;
        if tx.control_outstanding() {
            return Err(McsControlError::Busy);
        }
        Ok(Self {
            tx,
            rx,
            current: current_mcs,
            state: State::Idle,
            cooldown_started_ms: None,
            last_time_ms: now_ms,
        })
    }

    fn advance(&mut self, now_ms: u64) -> Result<(), McsControlError> {
        if now_ms < self.last_time_ms {
            return Err(SecurityError::ClockWentBackwards.into());
        }
        self.last_time_ms = now_ms;
        Ok(())
    }
    pub fn current_mcs(&self) -> u8 {
        self.current
    }
    pub fn requested_mcs(&self) -> u8 {
        match self.state {
            State::Idle => self.current,
            State::Awaiting(a) => a.target,
            State::Accepted(plan) => plan.target_mcs,
        }
    }
    pub fn pending_commit(&self) -> Option<McsCommit> {
        if let State::Accepted(plan) = self.state {
            Some(plan)
        } else {
            None
        }
    }
    pub fn cooldown_remaining_ms(&self, now_ms: u64) -> u64 {
        self.cooldown_started_ms.map_or(0, |start| {
            MCS_COOLDOWN_MS.saturating_sub(now_ms.saturating_sub(start))
        })
    }

    /// Ordinary data beacons remain available during cooldown, but not while
    /// negotiation or a sequence-boundary commit is outstanding.
    pub fn data_beacon(&mut self, tx_power: u8, now_ms: u64) -> Result<Beacon, McsControlError> {
        self.advance(now_ms)?;
        if !matches!(self.state, State::Idle) {
            return Err(McsControlError::Busy);
        }
        self.tx.expire_control(now_ms)?;
        Ok(self.tx.beacon(self.current, self.current, tx_power)?)
    }

    pub fn verify_peer_beacon(
        &mut self,
        beacon: Beacon,
        now_ms: u64,
    ) -> Result<u16, McsControlError> {
        self.advance(now_ms)?;
        Ok(self.rx.verify_beacon(beacon, now_ms)?)
    }

    /// RTO includes actual request/response airtime and media-queue delay. It is
    /// supplied by the caller; zero/overflowing intervals consume no beacon.
    pub fn request_upshift(
        &mut self,
        target: u8,
        tx_power: u8,
        yield_turn: bool,
        rto_ms: u64,
        now_ms: u64,
    ) -> Result<Beacon, McsControlError> {
        self.advance(now_ms)?;
        if !matches!(self.state, State::Idle) {
            return Err(McsControlError::Busy);
        }
        if target <= self.current || target > 4 {
            return Err(McsControlError::InvalidMcs);
        }
        if self.cooldown_remaining_ms(now_ms) != 0 {
            return Err(McsControlError::CoolingDown);
        }
        let deadline = Self::deadline(now_ms, rto_ms)?;
        let beacon = self.tx.begin_control(
            ControlRequest {
                current_mcs: self.current,
                target_mcs: target,
                tx_power,
                command: ControlCommand::McsCommitAck,
                yield_turn,
                deadline_ms: deadline,
            },
            now_ms,
        )?;
        self.state = State::Awaiting(Attempt {
            target,
            tx_power,
            yield_turn,
            count: 1,
            rto_ms,
            deadline_ms: deadline,
        });
        Ok(beacon)
    }
    fn deadline(now_ms: u64, rto_ms: u64) -> Result<u64, McsControlError> {
        if rto_ms == 0 {
            return Err(McsControlError::InvalidTiming);
        }
        now_ms
            .checked_add(rto_ms)
            .ok_or(McsControlError::InvalidTiming)
    }

    /// At most one fresh retry per poll, never a catch-up burst after a delayed
    /// worker. Three total attempts (initial plus two retries), then ten seconds
    /// of cooldown measured from the local abort decision.
    pub fn poll(&mut self, now_ms: u64) -> Result<McsControlEvent, McsControlError> {
        self.advance(now_ms)?;
        self.tx.expire_control(now_ms)?;
        let State::Awaiting(mut attempt) = self.state else {
            return Ok(McsControlEvent::None);
        };
        if now_ms < attempt.deadline_ms {
            return Ok(McsControlEvent::None);
        }
        if attempt.count >= MCS_UPSHIFT_ATTEMPTS {
            self.state = State::Idle;
            self.cooldown_started_ms = Some(now_ms);
            return Ok(McsControlEvent::Aborted);
        }
        let deadline = Self::deadline(now_ms, attempt.rto_ms)?;
        let beacon = self.tx.begin_control(
            ControlRequest {
                current_mcs: self.current,
                target_mcs: attempt.target,
                tx_power: attempt.tx_power,
                command: ControlCommand::McsCommitAck,
                yield_turn: attempt.yield_turn,
                deadline_ms: deadline,
            },
            now_ms,
        )?;
        attempt.count += 1;
        attempt.deadline_ms = deadline;
        self.state = State::Awaiting(attempt);
        Ok(McsControlEvent::Transmit(beacon))
    }

    /// Verifies raw CCF wire input itself; callers cannot supply an unverified
    /// ControlResponse to release the guard or construct an accepted plan.
    pub fn receive(
        &mut self,
        wire: [u8; 16],
        erasures: &[usize],
        now_ms: u64,
    ) -> Result<ControlResponse, McsControlError> {
        self.advance(now_ms)?;
        if matches!(self.state, State::Idle) {
            return Err(McsControlError::NoActiveRequest);
        }
        let response = self
            .rx
            .verify_control_response(self.tx, wire, erasures, now_ms)?;
        if let State::Awaiting(attempt) = self.state {
            let first_sequence = response
                .commit_sequence
                .expect("only matching MCS commit responses are admitted");
            self.state = State::Accepted(McsCommit {
                previous_mcs: self.current,
                target_mcs: attempt.target,
                first_sequence,
            });
        }
        Ok(response)
    }

    /// Confirm that the media scheduler applied the saved sequence-boundary plan.
    /// This is a trusted local action, not a decoded peer command. Until this
    /// call, current_mcs remains unchanged and another upshift is disallowed.
    pub fn complete_commit(&mut self, now_ms: u64) -> Result<McsCommit, McsControlError> {
        self.advance(now_ms)?;
        let State::Accepted(plan) = self.state else {
            return Err(McsControlError::NoCommit);
        };
        self.tx.reject_control(now_ms)?;
        self.current = plan.target_mcs;
        self.state = State::Idle;
        Ok(plan)
    }

    /// Trusted cancellation keeps the current rate and any existing cooldown.
    pub fn cancel(&mut self, now_ms: u64) -> Result<(), McsControlError> {
        self.advance(now_ms)?;
        self.tx.reject_control(now_ms)?;
        self.state = State::Idle;
        Ok(())
    }

    /// Caller establishes trusted local channel metric M < 0.60. No peer ACK is
    /// required. This cancels an upshift/commit without bypassing prior cooldown.
    pub fn emergency_downshift(&mut self, lower: u8, now_ms: u64) -> Result<(), McsControlError> {
        self.advance(now_ms)?;
        if lower >= self.current {
            return Err(McsControlError::InvalidMcs);
        }
        self.cancel(now_ms)?;
        self.current = lower;
        Ok(())
    }
}
