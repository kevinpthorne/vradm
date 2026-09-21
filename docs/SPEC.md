# Engineering Specification: Vocoder-Resilient Acoustic Data Modem (V-RADM)

**Document Version:** 3.7.0

**Status:** Closed Baseline Engineering Specification (Production-Hardened)

**Primary Targets:** `vradm-core` (Rust C-ABI Thread-Safe Engine), iOS 17+ Client Adapter, Linux / Asterisk 20+ PBX Gateway Daemon

---

## 1. System Architecture & Process Boundaries

V-RADM establishes a point-to-point IPv4 tunnel across speech-compressed cellular voice channels (VoLTE, VoNR, 3G AMR, carrier VoIP) and acoustic air gaps. It enables unmodified network applications—specifically **OpenSSH** and **Mosh (Mobile Shell)**—to operate reliably under severe bandwidth, latency, and transcoding constraints.

To eliminate TCP congestion window collapse over high-latency and half-duplex links (such as MCS 0 with 9.7-second RTOs), V-RADM implements an embedded **Split-Connection Performance Enhancing Proxy (TCP-PEP)** adhering to RFC 3135.

```
 ┌────────────────────────────────────────────────────────────────────────┐
 │                              iOS HOST                                  │
 │                                                                        │
 │  [Third-Party Applications] (Blink Shell, Termius, curl, etc.)         │
 │                            │ Native TCP (Port 22) / UDP (Mosh)         │
 │                            ▼                                           │
 │  ┌──────────────────────────────────────────────────────────────────┐  │
 │  │ NetworkExtension Sandbox (PacketTunnelProvider)                  │  │
 │  │  - Exposes split-tunnel utun interface (10.99.0.0/24, MTU 256)   │  │
 │  │  - Embedded TCP-PEP: Terminates TCP locally, spoofs ACKs         │  │
 │  │  - Mosh UDP: Direct passthrough with BEST_EFFORT flag            │  │
 │  └──────────────────────────────┬───────────────────────────────────┘  │
 │                                 │ Lock-Free IPC Ring Buffer            │
 │                                 │ (App Group Shared POSIX Memory)      │
 │                                 ▼                                      │
 │  ┌──────────────────────────────────────────────────────────────────┐  │
 │  │ Main App Process (Foreground / Background Audio Entitlements)    │  │
 │  │  - Modem Controller & Telemetry Dashboard (Status, SNR, VU)      │  │
 │  │  - libvradm_core Engine (IP Slicer, ARQ, RS FEC, PHY Modulator)  │  │
 │  │  - Continuous Sample-Slip DPLL (Sub-sample tracking)             │  │
 │  │  - AVAudioEngine (Topology A: USB DAC / Topology C: In-Call API) │  │
 │  └──────────────────────────────┬───────────────────────────────────┘  │
 └─────────────────────────────────┼──────────────────────────────────────┘
                                   │ Active Cellular Call (VoLTE / AMR-WB)
                                   ▼
 ┌────────────────────────────────────────────────────────────────────────┐
 │                      SERVER GATEWAY (Linux PBX)                        │
 │                                                                        │
 │  [Carrier Trunk] ──> Asterisk 20+ PBX Core (SIP Trunk / VoLTE Gateway) │
 │                            │ AudioSocket Protocol (TCP:9099, 8kHz PCM) │
 │                            ▼                                           │
 │  [vradmd Daemon] ──> Multi-Tenant Engine Pool (Thread-Safe C-ABI)      │
 │                            │ Reassembled Slices & PEP Rehydration      │
 │                            ▼                                           │
 │  [TCP-PEP Agent] ──> Local TCP Loopback to 127.0.0.1:22                │
 │  [UDP Router]    ──> Forward Mosh UDP Packets to 127.0.0.1:60000..60010│
 │                            │                                           │
 │  [Host Daemons] ───> sshd (Port 22) & mosh-server                      │
 └────────────────────────────────────────────────────────────────────────┘

```

### 1.1 The Parameter-Domain Channel Model

Cellular speech vocoders (specifically ACELP variants: AMR, AMR-WB, EVS) discard analog audio waveforms and inter-carrier phase relationships. They decompose audio into parametric speech features:

* **Linear Prediction (LP) Spectral Envelope:** Models vocal tract resonances (quantized as Line Spectral Pairs/ISFs).
* **Adaptive Codebook:** Models fundamental pitch periodicity as a sample-domain delay ($T_0$) and gain ($g_p$).
* **Algebraic Fixed Codebook:** Models the excitation residual using interleaved multi-pulse grids and gain ($g_c$).

V-RADM models the cellular voice channel as a **lossy parameter-quantization channel**. Demodulation relies on feature-distance estimation and soft-decision confidence decoding rather than hard-sliced phase boundaries.

### 1.2 Two-Channel PHY Decoupling

To eliminate circular boot-up dependencies—where a receiver must know the active Modulation and Coding Scheme (MCS) to demodulate the frame containing the MCS field—transmission is split into two asynchronous layers:

1. **Authenticated PLCP Control Channel:** A low-rate, noncoherent control beacon protected by a SipHash-2-4 MAC and dual Golay FEC. It announces transmitter state, active MCS, requested reverse MCS, and framing sequence.
2. **Payload Data Channel:** A variable-rate data carrier (MCS 0 through MCS 4) formatted into immutable 64-byte physical frames.

---

## 2. Canonical Wire Formats

