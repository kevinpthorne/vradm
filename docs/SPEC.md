# Engineering Specification: Vocoder-Resilient Acoustic Data Modem (V-RADM)

**Document Version:** 3.3.0

**Status:** Comprehensive Baseline Specification (Final Review Candidate)

**Primary Targets:** `vradm-core` (Rust C-ABI Shared Engine), iOS Client Adapter, Linux/Asterisk 20+ PBX Gateway Daemon

---

## 1. System Architecture & Operating Principles

V-RADM establishes a standard IPv4 point-to-point tunnel across speech-compressed cellular voice channels (VoLTE, VoNR, 3G AMR, carrier VoIP) and acoustic air gaps. It enables unmodified network applications—specifically **OpenSSH** and **Mosh (Mobile Shell)**—to function reliably under extreme bandwidth, latency, and transcoding constraints.

```
 ┌────────────────────────────────────────────────────────────────────────┐
 │                           CLIENT NODE (iOS)                            │
 │                                                                        │
 │  [Applications] ──> OpenSSH / Mosh Client (Unmodified User-Space)      │
 │                            │ Native IP Packets (MTU 128)               │
 │                            ▼                                           │
 │  [OS Networking] ──> NetworkExtension (PacketTunnelProvider / utun)    │
 │                            │ Raw IP Datagrams                          │
 │                            ▼                                           │
 │  [vradm-core] ─────> IP Slicer ──> Selective Repeat ARQ ──> GMD-RS(64,48)
 │                            │ Modulated 8/16 kHz Linear PCM             │
 │                            ▼                                           │
 │  [Audio I/O] ──────> Topology A (TRRS/USB-C DAC) or Topology C (Calls) │
 └────────────────────────────┬───────────────────────────────────────────┘
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
 │  [Host Daemons] ───> sshd (Port 22) & mosh-server (UDP 60000-60010)    │
 └────────────────────────────────────────────────────────────────────────┘

```

### 1.1 The Parameter-Domain Physical Channel

Cellular speech vocoders (specifically ACELP variants: AMR, AMR-WB, EVS) do not preserve analog audio waveforms or inter-carrier phase relationships. They decompose audio into parametric features:

* **Linear Prediction (LP) Spectral Envelope:** Models the human vocal tract (quantized as Line Spectral Pairs/ISFs).
* **Adaptive Codebook:** Models fundamental pitch periodicity ($T_0$) and pitch gain ($g_p$).
* **Algebraic Fixed Codebook:** Models the excitation residual using interleaved multi-pulse grids and gain ($g_c$).

V-RADM discards linear waveform modulation (such as standard wideband OFDM) over cellular voice channels. It synthesizes audio using **voiced speech invariants** (fundamental pitch contours, multi-harmonic resonance envelopes, and subframe-aligned algebraic excitations). Receiver demodulation extracts parameter distances, generating soft confidence metrics for error correction.

### 1.2 The Two-Channel PHY Architecture

To eliminate circular boot-up dependencies—where a receiver must know the Modulation and Coding Scheme (MCS) to demodulate the frame containing the MCS header—V-RADM divides transmission into two asynchronous layers:

1. **PLCP Control Channel:** A low-rate, noncoherent control beacon transmitted using fixed pitch-frequency-shift keying. It announces transmitter state, current MCS, requested reverse MCS, and framing sequence.
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
|    ACK_MAP    |  PAYLOAD_LEN  |  HEADER_CRC8  |               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+               +
|                                                               |
|        INFORMATION PAYLOAD DATA (Bytes 0x07..0x2D, 39 Bytes)  |
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
| `0x00..0x01` | `SYNC_WORD` | 16 bits | Fixed synchronization word: `0xD391` (Peak periodic sidelobe $\le 2$). |
| `0x02` | `CTRL` | 8 bits | Bit [7]: Mode (`0` = Standard IP Duplex, `1` = Simplex SOTP Broadcast)<br>

<br>Bits [6..4]: Active Frame MCS (`000` = MCS 0 .. `100` = MCS 4)<br>

<br>Bit [3]: TDD Turn Flag (`1` = Yield physical channel to peer)<br>

<br>Bits [2..0]: Wire Protocol Version (`001` = v3.3) |
| `0x03` | `SEQ` | 8 bits | Rolling transmit sequence number ($0\text{--}255$). |
| `0x04` | `ACK_MAP` | 8 bits | Bit [7]: Feedback Type (`0` = Cumulative ACK, `1` = Selective NACK)<br>

