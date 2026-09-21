# Engineering Specification: Vocoder-Resilient Acoustic Data Modem (V-RADM)

**Document Version:** 3.8.1

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
* **Byte 0x07:** `CCF_MAC` (Truncated 8-bit SipHash-2-4 MAC computed over bytes `0x02..0x06` using PSK). Frames failing MAC validation are dropped before state-machine ingestion (see §4.1 for Anti-DoS threat model and consecutive failure lockout).
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
 │ 0    │ Feature  │ 20     │ 16        │ 1           │ 4        │ 80.0       │ 46.25    │ 43.00 / 32.3│ ~24.0        │
 │ 1    │ Feature  │ 50     │ 256       │ 3           │ 8        │ 400.0      │ 231.25   │ 225.04      │ ~155.0       │
 │ 2*   │ Hybrid   │ 100    │ 256       │ 4           │ 8        │ 800.0      │ 455.38   │ 431.92      │ ~330.0       │
 │ 3*   │ Coherent │ 200    │ 65,536    │ 8           │ 16       │ 3,200.0    │ 1,793.94 │ 1,477.69    │ ~920.0       │
 │ 4    │ Waveform │ 250    │ 65,536    │ 8           │ 16       │ 4,000.0    │ 2,242.42 │ 1,769.14    │ ~1,450       │
 └──────┴──────────┴────────┴───────────┴─────────────┴──────────┴────────────┴──────────┴─────────────┴──────────────┘
 *Note: MCS 2 and MCS 3 are experimentally gated modes; operational enablement requires passing TC-04.
