# Bounded handshake PCM bridge

`handshake_bridge::HandshakeBridge` supplies the worker/audio boundary for
[HandshakeCoordinator](HANDSHAKE_COORDINATOR.md). It has two SPSC queues of 32
blocks, each holding at most 160 mono 8 kHz samples. Audio-side operations copy
samples, update atomics, and operate bounded queues. They allocate/free nothing,
perform no bootstrap DSP or entropy calls, and never wait for the worker.

This is a Rust library adapter. It does not open an audio device or provide a
mobile/PBX service or C API. Device buffer completion must come from the platform.

## Ownership and routing

Construct the bridge and an authenticated engine before starting callbacks.
Split each into its unique host/audio handles. Give `BridgeHost` and a coordinator
to `HandshakeWorker`; keep the engine's `HostHandle` on that worker. Keep
`BridgeAudio` and the same engine's `AudioHandle` on the audio owner. Serialize
capture, playback, and drain acknowledgments on that owner. A device completion
from another thread must be forwarded to it; do not share callback handles.

The worker calls `begin` with host entropy when initiating, then repeatedly calls
`pump(now_ms, entropy)` to decode capture, schedule retries, and fill playback.
The responder can start by pumping without `begin`. Use a trusted monotonic clock
and correctly paced PCM. Each pump consumes a bounded capture batch and renders
at most a queue's worth plus one retained block. On full playback, it retains the
already-rendered block and does not advance the coordinator again until it fits.

Callbacks call `BridgeAudio::process_audio(engine_audio, input)` and
`generate_audio(engine_audio, output)`. Before installation these route through
the handshake queues. They also service bounded engine command batches without
emitting engine PCM, so a full command queue cannot deadlock session installation.
Command servicing waits if an engine burst is active; use a fresh/quiescent
engine for initial bootstrap and finish or stop old device/engine audio explicitly
before restarting a handshake. The bridge does not truncate an engine burst.

After installation, both wrappers route directly to that authenticated engine.
The worker and audio wrappers must always use handles from the same engine.
`install_when_drained` publishes routing only after successfully enqueueing the
session. The next audio call applies that command at the engine's burst boundary.
Queue application packets after successful installation and observe the engine's
`authenticated_ready` for audio-owner application of the context.

## Playback fence and device completion

When the coordinator reaches `Ready`, the worker queues all remaining handshake
PCM, then a fence. Playback is sealed after that fence. The audio callback may
consume only part of a block; it retains the remainder across callbacks. When it
reaches the fence, output after that point is silence and
`PlaybackProgress::awaiting_device_drain` is true. A fence following an exactly
full output buffer is discovered on the next callback.

Call `playback_fence()` at that point and retain the returned `PlaybackFence`
with the platform drain request. Only after the platform confirms that all PCM
preceding the fence has actually played, call `acknowledge_played(token)` on the
audio owner. Do not fetch a new token when an old asynchronous completion arrives.
Tokens identify both the bridge and its generation; late completions cannot
acknowledge a later handshake or another bridge. Merely copying samples into a
callback buffer is not evidence of playback completion.

The worker calls `install_when_drained(engine_host, now_ms)`. Before acknowledgment
it returns `Ok(false)`. After acknowledgment it transfers the existing control
owners once and queues engine installation. If the engine queue is full, it
returns `WorkerError::Engine` and retains the transfer for retry. Call it again;
no counters or replay state are reconstructed. Successful admission returns
`Ok(true)` and enables routing. Repeated successful calls are idempotent until
reset. A failed admission never enables engine routing.

## Overflow, gaps and reset

Capture splits arbitrary callback slices into blocks. Full queues drop whole
blocks and return the dropped sample count. Stream offsets include dropped
samples. Before feeding the next accepted block to the coordinator, the worker
notices any gap and clears partial bootstrap/PLCP acquisition without resetting
session keys, replay state, deadlines, or failure credits. Device discontinuities
can also be signaled with `capture_discontinuity()`.

Playback underflow fills output with silence. `PlaybackProgress::samples` reports
actual copied samples, so an adapter can detect underflow during an expected
burst. It does not automatically repair a waveform interrupted by worker
starvation; handshake retry/restart remains necessary. The queues bound memory,
not scheduling latency or success under starvation. Each queue holds up to
640 ms of audio, with an additional partial callback block and retained worker
block on playback. These limits have not been qualified against device latency
or acoustic/TDD timing.

`HandshakeWorker::reset` resets the coordinator and advances the bridge generation,
invalidating queued capture/playback, partially rendered PCM, pending worker PCM,
and old fence acknowledgments. Only each queue's consumer removes its old items;
the worker does not access audio-owned cursor state. A callback already in progress
can finish its old buffer; subsequent calls observe the new generation. Physical
device queues must be cleared separately, and an installed engine must also be
reset/closed before rekey. The bridge cannot retract samples already sent to a
device. On a handshake timeout, pumping invalidates queued bridge PCM once and
leaves the coordinator in `TimedOut` for explicit host recovery.

## Validation and remaining work

Tests cover partial blocks, saturation without losing playback ordering, marked
capture gaps, concurrent endpoints, worker starvation, generation resets,
late/wrong-bridge drain tokens, timeout cleanup, engine queue-full retry, and the
complete bridged handshake-to-bidirectional-data path in MCS 2/3. Allocation
instrumentation verifies zero heap allocations and frees in bridge operations.

Still missing: actual platform device adapters and truthful drain notifications,
end-to-end device latency qualification, mutual readiness acknowledgment,
automatic outage/rekey coordination, CCF transactions, negotiated MCS changes,
and C ABI exposure. The coordinator's lost-confirmation and best-effort limitations
still apply when all bounded confirmation probes are lost or handoff is too late.
Coordinator-created initiator transfers now arm three idle probes in the engine;
no bridge callback DSP or allocation is added. M1 is not complete.