<br>Bits [6..0]: 7-frame selective bitmap covering frames $\text{SEQ}-7$ through $\text{SEQ}-1$. |
| `0x05` | `PAYLOAD_LEN` | 8 bits | Valid payload length $k$, where $0 \le k \le 39$. |
| `0x06` | `HEADER_CRC8` | 8 bits | CRC-8-CCITT covering bytes `0x02..0x05` ($x^8 + x^2 + x + 1$). |
| `0x07..0x2D` | `PAYLOAD` | 39 bytes | Encapsulated IP packet slice or SOTP block. Padded with `0x00` if $k < 39$. |
| `0x2E..0x2F` | `PAYLOAD_CRC16` | 16 bits | CRC-16-CCITT covering bytes `0x02..0x2D` ($x^{16} + x^{12} + x^5 + 1$). |
| `0x30..0x3F` | `RS_PARITY` | 16 bytes | Systematic $\text{RS}(64, 48)$ Galois field parity covering bytes `0x00..0x2F`. |

* **Total Protected Information Block:** Bytes `0x00..0x2F` = **48 bytes**.
* **Total FEC Parity Block:** Bytes `0x30..0x3F` = **16 bytes**.
* **Total Frame Length:** $48 + 16 = \mathbf{64\text{ bytes}}$ (512 bits).

---

## 3. Physical Layer (PHY) & Closed MCS Alphabets

Every Modulation and Coding Scheme (MCS) is bounded by its alphabet cardinality $Y$, independent dimensions $Z$, and bits per symbol $X = \log_2(Y)$. Raw physical throughput is:

$$R_{\text{raw}} = \text{Baud} \times \log_2(Y) = \text{Baud} \times X$$

The physical frame duration is:

$$T_{\text{frame}} = \frac{512\text{ bits}}{R_{\text{raw}}}$$

The net application-layer IP goodput (for maximum payload $k = 39\text{ bytes} = 312\text{ bits}$) is:

$$R_{\text{net}} = \frac{312\text{ bits}}{T_{\text{frame}}}$$

```
 ┌────────────────────────────────────────────────────────────────────────────────────────┐
 │                          MECHANICALLY DERIVED MCS SPECIFICATIONS                       │
 ├──────┬──────────┬────────┬───────────┬─────────────┬──────────┬────────────┬───────────┤
 │ MCS  │ Target   │ Baud   │ Alphabet  │ Independent │ Bits/Sym │ Raw PHY    │ Net IP    │
 │      │ Channel  │ (Bd)   │ Size (Y)  │ Dims (Z)    │ (X)      │ Rate (bps) │ Goodput   │
 ├──────┼──────────┼────────┼───────────┼─────────────┼──────────┼────────────┼───────────┤
 │ 0    │ Free-Air │ 20     │ 16        │ 1           │ 4        │ 80.0       │ 48.75 bps │
 │ 1    │ NB-AMR   │ 50     │ 256       │ 3           │ 8        │ 400.0      │ 243.8 bps │
 │ 2    │ Balanced │ 100    │ 256       │ 4           │ 8        │ 800.0      │ 487.5 bps │
 │ 3    │ WB-AMR   │ 200    │ 65,536    │ 8           │ 16       │ 3,200.0    │ 1,950 bps │
 │ 4    │ PCM/VoIP │ 250    │ 1,048,576 │ 10          │ 20       │ 5,000.0    │ 3,047 bps │
 └──────┴──────────┴────────┴───────────┴─────────────┴──────────┴────────────┴───────────┘

```

### 3.1 Modulation Profiles

#### MCS 0: Free-Air Acoustic TDD (High Reverberation)

* **Alphabet:** $Y = 16$ discrete fundamental pitch states ($F_0$).
* **Dimensions:** $Z = 1$ ($F_0 \in [120\text{ Hz}, 270\text{ Hz}]$ spaced uniformly at 10 Hz intervals).
* **Symbol Duration ($T_{\text{sym}}$):** $50.0\text{ ms}$ ($400\text{ samples at } 8\text{ kHz}, 800\text{ samples at } 16\text{ kHz}$).
* **Multipath Guard Window:** First $15.0\text{ ms}$ discarded by receiver. Integration occurs over the terminal $35.0\text{ ms}$ via Normalized Cross-Correlation (NCCF).
* **Frame Transmission Time ($T_{\text{frame}}$):** $\frac{512\text{ bits}}{4\text{ bits/sym}} \times 50.0\text{ ms} = \mathbf{6,400.0\text{ ms}}$.