```

* **Framing Structure (1 Differential Reference Symbol Per Frame):**
  * In phase-modulated modes (MCS 2, 3, 4), Symbol 0 ($m=0$) is reserved as a known differential reference symbol ($\phi_k(0) = \frac{k\pi}{4}$), resolving differential quadrant bootstrapping deterministically. Data symbols span $m = 1 \dots N_{\text{data}}$:
    * **MCS 2:** 1 reference symbol + 64 data symbols = 65 symbols ($650.0\text{ ms}$). Unadjusted Max L3 rate = $296\text{ bits} / 0.650\text{ s} = \mathbf{455.38\text{ bps}}$.
    * **MCS 3:** 1 reference symbol + 32 data symbols = 33 symbols ($165.0\text{ ms}$). Unadjusted Max L3 rate = $296\text{ bits} / 0.165\text{ s} = \mathbf{1,793.94\text{ bps}}$.
    * **MCS 4:** 1 reference symbol + 32 data symbols = 33 symbols ($132.0\text{ ms}$). Unadjusted Max L3 rate = $296\text{ bits} / 0.132\text{ s} = \mathbf{2,242.42\text{ bps}}$.
* **PLCP-Adjusted L3 Ceiling:** Accounts for the mandatory $565.0\text{ ms}$ PLCP control beacon emitted at session start, turn boundaries, and every 16 frames ($16 \times T_{\text{frame}} + 0.565\text{ s}$). For MCS 0, reflects 7-frame burst ($43.00\text{ bps}$) versus single-frame turn ($32.30\text{ bps}$).
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

  * This bounds out-of-band phase-modulation sidebands to $<-40\text{ dBc}$ while completely preventing cellular baseband stationary-whistle gating.
  * The receiver executes an identical synchronized generator, subtracting $\Delta \theta_{\text{dither}}(n)$ prior to constellation slicing and timing recovery.

#### Mid-Frame Continuous Sample-Slip Recovery: Multi-Carrier 4th-Power DLL (NDA)

To prevent sample slips across asynchronous clock boundaries (`AVAudioEngine` vs. cellular baseband) from misaligning OFDM bins or DQPSK symbols between PLCP beacons without sacrificing data throughput:

1. **PHY Baseband Signal Chain & 4th-Power Carrier Recovery:** Rather than sacrificing a dedicated subcarrier to an unmodulated pilot tone—which would degrade bit-loading—all subcarriers carry 2-bit DQPSK data throughout the payload ($Z=4$ for MCS 2, $Z=8$ for MCS 3). Carrier and sample-slip tracking relies on **4th-Power Non-Data-Aided (NDA)** processing:
   * **Bandpass Filtering:** The received audio $x[n]$ is filtered around active subcarrier $k$:
     $$z_k[n] = \operatorname{BPF}_k\{x[n]\}$$
   * **Complex Baseband Downmixing:** The subcarrier is downmixed to complex baseband:
     $$u_k[n] = z_k[n] \cdot e^{-j \frac{2\pi f_k n}{F_s}}$$
   * **4th-Power Non-Linearity:** The baseband signal is raised to the 4th power:
     $$q_k[n] = (u_k[n])^4$$
   * **Phase-Error Interpretation:** For ideal 4-state phase modulation, the fourth-power operation removes the discrete QPSK phase state ($4 \cdot \Delta \phi_k \equiv 0 \pmod{2\pi}$); residual channel phase, frequency offset, noise, and dither remain. Thus, $q_k[n]$ is a **near-DC phase-error reference**, where sample-timing error manifests as a steady-state phase error proportional to subcarrier frequency $f_k$.
   * **DQPSK Ambiguity Synergy:** Stripping 4-phase modulation via 4th-power nonlinearity inherently produces a four-fold ($\pm 90^\circ, \pm 180^\circ$) phase ambiguity. V-RADM's deliberate selection of **Differential QPSK (DQPSK)** renders this ambiguity completely moot: data is encoded strictly in the phase transition between adjacent symbols ($\Delta \phi = \text{wrap}_{2\pi}(\phi(m) - \phi(m-1))$), guaranteeing that the four-fold quadrant ambiguity cancels out algebraically without requiring pilot tones or absolute phase reference tracking.

2. **Multi-Carrier Diversity Combining (Squaring/Quartic Loss Mitigation):** Non-linear 4th-power processing incurs squaring/quartic loss, degrading effective SNR relative to a clean unmodulated tone. Because MCS 2 and MCS 3 are experimentally gated modes operating near the channel threshold ($M \to 0.60$), tracking off a single subcarrier would risk loss-of-lock under fading. To overcome squaring loss, the receiver coherently combines the 4th-power wiped residual across **all active subcarriers** ($Z=4$ in MCS 2, $Z=8$ in MCS 3), weighted by carrier amplitude $A_k$:

$$\bar{q}[n] = \sum_{k} A_k \cdot q_k[n]$$

   *The theoretical coherent-combining upper bound is $10 \log_{10}(Z)$ (+6.0 dB for MCS 2, +9.0 dB for MCS 3); actual effective combining gain SHALL be measured and validated in TC-10.*

3. **Mandatory 4th-Power Primary Loop (Anti-Cascade Protection):** The 4th-power NDA loop is mandated as the primary, unconditional timing-error detector. Sliced decision-directed (DD) modulation wiping is strictly prohibited as a coequal primary loop to prevent catastrophic **decision-directed loss-of-lock cascades** near the demotion threshold (where tentative symbol errors inject corrupt phase into the DLL, destabilizing timing and triggering burst demodulation collapse). DD tracking may only be enabled as an optional fine-tracking refinement in high-SNR regimes ($M \ge 0.85$).

4. **Symbol-Boundary Transient Exclusion Zone:** Phase transitions between adjacent DQPSK symbols are shaped by the raised-cosine edge window $w(n)$ ($L = 4\text{ samples at } 8\text{ kHz} = 0.5\text{ ms}$), causing brief transient fluctuations in the 4th-power residual at symbol boundaries. The 16-sample Early-Prompt-Late correlator is strictly constrained to the interior quiescent window of each symbol, excluding the $\pm 4$ sample transition region around boundaries:
   * **MCS 2 ($T_{\text{sym}} = 80\text{ samples}$):** The correlator integrates within the central quiescent window $n \in [16, 64]$ samples relative to symbol start.
   * **MCS 3 ($T_{\text{sym}} = 40\text{ samples}$):** The correlator integrates within the central quiescent window $n \in [12, 28]$ samples relative to symbol start.

5. **PLCP Bootstrap Handoff Tolerances:** The PLCP beacon trains coarse timing and frequency prior to payload handoff. To ensure the 4th-power payload tracking loop converges within its pull-in range, the PLCP receiver must deliver:
   * Maximum residual timing error at handoff: $|\Delta t_{\text{handoff}}| \le 2.0\text{ samples}$ ($0.25\text{ ms}$ at $8\text{ kHz}$, well within the Farrow interpolator's $\pm 8\text{ sample}$ pull-in range).
   * Maximum residual carrier frequency offset: $|\Delta f_{\text{handoff}}| \le \pm 12.5\text{ Hz}$ (well within the $\pm 25\text{ Hz}$ pull-in range of 100/200 Bd DQPSK).
   * The PLCP 2-FSK receiver must achieve $|\Delta t| \le 1.0\text{ sample}$ and $|\Delta f| \le \pm 5.0\text{ Hz}$ at $E_b/N_0 \ge 6.0\text{ dB}$.

6. **Early-Prompt-Late Correlator & Sub-Sample Farrow Resampler:** Within the quiescent symbol window, the receiver calculates early ($n - 1$), prompt ($n$), and late ($n + 1$) correlation energy across the combined residual $\bar{q}[n]$ over a sliding 16-sample window. If late energy exceeds prompt energy by $\ge 4.5\text{ dB}$, a $+1$ sample slip is flagged; if early energy dominates, a $-1$ sample slip is flagged. The fractional Farrow filter dynamically shifts delay by $\pm 1$ sample within $2.5\text{ ms}$, preserving constellation tracking without dropping frames.

7. **MCS 4 CP Correlator:** Cyclic prefix cross-correlation evaluates timing drift on every 4.0 ms slot, eliminating cumulative timing error.

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

* **Framing & Reference Slot:** Each frame comprises 33 OFDM slots ($132.0\text{ ms}$): Slot 0 ($m=0$) is a known differential reference slot ($D_k(0) = 1 + j$); Slots $m = 1 \dots 32$ carry the 512 bits (64 bytes) of protected payload and FEC.
* **Telephone-Band Carrier Allocation ($Z = 8$ Active Carriers):**

$$k \in \{2, 3, 5, 6, 7, 8, 9, 10\} \implies f_k \in \{571.4, 857.1, 1428.6, 1714.3, 2000.0, 2285.7, 2571.4, 2857.1\}\text{ Hz}$$

* **Demodulation:** Differential QPSK across consecutive OFDM symbol slots eliminates the requirement for absolute carrier-phase channel estimation under the assumption of sufficiently slow channel variation.

### 3.3 Output Level Conditioning & Peak-Safe Normalization

Multi-carrier waveforms exhibit crest factors (Peak-to-Average Power Ratio) that vary dramatically by mode. Applying a uniform RMS normalization with hard clipping will substantially clip 4-carrier and 8-carrier waveforms (e.g. MCS 3 worst-case crest factor is ~11.5 dB, whereas a naive -9 dBFS RMS clamped at -6 dBFS peak permits only 3.01 dB crest factor). To guarantee clean modulation reproduction without clipping:

1. **MCS-Dependent Target RMS:** Audio blocks are scaled according to mode-specific crest-factor budgets:

| Active MCS | Modulator Nature | Max Crest Factor ($CF_{\text{max}}$) | Target RMS ($V_{\text{target\_rms}}$) | Target RMS Level | Max Peak Ceiling | Linear Digital Headroom |
|---|---|---|---|---|---|---|
| **MCS 0** | Single tone pitch | ~3.01 dB | 0.3535 FS | -9.03 dBFS | $\le 0.50\text{ FS}$ | 3.01 dB |
| **MCS 1** | Formant speech atom | ~7.0 dB | 0.2239 FS | -13.00 dBFS | $\le 0.50\text{ FS}$ | 3.01 dB |
| **MCS 2** | 4-carrier DQPSK | ~8.5 dB | 0.1884 FS | -14.50 dBFS | $\le 0.50\text{ FS}$ | 3.01 dB |
| **MCS 3** | 8-carrier DQPSK | ~11.5 dB | 0.1334 FS | -17.50 dBFS | $\le 0.50\text{ FS}$ | 3.01 dB |
| **MCS 4** | 8-carrier CP-OFDM | ~10.0 dB | 0.1585 FS | -16.00 dBFS | $\le 0.50\text{ FS}$ | 3.01 dB |

2. **Peak-Constrained Normalization:** The scaling factor $g$ is constrained by both the mode target RMS and the maximum peak amplitude:

$$g = \min\left( \frac{V_{\text{target\_rms}}(\text{MCS})}{\sqrt{\frac{1}{N}\sum_{m=0}^{N-1} s^2[m]}}, \; \frac{V_{\text{peak\_max}}}{\max_{0 \le m < N} |s[m]|} \right), \quad \text{where } V_{\text{peak\_max}} = 0.50\text{ FS}$$

$$s[n] \leftarrow s[n] \cdot g$$

3. **Hyperbolic Soft-Saturation Safety Limiter:** To eliminate the severe high-order harmonic distortion spurs generated by rectangular hard clippers, any residual exceptional peak transients are conditioned through a hyperbolic tangent soft limiter:

$$s[n] \leftarrow V_{\text{peak\_max}} \cdot \tanh\left(\frac{s[n]}{V_{\text{peak\_max}}}\right)$$

This ensures that output samples are strictly bounded within $[-0.5\text{ FS}, +0.5\text{ FS}]$ ($-6.02\text{ dBFS Peak}$), providing $3.01\text{ dB}$ of analog DAC headroom without clipping multicarrier symbols.

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

### 4.1 Control Plane Integrity, Anti-Replay & Threat Model Scoping (Anti-DoS)

To prevent unauthorized over-the-air injection of corrupted PLCP commands, accidental cross-talk, stale replayed bursts, or malicious downgrade requests:

* **Keyed Integrity Protection:** Endpoints are provisioned with a 128-bit Pre-Shared Key (PSK).
* **Session Freshness & Anti-Replay Counters:**
  * **`SESSION_EPOCH`:** A 32-bit random session identifier exchanged during initial link handshake.
  * **`CTRL_COUNTER`:** A 16-bit monotonically increasing control counter tracked per direction, incremented on every transmitted control beacon or Compact Control Frame (CCF). The low 8 bits match the wire field `BEAC_SEQ = (uint8_t)(CTRL_COUNTER & 0xFF)`.
  * **`ANTI_REPLAY_WINDOW`:** Receivers maintain a 64-sequence sliding window bitmap. Incoming control frames with $\text{CTRL\_COUNTER} \le \text{counter\_max} - 64$ or whose corresponding bit in the sliding window is already set are rejected prior to SipHash evaluation or state modification. When a valid, fresh frame is verified, $\text{counter\_max} = \max(\text{counter\_max}, \text{CTRL\_COUNTER})$ and the bit is marked.
* **`BEACON_MAC8` & `CCF_MAC` Formulations:**
  * **`BEACON_MAC8`:** An 8-bit truncated **SipHash-2-4** MAC computed over the authenticated context:
    $$\text{BEACON\_MAC8} = \text{Trunc8}\Big(\text{SipHash-2-4}_{\text{PSK}}\big(\text{SESSION\_EPOCH} \,\|\, \text{CTRL\_COUNTER} \,\|\, \text{CUR\_MCS} \,\|\, \text{REQ\_MCS} \,\|\, \text{TX\_PWR} \,\|\, \text{BEAC\_SEQ}\big)\Big)$$
  * **`CCF_MAC`:** An 8-bit truncated **SipHash-2-4** MAC computed over CCF control context:
    $$\text{CCF\_MAC} = \text{Trunc8}\Big(\text{SipHash-2-4}_{\text{PSK}}\big(\text{SESSION\_EPOCH} \,\|\, \text{CTRL\_COUNTER} \,\|\, \text{CCF\_CTRL} \,\|\, \text{ACK\_BASE} \,\|\, \text{ACK\_MAP} \,\|\, \text{METRIC\_BITS}\big)\Big)$$
* **Threat Model & Cryptographic Scoping:**
  * An 8-bit MAC tag provides an intentional, low-overhead **keyed anti-tamper filter and rapid noise/cross-talk rejection checksum** ($2^8 = 256$ work factor per frame attempt). Given the channel's physical rate limits ($1.6\text{ s}$ to $9.2\text{ s}$ per turn), blind forgery attempts require tens of minutes of loud acoustic injection, triggering immediate collision backoffs or call drops.
  * **L4/L7 Cryptographic Delegation:** `BEACON_MAC8` and `CCF_MAC` are keyed integrity checks and are explicitly NOT intended to provide long-term cryptographic non-repudiation. True cryptographic mutual authentication, anti-forgery, replay defense, and confidentiality are strictly anchored at the application/transport layer via **OpenSSH** (SSH-2 host keys + ChaCha20-Poly1305 / AES-256-GCM authenticated transport) and **Mosh** (128-bit AES-OCB).
* **Anti-DoS Consecutive Failure Lockout Policy:**
  * If $\ge 3$ consecutive frames fail SipHash-2-4 MAC verification within any 60-second sliding window:
    1. The receiver flags `security_tamper_detected = 1` in `vradm_telemetry_t`.
    2. The receiver clamps all state-machine transitions, freezing MCS adaptation and rejecting all TDD grant changes.
    3. The modem enters a mandatory $10.0\text{ second}$ silent backoff state (`SILENT_LISTEN`).
    4. An asynchronous security alert is emitted to the host application.

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
* **Profile 2 (Free-Air Acoustic Half-Duplex with Compact ACKs):**
  * Single-Frame Cycle: Data Turn ($7.115\text{ s}$) + Guard ($0.15\text{ s}$) + CCF Turn ($1.75\text{ s}$) + Guard ($0.15\text{ s}$) = $9.165\text{ seconds}$. Nominal RTO = **$9.7\text{ seconds}$**.
  * Burst-Pipelined Cycle ($N_{\text{burst}} = 7$): Data Turn ($45.515\text{ s}$) + Guard ($0.15\text{ s}$) + CCF Turn ($1.75\text{ s}$) + Guard ($0.15\text{ s}$) = $47.565\text{ seconds}$. Nominal RTO = **$48.5\text{ seconds}$**.
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

1. **Local Termination (Client-Side):** The iOS `PacketTunnelProvider` intercepts outbound TCP SYN packets destined for `10.99.0.1:22`. It completes the three-way handshake locally on `utun`, spoofing immediate zero-delay ACKs back to the OpenSSH client.
2. **MCS-Adaptive Window Clamping:** Rather than using a static window clamp—which at MCS 0 represents nearly six minutes of buffered data before backpressure is noticed—the PEP dynamically clamps the advertised window based on the active MCS ladder:

| Active MCS | Raw Rate | App Goodput | Target Window Clamp ($W_{\text{clamp}}$) | Equivalent Slices | Max In-Flight Buffer Time |
|---|---|---|---|---|---|
| **MCS 0** | 80.0 bps | ~24.0 bps (3.0 B/s) | **74 bytes** | 2 frames | ~18.3 s (~2 TDD burst cycles) |
| **MCS 1** | 400.0 bps | ~155.0 bps (19.4 B/s) | **148 bytes** | 4 frames | ~7.8 s |
| **MCS 2** | 800.0 bps | ~330.0 bps (41.2 B/s) | **296 bytes** | 8 frames | ~7.2 s |
| **MCS 3** | 3,200.0 bps | ~920.0 bps (115.0 B/s) | **592 bytes** | 16 frames | ~5.1 s |
| **MCS 4** | 4,000.0 bps | ~1,450.0 bps (181.2 B/s) | **1,024 bytes** | 28 frames | ~5.6 s |

   * TCP window scaling is suppressed across all modes.
   * When the modem transitions MCS, the PEP dynamically updates the Advertised Window field in subsequent spoofed ACKs, bounding the client's unacknowledged queue to between $5\text{ and }18\text{ seconds}$ across the entire rate ladder.
3. **Link Slicing:** Plaintext stream bytes are packed directly into 37-byte fragments (`BEST_EFFORT = 0`) handled by V-RADM's link-layer Selective Repeat ARQ.
4. **Symmetric Server-Side TCP-PEP (`vradmd` $\leftrightarrow$ `sshd`):**
   * The server daemon `vradmd` implements an identical mirrored TCP-PEP terminating the server-facing TCP connection to `127.0.0.1:22` (`sshd`).
   * **Downlink Window Clamping:** The server PEP clamps its advertised window toward `sshd` according to the active downlink MCS using the identical $W_{\text{clamp}}$ table, suppressing window scaling on the `sshd` socket.
   * **Downlink Backpressure Propagation (Zero-Window):** Large server outputs (e.g., terminal screen redraws, `cat`, or directory listings) can rapidly overwhelm acoustic links. When `vradmd`'s acoustic transmit queue exceeds $2 \times W_{\text{clamp}}$, the server PEP advertises a `TCP Zero-Window` (`win = 0`) to `sshd`, immediately blocking `sshd` on `write()`/buffer flush and preventing unbounded socket memory allocation.
   * **Symmetric Rehydration:** Received downlink fragments are reassembled by the client-side PEP in `PacketTunnelProvider` and injected into the local `utun` interface.
5. **UDP Passthrough (Mosh):** Mosh traffic bypasses the TCP-PEP entirely. Packets are flagged with `BEST_EFFORT = 1`, bypassing ARQ retransmissions to let Mosh's state synchronization handle loss natively.

### 7.1.1 TCP-PEP Conformance Contract & State-Machine Subset (RFC 3135)

To ensure reliable, deterministic operation across high-latency, asymmetric acoustic links without running an unnecessarily complex TCP stack in kernel or extension space, V-RADM implements an RFC 3135 Section 3.8.1 (Split-Connection PEP) conforming minimal state-machine subset:

1. **Local Three-Way Handshake Termination (Fast SYN/ACK Spoofing):**
   * Upon intercepting an outbound TCP SYN packet from the local client (`OpenSSH`), the client-side PEP immediately generates a synthetic `SYN+ACK` segment (with zero synthetic RTT, initial sequence number $ISN_0 = 0$).
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
  Bytes 0x02..0x03: EXTENDED_SOURCE_SYMBOLS (K', 16 bits)
  Bytes 0x04..0x05: TOTAL_SOURCE_BLOCKS (Z, 16 bits, 1 <= Z <= 255)
  Bytes 0x06..0x09: TOTAL_BYTES (F, 32 bits, uncompressed file size)
  Bytes 0x0A..0x25: BLAKE3_224 (Leading 28 bytes [224 bits] of BLAKE3 checksum)

```

