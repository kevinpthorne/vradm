# Engineering Specification: Vocoder-Resilient Acoustic Data Modem (V-RADM)

**Document Version:** 3.8.4

**Status:** Closed Baseline Engineering Specification (Implementation-Ready Research Prototype)

**Primary Targets:** `vradm-core` (Rust C-ABI Thread-Safe Engine), iOS 17+ Client Adapter, Linux / Asterisk 20+ PBX Gateway Daemon

---

## 1. System Architecture & Process Boundaries

V-RADM provides transparent application connectivity across speech-compressed cellular voice channels (VoLTE, VoNR, 3G AMR, carrier VoIP) and acoustic air gaps using IPv4 packet transport for UDP and split-connection PEP transport for TCP. It enables unmodified network applications—specifically **OpenSSH** and **Mosh (Mobile Shell)**—to operate reliably under severe bandwidth, latency, and transcoding constraints.

To eliminate TCP congestion window collapse over high-latency and half-duplex links (such as MCS 0 with multi-second RTOs), V-RADM implements an embedded **Split-Connection Performance Enhancing Proxy (TCP-PEP)** adhering to RFC 3135.

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
 │                                 │ Lock-Free SPSC Ring Buffer           │
 │                                 │ (App Group Shared Memory / mmap)     │
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

<br>Bit [3]: TDD Turn Flag (`1` = Yield physical channel to peer / end of burst, `0` = Contiguous burst frame follows)<br>

<br>Bits [2..0]: Wire Protocol Version (`010` = v3.8) |
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
* **Byte 0x07:** `CCF_MAC` (Truncated 8-bit SipHash-2-4 MAC computed over `SESSION_EPOCH || LE16(c_req) || CCF_CTRL || ACK_BASE || ACK_MAP || CCF_CRC16` using $K_{\text{CTRL\_MAC}}$, binding the control frame to the active session epoch and the full 16-bit monotonic control counter of the burst being acknowledged; see §4.0 and §4.1). Frames failing MAC validation are dropped before state-machine ingestion (see §4.1 for unauthenticated frame handling and noise separation).
* **Bytes 0x08..0x0F:** Systematic $\text{RS}(16, 8)$ Galois field parity covering bytes `0x00..0x07` (8 parity bytes correcting up to $t = 4$ erroneous bytes).
* **Total CCF Duration at MCS 0:** $\frac{128\text{ bits}}{4\text{ bits/sym}} \times 50.0\text{ ms} = \mathbf{1.6\text{ seconds}}$.

---

## 3. Physical Layer (PHY) & Sample-Exact Modulation

### 3.1 Closed-Form MCS Specifications

```
 ┌──────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
 │                                          CLOSED-FORM MCS SPECIFICATIONS                                          │
 ├──────┬──────────┬────────┬───────────┬─────────────┬──────────┬────────────┬──────────┬─────────────┬──────────────┤
 │ MCS  │ Nature   │ Baud   │ Alphabet  │ Independent │ Bits/Sym │ Raw PHY    │ Max L3   │ PLCP-Adj L3 │ Empirical    │
 │      │          │ (Bd)   │ Size (Y)  │ Dims (Z)    │ (X)      │ Rate (bps) │ Rate(bps)│ Ceiling(bps)│ Planning(bps)│
 ├──────┼──────────┼────────┼───────────┼─────────────┼──────────┼────────────┼──────────┼─────────────┼──────────────┤
 │ 0    │ Feature  │ 20     │ 16        │ 1           │ 4        │ 80.0       │ 46.25    │ 43.56 / 32.3│ ~24.0        │
 │ 1    │ Feature  │ 50     │ 256       │ 3           │ 8        │ 400.0      │ 231.25   │ 225.04      │ ~155.0       │
 │ 2*   │ Hybrid   │ 100    │ 256       │ 4           │ 8        │ 800.0      │ 455.38   │ 431.92      │ ~330.0       │
 │ 3*   │ Coherent │ 200    │ 65,536    │ 8           │ 16       │ 3,200.0    │ 1,793.94 │ 1,477.69    │ ~920.0       │
 │ 4*   │ Waveform │ 250    │ 65,536    │ 8           │ 16       │ 4,000.0    │ 2,242.42 │ 1,769.14    │ ~1,450       │
 └──────┴──────────┴────────┴───────────┴─────────────┴──────────┴────────────┴──────────┴─────────────┴──────────────┘
 *Note: MCS 2, MCS 3, and MCS 4 are experimentally gated modes: operational enablement requires passing TC-04a (Balanced Cellular Gate for MCS 2), TC-04b (Wideband Cellular Gate for MCS 3), and TC-11 (G.711 VoIP Gate for MCS 4) respectively.
```

* **Framing Structure (1 Differential Reference Symbol Per Frame):**
  * In phase-modulated modes (MCS 2, 3, 4), Symbol 0 ($m=0$) is reserved as a known differential reference symbol ($\phi_k(0) = \frac{k\pi}{4}$), resolving differential quadrant bootstrapping deterministically. Data symbols span $m = 1 \dots N_{\text{data}}$:
    * **MCS 2:** 1 reference symbol + 64 data symbols = 65 symbols ($650.0\text{ ms}$). Unadjusted Max L3 rate = $296\text{ bits} / 0.650\text{ s} = \mathbf{455.38\text{ bps}}$.
    * **MCS 3:** 1 reference symbol + 32 data symbols = 33 symbols ($165.0\text{ ms}$). Unadjusted Max L3 rate = $296\text{ bits} / 0.165\text{ s} = \mathbf{1,793.94\text{ bps}}$.
    * **MCS 4:** 1 reference symbol + 32 data symbols = 33 symbols ($132.0\text{ ms}$). Unadjusted Max L3 rate = $296\text{ bits} / 0.132\text{ s} = \mathbf{2,242.42\text{ bps}}$.
* **PLCP-Adjusted L3 Ceiling:** Accounts for the mandatory $565.0\text{ ms}$ PLCP control beacon emitted at session start, turn boundaries, and every 16 frames ($16 \times T_{\text{frame}} + 0.565\text{ s}$). For MCS 0, reflects 7-frame burst ($43.56\text{ bps}$, $2,072\text{ bits} / 47.565\text{ s}$) versus single-frame turn ($32.30\text{ bps}$, $296\text{ bits} / 9.165\text{ s}$).
* **Empirical Planning Target:** Realistic end-to-end goodput estimate under representative channel packet error rates ($P_{\text{FER}} \le 1.0 \times 10^{-4}$) and TCP-PEP pacing.

### 3.2 Modulator Implementations & Continuous Sample-Slip Protection

#### Deterministic PRBS-7 Phase Dither (Anti-Gating / Anti-AGC)

Smartphone baseband AGC and noise-suppression algorithms treat stationary multi-tone carriers as acoustic whistle or background hum, suppressing them mid-frame. To prevent gating without degrading coherent demodulation:

* **Non-Circular Seeding:** To eliminate circular bootstrap dependencies (where the receiver would need to decode the frame to extract `SEQ` before it could subtract dither), the PRBS-7 generator ($x^7 + x^6 + 1$) is initialized using state already known from the preceding PLCP beacon and transmission context:

$$\text{PRBS\_SEED} = \left[(\text{BEAC\_SEQ} \oplus (\text{DIR} \ll 7) \oplus (\text{FRAME\_INDEX} \cdot 17)) \pmod{127}\right] + 1$$

Where:
* `BEAC_SEQ` is the 8-bit beacon sequence from the verified PLCP header.
* `DIR` $\in \{0, 1\}$ indicates direction (`0` = Gateway/PBX to Client, `1` = Client to Gateway).
* `FRAME_INDEX` $\in [0..15]$ is the zero-indexed frame counter within the 16-frame continuous sync interval.
* The $+ 1$ offset guarantees a non-zero initial state in $[1..127]$.

* **Symbol-Rate Stepping & Raised-Cosine Interpolation:**
  * `DITHER_UPDATE_RATE`: The PRBS-7 generator steps once per symbol period ($T_{\text{sym}}$), producing a discrete target phase perturbation $\Delta \theta_{\text{raw}}[m] \in \left[-\frac{\pi}{16}, +\frac{\pi}{16}\right]$.
  * **Continuous Transition Interpolation:** To eliminate high-frequency spectral splatter caused by abrupt sample-level phase discontinuities, phase transitions are smoothed sample-by-sample across symbol boundaries using the raised-cosine edge window ($L = 4\text{ samples at } 8\text{ kHz} = 0.5\text{ ms}$):

$$\Delta \theta_{\text{dither}}(n) = \Delta \theta_{\text{raw}}[m-1] + (\Delta \theta_{\text{raw}}[m] - \Delta \theta_{\text{raw}}[m-1]) \cdot \frac{1}{2}\left[1 - \cos\left(\frac{\pi (n_{\text{edge}} + 0.5)}{L}\right)\right]$$

  * This bounds out-of-band phase-modulation sidebands to $\le -40\text{ dBc}$ while empirically suppressing cellular baseband stationary-whistle gating across standard 3GPP VAD models (empirically verified in TC-05).
  * On the receiver, the synchronized PRBS-7 generator serves as a known prior/reference signal rather than an assumed linear sample-level cancellation through the non-linear ACELP vocoder. In the carrier tracking domain, the expected 4th-power phase rotation $4 \Delta \theta_{\text{dither}}[m]$ is subtracted from $\angle q_k$ prior to inter-carrier timing slope estimation and demapping.

#### Mid-Frame Continuous Sample-Slip Recovery: Multi-Carrier 4th-Power DLL (NDA)

To prevent sample slips across asynchronous clock boundaries (`AVAudioEngine` vs. cellular baseband) from misaligning OFDM bins or DQPSK symbols between PLCP beacons without sacrificing data throughput:

* **Timing Recovery Architecture Scope by Active MCS:**
  * **MCS 2 & MCS 3:** Primary symbol timing tracking is executed by the 4th-power inter-carrier phase-slope detector ($\hat{\tau} \propto -\partial\angle q_k/\partial f_k$) paired with complex baseband transition tracking on $u_k[n]$.
  * **MCS 4:** Primary symbol timing tracking is executed by the Cyclic Prefix (CP) cross-correlator on every 4.0 ms slot ($0.5\text{ ms}$ CP correlation peak). Because CP-OFDM subcarriers are demodulated in baseband FFT bins, 4th-power phase-slope timing tracking is disabled during MCS 4 operation.