#### MCS 1: Robust Narrowband Cellular (AMR-NB 4.75k–7.4k)

* **Alphabet:** $Y = 256$ joint speech feature states.
* **Dimensions:** $Z = 3$:
1. Pitch lag: 8 coarse fundamental frequency bins ($F_0 \in [100\text{ Hz}, 240\text{ Hz}]$, 3 bits).
2. Formant envelope: 8 synthetic vowel formant pairs ($F_1 \in [300, 800\text{ Hz}], F_2 \in [900, 2200\text{ Hz}]$, 3 bits).
3. Excitation alignment: 4 algebraic pulse grid offsets mapped to subframe boundaries (2 bits).


* **Symbol Duration ($T_{\text{sym}}$):** $20.0\text{ ms}$ ($50\text{ Bd}$, matching the 20 ms ACELP frame).
* **Frame Transmission Time ($T_{\text{frame}}$):** $\frac{512\text{ bits}}{8\text{ bits/sym}} \times 20.0\text{ ms} = \mathbf{1,280.0\text{ ms}}$.

#### MCS 2: Balanced Cellular (AMR-NB 10.2k/12.2k / AMR-WB 6.6k)

* **Alphabet:** $Y = 256$ multi-carrier phase states.
* **Dimensions:** $Z = 4$ harmonic subcarriers: $f_k \in \{600, 1000, 1400, 1800\}\text{ Hz}$.
* **Modulation:** Differential Quadrature Phase Shift Keying (DQPSK) per subcarrier (2 bits/carrier).
* **Symbol Duration ($T_{\text{sym}}$):** $10.0\text{ ms}$ ($100\text{ Bd}$, spanning two 5 ms subframes).
* **Frame Transmission Time ($T_{\text{frame}}$):** $\frac{512\text{ bits}}{8\text{ bits/sym}} \times 10.0\text{ ms} = \mathbf{640.0\text{ ms}}$.

#### MCS 3: Wideband Cellular Direct Cabled (AMR-WB 12.65k+ / VoLTE)

* **Alphabet:** $Y = 65,536$ states.
* **Dimensions:** $Z = 8$ harmonic subcarriers: $f_k = k \cdot 200\text{ Hz}$ for $k \in \{3, 4, 5, 6, 7, 8, 9, 10\}$ ($600\text{ Hz to } 2000\text{ Hz}$).
* **Modulation:** DQPSK per subcarrier (2 bits/carrier).
* **Symbol Duration ($T_{\text{sym}}$):** $5.0\text{ ms}$ ($200\text{ Bd}$, synchronous with 5 ms ACELP subframes).
* **Frame Transmission Time ($T_{\text{frame}}$):** $\frac{512\text{ bits}}{16\text{ bits/sym}} \times 5.0\text{ ms} = \mathbf{160.0\text{ ms}}$.

#### MCS 4: Uncompressed Audio / Carrier VoIP (G.711 / Clear Channel)

* **Alphabet:** $Y = 1,048,576$ states.
* **Dimensions:** $Z = 10$ orthogonal subcarriers ($f_k = 400\text{ Hz} + k \cdot 225\text{ Hz}$ for $k \in [0..9]$).
* **Modulation:** QPSK with Cyclic Prefix (CP-OFDM).
* **Timing Definition:**
* Useful FFT Integration Window ($T_{\text{useful}}$): Exactly $3.5\text{ ms}$ (28 samples at 8 kHz, 56 samples at 16 kHz).
* Cyclic Prefix ($T_{\text{cp}}$): Exactly $0.5\text{ ms}$ (4 samples at 8 kHz, 8 samples at 16 kHz).
* Total Slot Duration ($T_{\text{slot}}$): $T_{\text{useful}} + T_{\text{cp}} = 3.5\text{ ms} + 0.5\text{ ms} = \mathbf{4.0\text{ ms}}$ ($250\text{ Bd}$).


* **Frame Transmission Time ($T_{\text{frame}}$):** $\frac{512\text{ bits}}{20\text{ bits/sym}} \times 4.0\text{ ms} = \mathbf{102.4\text{ ms}}$.

### 3.2 Power Normalization & Edge Smoothing

1. **Constant-Power Constraint:** Total output RMS power is clamped at $-6.0\text{ dBFS}$. When downshifting active carriers from $N_1$ to $N_2$, per-carrier amplitude scales as:

$$A_k(N_2) = A_{\text{ref}} \sqrt{\frac{8}{N_2}}$$



Halving carriers from 8 (MCS 3) to 4 (MCS 2) increases power per remaining carrier by $+3.01\text{ dB}$, maximizing receiver SNR without digital clipping.
2. **Sample-Rate Invariant Smoothing Window:** Boundary smoothing transitions are fixed at **$0.5\text{ ms}$** across all sampling rates:
* At $F_s = 8,000\text{ Hz}$: Window length $L = 4\text{ samples}$.
* At $F_s = 16,000\text{ Hz}$: Window length $L = 8\text{ samples}$.
Raised-cosine edge envelope $w(n)$ applies to the first and last $L$ samples:

$$w(n) = \frac{1}{2}\left[1 - \cos\left(\frac{\pi (n + 0.5)}{L}\right)\right] \quad \text{for } n \in [0, L-1]$$





### 3.3 Carrier Tone-Detector & VAD Evasion

Carrier media gateways actively scan voice channels for legacy fax/modem signatures (e.g., V.25 $2100\text{ Hz}$ answering tones or T.30 $1100\text{ Hz}$ fax beeps) to switch lines into T.38 relay or G.711 clear-channel mode:

1. **Spectral Guard Band:** V-RADM subcarriers strictly exclude $1100\text{ Hz}$ and $2100\text{ Hz}$. Subcarrier banks are constrained to integer multiples of 200 Hz or odd-spaced bins.
2. **Biological Micro-Tremor:** All synthesized harmonic carriers incorporate a continuous $4.0\text{ Hz}$ sinusoidal micro-jitter ($\pm 2.5\text{ Hz}$ deviation). This mimics human vocal-cord tremor, evading carrier hardware tone detectors while remaining well within Goertzel filter integration bins.
3. **Voiced Maintenance Sequence:** During TDD channel holds, idle states, or ARQ stalls, the transmitter emits a continuous $-26\text{ dBFS}$ synthetic voiced sound ($F_0(t) = 130\text{ Hz} + 15\sin(2\pi \cdot 2.5 t)\text{ Hz}$, filtered through formants $F_1 = 550\text{ Hz}, F_2 = 1500\text{ Hz}$) to prevent carrier Voice Activity Detectors (VAD) from clamping the line or injecting Comfort Noise (CNG).

---

## 4. PLCP Control Channel & Dynamic Rate Adaptation

The receiver never inspects the data frame to determine its modulation scheme. Every transmission burst is preceded by a **Physical Layer Convergence Protocol (PLCP) Control Beacon** modulated via ultra-robust MCS 0 signaling.

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|      BARKER-13 DUAL-CHIRP     |CUR_MCS|REQ_MCS|TX_PWR |BEAC_SEQ|
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|  BEACON_CRC8  |
+-+-+-+-+-+-+-+-+

```

### 4.1 PLCP Beacon Structure

* **Preamble:** 13-chip Barker sequence (`+1 +1 +1 +1 +1 -1 -1 +1 +1 -1 +1 -1 +1`) modulated as hyperbolic pitch sweeps ($600\text{ Hz} \leftrightarrow 1800\text{ Hz}$).
* `CUR_MCS` (3 bits): The active MCS used to modulate the immediately following payload data frame.
* `REQ_MCS` (3 bits): The rate recommendation requested for the reverse link.
* `TX_PWR` (2 bits): Transmit power adjustment request (`00` = Hold, `01` = $+2\text{ dB}$, `10` = $-2\text{ dB}$, `11` = Reserved).
* `BEAC_SEQ` (8 bits): Rolling beacon identifier.
* `BEACON_CRC8` (8 bits): CRC-8 covering the beacon fields.

### 4.2 Rate Adaptation State Machine

Rate control is governed by an automated state machine computing a Link Quality Metric $M \in [0.0, 1.0]$:

$$M = 0.4 \cdot \bar{C}_{\text{sym}} + 0.3 \cdot (1.0 - P_{\text{FER}}) + 0.3 \cdot \left(1.0 - \frac{E_{\text{RS}}}{8}\right)$$

Where:

* $\bar{C}_{\text{sym}}$: Mean normalized symbol confidence ($0.0\text{--}1.0$) across the last 16 frames.
* $P_{\text{FER}}$: Frame Error Rate over a sliding 16-frame window.
* $E_{\text{RS}}$: Mean Reed-Solomon byte corrections per frame ($0\text{--}8$).

```
 ┌─────────────────┐       M >= 0.85 for 16 frames        ┌─────────────────┐
 │                 ├─────────────────────────────────────►│                 │
 │  Current MCS N  │                                      │  Promote: N+1   │
 │                 │◄─────────────────────────────────────┤                 │
 └────────┬────────┘             32-Frame Dwell           └─────────────────┘
          │
          │ M < 0.50 for 2 frames OR 2 consecutive CRC-16 failures
          ▼
 ┌─────────────────┐
 │  Demote: N-1    │
 └─────────────────┘

