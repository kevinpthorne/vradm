# Engineering Specification: Vocoder-Resilient Acoustic Data Modem (V-RADM)

**Document Version:** 3.4.0

**Status:** Implementation-Ready Engineering Baseline

**Primary Targets:** `vradm-core` (Rust C-ABI Engine), iOS 17+ Client Adapter, Linux / Asterisk 20+ PBX Gateway Daemon

---

## 1. System Architecture & Process Boundaries

V-RADM establishes a standard IPv4 point-to-point tunnel across speech-compressed cellular voice channels (VoLTE, VoNR, 3G AMR, carrier VoIP) and acoustic air gaps. It enables unmodified network applications—specifically **OpenSSH** and **Mosh (Mobile Shell)**—to function reliably under extreme bandwidth, latency, and transcoding constraints.

```
 ┌────────────────────────────────────────────────────────────────────────┐
 │                              iOS HOST                                  │
 │                                                                        │
 │  ┌──────────────────────────────────────────────────────────────────┐  │
 │  │ NetworkExtension Sandbox Process (PacketTunnelProvider)          │  │
 │  │  - Exposes virtual utun interface (MTU 128, IP 10.99.0.2)        │  │
 │  │  - Runs OpenSSH / Mosh client in user-space                      │  │
 │  └──────────────────────────────┬───────────────────────────────────┘  │
 │                                 │ IPC Bridge: App Group Shared         │
 │                                 │ Memory Ring Buffer / POSIX Socket    │
 │                                 ▼                                      │
 │  ┌──────────────────────────────────────────────────────────────────┐  │
 │  │ Main App Process (Foreground / Background Audio Entitlement)     │  │
 │  │  - Houses libvradm_core (IP Slicer, ARQ, RS FEC, PHY Modulator)  │  │
 │  │  - FIFO Decoupling Buffer (Resolves 5ms audio vs 4ms slot drift) │  │
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
 │                            │ Reassembled IP Packets                    │
 │                            ▼                                           │
 │  [OS Networking] ──> Linux Virtual Adapter (/dev/net/tun: vradm0)      │
 │                            │ Routed 10.99.0.1/24                       │
 │                            ▼                                           │
 │  [Host Daemons] ───> sshd (Port 22) & mosh-server (-p 60000:60010)     │
 └────────────────────────────────────────────────────────────────────────┘

```

### 1.1 The Parameter-Domain Physical Channel

Cellular speech vocoders (specifically ACELP variants: AMR, AMR-WB, EVS) do not preserve analog audio waveforms or inter-carrier phase relationships. They decompose audio into parametric features:

* **Linear Prediction (LP) Spectral Envelope:** Models the human vocal tract (quantized as Line Spectral Pairs/ISFs).
* **Adaptive Codebook:** Models fundamental pitch periodicity (sample-domain delay $T_0$) and pitch gain ($g_p$).
* **Algebraic Fixed Codebook:** Models the excitation residual using interleaved multi-pulse grids and gain ($g_c$).

V-RADM models the cellular voice channel as a **lossy parameter-quantization channel**. Demodulation relies on feature-distance estimation and soft-decision confidence decoding rather than hard-sliced phase boundaries.

### 1.2 Two-Channel PHY Decoupling

To eliminate circular boot-up dependencies—where a receiver must know the active Modulation and Coding Scheme (MCS) to demodulate the frame containing the MCS field—transmission is split into two asynchronous layers:

1. **PLCP Control Channel:** A low-rate, noncoherent control beacon transmitted using fixed pitch-frequency-shift keying. It announces transmitter state, active MCS, requested reverse MCS, and framing sequence.
2. **Payload Data Channel:** A variable-rate data carrier (MCS 0 through MCS 4) that immediately follows the PLCP beacon, formatted into immutable 64-byte physical frames.

---

## 2. Canonical Wire Format (64-Byte Physical PDU)

Every physical layer frame conforms to an immutable **64-byte (512-bit) layout**. All field offsets, checksum intervals, payload regions, and forward error correction parity blocks are derived mechanically from this structure.

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

### 2.1 Complete Byte Allocation Map

