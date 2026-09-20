# Engineering Specification: Vocoder-Resilient Acoustic Data Modem (V-RADM)

**Document Version:** 3.5.0

**Status:** Closed Baseline Engineering Specification

**Primary Targets:** `vradm-core` (Rust C-ABI Engine), iOS 17+ Client Adapter, Linux / Asterisk 20+ PBX Gateway Daemon

---

## 1. System Architecture & Process Boundaries

V-RADM establishes a standard IPv4 point-to-point tunnel across speech-compressed cellular voice channels (VoLTE, VoNR, 3G AMR, carrier VoIP) and acoustic air gaps. It enables unmodified network applications—specifically **OpenSSH** and **Mosh (Mobile Shell)**—to operate reliably under severe bandwidth, latency, and transcoding constraints.

```
 ┌────────────────────────────────────────────────────────────────────────┐
 │                              iOS HOST                                  │
 │                                                                        │
 │  ┌──────────────────────────────────────────────────────────────────┐  │
 │  │ NetworkExtension Process (PacketTunnelProvider)                  │  │
 │  │  - Intercepts IP packets from OS via virtual utun interface      │  │
 │  │  - MTU: 128 bytes | Subnet: 10.99.0.2/24                         │  │
 │  └──────────────────────────────┬───────────────────────────────────┘  │
 │                                 │ Lock-Free IPC Ring Buffer            │
 │                                 │ (App Group Shared POSIX Memory)      │
 │                                 ▼                                      │
 │  ┌──────────────────────────────────────────────────────────────────┐  │
 │  │ Main App Process (Foreground / Background Audio Entitlements)    │  │
 │  │  - User-Facing Terminal UI (Embedded LibSSH2 / LibMosh Core)     │  │
 │  │  - libvradm_core Engine (IP Slicer, ARQ, RS FEC, PHY Modulator)  │  │
 │  │  - Drift-Decoupling FIFO (Resolves 5.0ms IO vs 4.0ms slot timing)│  │
 │  │  - AVAudioEngine (Topology A: USB DAC / Topology C: In-Call API) │  │
 │  └──────────────────────────────┬───────────────────────────────────┘  │
 └─────────────────────────────────┼──────────────────────────────────────┘
                                   │ Active Cellular Call (VoLTE / AMR-WB)
                                   ▼
 ┌────────────────────────────────────────────────────────────────────────┐
 │                      SERVER GATEWAY (Linux PBX)                        │
 │                                                                        │
 │  [Carrier Trunk] ──> Asterisk 20+ PBX Core (SIP Trunk / VoLTE Gateway) │
 │                            │ AudioSocket Protocol (TCP:9099, Raw PCM)  │
 │                            ▼                                           │
 │  [vradmd Daemon] ──> vradm-core Engine (C-ABI Shared Library)          │
 │                            │ Reassembled IP Datagrams                  │
 │                            ▼                                           │
 │  [OS Networking] ──> Linux Virtual Adapter (/dev/net/tun: vradm0)      │
 │                            │ Routed 10.99.0.1/24                       │
 │                            ▼                                           │
 │  [Host Daemons] ───> sshd (Port 22) & mosh-server (-p 60000:60010)     │
 └────────────────────────────────────────────────────────────────────────┘

```

### 1.1 The Parameter-Domain Physical Channel

Cellular speech vocoders (specifically ACELP variants: AMR, AMR-WB, EVS) discard analog audio waveforms and inter-carrier phase relationships. They decompose audio into parametric speech features:

* **Linear Prediction (LP) Spectral Envelope:** Models the human vocal tract (quantized as Line Spectral Pairs/ISFs).
* **Adaptive Codebook:** Models fundamental pitch periodicity as a sample-domain delay ($T_0$) and pitch gain ($g_p$).
* **Algebraic Fixed Codebook:** Models the excitation residual using interleaved multi-pulse grids and gain ($g_c$).

V-RADM models the cellular voice channel as a **lossy parameter-quantization channel**. Demodulation relies on feature-distance estimation and soft-decision confidence decoding rather than hard-sliced phase boundaries.

### 1.2 Two-Channel PHY Decoupling

To eliminate circular boot-up dependencies—where a receiver must know the active Modulation and Coding Scheme (MCS) to demodulate the frame containing the MCS field—transmission is split into two asynchronous layers:

1. **PLCP Control Channel:** A low-rate, noncoherent control beacon transmitted using fixed pitch-frequency-shift keying. It announces transmitter state, active MCS, requested reverse MCS, and framing sequence.
2. **Payload Data Channel:** A variable-rate data carrier (MCS 0 through MCS 4) that immediately follows the PLCP beacon, formatted into immutable 64-byte physical frames.

---

## 2. Canonical Wire Formats

V-RADM defines two physical layer frame formats: the **Canonical Data PDU (64 Bytes)** for standard streaming payloads, and the **Compact Control Frame (16 Bytes)** for low-overhead signaling and rapid half-duplex turn-arounds.

### 2.1 Canonical Data Frame (64-Byte Physical PDU)

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|      SYNC_WORD (0xD391)       |      CTRL     |      SEQ      |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|    ACK_BASE   |    ACK_MAP    |  PAYLOAD_LEN  |  HEADER_CRC8  |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                                                               |
|        INFORMATION PAYLOAD DATA (Bytes 0x08..0x2D, 38 Bytes)  |
|                                                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|          PAYLOAD_CRC16        |                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+                               +
|                                                               |
|              REED-SOLOMON PARITY (Bytes 0x30..0x3F, 16 Bytes) |
|                                                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+