```

* **Startup Defaults:** Direct Cabled links boot at `MCS_3`; Free-Air links boot at `MCS_0`.
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
 Confidence Metric Computer: C_i = tanh(|Delta phi| / sigma_noise)
         │
         ▼
 8x8 Byte Block Deinterleaver (Permutes Bytes and Confidences Simultaneously)
         │
         ▼
 GMD / Chase Erasure Tagger:
 Flags e weakest bytes with C_i < 0.35 as Erasures (e <= 16, e even)
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

### 5.1 Matrix Block Interleaver Definition

The $8 \times 8$ byte block interleaver permutes the 64-byte frame across an 8-row by 8-column matrix to eliminate burst susceptibility:

* **Interleaver Write Order (Input):** Row-major ($0 \le r < 8, 0 \le c < 8 \implies \text{index} = r \times 8 + c$).
* **Interleaver Read Order (Output to PHY):** Column-major ($\text{index} = c \times 8 + r$).
* **Dispersal Verification:** At MCS 3 ($3,200\text{ bps}$ raw), a contiguous $20.0\text{ ms}$ drop corresponds to exactly:

$$\text{Burst Size} = 3,200\text{ bps} \times 0.020\text{ s} = 64\text{ bits} = \mathbf{8\text{ contiguous bytes}}$$



Because the transmitter reads out column-major, those 8 contiguous bytes constitute **one full column ($c$)**. When written column-major and read row-major at the receiver, the 8 lost bytes disperse into **exactly 1 byte per row across all 8 rows**, well within the $\text{RS}(64, 48)$ capacity of $t = 8$ correctable bytes per block.

### 5.2 Generalized Minimum Distance (GMD) Reed-Solomon Decoding

* **Field Arithmetic:** $\text{RS}(64, 48)$ over Galois Field $\text{GF}(2^8)$ with primitive polynomial $p(x) = x^8 + x^4 + x^3 + x^2 + 1$ (`0x11D`) and generator $g(x) = \prod_{j=0}^{15} (x - \alpha^j)$ with $\alpha = 0\text{x}02$.
* **Confidence Slicing:** For each byte, confidence $C_{\text{byte}}$ equals the minimum bit confidence within that byte. Bytes with $C_{\text{byte}} < \Theta_{\text{erase}} = 0.35$ are sorted by ascending confidence.
* **Decoding Invariant:** The algebraic decoder evaluates:

$$2t + e \le 16$$



Where $e$ is the number of flagged erasures and $t$ is the number of unflagged errors. Setting $e = 16$ erasures allows complete mathematical recovery when zero unflagged errors exist, doubling the correction margin over conventional hard-decision decoding.

### 5.3 Dynamic Retransmission Timeout (RTO) Closed Formulation

Retransmission timers scale dynamically based on channel latency and physical frame transmission duration:

$$\text{RTO} = \text{SRTT} + \max(4 \cdot \text{RTTVAR}, T_{\text{frame}}) + T_{\text{margin}}(\text{Profile})$$

* **Profile 1 (Direct Cabled Full-Duplex):** $T_{\text{margin}} = \mathbf{150\text{ ms}}$ (cellular core scheduling jitter).

$$\text{RTO}_{\text{cabled}} = 250\text{ ms} + \max(100\text{ ms}, 160\text{ ms}) + 150\text{ ms} = \mathbf{560\text{ ms}}$$


* **Profile 2 (Free-Air Acoustic Half-Duplex TDD):** $T_{\text{margin}} = \mathbf{500\text{ ms}}$ (acoustic travel time, reverberation decay, turn-around delay).

$$\text{RTO}_{\text{free-air}} = 1,200\text{ ms} + \max(400\text{ ms}, 6,400\text{ ms}) + 500\text{ ms} = \mathbf{8,100\text{ ms}}$$



---

## 6. Network Tunneling & Application Optimization

V-RADM encapsulates standard IPv4 datagrams. To maximize airtime efficiency, the 39-byte information payload area carries raw, unpadded IP packet fragments directly. Redundant link-layer AEAD encryption is eliminated; end-to-end security is delegated to **OpenSSH** or **Mosh**.

```
 ┌────────────────────────────────────────────────────────┐
 │           IP-OVER-VRADM DATAGRAM ENCAPSULATION         │
 └────────────────────────────────────────────────────────┘
  0                   1                   2                   3
  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
 +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
 |FRAG_ID|OFFSET |                      IP DATAGRAM FRAGMENT     |
 +-+-+-+-+-+-+-+-+                                               +
 |                                                               |
 |               Bytes 0x01..0x26 (Up to 38 Bytes Fragment)      |
 |                                                               |
 +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
  ◄──────────── 39-Byte Information Payload (0x07..0x2D) ────────►