| Byte Range | Field Name | Width | Scope & Operational Description |
| --- | --- | --- | --- |
| `0x00..0x01` | `SYNC_WORD` | 16 bits | Frame delimiter marker: `0xD391`. Used for frame boundary confirmation after PLCP synchronization. |
| `0x02` | `CTRL` | 8 bits | Bit [7]: Mode (`0` = Standard IP Duplex, `1` = Simplex SOTP Broadcast)<br>

<br>Bits [6..4]: Active Frame MCS (`000` = MCS 0 .. `100` = MCS 4)<br>

<br>Bit [3]: TDD Turn Flag (`1` = Yield physical channel to peer)<br>

<br>Bits [2..0]: Wire Protocol Version (`001` = v3.4) |
| `0x03` | `SEQ` | 8 bits | Rolling transmit sequence number ($0\text{--}255$). |
| `0x04` | `ACK_BASE` | 8 bits | Cumulative ACK: highest contiguous peer frame sequence number received in-order. |
| `0x05` | `ACK_MAP` | 8 bits | Bit [7]: Feedback Type (`0` = Normal Bitmap, `1` = Urgent NACK)<br>

<br>Bits [6..0]: Selective ACK bitmap for frames `ACK_BASE + 1` through `ACK_BASE + 7`. |
| `0x06` | `PAYLOAD_LEN` | 8 bits | Valid payload bytes $k$, where $0 \le k \le 38$. |
| `0x07` | `HEADER_CRC8` | 8 bits | CRC-8-CCITT covering bytes `0x02..0x06` ($x^8 + x^2 + x + 1$). |
| `0x08..0x2D` | `PAYLOAD` | 38 bytes | Information payload: 1-byte IP fragmentation header + up to 37 bytes IP data (or 38 bytes SOTP slice). |
| `0x2E..0x2F` | `PAYLOAD_CRC16` | 16 bits | CRC-16-CCITT covering bytes `0x02..0x2D` ($x^{16} + x^{12} + x^5 + 1$). |
| `0x30..0x3F` | `RS_PARITY` | 16 bytes | Systematic $\text{RS}(64, 48)$ Galois field parity covering bytes `0x00..0x2F`. |

* **Total Protected Information Block:** Bytes `0x00..0x2F` = **48 bytes**.
* **Total FEC Parity Block:** Bytes `0x30..0x3F` = **16 bytes**.
* **Total Frame Length:** $48 + 16 = \mathbf{64\text{ bytes}}$ (512 bits).

---

## 3. Physical Layer (PHY) & Closed MCS Alphabets

Every MCS is mathematically closed:

$$R_{\text{raw}} = \text{Baud} \times \log_2(Y) = \text{Baud} \times X$$

$$T_{\text{frame}} = \frac{512\text{ bits}}{R_{\text{raw}}}$$

With a 1-byte fragmentation header, the net IP data payload per frame is strictly **37 bytes (296 bits)**:

$$R_{\text{net\_IP}} = \frac{296\text{ bits}}{T_{\text{frame}}}$$

```
 ┌────────────────────────────────────────────────────────────────────────────────────────┐
 │                          MECHANICALLY DERIVED MCS SPECIFICATIONS                       │
 ├──────┬──────────┬────────┬───────────┬─────────────┬──────────┬────────────┬───────────┤
 │ MCS  │ Target   │ Baud   │ Alphabet  │ Independent │ Bits/Sym │ Raw PHY    │ Net IP    │
 │      │ Channel  │ (Bd)   │ Size (Y)  │ Dims (Z)    │ (X)      │ Rate (bps) │ Goodput   │
 ├──────┼──────────┼────────┼───────────┼─────────────┼──────────┼────────────┼───────────┤
 │ 0    │ Free-Air │ 20     │ 16        │ 1           │ 4        │ 80.0       │ 46.25 bps │
 │ 1    │ NB-AMR   │ 50     │ 256       │ 3           │ 8        │ 400.0      │ 231.25 bps│
 │ 2    │ Balanced │ 100    │ 256       │ 4           │ 8        │ 800.0      │ 462.50 bps│
 │ 3*   │ WB-AMR   │ 200    │ 65,536    │ 8           │ 16       │ 3,200.0    │ 1,850 bps │
 │ 4    │ PCM/VoIP │ 250    │ 1,048,576 │ 10          │ 20       │ 5,000.0    │ 2,891 bps │
 └──────┴──────────┴────────┴───────────┴─────────────┴──────────┴────────────┴───────────┘
 *Note: MCS 3 is an experimentally gated high-rate mode; availability is controlled by TC-04.

```