V-RADM defines two physical layer frame formats: the **Canonical Data Frame (64 Bytes)** for standard streaming payloads, and the **Compact Control Frame (16 Bytes)** for low-overhead signaling, authentication, and rapid half-duplex turn-arounds.

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

<br>Bits [2..0]: Wire Protocol Version (`001` = v3.7) |
| `0x03` | `SEQ` | 8 bits | Rolling transmit sequence number ($0\text{--}255$). |
| `0x04` | `ACK_BASE` | 8 bits | Cumulative ACK: highest contiguous peer sequence number received in-order. |
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

### 2.2 Authenticated Compact Control Frame (16-Byte CCF PDU)

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|      SYNC_WORD (0xD391)       |   CCF_CTRL    |   ACK_BASE    |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|    ACK_MAP    |    CCF_CRC16 (2 Bytes)        |   CCF_MAC     |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|              REED-SOLOMON PARITY (Bytes 0x08..0x0F, 8 Bytes)  |
|                                                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+

```

* **Bytes 0x00..0x01:** `SYNC_WORD` (`0xD391`).
* **Byte 0x02:** `CCF_CTRL` (Bit [7]: `1` = CCF Marker; Bits [6..4]: Target/Requested MCS; Bit [3]: TDD Yield Flag; Bits [2..0]: Command Type: `001` = Standalone ACK, `010` = MCS Commit Ack, `011` = TDD Grant).
* **Byte 0x03:** `ACK_BASE` (Cumulative ACK sequence number).
* **Byte 0x04:** `ACK_MAP` (7-bit selective ACK bitmap).
* **Bytes 0x05..0x06:** CRC-16-CCITT covering bytes `0x02..0x04`.
* **Byte 0x07:** `CCF_MAC` (Truncated 8-bit SipHash-2-4 MAC computed over bytes `0x02..0x06` using PSK). Frames failing MAC validation are dropped before state-machine ingestion.
* **Bytes 0x08..0x0F:** Systematic $\text{RS}(16, 8)$ Galois field parity covering bytes `0x00..0x07` (8 parity bytes correcting up to $t = 4$ erroneous bytes).
* **Total CCF Duration at MCS 0:** $\frac{128\text{ bits}}{4\text{ bits/sym}} \times 50.0\text{ ms} = \mathbf{1.6\text{ seconds}}$.

---

## 3. Physical Layer (PHY) & Sample-Exact Modulation

### 3.1 Closed-Form MCS Specifications

```
 ┌────────────────────────────────────────────────────────────────────────────────────────────────┐
 │                                CLOSED-FORM MCS SPECIFICATIONS                                  │
 ├──────┬──────────┬────────┬───────────┬─────────────┬──────────┬────────────┬──────────┬────────┤
 │ MCS  │ Nature   │ Baud   │ Alphabet  │ Independent │ Bits/Sym │ Raw PHY    │ Max L3   │ App    │
 │      │          │ (Bd)   │ Size (Y)  │ Dims (Z)    │ (X)      │ Rate (bps) │ Rate(bps)│ Goodput│
 ├──────┼──────────┼────────┼───────────┼─────────────┼──────────┼────────────┼──────────┼────────┤
 │ 0    │ Feature  │ 20     │ 16        │ 1           │ 4        │ 80.0       │ 46.25    │ ~24.0  │
 │ 1    │ Feature  │ 50     │ 256       │ 3           │ 8        │ 400.0      │ 231.25   │ ~155.0 │
 │ 2*   │ Hybrid   │ 100    │ 256       │ 4           │ 8        │ 800.0      │ 462.50   │ ~330.0 │
 │ 3*   │ Coherent │ 200    │ 65,536    │ 8           │ 16       │ 3,200.0    │ 1,850.00 │ ~920.0 │
 │ 4    │ Waveform │ 250    │ 65,536    │ 8           │ 16       │ 4,000.0    │ 2,312.50 │ ~1,450 │
 └──────┴──────────┴────────┴───────────┴─────────────┴──────────┴────────────┴──────────┴────────┘
 *Note: MCS 2 and MCS 3 are experimentally gated modes; operational enablement requires passing TC-04.