```

### 6.1 IP Fragmentation & MTU

* Virtual network adapters (`vradm0` on Linux, `NEPacketTunnelProvider` on iOS) enforce **$\text{MTU} = 128\text{ bytes}$**.
* A 128-byte IP packet splits across 4 physical frames ($38 + 38 + 38 + 14 = 128\text{ bytes}$).
* Link-layer Selective Repeat ARQ ensures 100% in-order, reliable delivery of fragments. The virtual adapter reassembles the fragments and injects the complete IP packet into the operating system network stack.

### 6.2 Host TCP Kernel Tuning (`/etc/sysctl.d/99-vradm.conf`)

```ini
# Disable window scaling to lock small TCP buffer footprint
net.ipv4.tcp_window_scaling = 0

# Restrict buffer limits to prevent bufferbloat on sub-2kbps links
net.ipv4.tcp_rmem = 256 512 1024
net.ipv4.tcp_wmem = 256 512 1024

# Clamp initial congestion window to 1 packet
# Enforce via route: ip route add 10.99.0.0/24 dev vradm0 initcwnd 1 initrwnd 1 rto_min 1000ms

```

### 6.3 OpenSSH Optimization Profile (`~/.ssh/config`)

By pruning the OpenSSH cipher proposal list, `SSH_MSG_KEXINIT` payload size is reduced from $\approx 3.5\text{ KB}$ down to **$\approx 1,160\text{ bytes}$**, enabling handshakes to complete in under 5 seconds over MCS 3:

```text
Host vradm-gw
    HostName 10.99.0.1
    Port 22
    User admin
    
    # Restrict KEX proposal to single algorithms
    KexAlgorithms curve25519-sha256
    Ciphers chacha20-poly1305@openssh.com
    MACs none
    HostKeyAlgorithms ssh-ed25519
    
    # Pre-seed verified host keys to avoid interactive prompt stalls
    StrictHostKeyChecking yes
    UserKnownHostsFile ~/.ssh/known_hosts
    
    # Strip unnecessary subsystem and agent bloat
    ForwardAgent no
    ForwardX11 no
    ServerAliveInterval 30
    ServerAliveCountMax 4
    TCPKeepAlive no
    Compression yes

```

### 6.4 Mosh (Mobile Shell) Over UDP Configuration

For highly degraded or acoustic links (MCS 0 / MCS 1), **Mosh** is the primary terminal driver. Mosh encapsulates encrypted terminal state diffs in tiny UDP datagrams, providing speculative local echo:

* **Client Command:**
```bash
mosh --ssh="ssh -F ~/.ssh/config" --server="mosh-server" 10.99.0.1

```


* **Behavior over V-RADM:** Keystrokes render instantly in the UI with 0 ms perceived latency. Lost frames do not stall the terminal cursor; Mosh resynchronizes screen state upon receiving the next frame.

---

## 7. Simplex Object Transfer Protocol (SOTP)

SOTP handles unidirectional, unacknowledged broadcast data drops (e.g., voicemail storage drops, automated audio recording drops).

```
 ┌────────────────────────────────────────────────────────┐
 │           SOTP INLINE METADATA FRAME STRUCTURE         │
 └────────────────────────────────────────────────────────┘
  Offset (Bytes)  Field Name     Width    Description
  ---------------------------------------------------------------------------
  0x00            DESC_MARKER    uint8_t  Fixed identifier: 0xFE
  0x01            OBJECT_ID      uint8_t  Unique session object ID
  0x02..0x03      TOTAL_BLOCKS   uint16_t K' (Extended source symbols required)
  0x04..0x07      TOTAL_BYTES    uint32_t Uncompressed file byte length
  0x08..0x0B      TRUNC_BLAKE3   uint32_t Leading 4 bytes of BLAKE3 hash
  0x0C..0x26      FOUNTAIN_DATA  uint8_t[27] Encoded repair payload slice