### 3.1 Detailed Modulation & Speech-Atom Synthesis

#### MCS 0: Free-Air Acoustic TDD (High Reverberation)

* **Alphabet:** $Y = 16$ discrete fundamental pitch states ($F_0$).
* **Dimensions:** $Z = 1$ ($F_0 \in [120\text{ Hz}, 270\text{ Hz}]$ spaced uniformly at 10 Hz intervals).
* **Symbol Duration ($T_{\text{sym}}$):** $50.0\text{ ms}$ ($400\text{ samples at } 8\text{ kHz}$).
* **Multipath Guard Window:** First $15.0\text{ ms}$ discarded. Demodulation occurs over the terminal $35.0\text{ ms}$ via Normalized Cross-Correlation (NCCF).

#### MCS 1: Robust Narrowband Cellular (AMR-NB 4.75k–7.4k)

* **Alphabet:** $Y = 256$ joint speech-feature states ($8\text{ bits/symbol}$ at $50\text{ Bd}$).
* **Synthesis Formula:**

$$s(n) = e(n; T_0, \delta) * h_{\text{formant}}(n; F_1, F_2)$$


* **Dimensions ($Z = 3$):**
1. **Pitch Lag Delay ($T_0$, 3 bits):** Sample-domain integer delay $T_0 \in \{33, 38, 43, 49, 56, 64, 72, 80\}$ samples at 8 kHz ($F_0 \approx 100\text{ Hz to } 242\text{ Hz}$). The sample-domain lag $T_0$ is authoritative.
2. **Formant Resonances ($F_1, F_2$, 3 bits):** Second-order cascaded biquad IIR filter modeling 8 discrete vowel timbres:

$$\{(300, 900), (350, 1400), (450, 1100), (500, 1700), (600, 1200), (650, 1900), (750, 1300), (800, 2100)\}\text{ Hz}$$


3. **Algebraic Grid Offset ($\delta$, 2 bits):** Excitation pulse offset $\delta \in \{0, 1, 2, 3\}\text{ samples}$ aligned to 5 ms ACELP subframe boundaries.



#### MCS 2: Balanced Cellular (AMR-NB 10.2k/12.2k / AMR-WB 6.6k)

* **Alphabet:** $Y = 256$ states ($8\text{ bits/symbol}$ at $100\text{ Bd}$).
* **Dimensions:** $Z = 4$ harmonic subcarriers: $f_k \in \{600, 1000, 1400, 1800\}\text{ Hz}$.
* **Modulation:** Differential Quadrature Phase Shift Keying (DQPSK) per subcarrier (2 bits/carrier).

#### MCS 3: Wideband Cellular Cabled (AMR-WB 12.65k+ / VoLTE)

* **Status:** Experimentally gated coherent high-rate mode. Must earn promotion via TC-04.
* **Alphabet:** $Y = 65,536$ states ($16\text{ bits/symbol}$ at $200\text{ Bd}$).
* **Dimensions:** $Z = 8$ harmonic subcarriers: $f_k = k \cdot 200\text{ Hz}$ for $k \in \{3, 4, 5, 6, 7, 8, 9, 10\}$ ($600\text{ Hz to } 2000\text{ Hz}$).
* **Modulation:** DQPSK per subcarrier (2 bits/carrier). Synchronous with 5 ms ACELP subframes.

#### MCS 4: CP-OFDM Uncompressed Audio (G.711 / High-Rate VoIP)

* **Orthogonal FFT Grid Design:** To resolve the orthogonality bug and clear the 1,100 Hz / 2,100 Hz carrier tone-detector guard bands:
* Integration Window ($T_{\text{useful}}$): Exactly $3.5\text{ ms}$ ($N_{\text{fft}} = 28\text{ samples at } 8\text{ kHz}$, $56\text{ samples at } 16\text{ kHz}$).
* Subcarrier Spacing: $\Delta f = \frac{1}{T_{\text{useful}}} = \frac{8000}{28} = \mathbf{285.714\text{ Hz}}$.
* Cyclic Prefix ($T_{\text{cp}}$): Exactly $0.5\text{ ms}$ ($4\text{ samples at } 8\text{ kHz}$, $8\text{ samples at } 16\text{ kHz}$).
* Total Slot Duration: $T_{\text{slot}} = 3.5\text{ ms} + 0.5\text{ ms} = \mathbf{4.0\text{ ms}}$ ($250\text{ Bd}$).