```

### 3.2 Modulator Implementations & Continuous Sample-Slip Protection

#### Deterministic PRBS-7 Phase Dither (Anti-Gating / Anti-AGC)

Smartphone baseband AGC and noise-suppression algorithms treat stationary multi-tone carriers as acoustic whistle or background hum, suppressing them mid-frame. To prevent gating without degrading coherent demodulation:

* A synchronized **PRBS-7** generator ($x^7 + x^6 + 1$, initialized with `SEQ`) produces a deterministic pseudo-random phase perturbation $\Delta \theta_{\text{dither}}(n) \in \left[-\frac{\pi}{16}, +\frac{\pi}{16}\right]$ applied across all subcarriers:

$$s(n) = w(n) \sum_{k} A_k \cos\left(\omega_k n + \phi_k(m) + \Delta \theta_{\text{dither}}(n)\right)$$


* The receiver executes an identical PRBS-7 generator synchronized to `SEQ`, subtracting $\Delta \theta_{\text{dither}}(n)$ prior to constellation slicing. The carrier exhibits time-varying non-stationary phase to OS-level DSP while preserving phase distance at the demapper.

#### Mid-Frame Continuous Sample-Slip Recovery (Delay-Locked Loop)

To prevent sample slips across asynchronous clock boundaries (`AVAudioEngine` vs. cellular baseband) from misaligning OFDM bins or DQPSK symbols between PLCP beacons:

1. **Pilot Delay-Locked Loop (DLL):** In MCS 2 and MCS 3, Subcarrier 0 ($600\text{ Hz}$) carries an unmodulated pilot reference.
2. **Early-Prompt-Late Correlator:** The receiver calculates early ($n - 1$), prompt ($n$), and late ($n + 1$) correlation energy across a sliding 16-sample window.
3. **Sub-Sample Fractional Farrow Filter:** If late energy exceeds prompt energy by $\ge 4.5\text{ dB}$, a $+1$ sample slip is flagged; if early energy dominates, a $-1$ sample slip is flagged. The input resampler dynamically shifts its Farrow interpolator delay line by $\pm 1$ sample within $2.5\text{ ms}$, preserving constellation tracking without dropping frames.
4. **MCS 4 CP Correlator:** Cyclic prefix cross-correlation evaluates timing drift on every 4.0 ms slot, eliminating cumulative timing error.

#### Raised-Cosine Edge Smoothing Window ($w(n)$)

Edge shaping duration is fixed at $0.5\text{ ms}$ ($L = 4\text{ samples at } 8\text{ kHz}, L = 8\text{ samples at } 16\text{ kHz}$):


$$w(n) = \begin{cases}  \frac{1}{2}\left[1 - \cos\left(\frac{\pi (n + 0.5)}{L}\right)\right] & 0 \le n < L \\  1.0 & L \le n < N_{\text{sym}} - L \\  \frac{1}{2}\left[1 - \cos\left(\frac{\pi (N_{\text{sym}} - 1 - n + 0.5)}{L}\right)\right] & N_{\text{sym}} - L \le n < N_{\text{sym}}  \end{cases}$$

#### MCS 0: Free-Air Acoustic TDD (Feature-Domain)

* **Alphabet:** $Y = 16$ fundamental pitch states ($F_0$).
* **Dimensions:** $Z = 1$ ($F_0(m) = 120\text{ Hz} + (m \cdot 10\text{ Hz})$ for $m \in [0..15]$).
* **Timing:** $T_{\text{sym}} = 50.0\text{ ms}$ ($400\text{ samples at } 8\text{ kHz}$). First $15.0\text{ ms}$ discarded; NCCF integration runs strictly over the final $35.0\text{ ms}$.

#### MCS 1: Robust Narrowband Cellular (Feature-Domain Reference Atom Codebook)

* **Alphabet:** $Y = 256$ joint speech-feature states ($8\text{ bits/symbol}$ at $50\text{ Bd}$, $T_{\text{sym}} = 20.0\text{ ms} = 160\text{ samples at } 8\text{ kHz}$).
* **Excitation Pulse Train ($e(n)$):**

$$e(n) = \sum_{p=0}^{\lfloor (159-\delta)/T_0 \rfloor} \delta_{\text{dirac}}[n - (\delta + p \cdot T_0)]$$


* Lag Delay ($T_0$, 3 bits): $T_0 \in \{33, 38, 43, 49, 56, 64, 72, 80\}\text{ samples at } 8\text{ kHz}$ ($F_0 \approx 100.0\text{ to } 242.4\text{ Hz}$).
* Grid Offset ($\delta$, 2 bits): $\delta \in \{0, 1, 2, 3\}\text{ samples}$.


* **Formant Filter ($h_{\text{vowel}}(n)$):** Direct-form II cascaded biquad IIR filter ($Q = 5.0$) modeling 8 vowel states:

$$\{(300, 900), (350, 1400), (450, 1100), (500, 1700), (600, 1200), (650, 1900), (750, 1300), (800, 2100)\}\text{ Hz}$$



Digital biquad coefficients for each resonance $F_c$:

$$\omega_0 = \frac{2\pi F_c}{F_s}, \quad \alpha = \frac{\sin(\omega_0)}{2Q}$$


$$b_0 = \alpha, \quad b_1 = 0, \quad b_2 = -\alpha, \quad a_0 = 1 + \alpha, \quad a_1 = -2\cos(\omega_0), \quad a_2 = 1 - \alpha$$


* **Synthesis:** $s(n) = w(n) \cdot [e(n) * h_{\text{vowel}}(n)]$.

#### MCS 2: Balanced Cellular (Experimentally Gated Hybrid)

* **Alphabet:** $Y = 256$ states ($8\text{ bits/symbol}$ at $100\text{ Bd}$, $T_{\text{sym}} = 10.0\text{ ms} = 80\text{ samples at } 8\text{ kHz}$).
* **Carrier Frequencies ($Z = 4$):** $f_k \in \{600, 1000, 1400, 1800\}\text{ Hz}$.
* **Sample-Exact Modulator:**

$$s(n) = w(n) \sum_{k=0}^{3} A_k \cos\left(\frac{2\pi f_k n}{F_s} + \phi_k(m) + \Delta \theta_{\text{dither}}(n)\right)$$


$$\phi_k(m) = \text{wrap}_{2\pi}\left(\phi_k(m-1) + \Delta \phi_k(m)\right), \quad \Delta \phi_k \in \left\{0, \frac{\pi}{2}, \pi, \frac{3\pi}{2}\right\}$$



Where $A_k = [0.8, 1.0, 0.9, 0.7]$.

#### MCS 3: Wideband Cellular Cabled (Experimentally Gated Coherent)

* **Alphabet:** $Y = 65,536$ states ($16\text{ bits/symbol}$ at $200\text{ Bd}$, $T_{\text{sym}} = 5.0\text{ ms} = 40\text{ samples at } 8\text{ kHz}$).
* **Carrier Frequencies ($Z = 8$):** $f_k = k \cdot 200\text{ Hz}$ for $k \in [3..10]$ ($600\text{ Hz to } 2000\text{ Hz}$).
* **Sample-Exact Modulator:**

$$s(n) = w(n) \sum_{k=3}^{10} A_k \cos\left(\frac{2\pi f_k n}{F_s} + \phi_k(m) + \Delta \theta_{\text{dither}}(n)\right)$$


$$A_k = [0.6, 0.9, 1.0, 0.85, 0.7, 0.5, 0.4, 0.3], \quad \phi_k(0) = \frac{k \pi}{4}$$



Transitions occur on 5 ms ACELP subframe boundaries.

#### MCS 4: Real-Valued Hermitian CP-OFDM (Conventional Waveform Modem)

* **Sampling Rate:** $F_s = 8,000\text{ Hz}$.
* **Orthogonal Subcarrier Spacing:** $\Delta f = \frac{1}{T_{\text{useful}}} = \frac{8000}{28} = \mathbf{285.714\text{ Hz}}$.
* **Hermitian Symmetric Real-Valued IFFT:**

$$N_{\text{fft}} = 28\text{ points}, \quad N_{\text{cp}} = 4\text{ points} \implies N_{\text{total}} = 32\text{ samples (4.0 ms, 250 Bd)}$$



For indices $k \in [0..14]$ with complex QPSK data symbols $D_k$:

$$X[k] = D_k, \quad X[28 - k] = D_k^*, \quad X[0] = X[14] = 0$$


$$x(n) = \frac{1}{\sqrt{N_{\text{fft}}}} \sum_{k=0}^{N_{\text{fft}}-1} X[k] e^{j \frac{2\pi k n}{N_{\text{fft}}}} \in \mathbb{R}$$


* **Telephone-Band Carrier Allocation ($Z = 8$ Active Carriers):**

$$k \in \{2, 3, 5, 6, 7, 8, 9, 10\} \implies f_k \in \{571.4, 857.1, 1428.6, 1714.3, 2000.0, 2285.7, 2571.4, 2857.1\}\text{ Hz}$$


* **Demodulation:** Differential QPSK across consecutive OFDM symbol slots eliminates the requirement for absolute carrier-phase channel estimation under the assumption of sufficiently slow channel variation.

### 3.3 Output Level Conditioning

1. **Target RMS Normalization:** Audio blocks $s[n]$ are scaled to a target RMS of $V_{\text{target\_rms}} = 0.3535\text{ FS}$ ($-9.03\text{ dBFS}$ RMS):

$$s[n] \leftarrow s[n] \cdot \frac{V_{\text{target\_rms}}}{\sqrt{\frac{1}{N}\sum_{m=0}^{N-1} s^2[m]}}$$


2. **Hard Peak Ceiling Limiter:** Output samples are clamped to prevent DAC saturation:

$$s[n] \leftarrow \text{clamp}(s[n], -0.5\text{ FS}, +0.5\text{ FS})$$



Peak sample magnitude is bounded to $\le 0.5\text{ FS} = -6.02\text{ dBFS Peak}$, providing $3.0\text{ dB}$ of digital headroom.

---

## 4. Authenticated Control Plane & PLCP Architecture

The receiver never inspects data frames to determine modulation parameters. Every burst is preceded by an **Authenticated PLCP Control Beacon**.

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|      BARKER-13 DUAL-CHIRP     |CUR_MCS|REQ_MCS|TX_PWR |BEAC_SEQ|
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|  BEACON_MAC8  | GOLAY CODEWORD 1 (24 Bits) / CODEWORD 2 (24B) |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+

```

