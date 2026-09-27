# Project: V-RADM Platform

## Current handoff — 2026-09-26

Read [docs/IMPLEMENTATION_STATUS.md](docs/IMPLEMENTATION_STATUS.md) for the verified
implementation inventory, this continuation's repairs, outstanding core gaps,
and the phone/phone + PBX/PBX direction. The architecture and file tree below
are the original **target design**, not a list of implemented components.
See [SPEC_CONFORMANCE.md](docs/SPEC_CONFORMANCE.md) for the focused audit of
known mismatches, explicit project profiles and incomplete requirements.
Only `vradm-core` currently exists. M1 remains in progress; M2–M6 are unstarted.
See [M1_PLAN.md](docs/M1_PLAN.md) for concrete completion work and the next
integrated checkpoint. [Endpoint](docs/ENDPOINT.md) now binds bootstrap,
authenticated packet I/O and reset/rekey behind one host/audio API.
[Live CCF feedback](docs/LIVE_CCF.md) now connects streamed compact acknowledgments
to that endpoint's ARQ in a full-duplex software profile.
Live burst RMS configuration and boundary-applied amplitude
commands now work; see [TX_AMPLITUDE.md](docs/TX_AMPLITUDE.md).
[Live MCS2→3 negotiation](docs/LIVE_MCS.md) now drains existing ARQ work,
recovers lost/forged commit replies and applies the reliable sequence boundary
through the same public Endpoint. Explicit emergency downshift preserves in-flight
data and cancels pending upshifts. General adaptation and acoustic scheduling
remain open; host-supplied metrics are explicit and not measured automatically.

The root Cargo workspace now supports `cargo test --workspace`. Sustained PCM
transfer has standalone feedback and bounded retry recovery; telemetry copies
use atomic fields. Receive-path follow-up adds fragment validation, protects
reliable reassembly from best-effort eviction, preserves packets across host
queue saturation and short poll buffers, and removes a FEC-ranking allocation.
Initial reliable sequence handling now preserves lost first packets; reset returns to sequence 0.
Reliable packets now wait for earlier missing packets in bounded reassembly storage;
both audio callbacks release held packets when host queue space returns.
Queued reset now uses local queue generations, preserves subsequent commands/data,
and keeps host-owned queue consumption and SOTP cleanup off the audio thread.
Safe Rust now has exclusive producer/consumer queue endpoints and host/audio
engine handles; shared raw engine operations require explicit unsafe contracts.
Standalone session-security primitives now cover bootstrap verification, key
derivation, control tags, replay windows, and bounded counters. A host-side
session manager now handles OS entropy,
retries, provisional confirmation, simultaneous initiation and nonce replay
admission, and a reset-persistent bootstrap MAC-failure budget. The aligned 100-baud bootstrap PCM payload codec now passes request/
accept loopback. An explicit opt-in Barker bootstrap acquisition profile now
finds these frames in streaming PCM. The opt-in Rust engine now receives
established sessions through a single-use counter-preserving handoff and verifies
PLCP before payload/ARQ. A host-owned PCM handshake coordinator now schedules
request/accept retries and authenticated provisional confirmation, then hands
off the existing counters. A bounded PCM bridge now separates worker/audio
ownership and gates engine routing on generation-bound device-drain tokens.
Coordinator-created initiators now carry three bounded fresh-counter idle
confirmation retries into the engine, canceled by a verified peer beacon.
Authenticated CCF transaction guards now enforce one outstanding request, full
counter binding, deadlines and one-time control effects with duplicate ACK updates.
An MCS negotiation policy now adds three-attempt upshifts, ten-second cooldown,
verified deferred commit plans and trusted emergency downshift. Receiver-side
commit reply admission now authenticates the request, checks the local metric
and signs against its full counter without changing the receive PHY.
Aligned CCF pitch PCM now supports fixed-memory modulation, NCCF decoding and
RS byte-erasure recovery through the authenticated control guard; see
[CCF_PCM_PROFILE.md](docs/CCF_PCM_PROFILE.md). A bounded CCF/EOT/guard renderer now emits sample-exact
control turns with busy rejection and explicit cancellation. An aligned turn
receiver now checks both EOT tones and waits through the guard before returning
an unverified CCF for MAC admission. The endpoint now acquires compact ACKs
from streaming PCM and dispatches verified responses to live ARQ. Acoustic TDD
scheduling, platform audio/drain adapters and the C ABI path remain unfinished; the legacy C constructor still sends unauthenticated PCM.
These are software prototype improvements, not field or codec qualification.
The tests are now visible to Git.