```

| Byte Range | Field Name | Width | Scope & Operational Description |
| --- | --- | --- | --- |
| `0x00..0x01` | `SYNC_WORD` | 16 bits | Fixed frame delimiter: `0xD391`. Used for frame confirmation after PLCP synchronization. |
| `0x02` | `CTRL` | 8 bits | Bit [7]: Mode (`0` = Standard IP Duplex, `1` = Simplex SOTP Broadcast)<br>

<br>Bits [6..4]: Active Frame MCS (`000` = MCS 0 .. `100` = MCS 4)<br>

<br>Bit [3]: TDD Turn Flag (`1` = Yield physical channel to peer)<br>

<br>Bits [2..0]: Wire Protocol Version (`001` = v3.5) |
| `0x03` | `SEQ` | 8 bits | Rolling transmit sequence number ($0\text{--}255$). |
| `0x04` | `ACK_BASE` | 8 bits | Cumulative ACK: highest contiguous peer sequence number received in-order. |
| `0x05` | `ACK_MAP` | 8 bits | Bit [7]: Feedback Type (`0` = Normal Bitmap, `1` = Urgent NACK)<br>

<br>Bits [6..0]: Selective ACK bitmap for frames `ACK_BASE + 1` through `ACK_BASE + 7`. |
| `0x06` | `PAYLOAD_LEN` | 8 bits | Valid payload length $k$, where $0 \le k \le 38$. |
| `0x07` | `HEADER_CRC8` | 8 bits | CRC-8-CCITT covering bytes `0x02..0x06` ($x^8 + x^2 + x + 1$). |
| `0x08..0x2D` | `PAYLOAD` | 38 bytes | Information payload: 1-byte IP fragmentation header + up to 37 bytes IP data (or 38 bytes SOTP slice). |
| `0x2E..0x2F` | `PAYLOAD_CRC16` | 16 bits | CRC-16-CCITT covering bytes `0x02..0x2D` ($x^{16} + x^{12} + x^5 + 1$). |
| `0x30..0x3F` | `RS_PARITY` | 16 bytes | Systematic $\text{RS}(64, 48)$ Galois field parity covering bytes `0x00..0x2F`. |

* **Total Protected Information Block:** Bytes `0x00..0x2F` = **48 bytes**.
* **Total FEC Parity Block:** Bytes `0x30..0x3F` = **16 bytes**.
* **Total Frame Length:** $48 + 16 = \mathbf{64\text{ bytes}}$ (512 bits).

### 2.2 Compact Control Frame (16-Byte CCF PDU)

To prevent empty feedback transmissions from burning 6.4 seconds of channel time in MCS 0, standalone ACKs, TDD grants, and MCS commit acknowledgments utilize the **16-byte Compact Control Frame**:

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|      SYNC_WORD (0xD391)       |   CCF_CTRL    |   ACK_BASE    |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|    ACK_MAP    |  CCF_CRC16    |                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+                               +
|              REED-SOLOMON PARITY (Bytes 0x08..0x0F, 8 Bytes)  |
|                                                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+

```

* **Bytes 0x00..0x01:** `SYNC_WORD` (`0xD391`).
* **Byte 0x02:** `CCF_CTRL` (Bit [7]: `1` = CCF Marker; Bits [6..4]: Requested MCS; Bit [3]: TDD Yield Flag; Bits [2..0]: Command Type: `001` = Standalone ACK, `010` = MCS Commit Ack, `011` = TDD Grant).
* **Bytes 0x03..0x04:** `ACK_BASE` and `ACK_MAP`.
* **Bytes 0x05..0x06:** CRC-16-CCITT covering bytes `0x02..0x04`.
* **Bytes 0x07..0x0F:** Systematic $\text{RS}(16, 8)$ Parity (8 parity bytes correcting up to $t = 4$ erroneous bytes).
* **Total CCF Duration at MCS 0:** $\frac{128\text{ bits}}{4\text{ bits/sym}} \times 50.0\text{ ms} = \mathbf{1.6\text{ seconds}}$ (a 75% reduction vs full 64-byte frame).

---

## 3. Physical Layer (PHY) & Sample-Exact Modulation

### 3.1 Formal Throughput Hierarchy

To prevent ambiguity between theoretical capacities and operational network delivery, three distinct rate tiers are formally defined:

1. **Raw PHY Rate ($R_{\text{PHY}}$):** Total raw bit rate emitted by the modulator:

$$R_{\text{PHY}} = \text{Baud} \times \log_2(Y) = \text{Baud} \times X$$


2. **Layer 3 Maximum Payload Rate ($R_{\text{L3}}$):** Maximum IP throughput across continuous 64-byte frames (37 bytes IP data per frame) before PLCP and ARQ overhead:

$$R_{\text{L3}} = \frac{296\text{ bits}}{T_{\text{frame}}}$$


3. **Application Goodput ($R_{\text{APP}}$):** Realizable end-to-end user data throughput through standard IP/TCP or IP/UDP sockets, incorporating PLCP cadence, IP/transport header overheads (128-byte MTU), and nominal channel ARQ:

$$R_{\text{APP}} = R_{\text{L3}} \times \eta_{\text{PLCP}} \times \eta_{\text{Transport}} \times (1 - P_{\text{FER}})$$



```
 ┌────────────────────────────────────────────────────────────────────────────────────────────────┐
 │                                CLOSED-FORM MCS SPECIFICATIONS                                  │
 ├──────┬──────────┬────────┬───────────┬─────────────┬──────────┬────────────┬──────────┬────────┤
 │ MCS  │ Target   │ Baud   │ Alphabet  │ Independent │ Bits/Sym │ Raw PHY    │ Max L3   │ App    │
 │      │ Channel  │ (Bd)   │ Size (Y)  │ Dims (Z)    │ (X)      │ Rate (bps) │ Rate(bps)│ Goodput│
 ├──────┼──────────┼────────┼───────────┼─────────────┼──────────┼────────────┼──────────┼────────┤
 │ 0    │ Free-Air │ 20     │ 16        │ 1           │ 4        │ 80.0       │ 46.25    │ ~24.0  │
 │ 1    │ NB-AMR   │ 50     │ 256       │ 3           │ 8        │ 400.0      │ 231.25   │ ~145.0 │
 │ 2    │ Balanced │ 100    │ 256       │ 4           │ 8        │ 800.0      │ 462.50   │ ~310.0 │
 │ 3*   │ WB-AMR   │ 200    │ 65,536    │ 8           │ 16       │ 3,200.0    │ 1,850.00 │ ~1,120 │
 │ 4    │ PCM/VoIP │ 250    │ 1,048,576 │ 8           │ 16       │ 4,000.0    │ 2,312.50 │ ~1,580 │
 └──────┴──────────┴────────┴───────────┴─────────────┴──────────┴────────────┴──────────┴────────┘
 *Note: MCS 3 is an experimentally gated high-rate mode; operational enablement requires passing TC-04.

```