```

### 7.1 Erasure Coding & Formal RFC 6330 Recovery Bounds

* **Objects $\le 32\text{ KB}$:** Systematic Reed-Solomon over $\text{GF}(2^8)$ utilizing a Cauchy generator matrix.
* **Objects $> 32\text{ KB}$:** RaptorQ Forward Error Correction conforming to RFC 6330.
* **Statistical Recovery Bounds:** For extended source block size $K'$ with random Encoding Symbol Identifier (ESI) distribution:
* Ingesting $K'$ symbols: Incomplete decoding failure probability $P_f \le 1.0 \times 10^{-2}$.
* Ingesting $K' + 1$ symbols: Failure probability $P_f \le 1.0 \times 10^{-4}$.
* Ingesting $K' + 2$ symbols: Failure probability $P_f \le 1.0 \times 10^{-6}$.


* **Mid-Stream Acquisition:** SOTP injects the inline descriptor into every 8th frame. A receiver joining mid-broadcast acquires metadata within 8 frames and begins collecting fountain slices without missing the transfer.

---

## 8. Platform Integration & Audio Routing Topologies

```
 [TOPOLOGY A: DIRECT CABLED DONGLE (iOS 17+ Production Baseline)]
 ┌────────────────┐ Lightning/USB-C ┌───────────────┐ 3.5mm TRRS ┌──────────────────┐
 │ iPhone         ├────────────────►│ Apple USB-C   ├────────────►│ Headset Jack of  │
 │ (V-RADM App)   │◄────────────────┤ Audio Adapter │◄────────────┤ Secondary Handset│
 └────────────────┘                 └───────────────┘             └──────────────────┘

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

### 8.1 iOS Audio Architecture Constraints

1. **Topology A (Primary Cabled Standard):** An external USB Audio Class adapter (e.g., Apple USB-C to 3.5mm Headphone Jack) connects to the iPhone running the modem. The analog TRRS plug feeds the analog headset jack of a secondary calling phone. This cleanly bypasses iOS telephony sandbox barriers.
2. **Topology B (In-App VoIP):** The application integrates a lightweight SIP stack over cellular packet data. `AVAudioEngine` retains native, unconstrained full-duplex PCM read/write access.
3. **Topology C (iOS 18.2+ Add Audio in Calls):** Utilizes `AVAudioApplication.microphoneInjectionPermission`.
* *Caveat:* iOS sums the injected application audio directly with ambient microphone audio. Muting the mic silences the injected app audio. Wind and background noise sum directly into the modem signal before reaching the cellular ACELP encoder. When operating under Topology C, the engine clamps adaptation to **MCS 0 or MCS 1** with a $-3\text{ dBFS}$ digital attenuation margin.


4. **Audio Session Configuration:**
```swift
let session = AVAudioSession.sharedInstance()
try session.setCategory(.playAndRecord, mode: .measurement, options: [.allowBluetooth, .mixWithOthers])
try session.setPreferredSampleRate(16000.0)
try session.setPreferredIOBufferDuration(0.005) // 5ms buffer
try session.setActive(true)

```


*Note:* `.measurement` mode disables system dynamics compression on a best-effort basis; it does not promise the complete absence of low-level hardware filtering.

### 8.2 Linux / Asterisk PBX AudioSocket Gateway

Asterisk routes incoming call audio directly to the `vradmd` daemon over TCP using the AudioSocket protocol.

```ini
; /etc/asterisk/extensions.conf
[vradm-inbound]
exten => 774,1,NoOp(Incoming V-RADM Carrier Link)
same  => n,Answer()
; Advisory caller ID filter (Gatekeeper only; not security authentication)
same  => n,GotoIf($["${CALLERID(num)}" != "+15550198372"]?reject)
; Hand off 8kHz linear PCM directly to vradmd TCP daemon
same  => n,AudioSocket(127.0.0.1:9099,4a8b7f32-5c21-4b76-90e1-0c1b72a9e3d1)
same  => n,Hangup()
same  => n(reject),NoOp(Unauthorized Call Dropped)
same  => n,Hangup()

```

---

## 9. Complete C-ABI Interface (`vradm_core.h`)

This exact header must be generated by `cbindgen` from `vradm-core` and consumed by Swift and Linux C/Rust wrappers.

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
#define VRADM_MAX_PAYLOAD_SIZE   39