## Architecture
V-RADM is a voice-channel acoustic data modem platform wrapping `vradm-core`, enabling secure IP data communications (SSH over port 22, Mosh over UDP 60000..60010) through cellular voice channels (VoLTE / AMR-WB) and PBX gateways.

```
Third-Party App (OpenSSH, Blink, Termius)
  │ TCP port 22 / Mosh UDP
  ▼
iOS NetworkExtension (PacketTunnelProvider)
  [Split-Tunnel 10.99.0.0/24, MTU 256, Client TCP-PEP]
  │ Lock-Free SPSC Ring Buffer (mmap, group.org.vradm)
  ▼
Main Mobile App (Flutter UI & libvradm_core FFI)
  [vradm_engine_t: IP Slicing, ARQ, RS FEC, PHY Modulator]
  │ Linear PCM Audio (8 kHz 16-bit signed mono)
  ▼
Cellular Link (VoLTE / AMR-WB) / Audio Interface
  │ AudioSocket TCP:9099
  ▼
Asterisk 20+ PBX Core
  │ AudioSocket Protocol (3-byte header, 16-byte UUID)
  ▼
vradmd Gateway Daemon (Rust)
  [Multi-Tenant Session Pool, vradm_engine_t per session]
  │ Reassembled IP Datagrams
  ▼
Server-Side TCP-PEP
  [Hysteresis Flow Control, MSS Clamping, Zero-Window Deadlock Mitigation]
  │ Local TCP Loopback (127.0.0.1:22)
  ▼
Local Service (sshd / echo service)
```

## Feature Inventory
| # | Feature | Description | Milestone | Source |
|---|---------|-------------|-----------|--------|
| 1 | Core C-ABI Engine Lifecycle | `vradm_create`, `vradm_destroy`, `vradm_reset` heap management and opaque handle | M1 | SPEC §10 |
| 2 | C-ABI Real-Time Audio I/O | `vradm_process_audio` and `vradm_generate_audio` non-blocking PCM sample processing | M1 | SPEC §10 |
| 3 | C-ABI IP Datagram I/O | `vradm_write_ip_packet` and `vradm_poll_ip_packet` L3 interface | M1 | SPEC §10 |
| 4 | C-ABI Commands & SOTP Transfer | `vradm_submit_cmd` and `vradm_sotp_*` command dispatch and object transfer | M1 | SPEC §10 |
| 5 | C-ABI Telemetry & Active MCS | `vradm_get_telemetry` seqlock snapshot and `vradm_get_active_mcs` | M1 | SPEC §10 |
| 6 | Physical Layer Modulation/Demodulation | Barker-13/2-FSK PLCP beacon, DQPSK/OFDM acoustic PHY, dither, soft limiter | M1 | SPEC §3, §4 |
| 7 | Shared Memory SPSC Header & Padding | Magic header `0x56524144`, 64-byte hardware cache-line aligned atomic indices | M2 | SPEC §9.3 |
| 8 | Lock-Free SPSC Acquire/Release | Atomic index progression, non-blocking queueing, drop on saturation | M2 | SPEC §9.3 |
| 9 | POSIX `mmap` Backing File IPC | File-backed shared memory region supporting macOS, Linux, and iOS App Group | M2 | SPEC §9.3 |
| 10 | Mock SPSC Ring Buffer Test Harness | Automated test verifying lock-free enqueue/dequeue, concurrency, crash recovery | M2 | ORIGINAL_REQUEST |
| 11 | Flutter Dart FFI C-ABI Wrapper | `dart:ffi` bindings, C-ABI struct layouts/alignments, engine lifecycle | M3 | SPEC §10, ORIGINAL_REQUEST |
| 12 | iOS NetworkExtension Tunnel Profile | `PacketTunnelProvider` split-tunnel 10.99.0.2/24, remote 10.99.0.1, MTU 256 | M3 | SPEC §7.3 |
| 13 | Client-Side TCP-PEP Interception | Local SYN+ACK spoofing, option stripping (`WSCALE`, `TSopt`, `SACK`), MSS 216 B | M3 | SPEC §7.1 |
| 14 | Client-Side Window Clamping & In-Band Teardown | MCS-adaptive window clamping table, `STREAM_CTRL_FIN`/`RST` with `URGENT_FLUSH` | M3 | SPEC §7.1.1 |
| 15 | Client Mosh UDP Passthrough | Ports 60000..60010 mapped to `BEST_EFFORT = 1` bypassing ARQ | M3 | SPEC §7.1 |
| 16 | Asterisk AudioSocket TCP Listener | Listener on `0.0.0.0:9099`, 3-byte header framing `[type, len_hi, len_lo]` | M4 | SPEC §9.2 |
| 17 | AudioSocket UUID Handshake & Routing | Parsing 16-byte RFC 4122 binary UUID and multi-session routing | M4 | SPEC §9.2 |
| 18 | Multi-Tenant Session Management | Thread-safe session pool, dedicated per-session `vradm_engine_t`, graceful cleanup | M4 | SPEC §9.1 |
| 19 | Server-Side TCP-PEP Local Termination | Split-connection TCP proxy terminating locally at `127.0.0.1:22` | M4 | SPEC §7.1 |
| 20 | Server Flow Control & Deadlock Mitigation | Hysteresis backpressure ($2 \times W_{clamp}$ to $1 \times W_{clamp}$) & unsolicited pure ACKs | M4 | SPEC §7.1.1 |
| 21 | Server TCP Load Test Verification | Simulated burst load test proving zero unintended zero-window deadlocks | M4 | ORIGINAL_REQUEST |
| 22 | Programmatic Mock Asterisk AudioSocket Client | TCP client simulating Asterisk streaming, UUID handshake, PCM chunks, control | M5 | ORIGINAL_REQUEST |
| 23 | Mock NetworkExtension Packet Harness | Programmatic IP packet injector/collector over SPSC shared memory | M5 | ORIGINAL_REQUEST |
| 24 | Closed-Loop End-to-End Pipeline | Full automated integration test: NET -> SPSC -> Core -> AudioSocket -> Daemon -> Echo -> Return | M5 | ORIGINAL_REQUEST |
| 25 | Final E2E Test Pass & Adversarial Hardening | 100% pass of E2E test suite (Tiers 1-4) and Tier 5 adversarial verification | M6 | Project Pattern |