### 4.1 Cryptographic Control Plane Authentication (Anti-DoS)

To prevent unauthorized over-the-air injection of spoofed PLCP commands (e.g., forcing a downgrade or crashing the state machine):

* Endpoints are provisioned with a 128-bit Pre-Shared Key (PSK).
* `BEACON_MAC8`: An 8-bit truncated **SipHash-2-4** MAC computed over (`CUR_MCS`, `REQ_MCS`, `TX_PWR`, `BEAC_SEQ`).
* Receivers decode Golay parity, verify the SipHash-2-4 MAC, and reject unauthorized beacons before any modulation changes or state updates occur.

### 4.2 Deterministic PLCP Parameters

* `PLCP_CHIP_RATE`: 200 chips/s ($5.0\text{ ms/chip}$).
* `PLCP_PREAMBLE`: 13 chips $\times 5.0\text{ ms} = \mathbf{65.0\text{ ms}}$ (Hyperbolic pitch sweep: $600\text{ Hz} \leftrightarrow 1800\text{ Hz}$).
* `PLCP_GUARD`: $10.0\text{ ms}$ silence before and after header.
* `PLCP_HEADER_PAYLOAD`: 16 bits (`CUR_MCS` [3b], `REQ_MCS` [3b], `TX_PWR` [2b], `BEAC_SEQ` [8b]) + 8-bit `BEACON_MAC8` = **24 information bits**.
* `PLCP_HEADER_FEC`: Two independent Extended Golay $[24, 12, 8]$ codewords (Codeword 1: Bits 0..11; Codeword 2: Bits 12..23). Total encoded header = **48 bits**.
* `PLCP_MODULATION`: 2-FSK ($1200\text{ Hz} = \text{Mark}, 1600\text{ Hz} = \text{Space}$) at $100\text{ Bd}$ ($10.0\text{ ms/bit}$). Total header duration = $48 \times 10.0\text{ ms} = \mathbf{480.0\text{ ms}}$.
* `PLCP_TOTAL_DURATION`: $65.0 + 10.0 + 480.0 + 10.0 = \mathbf{565.0\text{ ms}}$.
* **Cadence:** PLCP is transmitted at `SESSION_START`, `TDD_DATA_TURN`, `CONTINUOUS_SYNC` (every 16 frames), and `MCS_CHANGE`. *Compact Control Frames (CCFs) do NOT require a PLCP beacon.*

