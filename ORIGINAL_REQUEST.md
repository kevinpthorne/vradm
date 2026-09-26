# Original User Request

## Initial Request — 2026-09-21T13:56:51Z

# Teamwork Project Prompt — Draft

> Status: Launched
> Goal: Craft prompt → get user approval → delegate to teamwork_preview
> Requested team: [none — teamwork routes from the description]

A production-ready implementation of the full V-RADM platform, including the Rust `vradmd` Asterisk gateway daemon and the mobile client application, fully wrapping the `vradm-core` library.

Working directory: ~/teamwork_projects/vradm_platform
Integrity mode: demo

## Requirements

### R1. Cross-Platform Mobile Client (Flutter)
Build the mobile client application using Flutter (to support future Android expansion) that wraps the `vradm-core` Rust library via FFI. The client must implement the iOS NetworkExtension (PacketTunnelProvider) for the TCP-PEP and use the spec-defined App Group shared memory (lock-free SPSC ring buffers) for cross-process communication with the audio engine.

### R2. Asterisk Gateway Daemon (vradmd)
Build the `vradmd` server daemon in Rust. It must implement the Asterisk AudioSocket protocol over TCP, maintain a multi-tenant thread pool for concurrent call sessions, and implement the server-side TCP-PEP terminating locally (e.g., at `127.0.0.1:22`).

### R3. Automated Mock Integration Harnesses
Implement automated software mock harnesses for both the Asterisk AudioSocket interface and the iOS App Group IPC mechanism to enable robust, programmatic end-to-end testing of the pipeline without requiring physical hardware in the loop.

## Acceptance Criteria

### Flutter & iOS IPC Integration
- [ ] Automated tests verify the Flutter-to-Rust FFI boundary successfully initializes the `vradm-core` engine and correctly configures the C-ABI structs.
- [ ] A mock test script verifies that IP datagrams can be enqueued and dequeued through the lock-free SPSC shared memory ring buffer mechanism without blocking or data races.

### Asterisk Daemon (vradmd)
- [ ] A programmatic mock Asterisk client can connect to `vradmd` via TCP AudioSocket, triggering the daemon to successfully allocate a session thread and begin parsing PCM chunk headers.
- [ ] The TCP-PEP server-side implementation successfully passes a simulated local TCP load test without triggering unintended zero-window deadlocks.

### End-to-End Simulation
- [ ] An automated integration test successfully passes a simulated IP packet from the mock NetworkExtension, through the `vradm-core` link layer, out as mock AudioSocket PCM chunks, and successfully decodes it on a mock `vradmd` receiver instance.