### 3.2 Sample-Exact Modulator Formulations

#### MCS 0: Free-Air Acoustic TDD (High Reverberation)

* **Alphabet:** $Y = 16$ fundamental pitch states ($F_0$).
* **Dimensions:** $Z = 1$ ($F_0(m) = 120\text{ Hz} + (m \cdot 10\text{ Hz})$ for $m \in [0..15]$).
* **Timing:** $T_{\text{sym}} = 50.0\text{ ms}$ (400 samples at 8 kHz, 800 samples at 16 kHz).
* **Multipath Guard:** Demodulator discards first $15.0\text{ ms}$ of each symbol; NCCF integration runs strictly over the final $35.0\text{ ms}$.

#### MCS 1: Robust Narrowband Cellular (AMR-NB 4.75k–7.4k)

* **Alphabet:** $Y = 256$ joint speech feature states ($8\text{ bits/symbol}$ at $50\text{ Bd}$).
* **Reference Synthesis Equation:**

$$s(n) = \sum_{p} e(n - p \cdot T_0 - \delta) * h_{\text{vowel}}(n; F_1, F_2)$$


* **Dimensions ($Z = 3$):**
1. **Pitch Delay ($T_0$, 3 bits):** Sample-domain integer lag $T_0 \in \{33, 38, 43, 49, 56, 64, 72, 80\}$ samples at 8 kHz ($F_0 \in [100.0\text{ Hz}, 242.4\text{ Hz}]$).
2. **Formant Resonances ($F_1, F_2$, 3 bits):** Second-order cascaded biquad IIR filter ($Q = 5.0$) modeling 8 vowel states:

$$\{(300, 900), (350, 1400), (450, 1100), (500, 1700), (600, 1200), (650, 1900), (750, 1300), (800, 2100)\}\text{ Hz}$$


3. **Algebraic Grid Offset ($\delta$, 2 bits):** Excitation pulse offset $\delta \in \{0, 1, 2, 3\}\text{ samples}$ aligned to 5 ms subframe boundaries.



#### MCS 2: Balanced Cellular (AMR-NB 10.2k/12.2k / AMR-WB 6.6k)

* **Alphabet:** $Y = 256$ states ($8\text{ bits/symbol}$ at $100\text{ Bd}$, $T_{\text{sym}} = 10.0\text{ ms}$).
* **Carrier Frequencies ($Z = 4$):** $f_k \in \{600, 1000, 1400, 1800\}\text{ Hz}$.
* **Sample-Exact Modulator:**

$$s(n) = w(n) \sum_{k=0}^{3} A_k \cos\left(\frac{2\pi f_k n}{F_s} + \phi_k(m)\right)$$


$$\phi_k(m) = \text{wrap}_{2\pi}\left(\phi_k(m-1) + \Delta \phi_k(m)\right), \quad \Delta \phi_k \in \left\{0, \frac{\pi}{2}, \pi, \frac{3\pi}{2}\right\}$$



Where $A_k = [0.8, 1.0, 0.9, 0.7]$ provides formant-contour weighting, and $w(n)$ is a 0.5 ms raised-cosine edge taper.

#### MCS 3: Wideband Cellular Cabled (AMR-WB 12.65k+ / VoLTE)

* **Status:** Experimentally gated coherent high-rate mode.
* **Alphabet:** $Y = 65,536$ states ($16\text{ bits/symbol}$ at $200\text{ Bd}$, $T_{\text{sym}} = 5.0\text{ ms}$).
* **Carrier Frequencies ($Z = 8$):** $f_k = k \cdot 200\text{ Hz}$ for $k \in [3..10]$ ($600\text{ Hz to } 2000\text{ Hz}$).
* **Sample-Exact Modulator:**

$$s(n) = w(n) \sum_{k=3}^{10} A_k \cos\left(\frac{2\pi f_k n}{F_s} + \phi_k(m)\right)$$


$$A_k = [0.6, 0.9, 1.0, 0.85, 0.7, 0.5, 0.4, 0.3], \quad \phi_k(0) = \frac{k \pi}{4}$$



Phase transitions occur strictly on 5 ms ACELP subframe boundaries. Micro-tremor is disabled.

#### MCS 4: Real-Valued Hermitian CP-OFDM (G.711 / High-Rate VoIP)

* **Sampling Rate:** $F_s = 8,000\text{ Hz}$.
* **Orthogonal Subcarrier Spacing:** $\Delta f = \frac{1}{T_{\text{useful}}} = \frac{8000}{28} = \mathbf{285.714\text{ Hz}}$.
* **Hermitian Symmetric Real-Valued IFFT Structure:**

$$N_{\text{fft}} = 28\text{ points}, \quad N_{\text{cp}} = 4\text{ points} \implies N_{\text{total}} = 32\text{ samples (4.0 ms, 250 Bd)}$$



For subcarrier bin indices $k \in [0..14]$ with complex QPSK data symbols $D_k$:

$$X[k] = D_k, \quad X[28 - k] = D_k^*, \quad X[0] = X[14] = 0$$