### 4.3 Two-Phase MCS Commit Handshake

```
 Node A (Transmitter)                                 Node B (Receiver)
 ┌─────────────────┐                                 ┌─────────────────┐
 │ Requests MCS 3  │                                 │ Operating MCS 2 │
 └────────┬────────┘                                 └────────┬────────┘
          │                                                   │
          │ Phase 1: PLCP (CUR=2, REQ=3) + Data Frames        │
          ├──────────────────────────────────────────────────►│
          │                                                   │
          │ Phase 2: Authenticated CCF (ACK_BASE, COMMIT=3)   │
          │◄──────────────────────────────────────────────────┤
          │                                                   │
 ┌────────┴────────┐                                 ┌────────┴────────┐
 │ Switch to MCS 3 │                                 │ Switch to MCS 3 │
 │ at SEQ = S+1    │                                 │ at SEQ = S+1    │
 └─────────────────┘                                 └─────────────────┘

```

1. **Announcement:** Node A transmits its current burst at `CUR_MCS`, setting `REQ_MCS = target`.
2. **Commit Ack:** Node B decodes the request, verifies channel metric $M \ge 0.85$ and valid `CCF_MAC`, and responds with a Compact Control Frame setting `CCF_CTRL` to `MCS_COMMIT_ACK` with sequence boundary $S$.
3. **Synchronous Switchover:** Both nodes switch modulators and demodulators simultaneously at sequence $S + 1$.

---

## 5. Half-Duplex TDD Protocol Specification (MCS 0)

In open-air acoustic conditions, simultaneous bidirectional audio triggers phone-level Acoustic Echo Cancellation (AEC), destroying the link. MCS 0 enforces deterministic Time Division Duplexing (TDD).

```
 0s                  0.565s                 6.965s  7.115s      7.265s                 8.865s  9.015s     9.165s
 ┌───────────────────┬──────────────────────┬───────┬───────────┬──────────────────────┬───────┬──────────┐
 │ PLCP Control      │ Node A: Data Frame   │ EOT   │ Acoustic  │ Node B: CCF          │ EOT   │ Acoustic │
 │ Beacon (565 ms)   │ (64 Bytes, 512 bits) │ Tone  │ Decay     │ (16 Bytes, 128 bits) │ Tone  │ Decay    │
 └───────────────────┴──────────────────────┴───────┴───────────┴──────────────────────┴───────┴──────────┘
  ◄────────────── Node A Transmit Turn ────────────► ◄─ Guard ─► ◄── Node B Turn ─────► ◄─ Guard ─►

```

### 5.1 Deterministic TDD Ownership State Machine

1. **Token Ownership:** Initial channel ownership is assigned to the calling gateway (Asterisk PBX / Master node).
2. **Turn Structure:**
* **Data Turn:** PLCP Beacon ($565\text{ ms}$) + 1 Canonical Data Frame ($6,400\text{ ms}$) + EOT Tone ($150\text{ ms}$) = **$7,115\text{ ms}$**.
* **Control Turn (ACK/Grant):** 1 Authenticated CCF ($1,600\text{ ms}$, no PLCP) + EOT Tone ($150\text{ ms}$) = **$1,750\text{ ms}$**.


3. **End-of-Turn (EOT) Tone:** A $150.0\text{ ms}$ dual-tone burst ($1400\text{ Hz} + 1800\text{ Hz}$ at $-12.0\text{ dBFS}$) signals token yield.
4. **Acoustic Guard Window:** Delay of exactly **$150.0\text{ ms}$** following EOT allows room reverberation to decay.
5. **Deterministic Collision Recovery:**
* Upon turn timeout ($T_{\text{timeout}} = 12.0\text{ seconds}$), both nodes enter `SILENT_LISTEN`.
* Slave node enforces a mandatory backoff window of $4.0\text{ seconds}$.
* Master node waits $1.5\text{ seconds}$ and re-asserts channel ownership with a standalone PLCP beacon.



---

## 6. Soft-Decision Link Layer & Error Correction