1. **Analytic Signal Generation & 4th-Power Baseband Chain:** Rather than sacrificing a dedicated subcarrier to an unmodulated pilot tone—which would degrade bit-loading—all subcarriers carry 2-bit DQPSK data throughout the payload ($Z=4$ for MCS 2, $Z=8$ for MCS 3). Carrier and timing tracking relies on **4th-Power Non-Data-Aided (NDA)** processing:
   * **Analytic Bandpass Pre-Filtering:** To prevent unrejected negative-frequency image distortion at $-2f_k$ upon complex downmixing, the received real PCM audio $x[n]$ is converted to an analytic signal centered at subcarrier $k$:
     $$z_k[n] = \operatorname{BPF}^{\text{analytic}}_k\{x[n]\} = \operatorname{BPF}_k\{x[n]\} + j \cdot \mathcal{H}\{\operatorname{BPF}_k\{x[n]\}\}$$
     where $\mathcal{H}\{\cdot\}$ is the Hilbert transform (yielding an analytic bandpass signal with $\ge 40\text{ dB}$ image rejection).
   * **Complex Baseband Downmixing:** The analytic subcarrier is downmixed to complex baseband:
     $$u_k[n] = z_k[n] \cdot e^{-j \frac{2\pi f_k n}{F_s}}$$
   * **4th-Power Non-Linearity:** The complex baseband signal is raised to the 4th power:
     $$q_k[n] = (u_k[n])^4$$
   * **Phase-Error Interpretation:** For ideal 4-state phase modulation, the fourth-power operation removes the discrete QPSK phase state ($4 \cdot \Delta \phi_k \equiv 0 \pmod{2\pi}$); residual channel phase, frequency offset, noise, and dither remain. Thus, $q_k[n]$ is a **near-DC phase-error reference**, where sample-timing error manifests as a steady-state phase error proportional to subcarrier frequency $f_k$.
   * **DQPSK Ambiguity Synergy:** Stripping 4-phase modulation via 4th-power nonlinearity inherently produces a four-fold ($\pm 90^\circ, \pm 180^\circ$) phase ambiguity. V-RADM's deliberate selection of **Differential QPSK (DQPSK)** renders this ambiguity completely moot: data is encoded strictly in the phase transition between adjacent symbols ($\Delta \phi = \text{wrap}_{2\pi}(\phi(m) - \phi(m-1))$), guaranteeing that the four-fold quadrant ambiguity cancels out algebraically without requiring pilot tones or absolute phase reference tracking.

2. **Inter-Carrier Phase-Slope Symbol Timing Detector:**
   * A flat, constant-amplitude near-DC signal provides no energy transition for Early-Prompt-Late correlation. Instead, V-RADM extracts symbol timing directly from the **inter-carrier phase slope** across the subcarrier array.
   * Because 4th-power wipes the data constellation, each subcarrier's 4th-power phase is related to subcarrier frequency $f_k$ and timing error $\tau$ by:
     $$\angle q_k \approx \theta_0 - 8\pi f_k \tau + 4 \Delta \omega_c t$$
   * The derivative of 4th-power phase with respect to subcarrier frequency is directly proportional to the timing error:
     $$\frac{\partial \angle q_k}{\partial f_k} \approx -8\pi \tau$$
   * The receiver unwraps $\angle q_k$ across all active subcarriers ($Z=4$ for MCS 2, $Z=8$ for MCS 3) and computes the linear regression slope to estimate fractional timing delay $\hat{\tau}$:
     $$\hat{\tau} = -\frac{1}{8\pi} \frac{\sum_{k=0}^{Z-1} (f_k - \bar{f})(\angle q_k - \overline{\angle q})}{\sum_{k=0}^{Z-1} (f_k - \bar{f})^2}$$
     where $\bar{f} = \frac{1}{Z}\sum f_k$ and $\overline{\angle q} = \frac{1}{Z}\sum \angle q_k$.
   * This provides a continuous, highly sensitive symbol timing discriminator across all multicarrier modes without requiring dedicated pilot tones.

3. **Phase-Aligned Coherent Carrier Combining (Maximum Ratio Combining):**
   * Before summing across subcarriers for residual carrier frequency tracking, the timing-induced phase offset is removed from each carrier:
     $$\tilde{q}_k[n] = q_k[n] \cdot e^{j 8\pi f_k \hat{\tau}}$$
   * The phase-aligned residuals are combined using Maximum Ratio Combining (MRC) weights $w_k \propto \frac{|\mu_k|}{\sigma_k^2}$ based on measured post-4th-power SNR:
     $$\bar{q}[n] = \sum_{k=0}^{Z-1} w_k \cdot \tilde{q}_k[n]$$
   * Residual carrier frequency offset is tracked from the unwrapped phase rate: $\Delta f_0 = \frac{1}{8\pi} \frac{d}{dt} \text{unwrap}(\angle \bar{q}[n])$.
   * *The theoretical coherent combining upper bound is $10 \log_{10}(Z)$ (+6.0 dB for MCS 2, +9.0 dB for MCS 3); actual measured combining gain under speech codec distortion is governed by TC-10b.*

4. **Mandatory 4th-Power Primary Loop (Anti-Cascade Protection):** The 4th-power NDA loop is mandated as the primary, unconditional timing-error detector. Sliced decision-directed (DD) modulation wiping is strictly prohibited as a coequal primary loop to prevent catastrophic **decision-directed loss-of-lock cascades** near the demotion threshold (where tentative symbol errors inject corrupt phase into the DLL, destabilizing timing and triggering burst demodulation collapse). DD tracking may only be enabled as an optional fine-tracking refinement in high-SNR regimes ($M \ge 0.85$).

5. **Symbol-Boundary Transient Exclusion Zone (Filter Settling & Window Derivation):** Phase transitions between adjacent DQPSK symbols are shaped by the raised-cosine edge window $w(n)$ ($L = 4\text{ samples at } 8\text{ kHz} = 0.5\text{ ms}$) and filtered through the receiver's linear-phase FIR subcarrier separation bandpass filters ($\operatorname{BPF}_k$). The total transient duration at each symbol boundary is the sum of the edge-shaping window $L$ and the filter group delay settling time $\tau_g$:
   * **MCS 2 ($T_{\text{sym}} = 80\text{ samples}$, 100 Bd):** With 400 Hz carrier spacing, subcarrier separation uses an order-24 linear-phase FIR filter ($\tau_g = 12\text{ samples} = 1.5\text{ ms}$). Transients persist for $L + \tau_g = 4 + 12 = \mathbf{16\text{ samples}}$ ($2.0\text{ ms}$). To guarantee zero ISI and filter settling contamination in the timing recovery loop, the receiver excludes the initial 16 samples ($n \in [0, 15]$) and trailing 15 samples ($n \in [65, 79]$), constraining phase-slope evaluation to the interior steady-state quiescent window $n \in [16, 64]$ (49 quiescent samples).
   * **MCS 3 ($T_{\text{sym}} = 40\text{ samples}$, 200 Bd):** With 200 Hz carrier spacing on 5 ms ACELP subframe boundaries, the optimized order-16 FIR filter has group delay $\tau_g = 8\text{ samples}$ ($1.0\text{ ms}$). Boundary transients persist for $L + \tau_g = 4 + 8 = \mathbf{12\text{ samples}}$ ($1.5\text{ ms}$). The receiver excludes samples $n \in [0, 11]$ and $n \in [29, 39]$, constraining phase-slope evaluation strictly to the central quiescent window $n \in [12, 28]$ (17 quiescent samples).