The inverse discrete Fourier transform yields a strictly real-valued audio sequence:

$$x(n) = \frac{1}{\sqrt{N_{\text{fft}}}} \sum_{k=0}^{N_{\text{fft}}-1} X[k] e^{j \frac{2\pi k n}{N_{\text{fft}}}} \in \mathbb{R}$$


* **Telephone-Band Carrier Allocation ($Z = 8$ Active Carriers):**
To comply strictly with the ITU-T G.711 $300\text{--}3400\text{ Hz}$ passband while avoiding carrier tone-detector frequencies ($1100\text{ Hz}$, $1300\text{ Hz}$, $2100\text{ Hz}$):

$$k \in \{2, 3, 5, 6, 7, 8, 9, 10\} \implies f_k \in \{571.4, 857.1, 1428.6, 1714.3, 2000.0, 2285.7, 2571.4, 2857.1\}\text{ Hz}$$



*(Bin $k=4$ [$1142.9\text{ Hz}$] is zeroed to protect $1100/1300\text{ Hz}$ detector bands; bin $k=11$ [$3142.9\text{ Hz}$] and above are zeroed to provide anti-aliasing headroom).*
* **Demodulation:** Differential QPSK across consecutive OFDM symbol slots eliminates the requirement for complex pilot channel estimation.

### 3.3 Transmit Signal Conditioning & Detector Evasion

1. **Power Normalization:** Output RMS power is clamped at $-6.0\text{ dBFS}$. Active carrier amplitudes scale as:

$$A_k(N) = A_{\text{ref}} \sqrt{\frac{8}{N}}$$


2. **Biological Micro-Tremor Scope:** A $4.0\text{ Hz}$ sinusoidal micro-jitter ($\pm 2.5\text{ Hz}$) is applied **only to MCS 0 and MCS 1**. It is **strictly disabled for MCS 3 and MCS 4** to prevent phase disturbance in differential detectors. For MCS 2, micro-jitter is clamped to $\le \pm 0.5\text{ Hz}$.
3. **Voiced Maintenance Sequence:** During TDD channel pauses, idle states, or ARQ stalls, the transmitter emits a continuous $-26\text{ dBFS}$ synthetic voiced sound ($F_0(t) = 130\text{ Hz} + 15\sin(2\pi \cdot 2.5 t)\text{ Hz}$, formants $F_1 = 550\text{ Hz}, F_2 = 1500\text{ Hz}$) to prevent carrier Voice Activity Detectors (VAD) from entering Discontinuous Transmission (DTX) or Comfort Noise Generation (CNG).

---

## 4. PLCP Control Channel & MCS Commit Protocol

The receiver never inspects the data frame to determine its modulation scheme. Every burst is preceded by a **Physical Layer Convergence Protocol (PLCP) Control Beacon** modulated via ultra-robust 2-FSK.

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|      BARKER-13 DUAL-CHIRP     |CUR_MCS|REQ_MCS|TX_PWR |BEAC_SEQ|
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|  BEACON_CRC8  | GOLAY_PARITY  |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+

```

### 4.1 Deterministic PLCP Parameters

* `PLCP_CHIP_RATE`: 200 chips/s ($5.0\text{ ms/chip}$).
* `PLCP_PREAMBLE`: 13 chips $\times 5.0\text{ ms} = \mathbf{65.0\text{ ms}}$ (Hyperbolic pitch sweep: $600\text{ Hz} \leftrightarrow 1800\text{ Hz}$).
* `PLCP_GUARD`: $10.0\text{ ms}$ silence before and after header.
* `PLCP_HEADER_PAYLOAD`: 16 bits (`CUR_MCS` [3b], `REQ_MCS` [3b], `TX_PWR` [2b], `BEAC_SEQ` [8b]).
* `PLCP_HEADER_FEC`: 16-bit information protected by an Extended Golay $(24, 12)$ code + CRC-8, yielding 32 encoded bits.
* `PLCP_MODULATION`: 2-FSK ($1200\text{ Hz} = \text{Mark}, 1600\text{ Hz} = \text{Space}$) at $100\text{ Bd}$ ($10.0\text{ ms/bit}$). Total header duration = $32 \times 10.0\text{ ms} = \mathbf{320.0\text{ ms}}$.
* `PLCP_TOTAL_DURATION`: $65.0 + 10.0 + 320.0 + 10.0 = \mathbf{405.0\text{ ms}}$.
* **Transmission Cadence:** A PLCP beacon is transmitted:
1. At session initialization (`SESSION_START`).
2. At the beginning of each transmit turn in half-duplex TDD (`TDD_TURN`).
3. Exactly once every 16 continuous data frames during full-duplex streaming (`CONTINUOUS_SYNC`).
4. Immediately upon initiating an MCS transition (`MCS_CHANGE`).



### 4.2 Two-Phase MCS Commit Handshake

To prevent desynchronization races where a receiver misses an MCS announcement and attempts to decode symbols under the wrong demodulator:

```
 Node A (Transmitter)                                 Node B (Receiver)
 ┌─────────────────┐                                 ┌─────────────────┐
 │ Requests MCS 3  │                                 │ Operating MCS 2 │
 └────────┬────────┘                                 └────────┬────────┘
          │                                                   │
          │ Phase 1: PLCP (CUR=2, REQ=3) + Data Frames        │
          ├──────────────────────────────────────────────────►│
          │                                                   │
          │ Phase 2: CCF Response (ACK_BASE, COMMIT_MCS=3)    │
          │◄──────────────────────────────────────────────────┤
          │                                                   │
 ┌────────┴────────┐                                 ┌────────┴────────┐
 │ Switch to MCS 3 │                                 │ Switch to MCS 3 │
 │ at SEQ = S+1    │                                 │ at SEQ = S+1    │
 └─────────────────┘                                 └─────────────────┘