```
 Incoming PCM Audio
         │
         ▼
 Abstract Symbol Likelihood Slicer: estimate_symbol_likelihoods(rx, mcs)
         │
         ▼
 Byte Confidence Assignment: C_byte = min(C_symbols_in_byte)
         │
         ▼
 8x8 Byte Block Deinterleaver (Permutes Bytes and Confidences Simultaneously)
         │
         ▼
 GMD Erasure Tagger:
 Evaluates weakest bytes with C_byte < 0.35. Tests trial erasures: e in {16, 14, ..., 0}
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

### 6.1 Abstract Symbol Likelihood Estimator

Demodulators implement a decoupled likelihood interface producing normalized confidence $C_i \in [0.0, 1.0]$:

* **For Phase-Modulated Modes (MCS 2, 3, 4):**

$$C_i = \tanh(\text{SNR}_{\text{carrier}}) \cdot \left(1.0 - \frac{d}{\pi/4}\right)$$



Where $d = \min_q \vert{}\text{wrap}(\Delta \phi - \phi_q)\vert{}$ measures angular distance to the closest constellation target.
* **For Feature-Domain Modes (MCS 0, 1):**

$$C_i = \frac{\Lambda(\hat{S}) - \Lambda(S_{\text{second}})}{\Lambda(\hat{S})}$$



Where $\Lambda(S)$ is the normalized correlation peak or filter magnitude of candidate speech atom $S$.
* **Byte Confidence:** $C_{\text{byte}} = \min(C_{\text{symbols\_in\_byte}})$. Bytes with $C_{\text{byte}} < \Theta_{\text{erase}} = 0.35$ are flagged as erasures.

### 6.2 Bounded Link Quality Metric ($M$)

Decoder stress is normalized via the decoder burden metric $B \in [0.0, 1.0]$:

$$B = \frac{2t + e}{16}$$

The Link Quality Metric $M$ is mathematically bounded in $[0.0, 1.0]$:

$$M = 0.4 \cdot \bar{C}_{\text{sym}} + 0.3 \cdot (1.0 - P_{\text{FER}}) + 0.3 \cdot (1.0 - B)$$

### 6.3 Closed-Form Retransmission Timeout (RTO)

$$\text{RTO} = \text{SRTT} + \max(4 \cdot \text{RTTVAR}, T_{\text{frame}}) + T_{\text{margin}}(\text{Profile})$$

* **Profile 1 (Direct Cabled Full-Duplex):** Nominal RTO = **$560\text{ ms}$**.
* **Profile 2 (Free-Air Acoustic Half-Duplex with Compact ACKs):** Total cycle = Data Turn ($7.115\text{ s}$) + Guard ($0.15\text{ s}$) + CCF Turn ($1.75\text{ s}$) + Guard ($0.15\text{ s}$) = $9.165\text{ seconds}$. Nominal RTO = **$9.7\text{ seconds}$**.
* **Karn's Algorithm Mandate:** RTT updates MUST NOT be computed from retransmitted frames.

---

## 7. Transport Layer & Performance Enhancing Proxy (TCP-PEP)

Standard TCP stacks interpret multi-second acoustic RTOs and half-duplex stalls as network congestion, collapsing the congestion window ($cwnd$) to 1 segment and triggering destructive retransmission loops.

```
 [OpenSSH Client]                                          [Linux sshd]
        │                                                        │
        │ Local TCP (RTT < 1ms)                                  │ Local TCP (RTT < 1ms)
        ▼                                                        ▼
 ┌──────────────┐                                         ┌──────────────┐
 │ Client-Side  │                                         │ Server-Side  │
 │ TCP-PEP      │                                         │ TCP-PEP      │
 │ (iOS utun)   │                                         │ (vradmd)     │
 └──────┬───────┘                                         └──────┬───────┘
        │                                                        │
        │ Transparent Segment Slicing & Reliable Link ARQ        │
        └────────────────────────────────────────────────────────┘

```

### 7.1 Split-Connection TCP-PEP Architecture (RFC 3135)

1. **Local Termination:** The iOS `PacketTunnelProvider` intercepts outbound TCP SYN packets destined for `10.99.0.1:22`. It completes the three-way handshake locally on `utun`, spoofing immediate zero-delay ACKs back to the OpenSSH client.
2. **Window Clamping:** The local PEP clamps the client's advertised window to $1,024\text{ bytes}$ and suppresses TCP window scaling, pacing the application stream without triggering client-side TCP timeouts.
3. **Link Slicing:** Plaintext stream bytes are packed directly into 37-byte fragments (`BEST_EFFORT = 0`) handled by V-RADM's link-layer Selective Repeat ARQ.
4. **Gateway Rehydration:** The server daemon (`vradmd`) buffers received fragments, opens a local TCP connection to `127.0.0.1:22`, and feeds reassembled bytes directly into `sshd`.
5. **UDP Passthrough (Mosh):** Mosh traffic bypasses the TCP-PEP entirely. Packets are flagged with `BEST_EFFORT = 1`, bypassing ARQ retransmissions to let Mosh's state synchronization handle loss natively.

### 7.2 Sequence-Derived IP Fragmentation Header (Byte 0x08)

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|FRAG_IDX|TOT_FRAGS|B|             IP DATAGRAM FRAGMENT         |
+-+-+-+-+-+-+-+-+-+-++                                          +
|                                                               |
|               Bytes 0x01..0x25 (Up to 37 Bytes Data)          |
|                                                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+

```

* **Packet Identity:** $\text{PKT\_ID} \equiv \text{SEQ}_{\text{initial}}$.
* `FRAG_IDX` (Bits [7..4]): Fragment index ($0\text{--}15$).
* `TOTAL_FRAGS_MINUS_ONE` (Bits [3..1]): Total fragments minus one ($0\text{--}7$, supporting up to 8 fragments $\implies 296\text{ bytes}$ max datagram).
* `BEST_EFFORT` (Bit [0]): `1` = Unreliable datagram (Mosh UDP); `0` = Reliable in-order delivery (TCP-PEP stream).
* **Virtual MTU:** Standardized at **256 bytes** ($\lceil 256 / 37 \rceil = 7\text{ frames}$ per packet).

### 7.3 Split-Tunnel Network Profile