6. **PLCP Bootstrap Handoff Tolerances:** The PLCP beacon trains coarse timing and frequency prior to payload handoff. To ensure the 4th-power payload tracking loop converges within its pull-in range, the PLCP receiver must deliver:
   * Maximum residual timing error at handoff: $|\Delta t_{\text{handoff}}| \le 2.0\text{ samples}$ ($0.25\text{ ms}$ at $8\text{ kHz}$, well within the Farrow interpolator's $\pm 8\text{ sample}$ pull-in range).
   * Maximum residual carrier frequency offset: $|\Delta f_{\text{handoff}}| \le \pm 12.5\text{ Hz}$ (well within the $\pm 25\text{ Hz}$ pull-in range of 100/200 Bd DQPSK).
   * The PLCP 2-FSK receiver must achieve $|\Delta t| \le 1.0\text{ sample}$ and $|\Delta f| \le \pm 5.0\text{ Hz}$ at $E_b/N_0 \ge 6.0\text{ dB}$.

7. **Sub-Sample Farrow Resampler & Loop Filter:** The estimated timing error $\hat{\tau}$ is filtered through a 2nd-order proportional-integral (PI) loop filter driving a cubic Farrow interpolator. The resampler dynamically shifts sampling delay by $\pm 1$ sample within $\le 2.5\text{ ms}$, preserving constellation tracking without dropping frames. Across symbol boundaries ($L = 4$ edge window), baseband transition tracking (Gardner TED operating at 2 samples per symbol) provides secondary clock verification.

8. **MCS 4 CP Correlator:** Cyclic prefix cross-correlation evaluates timing drift on every 4.0 ms slot, eliminating cumulative timing error.

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
* Phase Offset ($\delta$, 2 bits): $\delta \in \{0, 1, 2, 3\}\text{ samples}$. Pulse positions are generated by a periodic pitch excitation pulse train; $\delta$ provides a fractional phase offset relative to the frame origin, emulating the excitation phase of cellular ACELP codebooks without forcing artificial subframe truncation.

* **Formant Filter ($h_{\text{vowel}}(n)$):** Direct-form II cascaded biquad IIR filter ($Q = 5.0$) modeling 8 vowel states:

$$\{(300, 900), (350, 1400), (450, 1100), (500, 1700), (600, 1200), (650, 1900), (750, 1300), (800, 2100)\}\text{ Hz}$$

Digital biquad coefficients for each resonance $F_c$:

$$\omega_0 = \frac{2\pi F_c}{F_s}, \quad \alpha = \frac{\sin(\omega_0)}{2Q}$$

$$b_0 = \alpha, \quad b_1 = 0, \quad b_2 = -\alpha, \quad a_0 = 1 + \alpha, \quad a_1 = -2\cos(\omega_0), \quad a_2 = 1 - \alpha$$

* **Coefficient Normalization ($a_0 = 1.0$):** Implementation filters normalize all transfer function coefficients by $a_0$:
  $$\tilde{b}_0 = \frac{b_0}{a_0}, \quad \tilde{b}_1 = \frac{b_1}{a_0} = 0, \quad \tilde{b}_2 = \frac{b_2}{a_0}, \quad \tilde{a}_1 = \frac{a_1}{a_0}, \quad \tilde{a}_2 = \frac{a_2}{a_0}, \quad \tilde{a}_0 = 1.0$$
* **Filter State Continuity:** In Direct-Form II realizations, internal delay states ($w[n-1], w[n-2]$) are maintained continuously across consecutive symbols within the same frame to preserve vocal tract phase coherence across speech atom transitions. States are reset to zero at frame boundaries.
* **Synthesis:** $s(n) = w(n) \cdot [e(n) * h_{\text{vowel}}(n)]$.

#### MCS 2: Balanced Cellular (Experimentally Gated Hybrid)

* **Alphabet:** $Y = 256$ states ($8\text{ bits/symbol}$ at $100\text{ Bd}$, $T_{\text{sym}} = 10.0\text{ ms} = 80\text{ samples at } 8\text{ kHz}$).
* **Carrier Frequencies ($Z = 4$):** $f_k \in \{600, 1000, 1400, 1800\}\text{ Hz}$.
* **Framing & Reference Symbol:** Each frame comprises 65 symbols ($650.0\text{ ms}$): Symbol 0 ($m=0$) is a known differential reference symbol ($\phi_k(0) = \frac{k\pi}{4}$); Symbols $m = 1 \dots 64$ carry the 512 bits (64 bytes) of protected payload and FEC.
* **Sample-Exact Modulator:**

$$s(n) = w(n) \sum_{k=0}^{3} A_k \cos\left(\frac{2\pi f_k n}{F_s} + \phi_k(m) + \Delta \theta_{\text{dither}}(n)\right)$$

$$\phi_k(m) = \text{wrap}_{2\pi}\left(\phi_k(m-1) + \Delta \phi_k(m)\right), \quad \Delta \phi_k \in \left\{0, \frac{\pi}{2}, \pi, \frac{3\pi}{2}\right\}$$

Where $A_k = [0.8, 1.0, 0.9, 0.7]$.

#### MCS 3: Wideband Cellular Cabled (Experimentally Gated Coherent)

* **Alphabet:** $Y = 65,536$ states ($16\text{ bits/symbol}$ at $200\text{ Bd}$, $T_{\text{sym}} = 5.0\text{ ms} = 40\text{ samples at } 8\text{ kHz}$).
* **Carrier Frequencies ($Z = 8$):** $f_k = k \cdot 200\text{ Hz}$ for $k \in [3..10]$ ($600\text{ Hz to } 2000\text{ Hz}$).
* **Framing & Reference Symbol:** Each frame comprises 33 symbols ($165.0\text{ ms}$): Symbol 0 ($m=0$) is a known differential reference symbol ($\phi_k(0) = \frac{k\pi}{4}$); Symbols $m = 1 \dots 32$ carry the 512 bits (64 bytes) of protected payload and FEC.
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

* **Sample-Exact Cyclic Prefix Prepending:**
  For each 28-point real IFFT output $x[0 \dots 27]$, the 32-sample transmitted slot is constructed by prepending the final $N_{\text{cp}} = 4$ samples:
  $$x_{\text{slot}}[n] = \begin{cases} x[24 + n], & 0 \le n < 4 \\ x[n - 4], & 4 \le n < 32 \end{cases}$$
  yielding $x_{\text{slot}} = [x[24], x[25], x[26], x[27], x[0], x[1], \dots, x[27]]$.
* **Framing & Reference Slot Construction:**
  Each frame comprises 33 OFDM slots ($132.0\text{ ms}$): Slot 0 ($m=0$) is a known differential reference slot ($D_k(0) = 1 + j$); Slots $m = 1 \dots 32$ carry the 512 bits (64 bytes) of protected payload and FEC. The reference slot undergoes identical 28-point IFFT modulation, cyclic prefix prepending, and peak-safe normalization as data slots $m = 1 \dots 32$.
* **Telephone-Band Carrier Allocation ($Z = 8$ Active Carriers):**

$$k \in \{2, 3, 5, 6, 7, 8, 9, 10\} \implies f_k \in \{571.4, 857.1, 1428.6, 1714.3, 2000.0, 2285.7, 2571.4, 2857.1\}\text{ Hz}$$

* **Demodulation:** Differential QPSK across consecutive OFDM symbol slots eliminates the requirement for absolute carrier-phase channel estimation under the assumption of sufficiently slow channel variation.

### 3.3 Output Level Conditioning & Peak-Safe Normalization

Multi-carrier waveforms exhibit crest factors (Peak-to-Average Power Ratio) that vary dramatically by mode. Applying a uniform RMS normalization with hard clipping will substantially clip 4-carrier and 8-carrier waveforms (e.g. MCS 2 coherent worst-case crest factor is ~8.96 dB and MCS 3 is ~11.52 dB, whereas a naive -9 dBFS RMS clamped at -6 dBFS peak permits only 3.01 dB crest factor). To guarantee clean modulation reproduction without clipping:

1. **MCS-Dependent Target RMS:** Audio blocks are scaled according to mode-specific crest-factor budgets:

| Active MCS | Modulator Nature | Max Crest Factor ($CF_{\text{max}}$) | Target RMS ($V_{\text{target\_rms}}$) | Target RMS Level | Max Peak Ceiling | Linear Digital Headroom |
|---|---|---|---|---|---|---|
| **MCS 0** | Single tone pitch | ~3.01 dB | 0.3535 FS | -9.03 dBFS | $\le 0.50\text{ FS}$ | 3.01 dB |
| **MCS 1** | Formant speech atom | ~7.0 dB | 0.2239 FS | -13.00 dBFS | $\le 0.50\text{ FS}$ | 3.01 dB |
| **MCS 2** | 4-carrier DQPSK | ~8.96 dB | 0.1778 FS | -15.00 dBFS | $\le 0.50\text{ FS}$ | 3.01 dB |
| **MCS 3** | 8-carrier DQPSK | ~11.52 dB | 0.1334 FS | -17.50 dBFS | $\le 0.50\text{ FS}$ | 3.01 dB |
| **MCS 4** | 8-carrier CP-OFDM | ~10.0 dB | 0.1585 FS | -16.00 dBFS | $\le 0.50\text{ FS}$ | 3.01 dB |

2. **Peak-Constrained Normalization:** The linear scaling factor $g$ is constrained by both the mode target RMS and the linear peak headroom target ($V_{\text{target\_peak}} = 0.45\text{ FS}$ / $-6.93\text{ dBFS Peak}$):

$$g = \min\left( \frac{V_{\text{target\_rms}}(\text{MCS})}{\sqrt{\frac{1}{N}\sum_{m=0}^{N-1} s_{\text{raw}}^2[m]}}, \; \frac{V_{\text{target\_peak}}}{\max_{0 \le m < N} |s_{\text{raw}}[m]|} \right), \quad \text{where } V_{\text{target\_peak}} = 0.45\text{ FS}$$

$$s[n] \leftarrow s_{\text{raw}}[n] \cdot g$$

3. **Hyperbolic Soft-Saturation Safety Limiter:** To eliminate the severe high-order harmonic distortion spurs generated by rectangular hard clippers, any residual exceptional peak transients exceeding $V_{\text{target\_peak}}$ are conditioned through a hyperbolic tangent soft limiter anchored at hard safety ceiling $V_{\text{peak\_max}} = 0.50\text{ FS}$ ($-6.02\text{ dBFS Peak}$):

$$s_{\text{out}}[n] \leftarrow V_{\text{peak\_max}} \cdot \tanh\left(\frac{s[n]}{V_{\text{peak\_max}}}\right), \quad \text{where } V_{\text{peak\_max}} = 0.50\text{ FS}$$

This ensures that output samples are strictly bounded within $[-0.5\text{ FS}, +0.5\text{ FS}]$ ($-6.02\text{ dBFS Peak}$), providing $3.01\text{ dB}$ of analog DAC headroom without hard clipping multicarrier symbols.

* **Intermodulation Distortion (IMD), Limiter Backstop Scoping & Saturation Metric:** Hyperbolic tangent soft saturation is inherently non-linear. When applied to composite multicarrier waveforms (MCS 2, 3, 4), any limiter activation generates intermodulation distortion (IMD) products that fall directly on or near active subcarriers, degrading Error Vector Magnitude (EVM).
  * **Limiter Saturation Operational Definition:** An audio sample $n$ is formally defined as a **limiter saturation event** if and only if pre-saturation $|s[n]| > V_{\text{target\_peak}} = 0.45\text{ FS}$ (entering the non-linear compression regime of the $\tanh$ curve).
  * **Acceptance Criterion:** Because the linear gain scaling stage ($g \le \frac{0.45}{\max |s_{\text{raw}}|}$) is sized with crest-factor margin, the $\tanh$ limiter acts strictly as an anomalous transient safety backstop. Under normal modulation, limiter saturation events SHALL occur on $< 0.05\%$ of samples, bounding IMD-induced EVM degradation to $\le 0.5\text{ dB}$ (empirically verified in TC-10c).

---

## 4. Keyed Integrity-Protected Control Plane & PLCP Architecture

The receiver never inspects data frames to determine modulation parameters. Every burst is preceded by an **Integrity-Protected PLCP Control Beacon**.

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|      BARKER-13 DUAL-CHIRP     |CUR_MCS|REQ_MCS|TX_PWR |BEAC_SEQ|
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|  BEACON_MAC8  | GOLAY CODEWORD 1 (24 Bits) / CODEWORD 2 (24B) |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+

```

### 4.0 Session Epoch Bootstrap Handshake & Re-Keying Lifecycle

Prior to transmitting user data or adapting modulation rates, endpoints establish cryptographic session state via a deterministic, two-way bootstrap handshake over the physical layer:

```
 Initiator (Node A)                                    Responder (Node B)
 ┌─────────────────┐                                   ┌─────────────────┐
 │ Generates R_A   │                                   │                 │
 └────────┬────────┘                                   └────────┬────────┘
          │                                                     │
          │ 1. SESSION_REQUEST (32-Byte Frame: R_A + MAC)       │
          ├────────────────────────────────────────────────────►│
          │                                            ┌────────┴────────┐
          │                                            │ Generates R_B   │
          │                                            │ Computes EPOCH  │
          │                                            └────────┬────────┘
          │ 2. SESSION_ACCEPT (32-Byte Frame: R_B + MAC)        │
          │◄────────────────────────────────────────────────────┤
 ┌────────┴────────┐                                            │
 │ Computes EPOCH  │                                            │
 │ Ready (State=0) │                                            │
 └─────────────────┘                                            │
```

1. **Exact 32-Byte Bootstrap Frame Layout:**
   ```
    0                   1                   2                   3
    0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
   +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
   |   MSG_TYPE    |             NONCE (R_A / R_B, 16 Bytes)       |
   +-+-+-+-+-+-+-+-+                                               +
   |                                                               |
   |                                                               |
   |                               +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
   |                               |          EPOCH_HINT           |
   +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
   |          EPOCH_HINT           |   BOOTSTRAP_MAC (8 Bytes)     |
   +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+                               +
   |                                                               |
   |                               +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
   |                               |  BOOTSTRAP_CRC16  | RESERVED  |
   +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
   ```
   * **Byte 0x00:** `MSG_TYPE` (`0xBE` = `SESSION_REQUEST`, `0xBF` = `SESSION_ACCEPT`).
   * **Bytes 0x01..0x10:** `NONCE` (128-bit CSPRNG nonce $R_A$ or $R_B$).
   * **Bytes 0x11..0x14:** `EPOCH_HINT` (`0x00000000` in request; computed `SESSION_EPOCH` in accept).
   * **Bytes 0x15..0x1C:** `BOOTSTRAP_MAC` (64-bit truncated SipHash-2-4 computed over bytes `0x00..0x14` using PSK).
   * **Bytes 0x1D..0x1E:** `BOOTSTRAP_CRC16` (CRC-16-CCITT covering bytes `0x00..0x1C`).
   * **Byte 0x1F:** `RESERVED` (`0x00`).
   * **Modulation & Timing:** Modulated via robust MCS 0 (or 2-FSK at 100 Bd). Handshake retransmission timeout $T_{\text{boot\_rto}} = 1.5\text{ seconds}$ with binary exponential backoff ($1.5\text{ s}, 3.0\text{ s}, 6.0\text{ s}$; up to 3 retries).
2. **Simultaneous Initiation Tie-Breaking:**
   * If both endpoints transmit `SESSION_REQUEST` concurrently, each node compares $\text{uint128}(R_A)$ with $\text{uint128}(R_{\text{received}})$.
   * The node presenting the numerically greater nonce ($\text{uint128}(R) > \text{uint128}(R_{\text{peer}})$) is designated the Initiator.
   * The node with the smaller nonce immediately yields, drops its pending request, treats the incoming request as valid, and responds with `SESSION_ACCEPT`.
3. **Session Epoch & Control Key Derivation (BLAKE3 KDF):**
   * Rather than invoking keyed hashing with a 16-byte key (which is invalid because BLAKE3 keyed mode requires exactly 32 bytes), endpoints utilize BLAKE3 Key Derivation Function (KDF) mode with application context strings:
     $$\begin{aligned}
     K_{\text{SESSION}} &= \text{BLAKE3\_derive\_key}\Big(\text{"VRADM-v3.8-SESSION-EPOCH"}, \; \text{PSK} \,\|\, R_A \,\|\, R_B\Big) \\
     \text{SESSION\_EPOCH} &= \text{LE32}(K_{\text{SESSION}}[0..3]) \\
     K_{\text{CTRL\_MAC}} &= \text{BLAKE3\_derive\_key}\Big(\text{"VRADM-v3.8-CONTROL-MAC"}, \; \text{PSK} \,\|\, \text{LE32}(\text{SESSION\_EPOCH})\Big)[0..15]
     \end{aligned}$$
   * $K_{\text{CTRL\_MAC}}$ is a distinct 128-bit key dedicated exclusively to control-plane SipHash-2-4 MAC evaluations, eliminating cross-primitive key reuse between BLAKE3 and SipHash.
4. **Counter Initialization:** Upon deriving `SESSION_EPOCH`:
   * Transmitter and receiver `CTRL_COUNTER` registers are reset to 0.
   * $c_{\text{rx\_max}}$ is reset to 0.
   * `ANTI_REPLAY_WINDOW` bitmap is cleared.
5. **Anti-Rollover Re-Keying Policy:**
   * To prevent counter wrap-around vulnerabilities on the 16-bit `CTRL_COUNTER` ($2^{16} = 65,536$), endpoints track the total beacons emitted in the active session.
   * When `CTRL_COUNTER` reaches **60,000**, the active node automatically initiates a new `SESSION_REQUEST` / `SESSION_ACCEPT` handshake cycle.
   * A fresh `SESSION_EPOCH` is derived, the counter resets to 0, and data transfer resumes seamlessly without dropping transport-layer connections.

### 4.1 Control Plane Integrity, Anti-Replay & Threat Model Scoping (Anti-DoS)

To prevent unauthorized over-the-air injection of corrupted PLCP commands, accidental cross-talk, stale replayed bursts, or malicious downgrade requests:

* **Keyed Integrity Protection:** Endpoints are provisioned with a 128-bit Pre-Shared Key (PSK) from which session keys are derived via BLAKE3 KDF (§4.0).
* **Session Freshness & Anti-Replay State:**
  * **`SESSION_EPOCH`:** A 32-bit random session identifier established during link handshake (§4.0), binding all control frames to the active session.
  * **`CTRL_COUNTER`:** A 16-bit monotonically increasing control counter tracked per direction, incremented on every transmitted PLCP control beacon. The low 8 bits are transmitted on the wire as `BEAC_SEQ = (uint8_t)(CTRL_COUNTER & 0xFF)`.
* **Deterministic 16-bit `CTRL_COUNTER` Inference Algorithm (RFC 3550 / SRTP):**
  * Because only the low 8 bits of `CTRL_COUNTER` are transmitted on the wire (`BEAC_SEQ`), the receiver deterministically reconstructs the full 16-bit candidate counter $\widehat{c} \in [0..65535]$ relative to its locally tracked highest verified counter $c_{\text{rx\_max}}$ (initialized to 0 at link start) using nearest-value inference:
    $$s_{\text{local}} = c_{\text{rx\_max}} \& 0\text{xFF}$$
    $$\Delta = (v_{\text{wire}} - s_{\text{local}}) \pmod{256}$$
    $$\text{If } \Delta > 128, \quad \Delta \leftarrow \Delta - 256$$
    $$\widehat{c} = \max(0, \min(65535, c_{\text{rx\_max}} + \Delta))$$
  * **Anti-Replay Window Check:**
    * Receivers maintain a 64-sequence sliding window bitmap (`ANTI_REPLAY_WINDOW`).
    * If $\widehat{c} \le c_{\text{rx\_max}} - 64$, or if bit $(c_{\text{rx\_max}} - \widehat{c})$ in the sliding window is already set, the beacon is discarded as an expired or duplicate replay prior to MAC evaluation.
    * Fresh candidates are evaluated against `BEACON_MAC8` using $\widehat{c}$.
    * Upon successful MAC verification:
      * If $\widehat{c} > c_{\text{rx\_max}}$, the sliding bitmap is shifted left by $(\widehat{c} - c_{\text{rx\_max}})$, bit 0 is set to 1, and $c_{\text{rx\_max}} \leftarrow \widehat{c}$.
      * If $\widehat{c} \le c_{\text{rx\_max}}$, bit $(c_{\text{rx\_max}} - \widehat{c})$ is marked as 1.
* **`BEACON_MAC8` & `CCF_MAC` Wire Formulations:**
  * **`BEACON_MAC8`:** An 8-bit truncated **SipHash-2-4** MAC computed over the candidate 16-bit counter $\widehat{c}$ and beacon payload using $K_{\text{CTRL\_MAC}}$:
    $$\text{BEACON\_MAC8} = \text{Trunc8}\Big(\text{SipHash-2-4}_{K_{\text{CTRL\_MAC}}}\big(\text{SESSION\_EPOCH} \,\|\, \widehat{c} \,\|\, \text{CUR\_MCS} \,\|\, \text{REQ\_MCS} \,\|\, \text{TX\_PWR} \,\|\, \text{BEAC\_SEQ}\big)\Big)$$
  * **`CCF_MAC` (16-bit Monotonic Anti-Replay Closure):** An 8-bit truncated **SipHash-2-4** MAC computed strictly over wire and session fields, binding the full 16-bit inferred control sequence $\widehat{c}_{\text{req}}$ of the burst being acknowledged:
    $$\text{CCF\_MAC} = \text{Trunc8}\Big(\text{SipHash-2-4}_{K_{\text{CTRL\_MAC}}}\big(\text{SESSION\_EPOCH} \,\|\, \text{LE16}(\widehat{c}_{\text{req}}) \,\|\, \text{CCF\_CTRL} \,\|\, \text{ACK\_BASE} \,\|\, \text{ACK\_MAP} \,\|\, \text{CCF\_CRC16}\big)\Big)$$
    *Because $\widehat{c}_{\text{req}} \in [0 \dots 59,999]$ is the full 16-bit inferred control counter rather than the 8-bit truncated `BEAC_SEQ`, `CCF_MAC` is mathematically unique across the entire 60,000-beacon lifetime of the session epoch. When Node A receives a CCF, it verifies that $\widehat{c}_{\text{req}}$ matches its locally recorded transmit counter for that burst ($c_{\text{tx\_last}}$). Stale CCFs from earlier turns—even if sharing the same `ACK_BASE` or 8-bit `BEAC_SEQ` wrap—produce mismatched MAC tags and are discarded before state machine processing.*
* **Threat Model, Cryptographic Scoping & Anti-DoS Immunity:**
  * **Elimination of Attacker-Induced Silent Lockout:** Earlier drafts enforced a silent listening backoff upon 3 consecutive MAC failures. In an adversarial wireless or acoustic environment, any unauthenticated attacker could exploit this to execute an availability Denial of Service (DoS)—muting the modem indefinitely by injecting forged frames every 10 seconds. In this specification:
    1. Unauthenticated frames (failing `BEACON_MAC8` or `CCF_MAC`) are **silently dropped** immediately without altering modem state, receiver listening status, or ARQ windows.
    2. The receiver **NEVER** enters a silent listening pause or halts reception on unauthenticated MAC errors.
    3. To prevent CPU exhaustion attacks from high-rate forged frame bursts, MAC verification attempts are throttled by an internal token bucket rate limiter to $\le 10\text{ verification failures per second}$. Frames exceeding this limit are discarded before running SipHash-2-4.
    4. Upon any MAC verification failure, the receiver increments the telemetry counter `security_tamper_detected` and emits an asynchronous warning to the host application.
  * **Strict Separation of Channel Noise from Tamper Failures:**
    To guarantee that ordinary channel fading (such as $P_{\text{FER}} = 10^{-2}$ in TC-02) never registers false tamper alarms, control frames MUST pass physical FEC and CRC integrity checks prior to MAC evaluation:
    1. For PLCP Beacons: If Barker-13 sync or either Extended Golay $[24, 12, 8]$ codeword fails decoding ($>3$ bit errors), the beacon is discarded as an uncorrectable physical layer erasure (`plcp_sync_erasure`). It does NOT count as a MAC failure.
    2. For CCFs: If `SYNC_WORD`, Reed-Solomon $\text{RS}(16, 8)$ decoding ($>4$ byte errors), or CRC-16-CCITT fails, the frame is dropped as a channel transmission error (`crc_failures++`). It does NOT count as a MAC failure.
    3. A frame is counted as a keyed integrity failure (`security_tamper_detected++`) **if and only if** physical FEC and CRC verification succeed, but the SipHash-2-4 MAC check fails.
    4. Because CRC-16 has an undetected channel error probability of $P_{\text{undetected}} \le 2^{-16} \approx 1.5 \times 10^{-5}$, the probability of random channel noise producing a frame that accidentally passes CRC-16 but fails SipHash is negligible ($< 1.5 \times 10^{-5}$ per burst erasure).
  * **L4/L7 Cryptographic Delegation:** `BEACON_MAC8` and `CCF_MAC` are keyed integrity filters and are explicitly NOT intended to provide long-term cryptographic non-repudiation. True cryptographic mutual authentication, anti-forgery, replay defense, and confidentiality are strictly anchored at the application/transport layer via **OpenSSH** (SSH-2 host keys + ChaCha20-Poly1305 / AES-256-GCM authenticated transport) and **Mosh** (128-bit AES-OCB).

### 4.2 Deterministic PLCP Parameters

* `PLCP_CHIP_RATE`: 200 chips/s ($5.0\text{ ms/chip}$).
* `PLCP_PREAMBLE`: 13 chips $\times 5.0\text{ ms} = \mathbf{65.0\text{ ms}}$ (Hyperbolic pitch sweep: $600\text{ Hz} \leftrightarrow 1800\text{ Hz}$).
* `PLCP_GUARD`: $10.0\text{ ms}$ silence before and after header.
* `PLCP_HEADER_PAYLOAD`: 16 bits (`CUR_MCS` [3b], `REQ_MCS` [3b], `TX_PWR` [2b], `BEAC_SEQ` [8b]) + 8-bit `BEACON_MAC8` = **24 information bits**.
* `PLCP_HEADER_FEC`: Two independent Extended Golay $[24, 12, 8]$ codewords (Codeword 1: Bits 0..11; Codeword 2: Bits 12..23). Total encoded header = **48 bits**.
* `PLCP_MODULATION`: 2-FSK ($1200\text{ Hz} = \text{Mark}, 1600\text{ Hz} = \text{Space}$) at $100\text{ Bd}$ ($10.0\text{ ms/bit}$). Total header duration = $48 \times 10.0\text{ ms} = \mathbf{480.0\text{ ms}}$.
* `PLCP_TOTAL_DURATION`: $65.0 + 10.0 + 480.0 + 10.0 = \mathbf{565.0\text{ ms}}$.
* **Cadence:** PLCP is transmitted at `SESSION_START`, `TDD_DATA_TURN`, `CONTINUOUS_SYNC` (every 16 frames), and `MCS_CHANGE`. *Compact Control Frames (CCFs) do NOT require a PLCP beacon.*

### 4.3 Two-Phase MCS Commit Handshake & Lost ACK Recovery

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
 │ at SEQ = S + 1  │                                 │ at SEQ = S + 1  │
 │ (S == ACK_BASE) │                                 │ (S == ACK_BASE) │
 └─────────────────┘                                 └─────────────────┘

```

1. **Announcement:** Node A transmits its current burst at `CUR_MCS`, setting `REQ_MCS = target`.
2. **Commit Ack:** Node B decodes the request, verifies channel metric $M \ge 0.85$ and valid `CCF_MAC`, and responds with a Compact Control Frame setting `CCF_CTRL` to `MCS_COMMIT_ACK` with commit sequence boundary $S \equiv \text{ACK\_BASE}$. The exact sequence boundary for rate switchover is defined as $\text{SEQ} = \text{ACK\_BASE} + 1$.
3. **Synchronous Switchover (Two Generals Resolution):**
   * Node B transitions its payload demodulator for sequences $\ge \text{ACK\_BASE} + 1$ **if and only if** it demodulates a valid PLCP header with `CUR_MCS == target`. Because the PLCP beacon is modulated via noncoherent 2-FSK at 100 Bd, it is universally decodable regardless of active payload mode, serving as the definitive, unambiguous transition trigger.
   * Node A only transitions its payload modulator to `target` for frame sequences starting at $\text{ACK\_BASE} + 1$ after receiving a verified `MCS_COMMIT_ACK` CCF from Node B.
4. **Lost `MCS_COMMIT_ACK` Recovery Policy:**
   * If Node B's CCF is lost, dropped, or corrupted over the air, Node A's retransmission timer ($T_{\text{RTO}}$) expires without receiving an `MCS_COMMIT_ACK`.
   * Node A **MUST NOT** switch to `target`. Node A remains at `CUR_MCS` and re-transmits `REQ_MCS = target` in its next burst.
   * Node A allows up to $N_{\text{mcs\_retry}} = 3$ consecutive attempts.
   * If no valid `MCS_COMMIT_ACK` is received after 3 attempts, Node A aborts the rate adaptation attempt, resets `REQ_MCS = CUR_MCS`, and enforces a mandatory **10.0-second rate-adaptation cooldown timer** before attempting another upshift.
5. **Unilateral Emergency Downshifts:**
   * If channel metrics degrade sharply ($M < 0.60$), downshifting is **unilateral and immediate**. Node A sets `CUR_MCS = lower` directly in its next PLCP beacon without requiring a prior two-phase commit handshake, preventing connection drops under sudden fading.

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

### 5.1 Deterministic TDD Ownership & Burst Pipelining State Machine

1. **Token Ownership:** Initial channel ownership is assigned to the calling gateway (Asterisk PBX / Master node).
2. **Turn Structure (Burst Pipelining):**
   * To prevent stop-and-wait channel starvation while fully exploiting the 7-frame selective-repeat window of `ACK_MAP`, transmitters may emit a contiguous burst of $1 \le N_{\text{burst}} \le 7$ Canonical Data Frames in a single Data Turn:
     * **PLCP Control Beacon:** $565.0\text{ ms}$ (transmitted strictly once at the beginning of the burst).
     * **Pipelined Data Frames:** $N_{\text{burst}} \times 6,400\text{ ms}$ transmitted contiguously with $0\text{ ms}$ inter-frame spacing. Frames $1 \dots N_{\text{burst}}-1$ clear Bit [3] of `CTRL` (`TDD_YIELD = 0`); the final frame $N_{\text{burst}}$ sets `TDD_YIELD = 1`.
     * **End-of-Turn (EOT) Tone:** $150.0\text{ ms}$ dual-tone burst ($1400\text{ Hz} + 1800\text{ Hz}$ at $-12.0\text{ dBFS}$) emitted immediately after the final frame.
     * **Total Data Turn Duration:** $T_{\text{data\_turn}} = 565\text{ ms} + (N_{\text{burst}} \times 6,400\text{ ms}) + 150\text{ ms}$.
       * $N_{\text{burst}} = 1$: **$7,115\text{ ms}$** (single frame).
       * $N_{\text{burst}} = 4$: **$26,315\text{ ms}$** (4-frame burst).
       * $N_{\text{burst}} = 7$: **$45,515\text{ ms}$** (full 256-byte datagram burst).
   * **Control Turn (ACK/Grant):**
     * 1 Authenticated CCF ($1,600\text{ ms}$, modulated at MCS 0, no PLCP required) + EOT Tone ($150\text{ ms}$) = **$1,750\text{ ms}$**.
     * Node B evaluates the received burst and sets `ACK_BASE` and the 7-bit `ACK_MAP` to selectively acknowledge all frames in the burst within this single control turn.
   * **256-Byte Datagram End-to-End Latency at MCS 0:**
     * A full 256-byte IP datagram requires $\lceil 256 / 37 \rceil = 7$ fragments.
     * In burst mode ($N_{\text{burst}} = 7$), the entire datagram is delivered in:
       $$T_{\text{burst\_256B}} = 565\text{ ms} + (7 \times 6,400\text{ ms}) + 150\text{ ms} + 150\text{ ms (guard)} + 1,600\text{ ms (CCF)} + 150\text{ ms} + 150\text{ ms} = \mathbf{47.565\text{ seconds}}$$
       (A 25.9% latency reduction compared to $7 \times 9.165\text{ s} = 64.155\text{ seconds}$ under strict stop-and-wait).
3. **End-of-Turn (EOT) Tone:** A $150.0\text{ ms}$ dual-tone burst ($1400\text{ Hz} + 1800\text{ Hz}$ at $-12.0\text{ dBFS}$) signals token yield.
4. **Acoustic Guard Window:** Delay of exactly **$150.0\text{ ms}$** following EOT allows room reverberation to decay.
5. **Deterministic Collision Recovery & Dynamic Timeout:**
   * Turn timeout is dynamically scaled based on negotiated maximum burst size and rounded up to the nearest integer second:
     $$T_{\text{timeout}} = \left\lceil T_{\text{PLCP}} + (N_{\text{burst\_max}} \times 6,400\text{ ms}) + T_{\text{EOT}} + T_{\text{guard}} + 3,000\text{ ms} \right\rceil_{1.0\text{ s}}$$
     * $N_{\text{burst\_max}} = 1$: $565 + 6,400 + 150 + 150 + 3,000 = 10,265\text{ ms} \implies T_{\text{timeout}} = \mathbf{11.0\text{ seconds}}$.
     * $N_{\text{burst\_max}} = 4$: $565 + 25,600 + 150 + 150 + 3,000 = 29,465\text{ ms} \implies T_{\text{timeout}} = \mathbf{30.0\text{ seconds}}$.
     * $N_{\text{burst\_max}} = 7$: $565 + 44,800 + 150 + 150 + 3,000 = 48,665\text{ ms} \implies T_{\text{timeout}} = \mathbf{49.0\text{ seconds}}$.
   * Upon turn timeout, both nodes enter `SILENT_LISTEN`.
   * Slave node enforces a mandatory backoff window of $4.0\text{ seconds}$.
   * Master node waits $1.5\text{ seconds}$ and re-asserts channel ownership with a standalone PLCP beacon.
6. **Interactive Low-Latency MTU/MSS Adaptation:**
   * For interactive terminal sessions (OpenSSH keystrokes), the TCP-PEP does not wait to assemble full 256-byte datagrams; it immediately flushes individual 37-byte fragments ($N_{\text{burst}} = 1$), achieving single-keystroke turnaround in $9.165\text{ s}$. Bulk file transfers (scp/sftp) dynamically scale up to $N_{\text{burst}} = 7$.
7. **Link-Layer In-Flight ARQ Window Cap ($W_{\text{ARQ}} = 8$):**
   * Because `ACK_MAP` spans 7 bits ($S+1 \dots S+7$) anchored at `ACK_BASE` ($S$), the receiver can acknowledge at most 8 distinct sequence numbers ($S \dots S+7$) in any single CCF.
   * To prevent protocol deadlocks and unacknowledged frame stalls where out-of-range frames cannot be indexed by `ACK_MAP`, the link-layer transmitter MUST strictly enforce a maximum in-flight window across all MCS modes:
     $$W_{\text{ARQ}} \le 8\text{ frames} \quad (296\text{ bytes of L3 payload})$$
   * Regardless of higher-layer PEP queue depths or operating MCS mode, the link layer will never transmit frame $S+8$ until `ACK_BASE` advances.

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

### 6.3 Closed-Form Retransmission Timeout (RTO) & ARQ Window Bounds

$$\text{RTO} = \text{SRTT} + \max(4 \cdot \text{RTTVAR}, T_{\text{frame}}) + T_{\text{margin}}(\text{Profile})$$

* **Profile 1 (Direct Cabled Full-Duplex):** Nominal RTO = **$560\text{ ms}$**.
* **Profile 2 (Free-Air Acoustic Half-Duplex with Compact ACKs):**
  * Single-Frame Cycle: Data Turn ($7.115\text{ s}$) + Guard ($0.15\text{ s}$) + CCF Turn ($1.75\text{ s}$) + Guard ($0.15\text{ s}$) = $9.165\text{ seconds}$. Nominal RTO = **$9.7\text{ seconds}$**.
  * Burst-Pipelined Cycle ($N_{\text{burst}} = 7$): Data Turn ($45.515\text{ s}$) + Guard ($0.15\text{ s}$) + CCF Turn ($1.75\text{ s}$) + Guard ($0.15\text{ s}$) = $47.565\text{ seconds}$. Nominal RTO = **$48.5\text{ seconds}$**.
* **Link-Layer In-Flight ARQ Window Bound ($W_{\text{ARQ}} = 8$):**
  The link-layer ARQ window is strictly bounded by the 7-bit selective repeat bitmap: $W_{\text{ARQ}} \le 8\text{ frames}$ ($296\text{ bytes}$). At MCS 3 ($165.0\text{ ms/frame}$, 33 symbols $\times 5.0\text{ ms}$) and MCS 4 ($132.0\text{ ms/frame}$, 33 slots $\times 4.0\text{ ms}$), $W_{\text{ARQ}} = 8$ frames corresponds to exactly $\mathbf{1.320\text{ seconds}}$ and $\mathbf{1.056\text{ seconds}}$ of continuous transmission respectively, which comfortably exceeds the nominal full-duplex RTO ($560\text{ ms}$), preventing channel starvation while ensuring that every in-flight frame is within the reach of `ACK_MAP`.
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

### 7.1 Split-Connection TCP-PEP Architecture (RFC 3135 §2.4 & §3.1)

V-RADM implements an application-transparent, transport-splitting TCP Performance Enhancing Proxy (TCP-PEP) informed by RFC 3135 §2.4 (Transport-level PEPs) and §3.1 (Split-Connection PEPs). Standard TCP stacks interpret multi-second acoustic RTOs and half-duplex stalls as congestion, collapsing $cwnd$ and retransmitting needlessly. The PEP terminates local TCP loops and feeds the acoustic link layer via an adaptive window:

1. **Local Termination (Client-Side):** The iOS `PacketTunnelProvider` intercepts outbound TCP SYN packets destined for `10.99.0.1:22`. It completes the three-way handshake locally on `utun`, spoofing immediate zero-delay ACKs back to the OpenSSH client.
2. **MCS-Adaptive Window Clamping & Link Window Alignment:** Rather than using a static window clamp—which at MCS 0 represents nearly six minutes of buffered data before backpressure is noticed—the PEP dynamically clamps the advertised window based on the active MCS ladder. To prevent bufferbloat while strictly respecting the link-layer ARQ window ($W_{\text{ARQ}} = 8\text{ frames} = 296\text{ bytes}$), the PEP balances local staging capacity against over-the-air in-flight limits:

| Active MCS | Raw Rate | App Goodput | Target Window Clamp ($W_{\text{clamp}}$) | Local Staging Slices | Max Over-The-Air In-Flight ($W_{\text{ARQ}}$) | Max In-Flight Buffer Time |
|---|---|---|---|---|---|---|
| **MCS 0** | 80.0 bps | ~24.0 bps (3.0 B/s) | **74 bytes** | 2 frames | 2 frames (74 B) | ~18.3 s (~2 TDD burst cycles) |
| **MCS 1** | 400.0 bps | ~155.0 bps (19.4 B/s) | **148 bytes** | 4 frames | 4 frames (148 B) | ~7.8 s |
| **MCS 2** | 800.0 bps | ~330.0 bps (41.2 B/s) | **296 bytes** | 8 frames | 8 frames (296 B) | ~7.2 s |
| **MCS 3** | 3,200.0 bps | ~920.0 bps (115.0 B/s) | **592 bytes** | 16 frames | 8 frames (296 B) | ~5.1 s |
| **MCS 4** | 4,000.0 bps | ~1,450.0 bps (181.2 B/s) | **1,036 bytes** | 28 frames | 8 frames (296 B) | ~5.7 s |

   * TCP window scaling is suppressed across all modes.
   * **Staging vs. Over-The-Air In-Flight Separation:** For high-throughput modes (MCS 3 and MCS 4), $W_{\text{clamp}}$ allows up to 16 and 28 frames in the local PEP socket staging queue to prevent client application write stalls during bulk transfers. However, the link layer transmitter strictly paces transmission so that at most $W_{\text{ARQ}} = 8\text{ frames}$ ($296\text{ bytes}$) are in-flight over the acoustic medium at any time, exactly matching the 7-bit selective repeat coverage of `ACK_MAP`.
   * When the modem transitions MCS, the PEP dynamically updates the Advertised Window field in subsequent spoofed ACKs, bounding the client's unacknowledged queue to between $5\text{ and }18\text{ seconds}$ across the entire rate ladder.
3. **Link Slicing:** Plaintext stream bytes are packed directly into 37-byte fragments (`BEST_EFFORT = 0`) handled by V-RADM's link-layer Selective Repeat ARQ.
4. **Symmetric Server-Side TCP-PEP (`vradmd` $\leftrightarrow$ `sshd`):**
   * The server daemon `vradmd` implements an identical mirrored TCP-PEP terminating the server-facing TCP connection to `127.0.0.1:22` (`sshd`).
   * **Downlink Window Clamping:** The server PEP clamps its advertised window toward `sshd` according to the active downlink MCS using the identical $W_{\text{clamp}}$ table, suppressing window scaling on the `sshd` socket.
   * **Downlink Backpressure Propagation (Zero-Window):** Large server outputs (e.g., terminal screen redraws, `cat`, or directory listings) can rapidly overwhelm acoustic links. When `vradmd`'s acoustic transmit queue exceeds $2 \times W_{\text{clamp}}$, the server PEP advertises a `TCP Zero-Window` (`win = 0`) to `sshd`, immediately blocking `sshd` on `write()`/buffer flush and preventing unbounded socket memory allocation.
   * **Symmetric Rehydration:** Received downlink fragments are reassembled by the client-side PEP in `PacketTunnelProvider` and injected into the local `utun` interface.
5. **UDP Passthrough (Mosh):** Mosh traffic bypasses the TCP-PEP entirely. Packets are flagged with `BEST_EFFORT = 1`, bypassing ARQ retransmissions to let Mosh's state synchronization handle loss natively.

### 7.1.1 TCP-PEP State-Machine Contract (RFC 3135 §2.4 / §3.1 Terminology)

To ensure reliable, deterministic operation across high-latency, asymmetric acoustic links without running an unnecessarily complex TCP stack in kernel or extension space, V-RADM implements an application-transparent split-connection TCP-PEP informed by RFC 3135:

1. **Local Three-Way Handshake Termination (Fast SYN/ACK Spoofing):**
   * Upon intercepting an outbound TCP SYN packet from the local client (`OpenSSH`), the client-side PEP immediately generates a synthetic `SYN+ACK` segment (with zero synthetic RTT, negotiating an RFC 6528 / RFC 9293 cryptographically pseudo-random Initial Sequence Number: $\text{ISN} \leftarrow \text{CSPRNG}(32)$).
   * **TCP Option Stripping:** The PEP forces clean baseline negotiation by stripping `WSCALE` (Window Scale, RFC 7323), `TSopt` (TCP Timestamps, RFC 7323), and `SACK-Permitted` (RFC 2018) from both client and server SYN packets.
   * **MSS Clamping:** The Maximum Segment Size (MSS) option is clamped to **216 bytes** ($\text{MTU}_{256} - 40\text{ bytes}$ for IPv4/TCP headers), guaranteeing that generated TCP segments fit within a single 7-fragment burst without secondary fragmentation.
2. **Transparent FIN Propagation & Half-Closed State:**
   * When an endpoint initiates graceful connection teardown via `close()` or `shutdown(SHUT_WR)`, emitting a TCP FIN segment:
     * The local PEP acknowledges all pending data up to the FIN sequence number.
     * The PEP emits an in-band control fragment with `URGENT_FLUSH = 1` and payload byte `0x00` designated as `STREAM_CTRL_FIN`.
     * The remote PEP receives `STREAM_CTRL_FIN` and calls `shutdown(fd, SHUT_WR)` on its corresponding local socket (`sshd` or client socket).
     * **Half-Close Support:** Full bidirectional half-close semantics are preserved: the link remains fully active and readable for reverse-direction data until the peer also emits a FIN.
3. **TCP RST Abort Propagation:**
   * If either local socket terminates abnormally, experiences connection reset, or emits a TCP RST segment:
     * The detecting PEP immediately transmits a high-priority `STREAM_CTRL_RST` control fragment (`URGENT_FLUSH = 1`, payload byte `0x01`).
     * The peer PEP upon reception immediately terminates its local socket using `setsockopt(..., SO_LINGER, {l_onoff: 1, l_linger: 0})` and delivers a TCP RST to the local process, purging all staged reassembly buffers without hanging.
4. **Symmetric Zero-Window Backpressure:**
   * When either endpoint's acoustic transmit queue exceeds $2 \times W_{\text{clamp}}$, the local PEP advertises `win = 0` (TCP Zero-Window) in its ACK stream toward the local producer (`sshd` or client).
   * As the acoustic modem transmits queued frames and the backlog drops below $1 \times W_{\text{clamp}}$, the PEP emits a window update segment restoring `win = W_{\text{clamp}}`, resuming local stream ingestion without buffer overflow.

### 7.2 Sequence-Derived IP Fragmentation Header (Byte 0x08)

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|U|FRAG_I|TOT_F|B|             IP DATAGRAM FRAGMENT             |
+-+-+-+-+-+-+-+-+-+-++                                          +
|                                                               |
|               Bytes 0x01..0x25 (Up to 37 Bytes Data)          |
|                                                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+

```

* **Packet Identity:** $\text{PKT\_ID} \equiv \text{SEQ}_{\text{initial}}$.
* `URGENT_FLUSH` (Bit [7]): `1` = High-priority / out-of-band interactive flush (e.g., SSH break / Ctrl-C / SIGINT), instructing the receiving PEP to immediately flush intermediate reassembly buffers and deliver data to the local socket; `0` = Standard stream fragment.
* `FRAG_IDX` (Bits [6..4]): Fragment index ($0\text{--}7$, 3 bits, matching the 8-fragment maximum).
* `TOTAL_FRAGS_MINUS_ONE` (Bits [3..1]): Total fragments minus one ($0\text{--}7$, 3 bits, supporting up to 8 fragments $\implies 296\text{ bytes}$ max datagram).
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
  Bytes 0x02..0x03: SYMBOL_SIZE_T (16 bits, fixed to 32)
  Bytes 0x04..0x05: TOTAL_SOURCE_BLOCKS (Z, 16 bits, 1 <= Z <= 255)
  Bytes 0x06..0x09: TOTAL_BYTES (F, 32 bits, uncompressed file size)
  Bytes 0x0A..0x25: BLAKE3_224 (Leading 28 bytes [224 bits] of BLAKE3 checksum)

```

### 8.1 RFC 6330 Object Transmission Information (OTI) & Deterministic Partitioning

To ensure deterministic encoding and decoding between independent RFC 6330 RaptorQ implementations, all SOTP sessions adhere strictly to the following parameters:

* **Common OTI Parameter Set:**
  * **Transfer Length ($F$):** Total transfer size in octets, strictly bounded within:
    $$1 \le F \le 460,248,480\text{ octets} \quad (\sim 438.9\text{ MiB})$$
    Empty objects ($F = 0$) are explicitly prohibited and rejected by the decoder ($K_t \ge 1$, $Z \ge 1$). The upper bound is mathematically derived from the maximum symbol partition: $Z_{\text{max}} \times K_{\text{max}} \times T = 255 \times 56,403 \times 32 = 460,248,480\text{ bytes}$. Conveyed in Manifest bytes `0x06..0x09`.
  * **Symbol Size ($T$):** Exactly **32 bytes** ($\text{Al} = 4$), conveyed in Manifest bytes `0x02..0x03`.
  * **Symbol Alignment ($\text{Al}$):** **4 bytes** (guaranteeing 32-bit hardware alignment across platforms).
  * **Number of Sub-Blocks ($N$):** **1** (sub-blocking disabled; $T = 32$ fits inside a single cache line).
  * **Number of Source Blocks ($Z$):** $1 \le Z \le 255$ (Manifest bytes `0x04..0x05`; byte `0x04` is `0x00`, byte `0x05` holds $Z$).
  * **Maximum Source Symbols per Block ($K_{\text{max}}$):** **56,403** (RFC 6330 ceiling).
  * **Network Byte Order (Big-Endian):** In strict accordance with RFC 6330 §3.2, all multi-byte fields across SOTP Data Frames and Metadata Manifests (`SYMBOL_SIZE_T`, `TOTAL_SOURCE_BLOCKS`, `TOTAL_BYTES`, `FEC_PAYLOAD_ID`) SHALL be transmitted in **network byte order (Big-Endian)**.
* **Deterministic Source Block Partitioning (RFC 6330 §4.4.1.2):**
  * Total source symbols: $K_t = \lceil F / T \rceil$.
  * Number of source blocks: $Z = \lceil K_t / K_{\text{max}} \rceil$.
  * Partitioning calculation:
    $$K_L = \lceil K_t / Z \rceil, \quad K_S = \lfloor K_t / Z \rfloor, \quad J = K_t - Z \cdot K_S$$
  * Exactly $J$ source blocks contain $K_L$ source symbols (blocks $0 \le i < J$), and $Z - J$ source blocks contain $K_S$ source symbols (blocks $J \le i < Z$).
  * For each block $i \in [0 \dots Z-1]$:
    $$K_i = \begin{cases} K_L, & 0 \le i < J \\ K_S, & J \le i < Z \end{cases}$$
  * For each block, the extended source block size $K'_i$ is looked up independently as the smallest systematic block size from RFC 6330 Table 2 such that $K'_i \ge K_i$.
* **Two-Level Padding Rules:**
  1. **Object-Length Zero-Padding (Final Source Symbol):** If $F \pmod T \ne 0$, the final source symbol of the transfer (symbol $K_{Z-1}-1$ in block $Z-1$) is padded with $T - (F \pmod T)$ zero octets (`0x00`). This padding octet sequence is part of the encoded symbol and is transmitted over the air.
  2. **Systematic Extended Block Padding (Virtual Symbols):** For each block $i$, the encoder appends $K'_i - K_i$ virtual zero symbols of size $T$. These virtual symbols are utilized internally by the RaptorQ matrix encoding/decoding arithmetic, but they are **virtual**: they are **NEVER transmitted over the air** and are **NEVER included in object hash computations**.
* **FEC Payload ID (RFC 6330 §3.2):**
  * Conveys the 8-bit Source Block Number (`SBN`) and 24-bit Encoding Symbol ID (`ESI`).
  * For block $i$:
    * Source symbols have $0 \le \text{ESI} < K_i$.
    * Repair symbols have $K_i \le \text{ESI} < 2^{24}$.
* **Object Integrity (`BLAKE3_224`):**
  * Manifest bytes `0x0A..0x25` contain `BLAKE3_224`, defined strictly as the first 28 bytes (224 bits) of the standard 32-byte BLAKE3 hash computed over the reconstructed $F$ bytes (`bytes[0..F-1]`). The zero padding in the final symbol is excluded from hash evaluation.
* **Manifest Transmission Cadence:**
  * SOTP Metadata Manifest frames are interleaved every 8 physical frames. A newly attached receiver requires at most 8 frames to acquire object metadata and begin RaptorQ symbol accumulation.

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
; Hand off 8kHz linear PCM to vradmd TCP daemon (uuid,service) using dynamic RFC 4122 UUID
same  => n,Set(SOCKET_UUID=${UUID()})
same  => n,AudioSocket(${SOCKET_UUID},127.0.0.1:9099)
same  => n,Hangup()
same  => n(reject),NoOp(Unauthorized Call Dropped)
same  => n,Hangup()

```

### 9.3 iOS Cross-Process IPC Architecture (Lock-Free SPSC Shared Memory)

The iOS architecture strictly separates the untrusted network packet processing (`PacketTunnelProvider` NetworkExtension sandbox) from the audio streaming engine (`AVAudioEngine` in Main App). To guarantee deterministic real-time audio throughput without dropping audio frames or stalling:

1. **Shared Memory Backing (App Group Container File):**
   * Shared memory is backed by a pre-allocated file located in the shared App Group directory via `FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: "group.org.vradm")`.
   * Both processes map the file via `mmap(NULL, SHM_SIZE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0)`.
   * **Prohibition of POSIX Named Semaphores:** POSIX named semaphores (`sem_open`) and `shm_open()` are unreliable or prohibited under iOS App Extension sandbox policies. V-RADM strictly forbids named semaphores and mutexes across process boundaries.

2. **Lock-Free Single-Producer Single-Consumer (SPSC) Ring Buffer:**
   * Communication uses two unidirectional, lock-free SPSC ring buffers:
     * `tx_ring`: `PacketTunnelProvider` (Producer) $\to$ Main App `vradm-core` (Consumer). Transports raw IP datagram fragments.
     * `rx_ring`: Main App `vradm-core` (Producer) $\to$ `PacketTunnelProvider` (Consumer). Transports reassembled IP slices.
   * **Cache-Line Padding:** Atomic indices (`head`, `tail` of type `atomic_uint32_t`) are explicitly aligned to separate 64-byte hardware cache lines (`alignas(64)`) to eliminate false sharing between CPU cores.
   * **Memory Ordering:** Index updates enforce release semantics (`memory_order_release`) upon enqueuing and acquire semantics (`memory_order_acquire`) upon dequeuing.

3. **Strict Real-Time Audio Constraints (`AVAudioEngine`):**
   * The real-time audio render callback thread (`vradm_process_audio` and `vradm_generate_audio`) MUST NEVER block, acquire locks, wait on cross-process condition variables, or perform dynamic memory allocation (`malloc`/`free`).
   * The audio engine strictly **polls** the SPSC `tx_ring` non-blockingly. If no new IP packets are available in the ring, the modulator outputs standard silence or voiced idle carriers without introducing audio dropouts.

4. **Non-Real-Time Cross-Process Signaling:**
   * To wake the Main App's background event loop when new IP packets are enqueued into the `tx_ring` by the NetworkExtension (non-real-time path), IPC uses **Darwin Notifications**:
     ```objc
     CFNotificationCenterPostNotification(CFNotificationCenterGetDarwinNotifyCenter(),
                                         CFSTR("org.vradm.ipc.tx_available"),
                                         NULL, NULL, TRUE);
     ```
   * The real-time audio thread never awaits or processes Darwin notifications; only the lower-priority asynchronous background queue receives notification events to trigger batch staging into the modem core.

5. **iOS Background Execution & Lifecycle Constraints:**
   * **`UIBackgroundModes`:** The Main App `Info.plist` declares `UIBackgroundModes` containing `audio` (`<string>audio</string>`).
   * **Audio Session Configuration:** The app initializes `AVAudioSession.sharedInstance()` with category `.playAndRecord`, mode `.measurement`, and options `[.allowBluetooth, .mixWithOthers]`. Mode `.measurement` requests the least-processed audio path available for the selected route; OS-level dynamics processing, automatic gain control (AGC), and acoustic echo cancellation (AEC) are suppressed on a best-effort basis. Because hardware audio pipelines across Apple devices and accessory routes (built-in speakerphone vs. wired Lightning/USB-C dongle) can exhibit device-dependent pre-emphasis or non-linear limiter behaviors, remaining route-specific hardware processing SHALL be characterized empirically per target device model. Background execution is preserved under `UIBackgroundModes = ["audio"]`, where continuous execution of the real-time audio render callback prevents iOS from suspending the Main App process when backgrounded or when the device screen is locked.
   * **NetworkExtension Lifecycle:** The NetworkExtension provider maintains packet handling while its tunnel session is active. Tunnel termination or provider restart is treated as a recoverable lifecycle event; the shared-memory protocol must tolerate producer/consumer disappearance and reattachment.

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
#define VRADM_MCS_0  0  // 20 Bd, Ortho-Pitch (80.0 bps raw / 46.25 bps L3, 6400 ms)
#define VRADM_MCS_1  1  // 50 Bd, Speech Atom (400.0 bps raw / 231.25 bps L3, 1280 ms)
#define VRADM_MCS_2  2  // 100 Bd, 4-DQPSK (800.0 bps raw / 455.38 bps L3, 650 ms)
#define VRADM_MCS_3  3  // 200 Bd, 8-DQPSK (3200.0 bps raw / 1793.94 bps L3, 165 ms)
#define VRADM_MCS_4  4  // 250 Bd, Real CP-OFDM (4000.0 bps raw / 2242.42 bps L3, 132 ms)

typedef uint32_t vradm_rate_t;
#define VRADM_RATE_8K   8000
#define VRADM_RATE_16K 16000

typedef struct vradm_engine vradm_engine_t;

/* =========================================================================
 * Struct Definitions (Strict Fixed-Width Alignment)
 * ========================================================================= */

typedef struct {
    vradm_mcs_t  startup_mcs;          // [0]
    uint8_t      reserved[3];          // [1..3]
    vradm_rate_t sample_rate;          // [4..7]
    uint8_t      auto_rate_adaptation; // [8]
    uint8_t      psk_key[16];          // [9..24] 128-bit Pre-Shared Key for PLCP/CCF MAC
    uint8_t      padding[3];           // [25..27]
    float        tx_amplitude;         // [28..31] Target RMS ceiling (Default: 0.3535 = -9.0 dBFS RMS)
} vradm_config_t; // Exactly 32 bytes

typedef struct {
    float       estimated_snr_db;         // [0..3]
    vradm_mcs_t active_tx_mcs;            // [4]
    vradm_mcs_t active_rx_mcs;            // [5]
    uint8_t     plcp_carrier_locked;      // [6]
    uint8_t     reserved;                 // [7] Struct padding
    uint32_t    security_tamper_detected; // [8..11] Cumulative counter of invalid PLCP/CCF MAC drops
    uint32_t    frames_transmitted;       // [12..15]
    uint32_t    frames_received;          // [16..19]
    uint32_t    rs_corrected_bytes;       // [20..23]
    uint32_t    rs_corrected_erasures;    // [24..27]
    uint32_t    crc_failures;             // [28..31]
    float       channel_metric_score;     // [32..35]
    int32_t     sample_slip_accum;        // [36..39] Cumulative sample slips corrected by DLL
} vradm_telemetry_t; // Exactly 40 bytes

typedef enum {
    VRADM_CMD_NONE = 0,
    VRADM_CMD_START_SOTP,
    VRADM_CMD_STOP_SOTP,
    VRADM_CMD_REQUEST_MCS,
    VRADM_CMD_RESET_SESSION,
    VRADM_CMD_SET_TX_PARAMS
} vradm_cmd_type_t;

typedef struct {
    vradm_cmd_type_t type;
    uint8_t          target_mcs;
    uint8_t          reserved[3];
    float            param_float;
    const void*      data_ptr;
    size_t           data_len;
} vradm_cmd_t;

#if defined(__STDC_VERSION__) && __STDC_VERSION__ >= 201112L
_Static_assert(sizeof(vradm_config_t) == 32, "vradm_config_t size mismatch: expected 32 bytes");
_Static_assert(sizeof(vradm_telemetry_t) == 40, "vradm_telemetry_t size mismatch: expected 40 bytes");
_Static_assert(offsetof(vradm_config_t, psk_key) == 9, "vradm_config_t: psk_key offset mismatch");
_Static_assert(offsetof(vradm_telemetry_t, security_tamper_detected) == 8, "vradm_telemetry_t: security_tamper_detected offset mismatch");
_Static_assert(offsetof(vradm_telemetry_t, sample_slip_accum) == 36, "vradm_telemetry_t: sample_slip_accum offset mismatch");
#endif

/* =========================================================================
 * Thread Ownership & Concurrency Contract
 * =========================================================================
 * 1. Audio Render Thread (Real-Time Audio Priority):
 *    - Exclusively calls vradm_process_audio() and vradm_generate_audio().
 *    - Guarantees: Non-blocking execution, zero heap allocations (malloc/free),
 *      zero mutex/condition variable blocking, lock-free SPSC polling.
 *    - Constraint: MUST NOT be called concurrently with itself on the same engine.
 *    - Applies queued host commands (vradm_cmd_t) deterministically at audio
 *      frame boundaries.
 *
 * 2. Host Network / Worker Thread:
 *    - Calls vradm_write_ip_packet(), vradm_poll_ip_packet(), and vradm_submit_cmd().
 *    - Safe to invoke concurrently with the Audio Render Thread on the same engine;
 *      data exchange is internally decoupled via lock-free SPSC ring buffers.
 *    - Control mutations (SOTP start/stop, MCS commit requests, and session resets)
 *      are submitted via vradm_submit_cmd() into the host->audio SPSC queue rather
 *      than modifying shared modulator state directly.
 *
 * 3. Telemetry / Diagnostics Thread:
 *    - Calls vradm_get_telemetry().
 *    - Safe to invoke concurrently from any thread at any time.
 *    - Coherent Seqlock Snapshot Protocol: The engine maintains a monotonically
 *      increasing 32-bit sequence counter (seq). The writer increments seq to an
 *      odd value prior to modifying telemetry fields and increments seq to an even
 *      value upon completion. The reader samples seq1, copies the 40-byte struct,
 *      and samples seq2, accepting the snapshot if and only if (seq1 == seq2 && (seq1 & 1) == 0).
 *      This guarantees tear-free atomic snapshots without mutex contention.
 *
 * 4. Engine Lifecycle Operations:
 *    - vradm_create(), vradm_reset(), vradm_destroy().
 *    - Strictly prohibited while audio callbacks or network polling threads are
 *      active on the instance. Calling vradm_destroy() concurrently with audio
 *      processing produces undefined behavior.
 * ========================================================================= */

/* --- Engine Lifecycle Management (Thread-Safe under Single-Owner Discipline) --- */
vradm_engine_t* vradm_create(const vradm_config_t* config);
void            vradm_destroy(vradm_engine_t* engine);
void            vradm_reset(vradm_engine_t* engine);

/* --- Asynchronous Host Command Queue (Lock-Free SPSC) --- */
int32_t vradm_submit_cmd(vradm_engine_t* engine, const vradm_cmd_t* cmd);

/* --- Real-Time Audio Streaming I/O (Zero Dynamic Allocations) --- */
void   vradm_process_audio(vradm_engine_t* engine, const int16_t* in_samples, size_t count);
size_t vradm_generate_audio(vradm_engine_t* engine, int16_t* out_samples, size_t max_count);

/* --- Mode A: IP Packet Datagram Stream (TUN / TCP-PEP Interface) --- */
int32_t vradm_write_ip_packet(vradm_engine_t* engine, const uint8_t* packet, size_t len);
int32_t vradm_poll_ip_packet(vradm_engine_t* engine, uint8_t* out_packet, size_t max_len);

/* --- Mode B: SOTP Simplex Object Transfer --- */
int32_t vradm_sotp_tx_init(vradm_engine_t* engine, const uint8_t* payload, size_t len, float redundancy_factor);
int32_t vradm_sotp_rx_poll(vradm_engine_t* engine, size_t* out_collected_symbols, size_t* out_required_symbols);
int32_t vradm_sotp_rx_fetch(vradm_engine_t* engine, uint8_t* out_buf, size_t max_len, uint8_t out_hash[28]); // 28-byte BLAKE3_224 digest

/* --- Telemetry & Link Status (Seqlock Coherent Snapshot) --- */
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
| **TC-04a** | Balanced Cellular Gate (MCS 2) | AMR-NB @ 12.2 kbps | Injected channel $\text{SNR} = 18\text{ dB}$, clock drift $\pm 50\text{ PPM}$ | Multi-carrier 4th-power NDA DLL maintains phase-aligned carrier and timing lock. Measured Application Goodput $R_{\text{APP}} \ge 250\text{ bps}$ for SSH/TCP-PEP or $\ge 320\text{ bps}$ for UDP bulk stream (against theoretical L3 maximum $431.92\text{ bps}$). $P_{\text{FER}} \le 1.0 \times 10^{-3}$. Zero RS decode crashes. |
| **TC-04b** | Wideband Cellular Cabled Gate (MCS 3) | AMR-WB @ 12.65 kbps | Resampling $16\text{k} \to 8\text{k} \to 16\text{k}$; $\pm 80\text{ PPM}$ clock drift | DPLL and DLL maintain lock. Measured Application Goodput $R_{\text{APP}} \ge 850\text{ bps}$ for SSH/TCP-PEP or $\ge 1,100\text{ bps}$ for UDP bulk stream. $P_{\text{FER}} \le 1.0 \times 10^{-3}$. Zero RS decode crashes. |
| **TC-05** | VAD & AGC Verification | 3GPP VAD Model 1 & 2 + Smartphone AGC model | PRBS-7 phase dither enabled; voiced maintenance carrier active | Measured over 10,000 independent 1-second trials. False DTX entry $P_{\text{DTX}} \le 0.01$. Zero AGC signal-clamping events. Dither-induced FER degradation $\Delta P_{\text{FER}} \le 0.5\%$ ($\le 0.005$) compared to un-dithered transmission. |
| **TC-06** | Burst Erasure Recovery | MCS 3 (165 ms frames) | 3 consecutive physical frame drops ($495\text{ ms}$ drop) | Selective Repeat ARQ triggers fast retransmission. Complete IP packet stream recovery within $\le \mathbf{1,450\text{ ms}}$ of drop start. Zero application errors. |
| **TC-07** | SOTP Mid-Stream Entry | AMR-WB @ 12.65 kbps | Receiver attaches at Frame 40 of a 100-symbol broadcast | Receiver syncs via Metadata Manifest within 8 frames. Object reconstructs with matching BLAKE3_224 checksum. |
| **TC-08a** | Real VoLTE Cellular Call (MCS 3) | Commercial Mobile VoLTE Network | Active 15-minute phone call between iPhone and Asterisk server | Interactive OpenSSH/TCP-PEP session maintained continuously. Keystroke round-trip confirmation time $\le 550\text{ ms}$. |
| **TC-08b** | Real Degraded / Free-Air Link (MCS 0/1) | Acoustic Speaker-to-Mic Air Gap / Degraded 3G Call | High ambient acoustic noise and multi-second frame periods | Mosh UDP terminal session maintained continuously. Predictive local echo renders keystrokes with $< 50\text{ ms}$ UI latency; remote screen converges within $1.5 \times T_{\text{frame}}$ after burst recovery. |
| **TC-09** | Concurrency & Thread-Safety | 8 concurrent AudioSocket TCP threads | Multi-channel load test on Linux daemon | Zero cross-session cross-talk, race conditions, or memory corruption. CPU scaling linear across threads. |
| **TC-10a** | Symbol Timing & Phase-Slope Detector | MCS 2, MCS 3, and MCS 4 loopback | Injected random single-sample slips ($\pm 1$ sample every $500\text{ ms}$) across SNR sweep down to demotion threshold ($\text{SNR} \in [8\text{ dB}, 18\text{ dB}]$) | For MCS 2 and MCS 3: Inter-carrier 4th-power phase-slope timing detector ($\hat{\tau} \propto -\partial\angle q_k/\partial f_k$) and complex baseband transition detector resolve slips within $\le 2.5\text{ ms}$ ($< 1$ symbol interval). For MCS 4: Cyclic-prefix correlation detector resolves slips within $\le 2.5\text{ ms}$. Zero bit slips; Farrow fractional resampler tracks phase step without symbol decoding failure. |
| **TC-10b** | 4th-Power Carrier Tracking & MRC Threshold | MCS 2 and MCS 3 loopback | Static carrier frequency offsets up to $\pm 12.5\text{ Hz}$ across SNR sweep ($8\text{ dB} \dots 20\text{ dB}$) | Phase-aligned MRC coherent combining achieves power-weighted squaring-loss mitigation gain: $\ge +5.74\text{ dB}$ for MCS 2 (4 carriers) and $\ge +7.56\text{ dB}$ for MCS 3 (8 carriers) calculated from profile amplitude weighting $(\sum A_k^2)^2 / \sum A_k^4$, or $+6.02\text{ dB}$ and $+9.03\text{ dB}$ under equal-power AWGN calibration ($10\log_{10} Z$). Carrier phase tracking lock maintained without cycle slipping down to channel metric demotion threshold $M = 0.60$. |
| **TC-10c** | Limiter Activation & Multicarrier EVM | MCS 2, MCS 3, and MCS 4 composite multicarrier waveforms at nominal and worst-case crest-factor alignments | Nominal RMS targets with rare crest-factor transients | Peak-constrained normalization bounds soft-limiter activation rate to $< 0.05\%$ of samples. Measured composite multicarrier EVM degradation due to limiter non-linearity is $\le 0.5\text{ dB}$ (EVM $\le -22\text{ dB}$ across MCS 2 and MCS 3, EVM $\le -24\text{ dB}$ for MCS 4). |
| **TC-10d** | Continuous Sample Clock Drift Tracking | MCS 2, MCS 3, and MCS 4 cabled loopback | Continuous clock offset $\Delta F_s / F_s \in \{\pm 20, \pm 40, \pm 80, \pm 100\}\text{ PPM}$ | *CI Smoke Tier (2,000 frames):* Farrow 3rd-order resampler and DLL dynamic tracking maintain synchronization without buffer overflow/underflow or bit slips. $P_{\text{FER}} \le 1.0 \times 10^{-4}$. Zero unrecoverable frame loss.<br>*Hardware Qualification Tier (10,000 frames):* Continuous lock over physical streaming durations without cumulative phase drift or frame loss: $108.3\text{ minutes}$ ($6,500\text{ s}$) for MCS 2, $27.5\text{ minutes}$ ($1,650\text{ s}$) for MCS 3, and $22.0\text{ minutes}$ ($1,320\text{ s}$) for MCS 4. |
| **TC-11** | G.711 VoIP Codec-in-the-Loop Gate (MCS 4) | G.711 $\mu$-law / A-law companding over simulated VoIP network | 20 ms RTP packetization, $\sigma = 5.0\text{ ms}$ packet arrival jitter absorbed by $40\text{ ms}$ playout jitter buffer; 1.0% random packet loss triggering standard G.711 Appendix I Packet Loss Concealment (PLC) waveform synthesis distortion; $\pm 50\text{ PPM}$ clock skew | Reed-Solomon RS(64,48) with GMD soft-decision erasure tagging recovers dropped/distorted packet slices in conjunction with Selective Repeat ARQ. Measured Application Goodput $R_{\text{APP}} \ge 1,200\text{ bps}$ for SSH/TCP-PEP or $\ge 1,450\text{ bps}$ for UDP bulk stream (against theoretical L3 maximum $1,769.14\text{ bps}$). $P_{\text{FER}} \le 1.0 \times 10^{-3}$. |

---

## 12. Direct Implementation Instructions

1. **Workspace Layout:** Scaffold a Cargo workspace with `crates/vradm-core` (`#![no_std]` core with `alloc` for initialization), `crates/vradm-server` (multi-threaded Asterisk daemon), `crates/vradm-cli` (diagnostics), and `tests/vocoder_bench`.
2. **Wire Format Verification:** Implement `crates/vradm-core/src/link/frame.rs` and verify bit-exact layout of both the 64-byte Canonical Data Frame and 16-byte Authenticated Compact Control Frame.
3. **PLCP Bootstrap & Authentication:** Implement the Barker-13 dual-chirp generator, dual Extended Golay $[24, 12, 8]$ codecs, SipHash-2-4 control plane MAC with DoS-immune token bucket rate limiting and silent drops, and 2-FSK modulator.
4. **Carrier & Timing Recovery (4th-Power NDA DLL):** Implement multi-carrier 4th-power phase extraction, inter-carrier phase-slope timing error detector ($\hat{\tau} \propto -\partial\angle q_k/\partial f_k$), phase-aligned MRC coherent combining, symbol-boundary transient exclusion zones, and fractional Farrow resampler in `crates/vradm-core/src/phy/dll.rs`.
5. **Soft-Decision RS Decoder:** Implement Chase/GMD soft-decision decoding in `crates/vradm-core/src/fec/rs.rs`, verifying that flagging 16 confidence-tagged erasures reconstructs corrupted frames under $2t + e \le 16$.
6. **Continuous Codec Validation:** Execute `tests/vocoder_bench` continuously against `libopencore-amr`, `vo-amrwbenc`, and G.711 before packaging C-ABI exports. Ensure zero regressions across tests TC-01 through TC-11 prior to platform deployment.