```

1. **Announcement:** Node A transmits its current burst at `CUR_MCS`, setting `REQ_MCS = target`.
2. **Commit Ack:** Node B decodes the request, verifies channel metric $M \ge 0.85$, and responds with a Compact Control Frame (CCF) setting `CCF_CTRL` command to `MCS_COMMIT_ACK` with the agreed sequence boundary $S$.
3. **Synchronous Switchover:** Both nodes switch their modulators and demodulators simultaneously at sequence $S + 1$. If Node A misses the commit CCF, it remains at `CUR_MCS` and re-announces on the next PLCP interval.

---

## 5. Half-Duplex TDD Protocol Specification (MCS 0)

In open-air acoustic conditions, simultaneous bidirectional audio triggers phone-level Acoustic Echo Cancellation (AEC), destroying the link. MCS 0 enforces deterministic Time Division Duplexing (TDD).

```
 0s                     6.4s    6.55s         8.15s    8.3s                  14.7s
 ┌──────────────────────┬───────┬─────────────┬────────┬─────────────────────┐
 │ Node A: Data Frame   │ EOT   │ Node B: CCF │ EOT    │ Node A: Data Frame  │
 │ (64 Bytes, 512 bits) │ Tone  │ (16 B, ACK) │ Tone   │ (64 Bytes, 512 bits)│
 └──────────────────────┴───────┴─────────────┴────────┴─────────────────────┘
  ◄──── A Transmit ────► Guard   ◄── B Trans ─► Guard   ◄──── A Transmit ────►

```

### 5.1 TDD Operational Rules

1. **Initial Medium Ownership:** The calling gateway (Asterisk PBX) acts as the Master node and owns the initial transmit slot upon carrier lock.
2. **Turn Duration Limit:** A transmit turn is limited to a maximum of **1 Canonical Data Frame (6.4 s)** or **1 Compact Control Frame (1.6 s)**.
3. **End-of-Turn (EOT) Tone:** Immediately following the terminal symbol of a frame, the transmitting node emits a **$150.0\text{ ms}$ dual-tone burst ($1400\text{ Hz} + 1800\text{ Hz}$ at $-12.0\text{ dBFS}$)** and reverts to receive mode.
4. **Turn Guard Interval:** The receiving node detects the EOT tone, waits for a **$150.0\text{ ms}$ Acoustic Decay Guard Window** (allowing room reverberation to settle), and then initiates transmission.
5. **Collision Recovery:** If a node misses an EOT tone, it waits for a turn timeout ($T_{\text{turn\_timeout}} = 8.5\text{ seconds}$). If no carrier is detected, the Master node asserts channel ownership by emitting a PLCP beacon.

---

## 6. Soft-Decision Link Layer & Error Correction

```
 Incoming PCM Audio
         │
         ▼
 Goertzel Bank / NCCF Pitch Correlator
         │
         ▼
 Soft Symbol Confidence Computer: C_i = tanh(SNR_carrier) * (1.0 - d / (pi/4))
         │
         ▼
 8x8 Byte Block Deinterleaver (Permutes Bytes and Confidences Simultaneously)
         │
         ▼
 GMD Erasure Tagger:
 Evaluates weakest bytes with C_byte < 0.35, sorting by ascending confidence.
 Tests trial erasure counts: e in {16, 14, 12, ..., 0}
         │
         ▼
 Berlekamp-Massey Errors-and-Erasures RS(64, 48) Decoder:
 Solves: 2t + e <= 16
         │
    ┌────┴────┐
    ▼         ▼
 Success    Failure ──> Decrement e by 2, re-decode iteratively down to e = 0
    │
    ▼
 Check PAYLOAD_CRC16

```

### 6.1 Bounded Link Quality Metric ($M$)

To eliminate negative values and normalize decoder stress across both errors and erasures, the normalized decoder burden $B$ is defined:

$$B = \frac{2t + e}{16} \in [0.0, 1.0]$$

The Link Quality Metric $M$ is strictly bounded in $[0.0, 1.0]$:

$$M = 0.4 \cdot \bar{C}_{\text{sym}} + 0.3 \cdot (1.0 - P_{\text{FER}}) + 0.3 \cdot (1.0 - B)$$

* $\bar{C}_{\text{sym}}$: Mean symbol confidence over the last 16 frames ($0.0\text{--}1.0$).
* $P_{\text{FER}}$: Frame Error Rate over the last 16 frames ($0.0\text{--}1.0$).
* $B$: Mean normalized decoder burden per frame ($0.0\text{--}1.0$).

### 6.2 Matrix Block Interleaver Mapping

* **Write Order (Input):** Row-major ($0 \le r < 8, 0 \le c < 8 \implies \text{index} = r \times 8 + c$).
* **Read Order (Output to PHY):** Column-major ($\text{index} = c \times 8 + r$).
* **Dispersal Guarantee:** At MCS 3 ($3,200\text{ bps}$ raw), an 8-byte burst erasure ($64\text{ bits} = 20.0\text{ ms}$) spanning a single column disperses into **exactly 1 byte per row across all 8 rows**, well within the $\text{RS}(64, 48)$ budget of $t \le 8$ correctable bytes.

### 6.3 Closed-Form Retransmission Timeout (RTO)

$$\text{RTO} = \text{SRTT} + \max(4 \cdot \text{RTTVAR}, T_{\text{frame}}) + T_{\text{margin}}(\text{Profile})$$

* **Profile 1 (Direct Cabled Full-Duplex):**

$$T_{\text{frame}} = 160\text{ ms}, \quad T_{\text{margin}} = 150\text{ ms}, \quad \text{Nominal RTO} = \mathbf{560\text{ ms}}$$


* **Profile 2 (Free-Air Acoustic Half-Duplex with Compact ACKs):**
Accounting for 1 Data Frame ($6.4\text{ s}$), 2 EOT guards ($0.3\text{ s}$ total), and 1 Compact Control Frame ACK ($1.6\text{ s}$):

$$T_{\text{cycle}} = 6.4\text{ s} + 0.15\text{ s} + 1.6\text{ s} + 0.15\text{ s} = 8.3\text{ s}$$


$$\text{RTO}_{\text{nominal}} = 8.3\text{ s} + 500\text{ ms (margin)} = \mathbf{8.8\text{ seconds}}$$


* **Karn's Algorithm Mandate:** RTT updates MUST NOT be computed from retransmitted frames.

---

## 7. Network Tunneling & Application Layer

```
 ┌────────────────────────────────────────────────────────┐
 │           IP-OVER-VRADM DATAGRAM ENCAPSULATION         │
 └────────────────────────────────────────────────────────┘
  0                   1                   2                   3
  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
 +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
 |PKT_ID |FRAG_ID|M|B|             IP DATAGRAM FRAGMENT          |
 +-+-+-+-+-+-+-+-+-+-+                                           +
 |                                                               |
 |               Bytes 0x01..0x25 (Up to 37 Bytes Data)          |
 |                                                               |
 +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
  ◄──────────── 38-Byte Information Payload (0x08..0x2D) ────────►