* **Subcarrier Frequency Allocation ($Z = 10$):**
Indices $k \in \{2, 3, 5, 6, 7, 8, 9, 10, 11, 12\}$ ($k = 4$ omitted to clear 1,100 Hz):

$$\{571.4, 857.1, 1428.6, 1714.3, 2000.0, 2285.7, 2571.4, 2857.1, 3142.9, 3428.6\}\text{ Hz}$$



*Guard Band Verification:* The closest subcarriers to 1,100 Hz are 857.1 Hz ($-242.9\text{ Hz}$) and 1428.6 Hz ($+328.6\text{ Hz}$). The closest to 2,100 Hz are 2000.0 Hz ($-100.0\text{ Hz}$) and 2285.7 Hz ($+185.7\text{ Hz}$). Both fall outside standard telecom tone-detector detection bandwidths ($\pm 40\text{ Hz}$).

### 3.2 Signal Conditioning & Micro-Tremor Scoping

1. **Power Normalization:** RMS power is capped at $-6.0\text{ dBFS}$. Active carrier amplitudes scale as:

$$A_k(N) = A_{\text{ref}} \sqrt{\frac{8}{N}}$$



*Note:* The rate controller evaluates channel metrics directly rather than assuming higher power per carrier guarantees higher reliability.
2. **Micro-Tremor Scope:** The $4.0\text{ Hz}$ biological micro-jitter ($\pm 2.5\text{ Hz}$) is enabled **only for MCS 0 and MCS 1**. It is **strictly disabled for MCS 3 and MCS 4**, preventing phase disturbance in coherent DQPSK/QPSK demodulators. For MCS 2, micro-jitter is clamped to $\le \pm 0.5\text{ Hz}$.
3. **Evasion Caveat:** Tone-detector bypass and VAD non-suppression SHALL be characterized empirically per carrier (verified via TC-05); universal non-suppression is not guaranteed.

---

## 4. PLCP Control Channel & Dynamic Rate Adaptation

Every transmission burst is preceded by a **Physical Layer Convergence Protocol (PLCP) Control Beacon** modulated via ultra-robust 2-FSK / chirp signaling.

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|      BARKER-13 DUAL-CHIRP     |CUR_MCS|REQ_MCS|TX_PWR |BEAC_SEQ|
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|  BEACON_CRC8  |
+-+-+-+-+-+-+-+-+