## Milestones
| # | Name | Scope | Dependencies | Status |
|---|------|-------|-------------|--------|
| 1 | M1: `vradm-core` Engine & PHY Layer | Implement `vradm_engine_t`, 13 C-ABI functions, PHY acoustic pipeline, `cdylib`/`staticlib` | none | IN PROGRESS |
| 2 | M2: Lock-Free SPSC Shared Memory & IPC Harness | Implement `vradm-shm` crate, 64-byte aligned lock-free SPSC IPC, POSIX `mmap`, and IPC test harness | none | PLANNED |
| 3 | M3: Flutter Mobile Client & iOS TCP-PEP | Implement Flutter FFI bindings, boundary tests, and `PacketTunnelProvider` TCP-PEP | M1, M2 | PLANNED |
| 4 | M4: Asterisk Gateway Daemon (`vradmd`) & Server TCP-PEP | Implement `vradmd` daemon, AudioSocket parser, multi-tenant pool, server TCP-PEP, load test | M1 | PLANNED |
| 5 | M5: Automated Mock Integration Harnesses & E2E Pipeline | Implement mock Asterisk client, mock NetworkExtension harness, and closed-loop E2E test | M1, M2, M3, M4 | PLANNED |
| 6 | M6: Final E2E Test Suite & Adversarial Hardening | Verify 100% passing E2E tests (Tiers 1-4) and execute Tier 5 adversarial coverage hardening | M5 | PLANNED |

## Interface Contracts

### 1. `vradm-core` C-ABI ↔ Host / Flutter / `vradmd`
- `vradm_create(const vradm_config_t* config) -> *mut vradm_engine_t`
- `vradm_destroy(vradm_engine_t* engine) -> void`
- `vradm_reset(vradm_engine_t* engine) -> void`
- `vradm_submit_cmd(vradm_engine_t* engine, const vradm_cmd_t* cmd) -> i32`
- `vradm_process_audio(vradm_engine_t* engine, const int16_t* in_samples, uint32_t count) -> void`
- `vradm_generate_audio(vradm_engine_t* engine, int16_t* out_samples, uint32_t max_count) -> uint32_t`
- `vradm_write_ip_packet(vradm_engine_t* engine, const uint8_t* packet, uint32_t len) -> i32`
- `vradm_poll_ip_packet(vradm_engine_t* engine, uint8_t* out_packet, uint32_t max_len) -> i32`
- `vradm_get_telemetry(const vradm_engine_t* engine, vradm_telemetry_t* out_telem) -> void`
- `vradm_get_active_mcs(const vradm_engine_t* engine) -> uint8_t`