```

### 7.1 IP Fragmentation Header (Byte 0x08)

* `PKT_ID` (Bits [7..5]): Rolling packet identifier ($0\text{--}7$).
* `FRAG_ID` (Bits [4..2]): Fragment index within packet ($0\text{--}7$).
* `MORE_FRAGS` (Bit [1]): `1` = More fragments follow; `0` = Final fragment.
* `BEST_EFFORT` (Bit [0]): `1` = Unreliable datagram (bypasses ARQ retransmissions for stale Mosh UDP packets); `0` = Reliable in-order delivery.
* **Virtual MTU:** Fixed at **128 bytes** ($\lceil 128 / 37 \rceil = 4\text{ frames}$ per packet).

### 7.2 OpenSSH Client Configuration (`~/.ssh/config`)

```text
Host vradm-gw
    HostName 10.99.0.1
    Port 22
    User admin
    
    # Pruned cipher suite to minimize KEXINIT footprint
    KexAlgorithms curve25519-sha256
    Ciphers chacha20-poly1305@openssh.com
    HostKeyAlgorithms ssh-ed25519
    
    # Pre-seeded host key
    StrictHostKeyChecking yes
    UserKnownHostsFile ~/.ssh/known_hosts
    
    ForwardAgent no
    ForwardX11 no
    ServerAliveInterval 30
    ServerAliveCountMax 4
    TCPKeepAlive no
    Compression yes

```

### 7.3 Mosh Invocation & Port Binding

Mosh runs over UDP with speculative local echo, eliminating typing latency over slow acoustic links:

```bash
mosh --ssh="ssh -F ~/.ssh/config" --server="mosh-server new -p 60000:60010" 10.99.0.1