```swift
let settings = NEPacketTunnelNetworkSettings(tunnelRemoteAddress: "10.99.0.1")
let ipv4Settings = NEIPv4Settings(addresses: ["10.99.0.2"], subnetMasks: ["255.255.255.0"])
ipv4Settings.includedRoutes = [NEIPv4Route(destinationAddress: "10.99.0.0", subnetMask: "255.255.255.0")]
settings.ipv4Settings = ipv4Settings
settings.mtu = 256

```

### 7.4 Canonical Mosh Invocation

```bash
mosh -p 60000:60010 --ssh="ssh -F ~/.ssh/config" 10.99.0.1

```

---

## 8. Simplex Object Transfer Protocol (SOTP)

SOTP handles unidirectional broadcast data drops (RFC 6330 RaptorQ compliant). SOTP frames occupy the 38-byte `PAYLOAD` field at physical offsets `0x08..0x2D`.

```
 ┌────────────────────────────────────────────────────────┐
 │        RFC 6330 SOTP DATA FRAME (DESC_MARKER = 0x00)   │
 └────────────────────────────────────────────────────────┘
  Byte 0x00:       DESC_MARKER (0x00)
  Bytes 0x01..0x04: RFC 6330 FEC PAYLOAD ID (8-bit SBN + 24-bit ESI)
  Bytes 0x05..0x24: SYMBOL_DATA (Exactly T = 32 Bytes RaptorQ Symbol)
  Byte 0x25:       RESERVED (0x00)

 ┌────────────────────────────────────────────────────────┐
 │           SOTP METADATA MANIFEST (DESC_MARKER = 0xFE)  │
 └────────────────────────────────────────────────────────┘
  Byte 0x00:       DESC_MARKER (0xFE)
  Byte 0x01:       OBJECT_ID (8 bits)
  Bytes 0x02..0x03: EXTENDED_SOURCE_SYMBOLS (K', 16 bits)
  Bytes 0x04..0x05: TOTAL_SOURCE_BLOCKS (Z, 16 bits)
  Bytes 0x06..0x09: TOTAL_BYTES (32 bits, uncompressed file size)
  Bytes 0x0A..0x25: TRUNC_BLAKE3 (Leading 22 bytes of BLAKE3 checksum)

```

* **Symbol Size ($T$):** Exactly **32 bytes** ($\text{Al} = 4$).
* **Manifest Cadence:** Emitted every 8 frames.

---

## 9. Platform Integration & Gateway Concurrency

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

### 9.1 Multi-Tenant Asterisk Gateway Concurrency

Asterisk AudioSocket spawns an independent TCP connection per call. To guarantee thread safety across concurrent sessions:

1. **Engine Reentrancy:** `vradm_engine_t` encapsulates all mutable channel state, buffers, DPLL delay lines, and session keys. Zero mutable static state exists across the library.
2. **Immutable Shared Math Tables:** Galois field lookup tables, Golay parity generator matrices, and raised-cosine windows are initialized at process boot as strictly immutable, read-only static structures implementing Rust's `Sync`.
3. **Session Thread Model:** The `vradmd` daemon spawns an isolated worker thread per AudioSocket connection:
```text
AudioSocket Client TCP:9099 ──> Thread Worker ──> vradm_create() ──> Event Loop

```



### 9.2 Asterisk Dialplan Configuration (`/etc/asterisk/extensions.conf`)

```ini
[vradm-inbound]
exten => 774,1,NoOp(Incoming V-RADM Carrier Link)
same  => n,Answer()
same  => n,GotoIf($["${CALLERID(num)}" != "+15550198372"]?reject)
; Hand off 8kHz linear PCM to vradmd TCP daemon (uuid,service)
same  => n,AudioSocket(4a8b7f32-5c21-4b76-90e1-0c1b72a9e3d1,127.0.0.1:9099)
same  => n,Hangup()
same  => n(reject),NoOp(Unauthorized Call Dropped)
same  => n,Hangup()

```

---

## 10. Complete C-ABI Interface (`vradm_core.h`)

All struct fields use explicit fixed-width integer types (`uint8_t`, `uint32_t`, `float`) to guarantee cross-language C-ABI stability.

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
    uint8_t      psk_key[16]; // 128-bit Pre-Shared Key for PLCP/CCF MAC
    uint8_t      padding[3];
    float        tx_amplitude; // Target RMS ceiling (Default: 0.3535 = -9.0 dBFS RMS)
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
    int32_t     sample_slip_accum; // Cumulative sample slips corrected by DLL
} vradm_telemetry_t;

/* --- Engine Lifecycle Management (Thread-Safe & Fully Reentrant) --- */
vradm_engine_t* vradm_create(const vradm_config_t* config);
void            vradm_destroy(vradm_engine_t* engine);
void            vradm_reset(vradm_engine_t* engine);

/* --- Real-Time Audio Streaming I/O (Zero Dynamic Allocations) --- */
void   vradm_process_audio(vradm_engine_t* engine, const int16_t* in_samples, size_t count);
size_t vradm_generate_audio(vradm_engine_t* engine, int16_t* out_samples, size_t max_count);

/* --- Mode A: IP Packet Datagram Stream (TUN / TCP-PEP Interface) --- */
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

## 11. Verification Matrix & Acceptance Test Criteria