typedef enum {
    VRADM_MCS_0 = 0, // 20 Bd, Orthogonal Pitch Hop (80 bps raw / 48.75 bps net)
    VRADM_MCS_1 = 1, // 50 Bd, Joint Pitch/Formant/Pulse (400 bps raw / 243.8 bps net)
    VRADM_MCS_2 = 2, // 100 Bd, 4-Carrier DQPSK (800 bps raw / 487.5 bps net)
    VRADM_MCS_3 = 3, // 200 Bd, 8-Carrier DQPSK (3200 bps raw / 1950.0 bps net)
    VRADM_MCS_4 = 4  // 250 Bd, 10-Carrier CP-QPSK (5000 bps raw / 3046.9 bps net)
} vradm_mcs_t;

typedef enum {
    VRADM_RATE_8K  = 8000,
    VRADM_RATE_16K = 16000
} vradm_rate_t;

typedef struct vradm_engine vradm_engine_t;

typedef struct {
    vradm_mcs_t  startup_mcs;
    vradm_rate_t sample_rate;
    bool         auto_rate_adaptation;
    float        tx_amplitude; // Max RMS ceiling (Default: 0.5 = -6.0 dBFS)
} vradm_config_t;

typedef struct {
    float       estimated_snr_db;
    vradm_mcs_t active_tx_mcs;
    vradm_mcs_t active_rx_mcs;
    uint32_t    frames_transmitted;
    uint32_t    frames_received;
    uint32_t    rs_corrected_bytes;
    uint32_t    rs_corrected_erasures;
    uint32_t    crc_failures;
    float       channel_metric_score;
    bool        plcp_carrier_locked;
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
| **TC-04** | Wideband Cabled | AMR-WB @ 12.65 kbps | Resampling $16\text{k} \to 8\text{k} \to 16\text{k}$; $\pm 80\text{ PPM}$ clock drift | DPLL maintains symbol lock. Net IP goodput $\ge 1,750\text{ bps}$ at MCS 3. |
| **TC-05** | VAD Characterization | 3GPP VAD Model 1 & 2 | Continuous voiced maintenance sequence | Discontinuous Transmission entry $P_{\text{DTX}} \le 0.01$. Comfort Noise insertion $P_{\text{CNG}} \le 0.005$. Reacquisition latency $\le 40\text{ ms}$. |
| **TC-06** | Burst Erasure Recovery | MCS 3 (160 ms frames) | 3 consecutive physical frame drops ($480\text{ ms}$ drop) | Selective Repeat ARQ triggers fast retransmit via `ACK_MAP`. Complete IP packet stream recovery within $850\text{ ms}$. |
| **TC-07** | SOTP Mid-Stream Entry | AMR-WB @ 12.65 kbps | Receiver attaches at Frame 40 of a 100-symbol broadcast | Receiver syncs via Inline Descriptor within 8 frames. Object reconstructs with matching BLAKE3 checksum. |
| **TC-08** | Real VoLTE Cellular Call | Commercial Mobile VoLTE Network | Active 15-minute phone call between iPhone and Asterisk server | Interactive OpenSSH/Mosh session maintained continuously. Keystroke round-trip time $\le 550\text{ ms}$. |

---

## 11. Direct Implementation Instructions

1. **Workspace Layout:** Scaffold a Cargo workspace with `crates/vradm-core` (`#![no_std]` core with `alloc` for initialization), `crates/vradm-server` (Asterisk daemon), `crates/vradm-cli` (diagnostics), and `tests/vocoder_bench`.
2. **Wire Format Verification:** Implement `crates/vradm-core/src/link/frame.rs` and verify bit-exact layout of the canonical 64-byte frame against Section 2.
3. **PLCP Bootstrap Engine:** Implement the Barker-13 dual-chirp generator and NCCF receiver before finalizing higher-order modulation algorithms. Ensure the receiver configures its demapper based strictly on the decoded PLCP beacon.
4. **Soft-Decision RS Decoder:** Implement Chase/GMD soft-decision decoding in `crates/vradm-core/src/fec/rs.rs`, verifying that flagging 16 confidence-tagged erasures reconstructs corrupted frames under $2t + e \le 16$.
5. **Continuous Codec Validation:** Execute `tests/vocoder_bench` continuously against `libopencore-amr` and `vo-amrwbenc` before packaging C-ABI exports. Ensure zero regressions across tests TC-01 through TC-07 prior to platform deployment.