```

---

## 8. Simplex Object Transfer Protocol (SOTP)

SOTP handles unidirectional, unacknowledged broadcast data drops. SOTP frames occupy the 38-byte `PAYLOAD` field at physical offsets `0x08..0x2D`.

```
 ┌────────────────────────────────────────────────────────┐
 │             SOTP DATA FRAME (DESC_MARKER = 0x00)       │
 └────────────────────────────────────────────────────────┘
  Byte 0x00: DESC_MARKER (0x00)
  Byte 0x01: SBN (8 bits, Source Block Number)
  Bytes 0x02..0x03: ESI (16 bits, Encoding Symbol ID: 0 .. 65,535)
  Bytes 0x04..0x25: SYMBOL_DATA (34 Bytes RaptorQ Symbol Payload, T = 34)

 ┌────────────────────────────────────────────────────────┐
 │           SOTP METADATA MANIFEST (DESC_MARKER = 0xFE)  │
 └────────────────────────────────────────────────────────┘
  Byte 0x00: DESC_MARKER (0xFE)
  Byte 0x01: OBJECT_ID (8 bits)
  Bytes 0x02..0x03: EXTENDED_SOURCE_SYMBOLS (K', 16 bits)
  Bytes 0x04..0x05: TOTAL_SOURCE_BLOCKS (Z, 16 bits)
  Bytes 0x06..0x09: TOTAL_BYTES (32 bits, uncompressed file size)
  Bytes 0x0A..0x0D: TRUNC_BLAKE3 (Leading 4 bytes of hash)
  Bytes 0x0E..0x25: BLAKE3_TAIL (Remaining 24 bytes of full 32-byte hash)

```

### 8.1 RaptorQ Framing Rules (RFC 6330)

* **Symbol Size ($T$):** Exactly **34 bytes**.
* **Source Block Limit ($K_{\max}$):** 1,024 symbols ($34.8\text{ KB}$). Objects $> 34.8\text{ KB}$ are partitioned into $Z = \lceil \text{TotalBytes} / (K_{\max} \cdot T) \rceil$ source blocks.
* **Metadata Cadence:** The Metadata Manifest frame is emitted **every 8 frames**. A receiver joining mid-broadcast acquires the manifest and full 32-byte BLAKE3 hash within 8 frames and begins collecting fountain slices without missing the transfer.

---

## 9. Platform Integration & Audio Routing Topologies

```
 [TOPOLOGY A: DIRECT CABLED DONGLE (iOS 17+ Production Baseline)]
 ┌────────────────┐ Lightning/USB-C ┌───────────────┐ 3.5mm TRRS ┌──────────────────────┐
 │ iPhone         ├────────────────►│ Apple USB-C   ├────────────►│ Headset Jack of      │
 │ (V-RADM App)   │◄────────────────┤ Audio Adapter │◄────────────┤ Secondary Handset    │
 └────────────────┘                 └───────────────┘             └──────────────────────┘

 [TOPOLOGY B: IN-APP VOIP CALL (iOS 17+ Production Baseline)]
 ┌────────────────┐ Cellular LTE/5G ┌───────────────┐ SIP/RTP    ┌──────────────────┐
 │ iPhone         ├────────────────►│ Carrier Data  ├────────────►│ Asterisk PBX     │
 │ (V-RADM App)   │                 │ Connection    │             │ (AudioSocket)    │
 └────────────────┘                 └───────────────┘             └──────────────────┘

 [TOPOLOGY C: "ADD AUDIO IN CALLS" (iOS 18.2+ Experimental)]
 ┌────────────────┐ Apple In-Call   ┌───────────────┐ VoLTE Call ┌──────────────────┐
 │ iPhone         ├─(Injection API)►│ iPhone Native ├────────────►│ Asterisk PBX     │
 │ (V-RADM App)   │                 │ Phone App     │             │ (AudioSocket)    │
 └────────────────┘                 └───────────────┘             └──────────────────┘

```

### 9.1 iOS Implementation Realities

* **Process Separation:** `NEPacketTunnelProvider` runs inside a sandboxed Network Extension process, while `AVAudioEngine` runs in the main app process.
* **IPC Bridge:** A lock-free shared memory ring buffer (`mmap` over an App Group container file) links the extension and main app with a target $p99 \le 1.0\text{ ms}$ IPC latency.
* **Application Shell:** The terminal UI is embedded inside the main iOS app via `LibMosh` or `LibSSH2`, consuming the virtual tunnel. Child CLI processes are not spawned in the sandbox.

### 9.2 Linux / Asterisk PBX AudioSocket Gateway

Asterisk routes call audio to `vradmd` via AudioSocket. Documented argument order: `AudioSocket(uuid,service)`.

```ini
; /etc/asterisk/extensions.conf
[vradm-inbound]
exten => 774,1,NoOp(Incoming V-RADM Carrier Link)
same  => n,Answer()
; Advisory caller ID filter (Gatekeeper only; not authentication)
same  => n,GotoIf($["${CALLERID(num)}" != "+15550198372"]?reject)
; Hand off 8kHz linear PCM to vradmd TCP daemon (uuid,service)
same  => n,AudioSocket(4a8b7f32-5c21-4b76-90e1-0c1b72a9e3d1,127.0.0.1:9099)
same  => n,Hangup()
same  => n(reject),NoOp(Unauthorized Call Dropped)
same  => n,Hangup()

```

#### AudioSocket Protocol Parsing (`vradmd` Daemon)

* Message Header: 3 bytes (`0x10` type byte + 16-bit big-endian payload length).
* Payload: Signed 16-bit linear PCM mono, little-endian, sampled at $8,000\text{ Hz}$ or $16,000\text{ Hz}$.

---

## 10. Complete C-ABI Interface (`vradm_core.h`)

```c
#ifndef VRADM_CORE_H
#define VRADM_CORE_H

#include <stdint.h>
#include <stddef.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

#define VRADM_FRAME_SIZE         64
#define VRADM_CCF_SIZE           16
#define VRADM_MAX_PAYLOAD_SIZE   38
#define VRADM_MAX_IP_DATA_SIZE   37

typedef uint8_t vradm_mcs_t;
#define VRADM_MCS_0  0  // 20 Bd, Ortho-Pitch (80 bps raw / 46.25 bps L3)
#define VRADM_MCS_1  1  // 50 Bd, Speech Atom (400 bps raw / 231.25 bps L3)
#define VRADM_MCS_2  2  // 100 Bd, 4-DQPSK (800 bps raw / 462.50 bps L3)
#define VRADM_MCS_3  3  // 200 Bd, 8-DQPSK (3200 bps raw / 1850.00 bps L3)
#define VRADM_MCS_4  4  // 250 Bd, Real CP-OFDM (4000 bps raw / 2312.50 bps L3)

typedef uint32_t vradm_rate_t;
#define VRADM_RATE_8K   8000
#define VRADM_RATE_16K 16000

typedef struct vradm_engine vradm_engine_t;

typedef struct {
    vradm_mcs_t  startup_mcs;
    uint8_t      reserved[3];
    vradm_rate_t sample_rate;
    uint8_t      auto_rate_adaptation;
    uint8_t      padding[3];
    float        tx_amplitude; // Max RMS ceiling (Default: 0.5 = -6.0 dBFS)
} vradm_config_t;

typedef struct {
    float       estimated_snr_db;
    vradm_mcs_t active_tx_mcs;
    vradm_mcs_t active_rx_mcs;
    uint8_t     plcp_carrier_locked;
    uint8_t     reserved;
    uint32_t    frames_transmitted;
    uint32_t    frames_received;
    uint32_t    rs_corrected_bytes;
    uint32_t    rs_corrected_erasures;
    uint32_t    crc_failures;
    float       channel_metric_score;
} vradm_telemetry_t;

/* --- Lifecycle --- */
vradm_engine_t* vradm_create(const vradm_config_t* config);
void            vradm_destroy(vradm_engine_t* engine);
void            vradm_reset(vradm_engine_t* engine);

/* --- Audio Streaming I/O (Real-Time Safe: Zero Allocations) --- */
void   vradm_process_audio(vradm_engine_t* engine, const int16_t* in_samples, size_t count);
size_t vradm_generate_audio(vradm_engine_t* engine, int16_t* out_samples, size_t max_count);

/* --- Mode A: IP Packet Datagram Stream (TUN Interface) --- */
int32_t vradm_write_ip_packet(vradm_engine_t* engine, const uint8_t* packet, size_t len);
int32_t vradm_poll_ip_packet(vradm_engine_t* engine, uint8_t* out_packet, size_t max_len);

/* --- Mode B: SOTP Simplex Object Transfer --- */
int32_t vradm_sotp_tx_init(vradm_engine_t* engine, const uint8_t* payload, size_t len, float redundancy_factor);
int32_t vradm_sotp_rx_poll(vradm_engine_t* engine, size_t* out_collected_symbols, size_t* out_required_symbols);
int32_t vradm_sotp_rx_fetch(vradm_engine_t* engine, uint8_t* out_buf, size_t max_len, uint8_t out_hash[32]);

/* --- Telemetry --- */
void vradm_get_telemetry(const vradm_engine_t* engine, vradm_telemetry_t* out_telem);

#ifdef __cplusplus
}
#endif