```

### 4.1 Deterministic PLCP Parameters

* `PLCP_CHIP_RATE`: 200 chips/s ($5.0\text{ ms/chip}$).
* `PLCP_PREAMBLE_DURATION`: 13 chips $\times 5.0\text{ ms} = \mathbf{65.0\text{ ms}}$ (Hyperbolic pitch sweep: $600\text{ Hz} \leftrightarrow 1800\text{ Hz}$).
* `PLCP_GUARD_1`: $10.0\text{ ms}$ silence.
* `PLCP_HEADER_DATA`: 24 bits (`CUR_MCS` [3b], `REQ_MCS` [3b], `TX_PWR` [2b], `BEAC_SEQ` [8b], `BEACON_CRC8` [8b]).
* `PLCP_HEADER_MODULATION`: 2-FSK ($1200\text{ Hz}$ / $1600\text{ Hz}$) at $100\text{ Bd}$ ($10.0\text{ ms/bit}$). Duration = $24 \times 10.0\text{ ms} = \mathbf{240.0\text{ ms}}$.
* `PLCP_GUARD_2`: $10.0\text{ ms}$ silence.
* `PLCP_TOTAL_DURATION`: $65.0 + 10.0 + 240.0 + 10.0 = \mathbf{325.0\text{ ms}}$.
* **Transmission Cadence:** PLCP is transmitted **once per contiguous burst** (up to 8 consecutive data frames), not before every individual frame. In continuous streaming mode, a PLCP beacon is emitted every 16 frames or upon any MCS transition.

### 4.2 Rate Adaptation State Machine

Rate control is governed by an automated state machine computing a Link Quality Metric $M \in [0.0, 1.0]$:

$$M = 0.4 \cdot \bar{C}_{\text{sym}} + 0.3 \cdot (1.0 - P_{\text{FER}}) + 0.3 \cdot \left(1.0 - \frac{E_{\text{RS}}}{8}\right)$$

Where:

* $\bar{C}_{\text{sym}}$: Mean normalized symbol confidence ($0.0\text{--}1.0$) across the last 16 frames.
* $P_{\text{FER}}$: Frame Error Rate over a sliding 16-frame window.
* $E_{\text{RS}}$: Mean Reed-Solomon byte corrections per frame ($0\text{--}8$).
* **Promotion Rule:** Promote by $+1$ MCS level when $M \ge 0.85$ continuously for 16 consecutive frames. Dwell time at new MCS is fixed at a minimum of 32 frames before further promotion.
* **Demotion Rule:** Demote by $-1$ MCS level immediately if 2 CRC-16 failures occur within a 4-frame window, or if $M < 0.50$ for 2 consecutive frames.
* **Emergency Rollback:** If 4 consecutive frames fail preamble synchronization or CRC-8 checks, the engine drops immediately to `MCS_1` (Cabled) or `MCS_0` (Free-Air).

---

## 5. Soft-Decision Link Layer & Error Correction

```
 Incoming PCM Audio
         │
         ▼
 Goertzel Filter Bank / NCCF Pitch Correlator
         │
         ▼
 Confidence Metric Slicer (Metric bounds distance to decision boundary)
         │
         ▼
 8x8 Byte Block Deinterleaver (Permutes Bytes and Confidences Simultaneously)
         │
         ▼
 GMD Erasure Tagger:
 Flags e weakest bytes (with C_byte < 0.35) as Erasures (tests e in {16, 14, ..., 0})
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

### 5.1 Generalized Minimum Distance (GMD) RS Decoding

The channel decoder implements **soft-symbol confidence-directed GMD erasure-assisted RS decoding**:

1. **DQPSK Confidence Formulation (MCS 2, 3, 4):**
Let $\phi_q \in \{0, \frac{\pi}{2}, \pi, \frac{3\pi}{2}\}$ be the constellation phase targets.

$$d = \min_q \vert{}\text{wrap}(\Delta \phi - \phi_q)\vert{} \in \left[0, \frac{\pi}{4}\right]$$


$$C_i = 1.0 - \frac{d}{\pi / 4} \in [0.0, 1.0]$$



$C_i = 1.0$ at ideal phase targets, decreasing monotonically to $0.0$ at decision boundaries.
2. **Speech Feature Confidence Formulation (MCS 0, 1):**

$$C_i = \frac{\Lambda(\hat{S}) - \Lambda(S_{\text{second}})}{\Lambda(\hat{S})} \in [0.0, 1.0]$$



Where $\Lambda(S)$ is the normalized correlation peak or Goertzel magnitude of candidate speech atom $S$.
3. **Byte Confidence & Erasure Tagging:**
$C_{\text{byte}} = \min(C_{\text{bits}})$. Bytes with $C_{\text{byte}} < \Theta_{\text{erase}} = 0.35$ are sorted in ascending order of confidence.
4. **Algebraic Decoding Loop:**
The Berlekamp-Massey algorithm evaluates $2t + e \le 16$. The decoder tests trial erasure counts $e \in \{16, 14, 12, \dots, 0\}$ as an optimization schedule until decoding succeeds and passes `PAYLOAD_CRC16`.

### 5.2 Matrix Block Interleaver

* **Write Order:** Row-major ($0 \le r < 8, 0 \le c < 8 \implies \text{index} = r \times 8 + c$).
* **Read Order:** Column-major ($\text{index} = c \times 8 + r$).
* **Burst Dispersal Guarantee:** At MCS 3 ($3,200\text{ bps}$ raw), an 8-byte burst ($64\text{ bits} = 20.0\text{ ms}$) spanning a single column disperses into **exactly 1 byte error per row across all 8 rows**, within the RS error correction budget ($t \le 8$).

### 5.3 Dynamic Retransmission Timeout (RTO) Closed Formulation

Retransmission timers scale dynamically based on channel latency and physical frame transmission duration:

$$\text{RTO} = \text{SRTT} + \max(4 \cdot \text{RTTVAR}, T_{\text{frame}}) + T_{\text{margin}}(\text{Profile})$$

* $T_{\text{margin}}(\text{Cabled}) = \mathbf{150\text{ ms}}$ (cellular core network scheduling jitter).
* $T_{\text{margin}}(\text{Free-Air}) = \mathbf{500\text{ ms}}$ (acoustic travel time, reverberation decay, turn-around delay).
* **Initial RTO:** $1,000\text{ ms}$ (Cabled) / $9,000\text{ ms}$ (Free-Air).
* **Min / Max Bounds:** $\text{RTO}_{\min} = 300\text{ ms}$ (Cabled) / $6,500\text{ ms}$ (Free-Air); $\text{RTO}_{\max} = 30,000\text{ ms}$.
* **Karn’s Algorithm Mandate:** RTT estimation updates MUST NOT be calculated from retransmitted frames.

---

## 6. Network Tunneling & Application Layer

```
 ┌────────────────────────────────────────────────────────┐
 │           IP-OVER-VRADM DATAGRAM ENCAPSULATION         │
 └────────────────────────────────────────────────────────┘
  0                   1                   2                   3
  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
 +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
 |PKT_ID |FRAG_ID|M|R|             IP DATAGRAM FRAGMENT          |
 +-+-+-+-+-+-+-+-+-+-+                                           +
 |                                                               |
 |               Bytes 0x01..0x25 (Up to 37 Bytes Data)          |
 |                                                               |
 +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
  ◄──────────── 38-Byte Information Payload (0x08..0x2D) ────────►

```

### 6.1 IP Fragmentation Header (Byte 0x08)

* `PKT_ID` (Bits [7..5]): Rolling packet identifier ($0\text{--}7$).
* `FRAG_ID` (Bits [4..2]): Fragment index within packet ($0\text{--}7$).
* `MORE_FRAGS` (Bit [1]): `1` = More fragments follow; `0` = Final fragment.
* `RESERVED` (Bit [0]): Fixed to `0`.
* **MTU:** Fixed at **128 bytes** ($\lceil 128 / 37 \rceil = 4\text{ frames}$ per packet).

### 6.2 ARQ Protocol: ACK_BASE & ACK_MAP Semantics

* `ACK_BASE` (Byte 0x04): The highest contiguous sequence number received in-order by the peer.
* `ACK_MAP` (Byte 0x05):
* Bit [7]: Feedback Type (`0` = Normal Bitmap, `1` = Urgent NACK).
* Bits [6..0]: Bitmap indicating receipt of frames `ACK_BASE + 1` through `ACK_BASE + 7`.


* The transmitter slides its window to `ACK_BASE` and fast-retransmits any frame marked with a `0` bit in `ACK_MAP`.

### 6.3 OpenSSH Client Optimization (`~/.ssh/config`)

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

### 6.4 Mosh (Mobile Shell) Invocation

Mosh is the primary operational shell driver for MCS 0 and MCS 1. Its UDP encapsulation and speculative local echo eliminate typing latency and prevent TCP duplicate-retransmission storms over un-tunable iOS stacks:

```bash
mosh --ssh="ssh -F ~/.ssh/config" --server="mosh-server new -p 60000:60010" 10.99.0.1

```

---

## 7. Simplex Object Transfer Protocol (SOTP)

SOTP handles unidirectional, unacknowledged broadcast data drops (e.g., voicemail storage drops, automated audio recording drops). The SOTP descriptor occupies the 38-byte `PAYLOAD` field at physical offsets `0x08..0x2D`.

```
 ┌────────────────────────────────────────────────────────┐
 │           SOTP INLINE METADATA FRAME STRUCTURE         │
 └────────────────────────────────────────────────────────┘
  Offset (Bytes)  Field Name     Width    Description
  ---------------------------------------------------------------------------
  0x00            DESC_MARKER    uint8_t  Fixed identifier: 0xFE
  0x01            OBJECT_ID      uint8_t  Unique session object ID
  0x02..0x03      SOURCE_SYMS    uint16_t K' (Extended source symbols required)
  0x04..0x07      TOTAL_BYTES    uint32_t Uncompressed file byte length
  0x08..0x0B      TRUNC_BLAKE3   uint32_t Leading 4 bytes of BLAKE3 checksum
  0x0C..0x25      FOUNTAIN_DATA  uint8_t[26] Encoded repair payload slice

```