### 2. Lock-Free SPSC Shared Memory IPC ↔ NetworkExtension & Main App
- Magic: `0x56524144` ("VRAD")
- Header: Version 1, 64-byte header, offsets for `tx_ring` and `rx_ring`
- Ring Control: `head: atomic_uint32_t` (alignas 64), `tail: atomic_uint32_t` (alignas 64)
- Capacity: 256 slots
- Slot Format: `len: uint32_t` (4 B) + `payload: uint8_t[256]` (256 B) = 260 B per slot
- Memory Ordering: Enqueue uses `Release` on `head`; Dequeue uses `Acquire` on `head`, `Release` on `tail`

### 3. Asterisk PBX ↔ `vradmd` Daemon (AudioSocket)
- Wire Header: 3 bytes: `[type: u8, len_hi: u8, len_lo: u8]`
- Message Types:
  - `0x01` (UUID Handshake): 16-byte RFC 4122 binary UUID (sent first upon connection)
  - `0x10` (PCM Audio): Signed linear 16-bit mono 8 kHz PCM (typically 320 bytes = 20 ms = 160 samples)
  - `0x00` / `0x01` (Hangup): Channel close
  - `0x03` (DTMF): In-band tone character
  - `0xFF` / `0x02` (Error): Bitmask or error code

### 4. TCP-PEP ↔ Local TCP Socket (`127.0.0.1:22`)
- SYN Handshake: Immediate synthetic SYN+ACK with zero synthetic RTT, ISN generated via CSPRNG
- Options: `WSCALE`, `TSopt`, `SACK-Permitted` stripped
- Clamping: MSS clamped to 216 bytes; Window clamped to $W_{\text{clamp}}$ (74..1036 B depending on MCS)
- Hysteresis: Stop read & win=0 when queue > $2 \times W_{\text{clamp}}$; resume read & send unsolicited pure ACK with win=$W_{\text{clamp}}$ when queue < $1 \times W_{\text{clamp}}$

## Code Layout
```
/Users/kevint/dev/vradm/
├── Cargo.toml                      # Root Cargo workspace
├── vradm-core/                     # Core Rust crate
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── c_abi.rs                # C-ABI structs and extern "C" functions
│       ├── engine.rs               # vradm_engine_t implementation & state
│       ├── phy.rs                  # Physical layer modulation/demodulation & beacon
│       ├── arq.rs                  # ARQ slicer and reassembler
│       ├── framing.rs              # Frame encode/decode and interleaver
│       ├── fec.rs                  # Golay and Reed-Solomon codecs
│       └── crc.rs                  # CRC-8 and CRC-16
├── vradm-shm/                      # Lock-Free SPSC Shared Memory crate
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs                  # Public SPSC ring buffer API
│       ├── shm.rs                  # POSIX mmap implementation
│       ├── ring.rs                 # 64-byte aligned lock-free SPSC logic
│       └── harness.rs              # Mock IPC test runner
├── vradmd/                         # Asterisk Gateway Daemon crate
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs                 # CLI entry point and TCP listener
│       ├── audiosocket.rs          # AudioSocket protocol parser & framer
│       ├── session.rs              # Multi-tenant session pool & lifecycle
│       └── pep.rs                  # Server-side TCP-PEP with hysteresis flow control
├── mobile/                         # Flutter Mobile Client & iOS NetworkExtension
│   ├── pubspec.yaml                # Flutter project manifest
│   ├── lib/
│   │   ├── main.dart               # Flutter UI & state
│   │   └── vradm_ffi.dart          # Dart FFI bindings for vradm-core
│   ├── test/
│   │   └── ffi_test.dart           # FFI boundary & struct alignment tests
│   └── ios/
│       └── PacketTunnelProvider.swift # iOS NetworkExtension TCP-PEP
└── tests/                          # Automated Integration Test Harnesses
    ├── Cargo.toml
    └── src/
        ├── mock_audiosocket.rs     # Programmatic Asterisk AudioSocket client
        ├── mock_network_extension.rs # Mock NetworkExtension packet injector
        ├── e2e_pipeline.rs         # Closed-loop end-to-end integration test
        └── pep_load_test.rs        # TCP-PEP zero-window deadlock load test
```