### 8.1 RFC 6330 Object Transmission Information (OTI) & Symbol Partitioning

To ensure deterministic encoding and decoding between independent RFC 6330 RaptorQ implementations, all SOTP sessions adhere strictly to the following parameters:

* **Common OTI Parameter Set:**
  * **Transfer Length ($F$):** Total transfer size in octets ($0 \le F < 2^{32}$), conveyed in Manifest bytes `0x06..0x09`.
  * **Symbol Size ($T$):** Exactly **32 bytes** ($\text{Al} = 4$).
  * **Symbol Alignment ($\text{Al}$):** **4 bytes** (guaranteeing 32-bit hardware alignment across platforms).
  * **Number of Sub-Blocks ($N$):** **1** (sub-blocking disabled; $T = 32$ fits inside a single cache line).
  * **Number of Source Blocks ($Z$):** $1 \le Z \le 255$ (Manifest bytes `0x04..0x05`; byte 0x04 is `0x00`, byte 0x05 holds $Z$).
  * **Maximum Source Symbols per Block ($K_{\text{max}}$):** **56,403** (RFC 6330 ceiling).
* **Source Block Partitioning (RFC 6330 §4.4.1.2):**
  * Total source symbols: $K_t = \lceil F / T \rceil$.
  * Number of source blocks: $Z = \lceil K_t / K_{\text{max}} \rceil$.
  * Partitioning calculation:
    $$K_L = \lceil K_t / Z \rceil, \quad K_S = \lfloor K_t / Z \rfloor, \quad J = K_t - Z \cdot K_S$$
  * Exactly $J$ source blocks contain $K_L$ source symbols, and $Z - J$ source blocks contain $K_S$ source symbols.
  * For each block, the extended source block size $K'$ is the smallest systematic block size from RFC 6330 Table 2 such that $K' \ge K$. Padded symbols ($K' - K$) are zero-filled by the encoder and are never transmitted over the air.
* **FEC Payload ID (RFC 6330 §3.2):**
  * Conveys the 8-bit Source Block Number (`SBN`) and 24-bit Encoding Symbol ID (`ESI`).
  * Source symbols have $0 \le \text{ESI} < K$; repair symbols have $K \le \text{ESI} < 2^{24}-1$.
* **Object Integrity (`BLAKE3_224`):**
  * Manifest bytes `0x0A..0x25` contain `BLAKE3_224`, defined as the first 28 bytes (224 bits) of the standard 32-byte BLAKE3 hash computed over the reconstructed $F$ bytes.
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
; Hand off 8kHz linear PCM to vradmd TCP daemon (uuid,service) using dynamic unique channel ID
same  => n,AudioSocket(${CHANNEL(uniqueid)},127.0.0.1:9099)
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
   * **Audio Session Configuration:** The app initializes `AVAudioSession.sharedInstance()` with category `.playAndRecord`, mode `.voiceChat`, and options `[.allowBluetooth, .mixWithOthers]`. The continuous execution of the real-time audio render callback prevents iOS from suspending the Main App process when backgrounded or when the device screen is locked.
   * **NetworkExtension Lifecycle:** `PacketTunnelProvider` executes in its own sandboxed daemon process managed directly by iOS `nesessionmanager`. Because the VPN tunnel remains active, the extension is not subject to standard app suspension, maintaining unbroken IP packet flow across the shared memory ring buffer while audio is running.

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
    uint8_t     security_tamper_detected; // Flagged (1) when >= 3 consecutive MAC failures occur in 60s
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
int32_t vradm_sotp_rx_fetch(vradm_engine_t* engine, uint8_t* out_buf, size_t max_len, uint8_t out_hash[28]); // 28-byte BLAKE3_224 digest

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
| **TC-05** | VAD & AGC Verification | 3GPP VAD Model 1 & 2 + Smartphone AGC model | PRBS-7 phase dither enabled; voiced maintenance carrier active | Measured over 10,000 independent 1-second trials. False DTX entry $P_{\text{DTX}} \le 0.01$. Zero AGC signal-clamping events. Dither-induced FER degradation $\Delta P_{\text{FER}} \le 0.5\%$ ($\le 0.005$) compared to un-dithered transmission. |
| **TC-06** | Burst Erasure Recovery | MCS 3 (165 ms frames) | 3 consecutive physical frame drops ($495\text{ ms}$ drop) | Selective Repeat ARQ triggers fast retransmission. Complete IP packet stream recovery within $\le \mathbf{1,450\text{ ms}}$ of drop start. Zero application errors. |
| **TC-07** | SOTP Mid-Stream Entry | AMR-WB @ 12.65 kbps | Receiver attaches at Frame 40 of a 100-symbol broadcast | Receiver syncs via Metadata Manifest within 8 frames. Object reconstructs with matching BLAKE3_224 checksum. |
| **TC-08a** | Real VoLTE Cellular Call (MCS 3) | Commercial Mobile VoLTE Network | Active 15-minute phone call between iPhone and Asterisk server | Interactive OpenSSH/TCP-PEP session maintained continuously. Keystroke round-trip confirmation time $\le 550\text{ ms}$. |
| **TC-08b** | Real Degraded / Free-Air Link (MCS 0/1) | Acoustic Speaker-to-Mic Air Gap / Degraded 3G Call | High ambient acoustic noise and multi-second frame periods | Mosh UDP terminal session maintained continuously. Predictive local echo renders keystrokes with $< 50\text{ ms}$ UI latency; remote screen converges within $1.5 \times T_{\text{frame}}$ after burst recovery. |
| **TC-09** | Concurrency & Thread-Safety | 8 concurrent AudioSocket TCP threads | Multi-channel load test on Linux daemon | Zero cross-session cross-talk, race conditions, or memory corruption. CPU scaling linear across threads. |
| **TC-10** | Sample-Slip Resilience & Squaring-Loss Sweep | MCS 2, MCS 3, and MCS 4 loopback | Injected random single-sample slips ($\pm 1$ sample every $500\text{ ms}$) across SNR sweep down to demotion threshold ($\text{SNR} \in [8\text{ dB}, 18\text{ dB}]$) | Multi-carrier 4th-power NDA DLL corrects slips within $\le 2.5\text{ ms}$. Multi-carrier array combining prevents squaring-loss unlock and decision-directed error cascades. Constellation lock maintained without frame loss down to $M = 0.60$. |
| **TC-10b** | Continuous Clock Drift Tracking | MCS 2, MCS 3, and MCS 4 cabled loopback | Injected continuous clock offset $\Delta F_s / F_s \in \{\pm 20, \pm 40, \pm 80, \pm 100\}\text{ PPM}$ across 10,000 frames | Farrow 3rd-order resampler and 4th-power NDA DLL dynamic tracking maintain synchronization without buffer overflow/underflow or bit slips. $P_{\text{FER}} \le 1.0 \times 10^{-4}$. Zero unrecoverable frame loss. |

---

## 12. Direct Implementation Instructions

1. **Workspace Layout:** Scaffold a Cargo workspace with `crates/vradm-core` (`#![no_std]` core with `alloc` for initialization), `crates/vradm-server` (multi-threaded Asterisk daemon), `crates/vradm-cli` (diagnostics), and `tests/vocoder_bench`.
2. **Wire Format Verification:** Implement `crates/vradm-core/src/link/frame.rs` and verify bit-exact layout of both the 64-byte Canonical Data Frame and 16-byte Authenticated Compact Control Frame.
3. **PLCP Bootstrap & Authentication:** Implement the Barker-13 dual-chirp generator, dual Extended Golay $[24, 12, 8]$ codecs, SipHash-2-4 control plane MAC with consecutive-failure lockout, and 2-FSK modulator.
4. **Carrier & Timing Recovery (4th-Power NDA DLL):** Implement multi-carrier 4th-power phase extraction and diversity combining across all active subcarriers, symbol-boundary transient exclusion zones, and fractional Farrow resampler in `crates/vradm-core/src/phy/dll.rs`.
5. **Soft-Decision RS Decoder:** Implement Chase/GMD soft-decision decoding in `crates/vradm-core/src/fec/rs.rs`, verifying that flagging 16 confidence-tagged erasures reconstructs corrupted frames under $2t + e \le 16$.
6. **Continuous Codec Validation:** Execute `tests/vocoder_bench` continuously against `libopencore-amr` and `vo-amrwbenc` before packaging C-ABI exports. Ensure zero regressions across tests TC-01 through TC-10b prior to platform deployment.