### 7.1 RaptorQ Parameter Definitions (RFC 6330)

* Symbol Size ($T$): **37 bytes**.
* Maximum Source Block Size ($K_{\max}$): 1,024 symbols ($\approx 37.8\text{ KB}$). Objects $> 37.8\text{ KB}$ are partitioned into $Z = \lceil \text{TotalBytes} / (K_{\max} \cdot T) \rceil$ source blocks.
* Extended Source Symbols ($K'$): Number of symbols in the extended source block.
* Full Authentication: The full 256-bit (32-byte) BLAKE3 hash is transmitted across dedicated metadata manifest frames. The 4-byte `TRUNC_BLAKE3` serves purely as an early collision filter.

---

## 8. Platform Integration & Audio Routing Topologies

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

### 8.1 iOS Process Boundary Architecture

`NEPacketTunnelProvider` runs inside a sandboxed Network Extension process, while `AVAudioEngine` runs inside the main application process.

* **IPC Bridge:** A lock-free circular ring buffer backed by POSIX shared memory (`mmap` over an App Group container file) links the two processes.
* **Latency Budget:** Ring buffer IPC latency is bounded to $\le 1.0\text{ ms}$.
* **Drift Decoupling FIFO:** A jitter buffer in `vradm-core` decouples the iOS $5.0\text{ ms}$ `IOBufferDuration` from MCS 4's $4.0\text{ ms}$ slot clock.

---

## 9. Complete C-ABI Interface (`vradm_core.h`)

All struct fields use explicit fixed-width integer types (`uint8_t`, `uint32_t`, `float`) to guarantee C-ABI stability across toolchains.

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
#define VRADM_MAX_PAYLOAD_SIZE   38
#define VRADM_MAX_IP_DATA_SIZE   37

typedef uint8_t vradm_mcs_t;
#define VRADM_MCS_0  0  // 20 Bd, Orthogonal Pitch Hop (80 bps raw / 46.25 bps net)
#define VRADM_MCS_1  1  // 50 Bd, Joint Pitch/Formant/Pulse (400 bps raw / 231.25 bps net)
#define VRADM_MCS_2  2  // 100 Bd, 4-Carrier DQPSK (800 bps raw / 462.50 bps net)
#define VRADM_MCS_3  3  // 200 Bd, 8-Carrier DQPSK (3200 bps raw / 1850.0 bps net)
#define VRADM_MCS_4  4  // 250 Bd, 10-Carrier CP-OFDM (5000 bps raw / 2890.6 bps net)

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

/* --- Engine Lifecycle Management --- */
vradm_engine_t* vradm_create(const vradm_config_t* config);
void            vradm_destroy(vradm_engine_t* engine);
void            vradm_reset(vradm_engine_t* engine);

/* --- Real-Time Audio Streaming I/O (Zero Dynamic Allocations) --- */
void   vradm_process_audio(vradm_engine_t* engine, const int16_t* in_samples, size_t count);
size_t vradm_generate_audio(vradm_engine_t* engine, int16_t* out_samples, size_t max_count);

/* --- Mode A: IP Packet Datagram Stream (TUN Interface) --- */
int32_t vradm_write_ip_packet(vradm_engine_t* engine, const uint8_t* packet, size_t len);
int32_t vradm_poll_ip_packet(vradm_engine_t* engine, uint8_t* out_packet, size_t max_len);

/* --- Mode B: SOTP Simplex Object Transfer --- */
int32_t vradm_sotp_tx_init(vradm_engine_t* engine, const uint8_t* payload, size_t len, float redundancy_factor);
int32_t vradm_sotp_rx_poll(vradm_engine_t* engine, size_t* out_collected_symbols, size_t* out_required_symbols);
int32_t vradm_sotp_rx_fetch(vradm_engine_t* engine, uint8_t* out_buf, size_t max_len, uint8_t out_hash[32]);

/* --- Telemetry & Link Status --- */
void vradm_get_telemetry(const vradm_engine_t* engine, vradm_telemetry_t* out_telem);

#ifdef __cplusplus
}
#endif

#endif // VRADM_CORE_H