```
                            AUTOMATED TEST HARNESS PIPELINE
 ┌─────────────────┐       ┌─────────────────────────────────┐       ┌─────────────────┐
 │ Generated Audio │ ────► │ 3GPP Reference C Codecs         │ ────► │ Demodulator     │
 │ (vradm-core)    │       │ - AMR-NB (4.75k to 12.2k)       │       │ (vradm-core)    │
 └─────────────────┘       │ - AMR-WB (6.60k to 23.85k)      │       └────────┬────────┘
                           │ - Injected Frame Drops & Skew   │                │
                           │ - Sample Slips (+/- 1 sample)   │                ▼
                           │ - Dynamic Mode Downshifting     │       ┌─────────────────┐
                           └─────────────────────────────────┘       │ Acceptance Pass │
                                                                     └─────────────────┘

```

| ID | Test Category | Channel Configuration | Injected Impairment | Pass / Fail Acceptance Criteria |
| --- | --- | --- | --- | --- |
| **TC-01** | Math Loopback | In-memory loopback | Zero noise, synchronous clock | Zero observed bit errors over $30 \times 10^6$ tested bits ($\implies P_e \le 1.0 \times 10^{-7}$ at 95% Clopper-Pearson confidence). Zero RS corrections. |
| **TC-02** | AMR-NB Robustness | AMR-NB @ 12.2 kbps | Injected channel erasure rate = 1.0%; AWGN $\text{SNR} = 18\text{ dB}$ | Zero unrecoverable frames over 30,000 frames ($\implies P_{\text{FER}} \le 1.0 \times 10^{-4}$ at 95% Clopper-Pearson confidence). Zero payload corruption. |
| **TC-03** | Dynamic Codec Adaptation | AMR-NB stepped down from 12.2k to 4.75k | Mode switch occurs at Frame 100 | Link metric $M$ initiates automatic downshift to MCS 1 within 4 frames. Zero dropped IP packets. |
| **TC-04** | Wideband Cabled Gate | AMR-WB @ 12.65 kbps | Resampling $16\text{k} \to 8\text{k} \to 16\text{k}$; $\pm 80\text{ PPM}$ clock drift | DPLL and DLL maintain lock. Measured Application Goodput $R_{\text{APP}} \ge 850\text{ bps}$ for SSH/TCP-PEP or $\ge 1,100\text{ bps}$ for UDP bulk stream. |
| **TC-05** | VAD & AGC Verification | 3GPP VAD Model 1 & 2 + Smartphone AGC model | PRBS-7 phase dither enabled; voiced maintenance carrier active | Measured over 10,000 independent 1-second trials. False DTX entry $P_{\text{DTX}} \le 0.01$. Zero AGC signal-clamping events. |
| **TC-06** | Burst Erasure Recovery | MCS 3 (160 ms frames) | 3 consecutive physical frame drops ($480\text{ ms}$ drop) | Selective Repeat ARQ triggers fast retransmission. Complete IP packet stream recovery within $\le \mathbf{1,450\text{ ms}}$ of drop start. Zero application errors. |
| **TC-07** | SOTP Mid-Stream Entry | AMR-WB @ 12.65 kbps | Receiver attaches at Frame 40 of a 100-symbol broadcast | Receiver syncs via Metadata Manifest within 8 frames. Object reconstructs with matching BLAKE3 checksum. |
| **TC-08a** | Real VoLTE Cellular Call (MCS 3) | Commercial Mobile VoLTE Network | Active 15-minute phone call between iPhone and Asterisk server | Interactive OpenSSH/TCP-PEP session maintained continuously. Keystroke round-trip confirmation time $\le 550\text{ ms}$. |
| **TC-08b** | Real Degraded / Free-Air Link (MCS 0/1) | Acoustic Speaker-to-Mic Air Gap / Degraded 3G Call | High ambient acoustic noise and multi-second frame periods | Mosh UDP terminal session maintained continuously. Predictive local echo renders keystrokes with $< 50\text{ ms}$ UI latency; remote screen converges within $1.5 \times T_{\text{frame}}$ after burst recovery. |
| **TC-09** | Concurrency & Thread-Safety | 8 concurrent AudioSocket TCP threads | Multi-channel load test on Linux daemon | Zero cross-session cross-talk, race conditions, or memory corruption. CPU scaling linear across threads. |
| **TC-10** | Sample-Slip Resilience | MCS 3 and MCS 4 cabled loopback | Injected random single-sample slips ($\pm 1$ sample every 500ms) | Pilot Delay-Locked Loop corrects slips within $2.5\text{ ms}$. Constellation lock maintained without frame loss. |

---

## 12. Direct Implementation Instructions

1. **Workspace Layout:** Scaffold a Cargo workspace with `crates/vradm-core` (`#![no_std]` core with `alloc` for initialization), `crates/vradm-server` (multi-threaded Asterisk daemon), `crates/vradm-cli` (diagnostics), and `tests/vocoder_bench`.
2. **Wire Format Verification:** Implement `crates/vradm-core/src/link/frame.rs` and verify bit-exact layout of both the 64-byte Canonical Data Frame and 16-byte Authenticated Compact Control Frame.
3. **PLCP Bootstrap & Authentication:** Implement the Barker-13 dual-chirp generator, dual Extended Golay $[24, 12, 8]$ codecs, SipHash-2-4 control plane MAC, and 2-FSK modulator.
4. **Soft-Decision RS Decoder:** Implement Chase/GMD soft-decision decoding in `crates/vradm-core/src/fec/rs.rs`, verifying that flagging 16 confidence-tagged erasures reconstructs corrupted frames under $2t + e \le 16$.
5. **Continuous Codec Validation:** Execute `tests/vocoder_bench` continuously against `libopencore-amr` and `vo-amrwbenc` before packaging C-ABI exports. Ensure zero regressions across tests TC-01 through TC-10 prior to platform deployment.