#endif // VRADM_CORE_H

```

---

## 11. Verification Matrix & Acceptance Test Criteria

```
                            AUTOMATED TEST HARNESS PIPELINE
 ┌─────────────────┐       ┌─────────────────────────────────┐       ┌─────────────────┐
 │ Generated Audio │ ────► │ 3GPP Reference C Codecs         │ ────► │ Demodulator     │
 │ (vradm-core)    │       │ - AMR-NB (4.75k to 12.2k)       │       │ (vradm-core)    │
 └─────────────────┘       │ - AMR-WB (6.60k to 23.85k)      │       └────────┬────────┘
                           │ - Injected Frame Drops & Skew   │                │
                           │ - Dynamic Mode Downshifting     │                ▼
                           └─────────────────────────────────┘       ┌─────────────────┐
                                                                     │ Acceptance Pass │
                                                                     └─────────────────┘

```

| ID | Test Category | Channel Configuration | Injected Impairment | Pass / Fail Acceptance Criteria |
| --- | --- | --- | --- | --- |
| **TC-01** | Math Loopback | In-memory loopback | Zero noise, synchronous clock | Zero observed bit errors over $30 \times 10^6$ bits ($\implies P_e < 1.0 \times 10^{-7}$ at 95% Clopper-Pearson confidence). Zero RS corrections. |
| **TC-02** | AMR-NB Robustness | AMR-NB @ 12.2 kbps | Injected channel erasure rate = 1.0%; AWGN $\text{SNR} = 18\text{ dB}$ | Zero unrecoverable frames over 30,000 frames ($\implies P_{\text{FER}} \le 1.0 \times 10^{-4}$ at 95% Clopper-Pearson confidence). Zero payload corruption. |
| **TC-03** | Dynamic Codec Adaptation | AMR-NB stepped down from 12.2k to 4.75k | Mode switch occurs at Frame 100 | Link metric $M$ initiates automatic downshift to MCS 1 within 4 frames. Zero dropped IP packets. |
| **TC-04** | Wideband Cabled Gate | AMR-WB @ 12.65 kbps | Resampling $16\text{k} \to 8\text{k} \to 16\text{k}$; $\pm 80\text{ PPM}$ clock drift | DPLL maintains symbol lock. Measured Application Goodput $R_{\text{APP}} \ge 1,050\text{ bps}$ at MCS 3. |
| **TC-05** | VAD Characterization | 3GPP VAD Model 1 & 2 | Continuous voiced maintenance sequence | Measured over 10,000 independent 1-second trials. False DTX entry $P_{\text{DTX}} \le 0.01$. Comfort Noise insertion $P_{\text{CNG}} \le 0.005$. Reacquisition latency $\le 40\text{ ms}$. |
| **TC-06** | Burst Erasure Recovery | MCS 3 (160 ms frames) | 3 consecutive physical frame drops ($480\text{ ms}$ drop) | Selective Repeat ARQ initiates fast retransmission. Complete IP stream recovery within $\le 1,150\text{ ms}$ of drop start. Zero application errors. |
| **TC-07** | SOTP Mid-Stream Entry | AMR-WB @ 12.65 kbps | Receiver attaches at Frame 40 of a 100-symbol broadcast | Receiver syncs via Metadata Manifest within 8 frames. Object reconstructs with matching 32-byte BLAKE3 hash. |
| **TC-08a** | Real VoLTE Cellular Call (MCS 3) | Commercial Mobile VoLTE Network | Active 15-minute phone call between iPhone and Asterisk server | Interactive OpenSSH session maintained continuously. Keystroke round-trip confirmation time $\le 550\text{ ms}$. |
| **TC-08b** | Real Degraded / Free-Air Link (MCS 0/1) | Acoustic Speaker-to-Mic Air Gap / Degraded 3G Call | High ambient acoustic noise and multi-second frame periods | Mosh UDP terminal session maintained continuously. Predictive local echo renders keystrokes with $< 50\text{ ms}$ UI latency; remote screen converges within $1.5 \times T_{\text{frame}}$ after burst recovery. |

---

## 12. Direct Implementation Instructions

1. **Workspace Layout:** Scaffold a Cargo workspace with `crates/vradm-core` (`#![no_std]` core with `alloc` for initialization), `crates/vradm-server` (Asterisk daemon), `crates/vradm-cli` (diagnostics), and `tests/vocoder_bench`.
2. **Wire Format Verification:** Implement `crates/vradm-core/src/link/frame.rs` and verify bit-exact layout of both the 64-byte Canonical Data Frame and 16-byte Compact Control Frame.
3. **PLCP Bootstrap Engine:** Implement the Barker-13 dual-chirp generator, Golay $(24, 12)$ codec, and 2-FSK modulator. Ensure the receiver configures its demapper based strictly on the decoded PLCP beacon.
4. **Soft-Decision RS Decoder:** Implement Chase/GMD soft-decision decoding in `crates/vradm-core/src/fec/rs.rs`, verifying that flagging 16 confidence-tagged erasures reconstructs corrupted frames under $2t + e \le 16$.
5. **Continuous Codec Validation:** Execute `tests/vocoder_bench` continuously against `libopencore-amr` and `vo-amrwbenc` before packaging C-ABI exports. Ensure zero regressions across tests TC-01 through TC-07 prior to platform deployment.