```

---

## 10. Verification Matrix & Acceptance Test Criteria

Engine builds must pass all milestones under the automated codec-in-the-loop harness before field deployment.

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
| **TC-01** | Math Loopback | In-memory loopback | Zero noise, synchronous clock | Zero observed bit errors over $10^7$ tested bits ($P_e < 10^{-7}$). Zero RS corrections. |
| **TC-02** | AMR-NB Robustness | AMR-NB @ 12.2 kbps | Injected channel erasure rate = 1.0%; AWGN $\text{SNR} = 18\text{ dB}$ | Residual unrecoverable PHY frame rate $P_{\text{FER}} \le 1.0 \times 10^{-4}$. Zero uncorrected IP payload errors over $10^6$ bits. |
| **TC-03** | Dynamic Codec Adaptation | AMR-NB stepped down from 12.2k to 4.75k | Mode switch occurs at Frame 100 | Link metric $M$ initiates automatic downshift to MCS 1 within 4 frames. Zero dropped IP packets. |
| **TC-04** | Wideband Cabled Gate | AMR-WB @ 12.65 kbps | Resampling $16\text{k} \to 8\text{k} \to 16\text{k}$; $\pm 80\text{ PPM}$ clock drift | DPLL maintains symbol lock. Measured application goodput $\ge 1,450\text{ bps}$ at MCS 3. |
| **TC-05** | VAD Characterization | 3GPP VAD Model 1 & 2 | Continuous voiced maintenance sequence | Measured over 10,000 independent 1-second trials. False DTX entry $P_{\text{DTX}} \le 0.01$. Comfort Noise insertion $P_{\text{CNG}} \le 0.005$. Reacquisition latency $\le 40\text{ ms}$. |
| **TC-06** | Burst Erasure Recovery | MCS 3 (160 ms frames) | 3 consecutive physical frame drops ($480\text{ ms}$ drop) | Selective Repeat ARQ triggers fast retransmit via `ACK_MAP`. Complete IP packet stream recovery within $850\text{ ms}$. |
| **TC-07** | SOTP Mid-Stream Entry | AMR-WB @ 12.65 kbps | Receiver attaches at Frame 40 of a 100-symbol broadcast | Receiver syncs via Inline Descriptor within 8 frames. Object reconstructs with matching BLAKE3 checksum. |
| **TC-08a** | Real VoLTE Cellular Call (MCS 3) | Commercial Mobile VoLTE Network | Active 15-minute phone call between iPhone and Asterisk server | Interactive OpenSSH session maintained continuously. Keystroke round-trip echo time $\le 550\text{ ms}$. |
| **TC-08b** | Real Degraded / Free-Air Link (MCS 0/1) | Acoustic Speaker-to-Mic Air Gap / Degraded 3G Call | High ambient acoustic noise and multi-second frame periods | Mosh UDP terminal session maintained continuously. Speculative local echo displays keystrokes with $< 50\text{ ms}$ perceived latency; terminal resynchronizes within $1.5 \times T_{\text{frame}}$ after frame drops. |

---

## 11. Direct Implementation Instructions

1. **Workspace Layout:** Scaffold a Cargo workspace with `crates/vradm-core` (`#![no_std]` core with `alloc` for initialization), `crates/vradm-server` (Asterisk daemon), `crates/vradm-cli` (diagnostics), and `tests/vocoder_bench`.
2. **Wire Format Verification:** Implement `crates/vradm-core/src/link/frame.rs` and verify bit-exact layout of the canonical 64-byte frame against Section 2.
3. **PLCP Bootstrap Engine:** Implement the Barker-13 dual-chirp generator and NCCF receiver before finalizing higher-order modulation algorithms. Ensure the receiver configures its demapper based strictly on the decoded PLCP beacon.
4. **Soft-Decision RS Decoder:** Implement Chase/GMD soft-decision decoding in `crates/vradm-core/src/fec/rs.rs`, verifying that flagging 16 confidence-tagged erasures reconstructs corrupted frames under $2t + e \le 16$.
5. **Continuous Codec Validation:** Execute `tests/vocoder_bench` continuously against `libopencore-amr` and `vo-amrwbenc` before packaging C-ABI exports. Ensure zero regressions across tests TC-01 through TC-07 prior to platform deployment.