# Engineering Specification: Vocoder-Resilient Acoustic Data Modem (V-RADM)

**Document Version:** 3.8.9

**Status:** Closed Baseline Engineering Specification (Implementation-Ready Research Prototype)

**Primary Targets:** `vradm-core` (Rust C-ABI Thread-Safe Engine), iOS 17+ Client Adapter, Linux / Asterisk 20+ PBX Gateway Daemon

---

## 1. System Architecture & Process Boundaries

V-RADM provides transparent application connectivity across speech-compressed cellular voice channels (VoLTE, VoNR, 3G AMR, carrier VoIP) and acoustic air gaps using IPv4 packet transport for UDP and split-connection PEP transport for TCP. It enables unmodified network applications—specifically **OpenSSH** and **Mosh (Mobile Shell)**—to operate reliably under severe bandwidth, latency, and transcoding constraints.

To eliminate TCP congestion window collapse over high-latency and half-duplex links (such as MCS 0 with multi-second RTOs), V-RADM implements an embedded **Split-Connection Performance Enhancing Proxy (TCP-PEP)** informed by RFC 3135 (Informational).

* **Deployment Topology Operational Hierarchy:**
  * **Topology C (Primary Field Baseline, iOS 18.2+):** Leverages Apple's "Add Audio in Calls" API to inject synthesized modem PCM directly into an active native cellular voice call (VoLTE/AMR), requiring one-time manual user enablement via *Settings → Accessibility → Live Speech / Add Audio in Calls*. This provides single-phone, standalone operation for field and backcountry environments without extra hardware or secondary devices.
  * **Topology B (In-App VoIP Fallback, iOS 17+):** Establishes an in-app SIP/RTP session directly over cellular data to the Asterisk PBX, enabling single-device modem operation on iOS versions prior to 18.2.
  * **Topology A (Hardware Qualification & Lab Baseline):** Utilizes a wired TRRS dongle interconnecting the primary iPhone to a secondary handset or acoustic test rig. This serves as the reference hardware validation baseline, eliminating ambient acoustic noise, room reverberation, and handset AEC variables during DSP development, and provides an emergency zero-data-coverage failover.


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
| `0x00..0x01` | `SYNC_WORD` | 16 bits | Fixed frame delimiter: `0xD391`. In-band Layer 2 frame alignment and integrity delimiter evaluated *after* $8 \times 8$ block deinterleaving. It is NOT a raw physical serialized leading delimiter on the acoustic channel. |
| `0x02` | `CTRL` | 8 bits | Bit [7]: Mode (`0` = Standard IP Duplex, `1` = Simplex SOTP Broadcast)<br><br>Bits [6..4]: Active Frame MCS (`000` = MCS 0 .. `100` = MCS 4)<br><br>Bit [3]: TDD Turn Flag (`1` = Yield physical channel to peer / end of burst, `0` = Contiguous burst frame follows)<br><br>Bit [2]: `FRAME_CLASS` (`0` = Reliable Stream, `1` = Best-Effort Datagram)<br><br>Bits [1..0]: Wire Protocol Version (`10` = v3.8) |
| `0x03` | `SEQ` | 8 bits | Rolling transmit sequence number ($0\text{--}255$).<br>• When `FRAME_CLASS = 0`: Carries `REL_SEQ` ($0\text{--}255$), participating in Selective Repeat ARQ.<br>• When `FRAME_CLASS = 1`: Carries `BE_SEQ` ($0\text{--}255$), used solely for fragment reassembly and deduplication. Does NOT consume `REL_SEQ`. |
| `0x04` | `ACK_BASE` | 8 bits | Cumulative ACK: highest contiguous peer `REL_SEQ` received in-order. Strictly tracks reliable stream frames (`FRAME_CLASS = 0`). Best-effort frames never advance or stall `ACK_BASE`. |
| `0x05` | `ACK_MAP` | 8 bits | Bit [7] (MSB): Feedback Type (`0` = Normal Bitmap, `1` = Urgent NACK).<br><br>Bits [6..0]: Selective ACK bitmap for reliable frames `seq_advance(ACK_BASE, 1)` through `seq_advance(ACK_BASE, 7)` (`1` = received/acknowledged, `0` = missing/unacknowledged):<br>• Bit 0 (LSB): `seq_advance(ACK_BASE, 1)`<br>• Bit 1: `seq_advance(ACK_BASE, 2)`<br>• Bit 2: `seq_advance(ACK_BASE, 3)`<br>• Bit 3: `seq_advance(ACK_BASE, 4)`<br>• Bit 4: `seq_advance(ACK_BASE, 5)`<br>• Bit 5: `seq_advance(ACK_BASE, 6)`<br>• Bit 6: `seq_advance(ACK_BASE, 7)`<br>*Best-effort frames (`FRAME_CLASS = 1`) never participate in or stall `ACK_MAP`.*<br><br>*ARQ Window Coverage:* Combined with `ACK_BASE` (which is implicitly acknowledged), the 7 selective forward bits cover exactly $W_{\text{ARQ}} = 8$ distinct sequence numbers simultaneously. |
| `0x06` | `PAYLOAD_LEN` | 8 bits | Valid payload bytes $k$, where $0 \le k \le 38$.<br><br>• **Zero-Length Frames ($k = 0$):** Fully legal at Layer 2 for keepalives and pure ARQ feedback (`ACK_BASE` / `ACK_MAP`). Bytes `0x08..0x2D` are filled with zero padding; `PAYLOAD_CRC16` covers bytes `0x02..0x2D`. The IP reassembly engine ignores $k = 0$ frames (no IP packet slice delivered to TUN).<br>• **Data Frames ($1 \le k \le 38$):** Carries $k$ information bytes starting at Byte `0x08`. Bytes from $0x08 + k$ to $0x2D$ are zero-padded prior to CRC and RS parity computation. |
| `0x07` | `HEADER_CRC8` | 8 bits | CRC-8 covering bytes `0x02..0x06`. Parameters: Polynomial `0x07` ($x^8 + x^2 + x + 1$), Init `0x00`, RefIn `false`, RefOut `false`, XorOut `0x00`. (Test vector: ASCII `"123456789"` $\implies$ `0xF4`). |
| `0x08..0x2D` | `PAYLOAD` | 38 bytes | Information payload: 1-byte IP fragmentation header + up to 37 bytes IP data (or 38 bytes SOTP slice). |
| `0x2E..0x2F` | `PAYLOAD_CRC16` | 16 bits | CRC-16 covering bytes `0x02..0x2D`. Parameters: CRC-16/CCITT-FALSE, Polynomial `0x1021` ($x^{16} + x^{12} + x^5 + 1$), Init `0xFFFF`, RefIn `false`, RefOut `false`, XorOut `0x0000`. (Test vector: ASCII `"123456789"` $\implies$ `0x29B1`). Multi-byte field encoded in Big-Endian. |
| `0x30..0x3F` | `RS_PARITY` | 16 bytes | Systematic $\text{RS}(64, 48)$ Galois field parity covering bytes `0x00..0x2F` (see §6.0 for exact field and generator parameters). |

* **Total Protected Information Block:** Bytes `0x00..0x2F` = **48 bytes**.
* **Total FEC Parity Block:** Bytes `0x30..0x3F` = **16 bytes**.
* **Total Frame Length:** $48 + 16 = \mathbf{64\text{ bytes}}$ (512 bits).

#### Two-Layer Framing & Interleaving Architecture
To prevent confusion between logical frame structures and physical serialized symbols:
1. **Layer 2 (Logical Link Layer) - Canonical Data Frame (64 Bytes):**
   Comprises the complete systematic $\text{RS}(64, 48)$ codeword $[m_0 \dots m_{47} \mid p_0 \dots p_{15}]$. `SYNC_WORD` (`0xD391`) resides at logical bytes `0x00..0x01` as the first two message bytes ($m_0, m_1$), protected by both the RS code and block interleaving.
2. **Layer 1 (Physical Framing & Transmission):**
   The entire 64-byte logical frame is loaded into the $8 \times 8$ byte block interleaver ($\pi(i) = (i \bmod 8) \cdot 8 + \lfloor i/8 \rfloor$) and transmitted as serialized symbols.
3. **Receiver Synchronization & Demapping:**
   Raw physical burst synchronization and symbol clock alignment are established at Layer 1 exclusively by the PLCP Barker-13 dual-chirp preamble and the continuous sample-slip DLL (or CP correlator). Once 64 physical channel bytes and soft confidences are gathered, Layer 1 applies the inverse block deinterleaver ($\pi^{-1}$). Layer 2 then evaluates `bytes[0..1] == 0xD391` to confirm post-deinterleave frame alignment prior to RS errors-and-erasures decoding.


#### Canonical Modulo-256 Serial Number Arithmetic Primitives
Because 8-bit sequence numbers (`REL_SEQ`, `BE_SEQ`, `ACK_BASE`) wrap at 256 ($0\text{xFF} \to 0\text{x00}$), integer comparisons (e.g. `$a > b$`) produce invalid results across boundary wraps. All link-layer sequence comparisons, ARQ window evaluations, MCS commit sequence switchovers, and fragment aging strictly mandate RFC 1982 Serial Number Arithmetic defined by the following canonical primitives:

$$\operatorname{seq\_after}(a, b) \iff (a \ne b) \land (((a - b) \bmod 256) < 128)$$
$$\operatorname{seq\_after\_eq}(a, b) \iff ((a - b) \bmod 256) < 128$$
$$\operatorname{seq\_diff}(a, b) \equiv (a - b) \bmod 256$$
$$\operatorname{seq\_advance}(s, n) \equiv (s + n) \bmod 256$$

With an in-flight ARQ window bounded by $W_{\text{ARQ}} \le 8$ frames, any active sequence distance satisfies $\operatorname{seq\_diff}(a, b) \le 8 \ll 128$, guaranteeing unambiguous window evaluation and zero boundary aliasing.


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
* **Byte 0x04:** `ACK_MAP` (7-bit selective ACK bitmap matching Section 2.1 bit ordering).
* **Bytes 0x05..0x06:** `CCF_CRC16` (CRC-16/CCITT-FALSE covering bytes `0x02..0x04`, identical parameters to `PAYLOAD_CRC16`: Poly `0x1021`, Init `0xFFFF`, RefIn `false`, RefOut `false`, XorOut `0x0000`, Big-Endian).
* **Byte 0x07:** `CCF_MAC` (Truncated 8-bit SipHash-2-4 MAC computed over `SESSION_EPOCH || LE16(c_req) || CCF_CTRL || ACK_BASE || ACK_MAP || CCF_CRC16` using $K_{\text{CTRL\_MAC}}$, binding the control frame to the active session epoch and the full 16-bit monotonic control counter of the burst being acknowledged; see §4.0 and §4.1). Frames failing MAC validation are dropped before state-machine ingestion (see §4.1 for unauthenticated frame handling and noise separation).
* **Bytes 0x08..0x0F:** Systematic $\text{RS}(16, 8)$ Galois field parity covering bytes `0x00..0x07` (8 parity bytes correcting up to $t = 4$ erroneous bytes; see §6.0).
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
 │ 0    │ Feature  │ 20     │ 16        │ 1           │ 4        │ 80.0       │ 46.25    │ 43.56 (Burst) / 32.30 (Single) │ ~24.0        │
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
  * On the receiver, the synchronized PRBS-7 generator serves as a known prior/reference signal. Because the reference symbol ($m = 0$) contains dither $\Delta \theta_{\text{dither}}[0]$ and symbol $m$ contains $\Delta \theta_{\text{dither}}[m]$, the 4th-power reference-normalized ratio $r_k[n]$ contains the differential dither rotation $\theta_{\text{dither,residual}}[m] = 4(\Delta \theta_{\text{dither}}[m] - \Delta \theta_{\text{dither}}[0])$. The receiver explicitly subtracts this differential term from $\angle r_k[n]$ prior to inter-carrier timing slope estimation, carrier tracking, and demapping:
    $$\angle r_k^{\text{clean}}[n] = \angle r_k[n] - 4\left(\Delta \theta_{\text{dither}}[m] - \Delta \theta_{\text{dither}}[0]\right)$$

#### Mid-Frame Continuous Sample-Slip Recovery: Multi-Carrier 4th-Power DLL (NDA)

To prevent sample slips across asynchronous clock boundaries (`AVAudioEngine` vs. cellular baseband) from misaligning OFDM bins or DQPSK symbols between PLCP beacons without sacrificing data throughput:

* **Timing Recovery Architecture Scope by Active MCS:**
  * **MCS 2 & MCS 3:** Primary symbol timing tracking is executed by the 4th-power inter-carrier phase-slope detector ($\hat{\tau} \propto -\partial\angle r_k^{\text{clean}}/\partial f_k$) paired with complex baseband transition tracking on $u_k[n]$.
  * **MCS 4:** Primary symbol timing tracking is executed by the Cyclic Prefix (CP) cross-correlator on every 4.0 ms slot ($0.5\text{ ms}$ CP correlation peak). Because CP-OFDM subcarriers are demodulated in baseband FFT bins, 4th-power phase-slope timing tracking is disabled during MCS 4 operation.

1. **Analytic Signal Generation & 4th-Power Baseband Chain:** Rather than sacrificing a dedicated subcarrier to an unmodulated pilot tone—which would degrade bit-loading—all subcarriers carry 2-bit DQPSK data throughout the payload ($Z=4$ for MCS 2, $Z=8$ for MCS 3). Carrier and timing tracking relies on **4th-Power Non-Data-Aided (NDA)** processing:
   * **Analytic Bandpass Pre-Filtering:** To prevent unrejected negative-frequency image distortion at $-2f_k$ upon complex downmixing, the received real PCM audio $x[n]$ is converted to an analytic signal centered at subcarrier $k$:
     $$z_k[n] = \text{BPF}_k\{x[n]\} = x[n] * h_{\text{analytic}, k}[n] \in \mathbb{C}$$
   * **Analytic Downmixing:**
     $$u_k[n] = z_k[n] e^{-j 2\pi f_k n / F_s}$$
   * **4th-Power PSK Data Stripping:**
     $$q_k[n] = (u_k[n])^4$$
   * **Phase-Error Interpretation & Reference Phase Contribution:** For ideal 4-state phase modulation, the fourth-power operation strips the data constellation ($4 \cdot \Delta \phi_k \equiv 0 \pmod{2\pi}$). However, the transmit reference symbol ($m = 0$) sets initial carrier phases to $\phi_k(0) = \frac{k\pi}{4}$. Raising this to the 4th power contributes a non-zero, carrier-dependent phase:
     $$4 \cdot \phi_k(0) = k\pi \implies e^{j 4\phi_k(0)} = e^{j k\pi} = (-1)^k$$
     Across subcarrier frequencies $f_k = f_0 + k \cdot \Delta f$, this creates an artificial phase slope $\frac{\partial (k\pi)}{\partial f_k} = \frac{\pi}{\Delta f}$. If uncorrected, a timing regression on $\angle q_k$ would falsely perceive this reference-phase slope as propagation delay, producing a deterministic timing bias of $\tau_{\text{bias}} = -\frac{1}{8\pi}\frac{\pi}{\Delta f} = -\frac{1}{8\Delta f}$ ($-0.3125\text{ ms} = -2.5\text{ samples}$ on MCS 2; $-0.625\text{ ms} = -5.0\text{ samples}$ on MCS 3). Furthermore, the alternating signs $(-1)^k$ cause catastrophic destructive cancellation in positive-weight MRC combining.

2. **Reference-Normalized Variable & Unbiased Phase-Slope Timing Detector:**
   * To simultaneously eliminate the reference-phase slope bias and cancel static acoustic channel phase/group delay, the receiver normalizes the current 4th-power sample against the reference symbol sample $q_{k, \text{ref}}$ (measured during the interior quiescent window of symbol $m = 0$):
     $$r_k[n] = \frac{q_{k, \text{current}}[n] \cdot q^*_{k, \text{ref}}}{|q_{k, \text{ref}}|}$$
   * Because $q_{k, \text{ref}} \propto e^{j(4\phi_k(0) + 4\theta_{k, \text{channel}} + 4\Delta\theta_{\text{dither}}[0])}$, conjugate multiplication algebraically cancels both the transmit reference phase $4\phi_k(0) = k\pi$ and the static acoustic channel phase $4\theta_{k, \text{channel}}$. Removing the differential dither leaves only dynamic sample-slip timing error $\Delta \tau$ and residual carrier frequency offset:
     $$\angle r_k^{\text{clean}}[n] = \angle r_k[n] - 4(\Delta\theta_{\text{dither}}[m] - \Delta\theta_{\text{dither}}[0]) \approx -8\pi f_k \Delta \tau + 4 \Delta \omega_c t$$
   * The derivative of the cleaned normalized phase with respect to subcarrier frequency is strictly proportional to dynamic timing drift:
     $$\frac{\partial \angle r_k^{\text{clean}}}{\partial f_k} \approx -8\pi \Delta \tau$$
   * The receiver unwraps $\angle r_k^{\text{clean}}$ across active subcarriers ($Z=4$ for MCS 2, $Z=8$ for MCS 3) and computes the linear regression slope to estimate fractional timing delay $\Delta \hat{\tau}$:
     $$\Delta \hat{\tau} = -\frac{1}{8\pi} \frac{\sum_{k=0}^{Z-1} (f_k - \bar{f})(\angle r_k^{\text{clean}} - \overline{\angle r^{\text{clean}}})}{\sum_{k=0}^{Z-1} (f_k - \bar{f})^2}$$
     where $\bar{f} = \frac{1}{Z}\sum f_k$ and $\overline{\angle r^{\text{clean}}} = \frac{1}{Z}\sum \angle r_k^{\text{clean}}$.
   * Because the reference phase slope, differential dither, and static channel group delay are canceled, the timing bias is identically zero ($\tau_{\text{bias}} \equiv 0$).


3. **Phase-Aligned Coherent Carrier Combining (Maximum Ratio Combining):**
   * Before summing across subcarriers for residual carrier frequency tracking, dynamic timing phase is removed from each normalized residual:
     $$\tilde{r}_k[n] = r_k[n] \cdot e^{j 8\pi f_k \Delta \hat{\tau}}$$
   * Because $r_k[n]$ has already canceled the per-carrier reference phase $k\pi$ and channel phase $\theta_k$, derotation by $\Delta \hat{\tau}$ aligns all subcarrier residuals to an identical zero nominal phase angle ($0\text{ rad}$).
   * The phase-aligned residuals are combined using Maximum Ratio Combining (MRC) weights $w_k \propto \frac{|\mu_k|}{\sigma_k^2}$ based on measured post-4th-power SNR:
     $$\bar{r}[n] = \sum_{k=0}^{Z-1} w_k \cdot \tilde{r}_k[n]$$
   * Because all components are strictly co-phased, summation is fully constructive (zero cancellation from $(-1)^k$).
   * Residual carrier frequency offset is tracked from the unwrapped phase rate: $\Delta f_0 = \frac{1}{8\pi} \frac{d}{dt} \text{unwrap}(\angle \bar{r}[n])$.
   * *The theoretical coherent combining upper bound is $10 \log_{10}(Z)$ (+6.02 dB for MCS 2, +9.03 dB for MCS 3); actual measured combining gain under speech codec distortion is governed by TC-10b.*

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
* **Unvoiced / Noise Segment Detection & Erasure Policy:**
  During the 35.0 ms NCCF integration window, the receiver evaluates the normalized autocorrelation peak $\Lambda(\hat{S})$. Under normal voiced conditions, $\Lambda(\hat{S}) \ge 0.60\text{--}0.95$. In adverse ambient acoustic environments (wind gusts, campfire noise, acoustic transients, or vocal babble), if the maximum NCCF correlation peak falls below the unvoiced noise floor threshold:
  $$\Lambda(\hat{S}) < \Theta_{\text{unvoiced}} = 0.30$$
  or if the signal energy lacks periodicity, the received segment is classified as **Unvoiced/Noise**:
  * **Zero-Confidence Erasure Tagging:** The demodulator SHALL assign confidence $C_i = 0.0$ and designate the symbol as an unconditional trial erasure ($e$) for the Reed-Solomon GMD soft-decision decoder.
  * **Prohibition of Symbol Hold / Repetition:** Substituting or repeating the previous symbol (hold) is **strictly prohibited**. Repeating a stale symbol transforms an acoustic drop into an *undetected symbol error*, which consumes two degrees of freedom ($2s$) in the Reed-Solomon error budget ($2s + e \le 16$). Tagging an explicit erasure consumes only a single degree of freedom ($1e$), doubling the decoder's resilience to acoustic noise bursts.
  * **Prohibition of Premature Frame Discard:** The demodulator SHALL NOT abort or discard the 6.4-second physical frame on isolated unvoiced segments. The $\text{RS}(64, 48)$ code accommodates up to 16 completely erased symbols ($800\text{ ms}$ of continuous acoustic noise). Only if total uncorrectable errors exceed $2s + e > 16$ does the frame trigger an RS failure, caught by `PAYLOAD_CRC16` and recovered via Selective Repeat ARQ.

#### MCS 1: Robust Narrowband Cellular (Feature-Domain Reference Atom Codebook)

* **Alphabet:** $Y = 256$ joint speech-feature states ($8\text{ bits/symbol}$ at $50\text{ Bd}$, $T_{\text{sym}} = 20.0\text{ ms} = 160\text{ samples at } 8\text{ kHz}$).
* **Bit-to-Symbol Parameter Mapping:**
  Each 8-bit byte $S \in [0 \dots 255]$ ($S = b_7 b_6 b_5 b_4 b_3 b_2 b_1 b_0$) maps deterministically to the triad of speech atom synthesis parameters:
  $$\begin{aligned}
  \text{vowel\_idx} &= (S \gg 5) \& 0\text{x07} && \text{(Bits [7..5], 3 bits, selects Formant pair } 0 \dots 7) \\
  \text{pitch\_idx} &= (S \gg 2) \& 0\text{x07} && \text{(Bits [4..2], 3 bits, selects Pitch Lag } T_0[0 \dots 7]) \\
  \text{phase\_idx} &= S \& 0\text{x03}          && \text{(Bits [1..0], 2 bits, selects Phase Offset } \delta \in \{0, 1, 2, 3\}\text{ samples})
  \end{aligned}$$
* **Canonical Formant Table ($F_1, F_2$ in Hz, $Q = 5.0$):**
  - Index 0: $(300, 900)\text{ Hz}$ (`/u/`)
  - Index 1: $(350, 1400)\text{ Hz}$ (`/o/`)
  - Index 2: $(450, 1100)\text{ Hz}$ (`/ɔ/`)
  - Index 3: $(500, 1700)\text{ Hz}$ (`/ə/`)
  - Index 4: $(600, 1200)\text{ Hz}$ (`/a/`)
  - Index 5: $(650, 1900)\text{ Hz}$ (`/æ/`)
  - Index 6: $(750, 1300)\text{ Hz}$ (`/e/`)
  - Index 7: $(800, 2100)\text{ Hz}$ (`/i/`)
* **Canonical Pitch Lag Table ($T_0$ in samples at $8\text{ kHz}$):**
  - Index 0: $T_0 = 33\text{ samples}$ ($F_0 = 242.4\text{ Hz}$)
  - Index 1: $T_0 = 38\text{ samples}$ ($F_0 = 210.5\text{ Hz}$)
  - Index 2: $T_0 = 43\text{ samples}$ ($F_0 = 186.0\text{ Hz}$)
  - Index 3: $T_0 = 49\text{ samples}$ ($F_0 = 163.3\text{ Hz}$)
  - Index 4: $T_0 = 56\text{ samples}$ ($F_0 = 142.9\text{ Hz}$)
  - Index 5: $T_0 = 64\text{ samples}$ ($F_0 = 125.0\text{ Hz}$)
  - Index 6: $T_0 = 72\text{ samples}$ ($F_0 = 111.1\text{ Hz}$)
  - Index 7: $T_0 = 80\text{ samples}$ ($F_0 = 100.0\text{ Hz}$)
* **Excitation Pulse Train ($e(n)$):**

$$e(n) = \sum_{p=0}^{\lfloor (159-\delta)/T_0 \rfloor} \delta_{\text{dirac}}[n - (\delta + p \cdot T_0)]$$

* **Phase Offset ($\delta$, 2 bits):** $\delta = \text{phase\_idx} \in \{0, 1, 2, 3\}\text{ samples}$. Pulse positions are generated by a periodic pitch excitation pulse train; $\delta$ provides a fractional sample phase offset relative to symbol origin, emulating the excitation phase of cellular ACELP codebooks without forcing artificial subframe truncation.
* **Formant Filter ($h_{\text{vowel}}(n)$):** Direct-form II cascaded biquad IIR filter ($Q = 5.0$) modeling the selected vowel state. Digital biquad coefficients for each resonance $F_c$:

$$\omega_0 = \frac{2\pi F_c}{F_s}, \quad \alpha = \frac{\sin(\omega_0)}{2Q}$$

$$b_0 = \alpha, \quad b_1 = 0, \quad b_2 = -\alpha, \quad a_0 = 1 + \alpha, \quad a_1 = -2\cos(\omega_0), \quad a_2 = 1 - \alpha$$

* **Coefficient Normalization ($a_0 = 1.0$):** Implementation filters normalize all transfer function coefficients by $a_0$:
  $$\tilde{b}_0 = \frac{b_0}{a_0}, \quad \tilde{b}_1 = \frac{b_1}{a_0} = 0, \quad \tilde{b}_2 = \frac{b_2}{a_0}, \quad \tilde{a}_1 = \frac{a_1}{a_0}, \quad \tilde{a}_2 = \frac{a_2}{a_0}, \quad \tilde{a}_0 = 1.0$$
* **Filter State Continuity:** In Direct-Form II realizations, internal delay states ($w[n-1], w[n-2]$) are maintained continuously across consecutive symbols within the same frame to preserve vocal tract phase coherence across speech atom transitions. States are reset to zero at frame boundaries.
* **Canonical Zero-Initial-State Prototype Bank:**
  To ensure deterministic, stateless, and error-propagation-free demodulation across platforms:
  * **Offline Precomputation:** The 256 prototype speech atoms $\hat{s}_S[n]$ ($S \in [0 \dots 255]$, $N_{\text{sym}} = 160\text{ samples}$) are precomputed offline and stored in a static ROM lookup table ($256 \times 160 \times 4\text{ bytes} = 160\text{ KiB}$), synthesized with **zero initial filter states** ($w[-1] = w[-2] = 0$).
  * **Transient Settling & Confidence Bias Characterization:** Because the transmitter maintains continuous Direct-Form II filter state across consecutive symbols within a frame, symbols $m \ge 1$ carry a residual formant tail from symbol $m-1$. For the $Q = 5.0$ biquad resonators ($F_1, F_2 \in [300, 2100]\text{ Hz}$), the filter impulse response has a decay envelope time constant $\tau = \frac{1}{\pi B} \approx 2.0\text{--}5.0\text{ ms}$ ($16\text{--}40\text{ samples}$). The filter state transient is confined to the initial $10\text{--}25\%$ of the 20.0 ms symbol period, after which the waveform is dominated by steady-state periodic pitch pulse excitation. Correlating against zero-initial-state prototypes introduces a minor, deterministic confidence pessimism of $\Delta\Lambda \approx 0.02\text{--}0.05$ on symbols $m \ge 1$.
  * **Immutability of Error Control:** Because the Reed-Solomon decoder utilizes $C_{\text{byte}}$ strictly for *erasure ranking* in the GMD trial erasure sequence ($e \in \{16, 14, \dots, 0\}$) and not for soft-symbol accumulation, this slight pessimism is entirely benign: it marginally promotes borderline symbols into trial erasures, which the $\text{RS}(64, 48)$ decoder eliminates without loss of error correction capability. Conversely, attempting to track dynamic carry-over state at the receiver would introduce catastrophic error propagation from misclassified symbols and multiply CPU overhead $256\times$.
* **Synthesis:** $s(n) = w(n) \cdot [e(n) * h_{\text{vowel}}(n)]$.

#### MCS 2: Balanced Cellular (Experimentally Gated Hybrid)

* **Alphabet:** $Y = 256$ states ($8\text{ bits/symbol}$ at $100\text{ Bd}$, $T_{\text{sym}} = 10.0\text{ ms} = 80\text{ samples at } 8\text{ kHz}$).
* **Carrier Frequencies ($Z = 4$):** $f_k \in \{600, 1000, 1400, 1800\}\text{ Hz}$.
* **Framing & Reference Symbol:** Each frame comprises 65 symbols ($650.0\text{ ms}$): Symbol 0 ($m=0$) is a known differential reference symbol ($\phi_k(0) = \frac{k\pi}{4}$); Symbols $m = 1 \dots 64$ carry the 512 bits (64 bytes) of protected payload and FEC.
* **Sample-Exact Modulator:**

$$s(n) = w(n) \sum_{k=0}^{3} A_k \cos\left(\frac{2\pi f_k n}{F_s} + \phi_k(m) + \Delta \theta_{\text{dither}}(n)\right)$$

$$\phi_k(m) = \text{wrap}_{2\pi}\left(\phi_k(m-1) + \Delta \phi_k(m)\right), \quad \Delta \phi_k \in \left\{0, \frac{\pi}{2}, \pi, \frac{3\pi}{2}\right\}$$

Where $A_k = [0.8, 1.0, 0.9, 0.7]$.

* **Dibit-to-Phase Differential Gray Mapping (MCS 2 & MCS 3):**
Each pair of information bits $(b_1, b_0)$ modulates carrier $k$ according to canonical Gray coding:

| Dibit ($b_1 b_0$) | Phase Increment ($\Delta \phi_k(m)$) | Radial Angle (Degrees) |
|:---:|:---:|:---:|
| `00` | $0$ | $0^\circ$ |
| `01` | $+\frac{\pi}{2}$ | $+90^\circ$ |
| `11` | $+\pi$ | $+180^\circ$ |
| `10` | $+\frac{3\pi}{2} \equiv -\frac{\pi}{2}$ | $+270^\circ$ ($-90^\circ$) |

* **Bit Packing Order:** For MCS 2 ($Z=4$), each 8-bit symbol maps 4 dibits: Carrier 0 ($600\text{ Hz}$) takes bits [7..6], Carrier 1 ($1000\text{ Hz}$) takes [5..4], Carrier 2 ($1400\text{ Hz}$) takes [3..2], Carrier 3 ($1800\text{ Hz}$) takes [1..0]. For MCS 3 ($Z=8$), each 16-bit symbol maps 8 dibits: Carrier 3 ($600\text{ Hz}$) takes [15..14], down to Carrier 10 ($2000\text{ Hz}$) taking bits [1..0].


#### MCS 3: Wideband Cellular Cabled (Experimentally Gated Coherent)

* **Alphabet:** $Y = 65,536$ states ($16\text{ bits/symbol}$ at $200\text{ Bd}$, $T_{\text{sym}} = 5.0\text{ ms} = 40\text{ samples at } 8\text{ kHz}$).
* **Carrier Frequencies ($Z = 8$):** $f_k = k \cdot 200\text{ Hz}$ for $k \in [3..10]$ ($600\text{ Hz to } 2000\text{ Hz}$).
* **Framing & Reference Symbol:** Each frame comprises 33 symbols ($165.0\text{ ms}$): Symbol 0 ($m=0$) is a known differential reference symbol ($\phi_k(0) = \frac{k\pi}{4}$); Symbols $m = 1 \dots 32$ carry the 512 bits (64 bytes) of protected payload and FEC.
* **Sample-Exact Modulator:**

$$s(n) = w(n) \sum_{k=3}^{10} A_k \cos\left(\frac{2\pi f_k n}{F_s} + \phi_k(m) + \Delta \theta_{\text{dither}}(n)\right)$$

$$A_k = [0.6, 0.9, 1.0, 0.85, 0.7, 0.5, 0.4, 0.3], \quad \phi_k(0) = \frac{k \pi}{4}$$

* **Symbol Grid & AudioSocket / ACELP Block Alignment:**
  Transitions occur on 5.0 ms ACELP subframe boundaries ($T_{\text{sym}} = 40\text{ samples at } 8\text{ kHz}$):
  * **Subframe Grid Invariant:** All 5.0 ms symbol boundaries throughout transmission SHALL occur at sample indices $n \equiv 0 \pmod{40}$ relative to the start of the audio stream.
  * **AudioSocket Phase Locking:** Cellular ACELP vocoders (AMR-NB, AMR-WB, EVS) and the PBX AudioSocket interface packetize linear PCM into discrete 20 ms blocks ($160\text{ samples}$) or 40 ms blocks ($320\text{ samples}$). To prevent arbitrary phase offsets between modem symbols and the vocoder's internal 4-subframe analysis windows, the MCS 3 modulator SHALL phase-lock its symbol grid to sample index 0 of the first PCM block received from AudioSocket after PLCP handoff.
  * **PLCP Guard Alignment:** The PLCP beacon (520 samples Barker-13 + 3,840 samples 2-FSK beacon) spans 4,360 samples ($545.0\text{ ms}$). To maintain joint phase-locking to both the 40-sample (5 ms) subframe grid and the 160-sample (20 ms) AudioSocket packet grid, the transmitter emits exactly $35.0\text{ ms}$ ($280\text{ samples} = 7 \times 40$) of post-beacon guard silence before Symbol 0 of the data frame. This establishes the total PLCP interval at exactly $4,360 + 280 = \mathbf{4,640\text{ samples}}$ ($580.0\text{ ms}$), which is simultaneously an exact integer multiple of 40 samples ($116 \times 40\text{ samples}$) and 160 samples ($29 \times 160\text{ samples}$).

#### MCS 4: Real-Valued Hermitian CP-OFDM (Conventional Waveform Modem)

* **Sampling Rate:** $F_s = 8,000\text{ Hz}$.
* **Orthogonal Subcarrier Spacing:** $\Delta f = \frac{1}{T_{\text{useful}}} = \frac{8000}{28} = \mathbf{285.714\text{ Hz}}$.
* **Hermitian Symmetric Real-Valued IFFT:**

$$N_{\text{fft}} = 28\text{ points}, \quad N_{\text{cp}} = 4\text{ points} \implies N_{\text{total}} = 32\text{ samples (4.0 ms, 250 Bd)}$$

To enforce real-valued time-domain samples without index collision at DC ($k=0$) or Nyquist ($k=14$), frequency-domain bins are mapped via Hermitian conjugate symmetry:

$$X[k] = D_k, \quad X[28 - k] = D_k^* \quad \text{for } k \in [1 \dots 13], \quad X[0] = X[14] = 0$$

where inactive bins $k \in \{1, 4, 11, 12, 13\}$ are set to zero ($D_k = 0$). The synthesized real time-domain sequence is:

$$x(n) = \frac{1}{\sqrt{N_{\text{fft}}}} \sum_{k=0}^{N_{\text{fft}}-1} X[k] e^{j \frac{2\pi k n}{N_{\text{fft}}}} \in \mathbb{R}$$

* **Telephone-Band Carrier Allocation ($Z = 8$ Active Carriers):**

$$k \in \{2, 3, 5, 6, 7, 8, 9, 10\} \implies f_k \in \{571.4, 857.1, 1428.6, 1714.3, 2000.0, 2285.7, 2571.4, 2857.1\}\text{ Hz}$$

* **Unit-Energy Constellation Mapping:**
  Data bits are mapped to unit-energy Gray-coded QPSK symbols:
  $$D_k = \frac{1}{\sqrt{2}} \left( (1 - 2b_0) + j(1 - 2b_1) \right), \quad b_0, b_1 \in \{0, 1\}$$
* **Sample-Exact Cyclic Prefix Prepending:**
  For each 28-point real IFFT output $x[0 \dots 27]$, the 32-sample transmitted slot is constructed by prepending the final $N_{\text{cp}} = 4$ samples:
  $$x_{\text{slot}}[n] = \begin{cases} x[24 + n], & 0 \le n < 4 \\ x[n - 4], & 4 \le n < 32 \end{cases}$$
  yielding $x_{\text{slot}} = [x[24], x[25], x[26], x[27], x[0], x[1], \dots, x[27]]$.
* **Framing & Reference Slot Construction:**
  Each frame comprises 33 OFDM slots ($132.0\text{ ms}$): Slot 0 ($m=0$) is a known differential reference slot with $D_k(0) = \frac{1 + j}{\sqrt{2}}$ for all 8 active subcarriers $k \in \{2, 3, 5, 6, 7, 8, 9, 10\}$; Slots $m = 1 \dots 32$ carry the 512 bits (64 bytes) of protected payload and FEC. The reference slot undergoes identical 28-point IFFT modulation, cyclic prefix prepending, and peak-safe normalization as data slots $m = 1 \dots 32$.
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
   * **Bytes 0x11..0x14:** `EPOCH_HINT` (Set to `0x00000000` [`RESERVED`] in `SESSION_REQUEST`; carries computed 32-bit `SESSION_EPOCH` in `SESSION_ACCEPT`).
   * **Bytes 0x15..0x1C:** `BOOTSTRAP_MAC` (64-bit truncated SipHash-2-4 computed using PSK):
      * **For `SESSION_REQUEST` (`MSG_TYPE = 0xBE`):**
        $$\text{BOOTSTRAP\_MAC}_{\text{req}} = \text{Trunc64}\Big(\text{SipHash-2-4}_{\text{PSK}}\big(\text{MSG\_TYPE} \,\|\, R_A \,\|\, \text{LE32}(\text{EPOCH\_HINT})\big)\Big)$$
        (computed over the 21 bytes spanning `0x00..0x14` of the request frame).
      * **For `SESSION_ACCEPT` (`MSG_TYPE = 0xBF`):**
        $$\text{BOOTSTRAP\_MAC}_{\text{accept}} = \text{Trunc64}\Big(\text{SipHash-2-4}_{\text{PSK}}\big(R_A \,\|\, \text{MSG\_TYPE} \,\|\, R_B \,\|\, \text{LE32}(\text{EPOCH\_HINT})\big)\Big)$$
        (computed over 37 bytes, binding the initiator's active nonce $R_A$ to prevent replay or cross-session injection of accept responses; the initiator strictly verifies this tag against its pending $R_A$).
   * **Bytes 0x1D..0x1E:** `BOOTSTRAP_CRC16` (CRC-16/CCITT-FALSE covering bytes `0x00..0x1C`: Poly `0x1021`, Init `0xFFFF`, RefIn `false`, RefOut `false`, XorOut `0x0000`, Big-Endian).
   * **Byte 0x1F:** `RESERVED` (`0x00`).
   * **Modulation & Modulation-Specific RTO:** Modulated via robust 2-FSK at 100 Bd ($256\text{ bits} \times 10.0\text{ ms} = 2.56\text{ s}$ transmission) or MCS 0 ($64\text{ symbols} \times 50.0\text{ ms} = 3.20\text{ s}$ transmission). The handshake retransmission timeout $T_{\text{boot\_rto}}$ accounts for physical transmission time, acoustic reverberation guard ($150\text{ ms}$), receiver processing margin ($500\text{ ms}$), and peer response transmission:
     - **For 2-FSK Bootstrap (100 Bd):** $T_{\text{boot\_rto}} = \mathbf{6.0\text{ seconds}}$ with exponential backoff ($6.0\text{ s}, 9.0\text{ s}, 13.5\text{ s}$; up to 3 retries).
     - **For MCS 0 Bootstrap (20 Bd):** $T_{\text{boot\_rto}} = \mathbf{7.5\text{ seconds}}$ with exponential backoff ($7.5\text{ s}, 11.25\text{ s}, 16.875\text{ s}$; up to 3 retries).
   * **Handshake Replay & Transaction State Binding:** A responder binds session negotiation to its outstanding transaction state. While in an established active session, incoming `SESSION_REQUEST` frames containing nonces matching the active session or cached nonces from the preceding 24 hours MUST be silently discarded without generating `SESSION_ACCEPT`. When initiating a session, an endpoint binds its `SESSION_ACCEPT` acceptance strictly to the active outstanding nonce $R_A$ it transmitted.
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
  * **Extended Outage Counter Resynchronization (>127-Gap Recovery):** The RFC 3550 nearest-value unwrapping window has an unambiguous range of $\pm 127$ counter advances. If the receiver experiences an extended channel outage or deep fade such that no verified PLCP beacon is decoded for $\Delta c > 127$ counter increments (or outage duration $T_{\text{outage}} > 128 \times T_{\text{beacon}}$), the receiver MUST invalidate its sequence inference state (`c_rx_max = INVALID`), reset the anti-replay bitmap, and require an authenticated session resynchronization via a fresh bootstrap exchange before accepting subsequent PLCP beacons or CCF commands.
* **`BEACON_MAC8` & `CCF_MAC` Wire Formulations:**
  * **`BEACON_MAC8`:** An 8-bit truncated **SipHash-2-4** MAC computed over the candidate 16-bit counter $\widehat{c}$ and beacon payload using $K_{\text{CTRL\_MAC}}$:
    $$\text{BEACON\_MAC8} = \text{Trunc8}\Big(\text{SipHash-2-4}_{K_{\text{CTRL\_MAC}}}\big(\text{LE32}(\text{SESSION\_EPOCH}) \,\|\, \text{LE16}(\widehat{c}) \,\|\, \text{CUR\_MCS} \,\|\, \text{REQ\_MCS} \,\|\, \text{TX\_PWR} \,\|\, \text{BEAC\_SEQ}\big)\Big)$$
  * **`CCF_MAC` (16-bit Monotonic Anti-Replay Closure):** An 8-bit truncated **SipHash-2-4** MAC computed strictly over wire and session fields, binding the full 16-bit inferred control sequence $\widehat{c}_{\text{req}}$ of the burst being acknowledged:
    $$\text{CCF\_MAC} = \text{Trunc8}\Big(\text{SipHash-2-4}_{K_{\text{CTRL\_MAC}}}\big(\text{LE32}(\text{SESSION\_EPOCH}) \,\|\, \text{LE16}(\widehat{c}_{\text{req}}) \,\|\, \text{CCF\_CTRL} \,\|\, \text{ACK\_BASE} \,\|\, \text{ACK\_MAP} \,\|\, \text{CCF\_CRC16}\big)\Big)$$
    *Because $\widehat{c}_{\text{req}} \in [0 \dots 59,999]$ is the full 16-bit inferred control counter rather than the 8-bit truncated `BEAC_SEQ`, the 11-byte MAC preimage ($4 + 2 + 1 + 1 + 1 + 2 = 11\text{ B}$) is cryptographically unique across the entire 60,000-beacon lifetime of the session epoch. When Node A receives a CCF, it verifies that $\widehat{c}_{\text{req}}$ matches its locally recorded transmit counter for that burst ($c_{\text{tx\_last}}$). Stale CCFs from earlier turns—even if sharing the same `ACK_BASE` or 8-bit `BEAC_SEQ` wrap—produce mismatched MAC tags and are discarded before state machine processing. However, because the truncated tag length is 8 bits ($2^8 = 256$ states), `CCF_MAC` operates strictly as a lightweight authenticated integrity filter against accidental channel corruption, stale turn replays, and casual blind injection; it does NOT provide cryptographic work-factor security against an active adversary repeatedly guessing tags over the air. Application data confidentiality and cryptographically strong authentication remain anchored in SSH-2 and Mosh.*
  * **Single-Outstanding-Request Invariant:** At most one authenticated control transaction requiring a CCF response (e.g., MCS commit handshake or TDD turn handover) may be outstanding per direction at any given time. A new control transaction cannot be initiated until the active transaction has been committed, rejected, or timed out ($T_{\text{RTO}}$). Because $\widehat{c}_{\text{req}}$ is inferred by the receiver from the active transaction context rather than transported explicitly in the CCF wire format, this invariant guarantees that arriving CCFs map deterministically and unambiguously to exactly one transaction context, preventing race conditions between overlapping requests.
  * **CCF Semantic Idempotency & Duplicate Suppression:** For each inferred monotonic request counter $\widehat{c}_{\text{req}}$, each command class (e.g. `TDD_YIELD`, `MCS_COMMIT_ACK`, `TDD_GRANT`) has exactly one accepted semantic state transition. Subsequent authenticated duplicate CCF frames matching the active $\widehat{c}_{\text{req}}$ are processed as link-level retransmissions to update ARQ acknowledgment bitmaps, but MUST NOT trigger repeated state-machine transitions (e.g., granting channel ownership multiple times or executing redundant MCS step-downs).

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

### 4.2 Deterministic PLCP Parameters & Waveform Construction

* `PLCP_CHIP_RATE`: 200 chips/s ($5.0\text{ ms/chip}$, 40 samples at $8\text{ kHz}$).
* `PLCP_PREAMBLE`: 13 chips $\times 5.0\text{ ms} = \mathbf{65.0\text{ ms}}$ (520 samples at $8\text{ kHz}$).
* `PLCP_GUARD`: $10.0\text{ ms}$ silence (80 zero samples) before and after header.
* `PLCP_HEADER_PAYLOAD`: 16 bits (`CUR_MCS` [3b], `REQ_MCS` [3b], `TX_PWR` [2b], `BEAC_SEQ` [8b]) + 8-bit `BEACON_MAC8` = **24 information bits**.
* `PLCP_HEADER_FEC` & Bit-Exact Codeword Partitioning:
  The 24 information bits are partitioned into two 12-bit words ($m^{(1)}, m^{(2)}$), each independently encoded by the systematic Extended Golay $[24, 12, 8]$ codec into 24-bit codewords ($c^{(1)}, c^{(2)}$):
  * **Codeword 1 ($m^{(1)} \in [0 \dots 4095]$, 12 bits):**
    - Bits [11..9] (MSB): `CUR_MCS` (3 bits, values `000` through `100`).
    - Bits [8..6]: `REQ_MCS` (3 bits, values `000` through `100`).
    - Bits [5..4]: `TX_PWR` (2 bits, `00` = 0 dB, `01` = -3 dB, `10` = -6 dB, `11` = -9 dB).
    - Bits [3..0] (LSB): `BEAC_SEQ[7..4]` (High nibble of beacon counter, 4 bits).
    $$m^{(1)} = (\text{CUR\_MCS} \ll 9) \mid (\text{REQ\_MCS} \ll 6) \mid (\text{TX\_PWR} \ll 4) \mid ((\text{BEAC\_SEQ} \gg 4) \& 0\text{x0F})$$
    yielding 24-bit encoded codeword $c^{(1)} = (m^{(1)} \ll 12) \mid (m^{(1)} \cdot P_{12} \pmod 2)$.
  * **Codeword 2 ($m^{(2)} \in [0 \dots 4095]$, 12 bits):**
    - Bits [11..8] (MSB): `BEAC_SEQ[3..0]` (Low nibble of beacon counter, 4 bits).
    - Bits [7..0] (LSB): `BEACON_MAC8` (Complete 8-bit SipHash-2-4 MAC, preserved intact without boundary splitting).
    $$m^{(2)} = ((\text{BEAC\_SEQ} \& 0\text{x0F}) \ll 8) \mid \text{BEACON\_MAC8}$$
    yielding 24-bit encoded codeword $c^{(2)} = (m^{(2)} \ll 12) \mid (m^{(2)} \cdot P_{12} \pmod 2)$.
  * **Serialized Transmission:** Codeword 1 ($c^{(1)}$, 24 bits, MSB first) is serialized immediately followed by Codeword 2 ($c^{(2)}$, 24 bits, MSB first), forming the complete 48-bit header payload.
* `PLCP_MODULATION`: 2-FSK ($1200\text{ Hz} = \text{Mark}, 1600\text{ Hz} = \text{Space}$) at $100\text{ Bd}$ ($10.0\text{ ms/bit}$). Total header duration = $48 \times 10.0\text{ ms} = \mathbf{480.0\text{ ms}}$.
* `PLCP_TOTAL_DURATION`: $65.0 + 10.0 + 480.0 + 10.0 = \mathbf{565.0\text{ ms}}$.
* **Cadence:** PLCP is transmitted at `SESSION_START`, `TDD_DATA_TURN`, `CONTINUOUS_SYNC` (every 16 frames), and `MCS_CHANGE`. *Compact Control Frames (CCFs) do NOT require a PLCP beacon.*

#### Barker-13 Dual-Chirp Waveform Construction
The 65.0 ms preamble uses the 13-chip Barker sequence:
$$B = [+1, +1, +1, +1, +1, -1, -1, +1, +1, -1, +1, -1, +1] \quad \text{for } k \in [0 \dots 12]$$

Each 5.0 ms chip ($N_{\text{chip}} = 40\text{ samples}$ at $F_s = 8\text{ kHz}$) is synthesized as a paired dual-chirp composed of an up-chirp followed by a down-chirp ($T_{\text{sub}} = 2.5\text{ ms} = 20\text{ samples}$ each) spanning $f_1 = 600\text{ Hz}$ to $f_2 = 1800\text{ Hz}$:
* **Up-Chirp ($0 \le m < 20$):**
  $$c_{\text{up}}(m) = \cos\left(2\pi \left(f_1 \frac{m}{F_s} + \frac{f_2 - f_1}{2 T_{\text{sub}}} \left(\frac{m}{F_s}\right)^2\right)\right)$$
* **Down-Chirp ($20 \le m < 40$):**
  To guarantee phase and frequency continuity without instantaneous phase jumps across sub-symbols, the down-chirp's initial phase is mathematically aligned to the terminal phase of the up-chirp at $t = T_{\text{sub}}$:
  $$\phi_{\text{down\_init}} = 2\pi \left(\frac{f_1 + f_2}{2}\right) T_{\text{sub}} = 2\pi (1200\text{ Hz})(0.0025\text{ s}) = 6\pi \equiv 0\pmod{2\pi}$$
  $$c_{\text{down}}(m - 20) = \cos\left(\phi_{\text{down\_init}} + 2\pi \left(f_2 \frac{m-20}{F_s} - \frac{f_2 - f_1}{2 T_{\text{sub}}} \left(\frac{m-20}{F_s}\right)^2\right)\right)$$
* **Phase and Frequency Continuity:**
  At the mid-chip transition boundary ($m = 20$, $t = T_{\text{sub}}$), the up-chirp phase reaches $6\pi \equiv 0\pmod{2\pi}$ with instantaneous frequency $f_2 = 1800\text{ Hz}$, exactly matching the down-chirp entry phase ($0\text{ rad}$) and entry frequency ($1800\text{ Hz}$). At the end of the chip ($m = 40$, $t = 2T_{\text{sub}}$), the down-chirp phase reaches $6\pi + 6\pi = 12\pi \equiv 0\pmod{2\pi}$ with instantaneous frequency $f_1 = 600\text{ Hz}$, perfectly matching the $0\text{ rad}$ entry phase of the succeeding chip.
* **Synthesized Chip Sample:**
  $$s_{\text{preamble}}[40k + m] = B[k] \cdot w_{\text{taper}}[m] \cdot \begin{cases} c_{\text{up}}(m) & 0 \le m < 20 \\ c_{\text{down}}(m-20) & 20 \le m < 40 \end{cases}$$
  where $w_{\text{taper}}[m]$ applies a 2-sample raised-cosine taper at chip edges ($m \in \{0, 1, 38, 39\}$) to ensure phase and amplitude envelope continuity. The dual-chirp structure provides matched-filter Doppler immunity and precise sub-sample correlation peak resolution.
* **Sampled Waveform Step Limits:**
  Across the complete 520-sample synthesized Barker preamble waveform:
  1. Intra-chip continuous sample step: $|s_{\text{preamble}}[n] - s_{\text{preamble}}[n-1]| \le 1.20$ for all $n \in [1 \dots 519]$ (nominal maximum discrete sample step is $\approx 1.167$).
  2. Inter-chip boundary transition step: $|s_{\text{preamble}}[40k] - s_{\text{preamble}}[40k-1]| \le 0.30$ for all chip boundaries $k \in [1 \dots 12]$ (including adjacent Barker chips with opposite sign polarity, where the 2-sample raised-cosine edge taper strictly limits boundary transitions to $\le 0.276$).

#### Extended Golay $[24, 12, 8]$ Canonical Parity Matrix & Codec Parameters
The PLCP header FEC uses the systematic Extended Binary Golay $[24, 12, 8]$ code with generator matrix $G = [I_{12} \mid P_{12}]$, encoding 12 information bits $m = [m_{11} \dots m_0]$ into 24 codeword bits $c = [m \mid p]$ where $p = m \cdot P_{12} \pmod 2$.

To guarantee that all implementations construct an identical, interoperable codec, $P_{12}$ is explicitly defined by the canonical $12 \times 12$ binary matrix:

```text
Row  0: 0xDC5 -> [1, 1, 0, 1, 1, 1, 0, 0, 0, 1, 0, 1]
Row  1: 0x6E3 -> [0, 1, 1, 0, 1, 1, 1, 0, 0, 0, 1, 1]
Row  2: 0xB71 -> [1, 0, 1, 1, 0, 1, 1, 1, 0, 0, 0, 1]
Row  3: 0x5B9 -> [0, 1, 0, 1, 1, 0, 1, 1, 1, 0, 0, 1]
Row  4: 0x2DD -> [0, 0, 1, 0, 1, 1, 0, 1, 1, 1, 0, 1]
Row  5: 0x16F -> [0, 0, 0, 1, 0, 1, 1, 0, 1, 1, 1, 1]
Row  6: 0x8B7 -> [1, 0, 0, 0, 1, 0, 1, 1, 0, 1, 1, 1]
Row  7: 0xC5B -> [1, 1, 0, 0, 0, 1, 0, 1, 1, 0, 1, 1]
Row  8: 0xE2D -> [1, 1, 1, 0, 0, 0, 1, 0, 1, 1, 0, 1]
Row  9: 0x717 -> [0, 1, 1, 1, 0, 0, 0, 1, 0, 1, 1, 1]
Row 10: 0xB8B -> [1, 0, 1, 1, 1, 0, 0, 0, 1, 0, 1, 1]
Row 11: 0xFFE -> [1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0]
```

* **Mandatory Algebraic Properties & Test Vectors:**
  1. **Self-Duality:** The code is self-dual: $G G^T \equiv 0 \pmod 2$, which holds if and only if $P_{12} P_{12}^T \equiv I_{12} \pmod 2$.
  2. **Minimum Distance:** $d_{\text{min}} = 8$.
  3. **Exact Weight Enumerator:** Across all $2^{12} = 4,096$ codewords:
     $$A_0 = 1, \quad A_8 = 759, \quad A_{12} = 2576, \quad A_{16} = 759, \quad A_{24} = 1$$
     Every codeword has Hamming weight divisible by 4 (doubly-even code).
  4. **Error Correction Capability:** Corrects up to 3 arbitrary bit errors per 24-bit codeword ($t = 3$). Codewords with $\ge 4$ bit errors are flagged as uncorrectable physical erasures.
  5. **Standard Test Vectors:**
     * **Encoding Test Vector:** Information word $m = \text{0x5A5}$ ($[0, 1, 0, 1, 1, 0, 1, 0, 0, 1, 0, 1]$ where bit 11 is MSB `0` and bit 0 is LSB `1`) generates parity $p = m \cdot P_{12} = \text{0x1D9}$ ($[0, 0, 0, 1, 1, 1, 0, 1, 1, 0, 0, 1]$), producing 24-bit codeword $c = (m \ll 12) \mid p = \mathbf{\text{0x5A51D9}}$ with exact Hamming weight 12.
     * **Error Correction Verification Vector:** Corrupting $c$ with 3 bit errors via bitwise XOR mask $\text{0xA00040}$ ($c \oplus \text{0xA00040} = \mathbf{\text{0xFA5199}}$) produces a received vector that decodes uniquely back to information word $\mathbf{\text{0x5A5}}$ at minimum Hamming distance 3.


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
2. **Commit Ack:** Node B decodes the request, verifies channel metric $M \ge 0.85$ and valid `CCF_MAC`, and responds with a Compact Control Frame setting `CCF_CTRL` to `MCS_COMMIT_ACK` with commit sequence boundary $S \equiv \text{ACK\_BASE}$. The exact sequence boundary for rate switchover is defined as $\text{SEQ} = \operatorname{seq\_advance}(\text{ACK\_BASE}, 1) = (\text{ACK\_BASE} + 1) \bmod 256$.
3. **Synchronous Switchover (Two Generals Resolution):**
   * Node B transitions its payload demodulator for sequences satisfying $\operatorname{seq\_after\_eq}(\text{SEQ}, \operatorname{seq\_advance}(\text{ACK\_BASE}, 1))$ **if and only if** it demodulates a valid PLCP header with `CUR_MCS == target`. Because the PLCP beacon is modulated via noncoherent 2-FSK at 100 Bd, it is universally decodable regardless of active payload mode, serving as the definitive, unambiguous transition trigger.
   * Node A only transitions its payload modulator to `target` for frame sequences starting at $\operatorname{seq\_advance}(\text{ACK\_BASE}, 1)$ after receiving a verified `MCS_COMMIT_ACK` CCF from Node B.

4. **Lost `MCS_COMMIT_ACK` Recovery Policy:**
   * If Node B's CCF is lost, dropped, or corrupted over the air, Node A's retransmission timer ($T_{\text{RTO}}$) expires without receiving an `MCS_COMMIT_ACK`.
   * Node A **MUST NOT** switch to `target`. Node A remains at `CUR_MCS` and re-transmits `REQ_MCS = target` in its next burst.
   * Node A allows up to $N_{\text{mcs\_retry}} = 3$ consecutive attempts.
   * If no valid `MCS_COMMIT_ACK` is received after 3 attempts, Node A aborts the rate adaptation attempt, resets `REQ_MCS = CUR_MCS`, and enforces a mandatory **10.0-second rate-adaptation cooldown timer** before attempting another upshift.
5. **Unilateral Emergency Downshifts:**
   * If channel metrics degrade sharply ($M < 0.60$), downshifting is **unilateral and immediate**. Node A sets `CUR_MCS = lower` directly in its next PLCP beacon without requiring a prior two-phase commit handshake, preventing connection drops under sudden fading.

---

## 5. Half-Duplex Time-Division Duplexing (TDD Protocol)

In free-air acoustic speakerphone environments (`RTO_PROFILE = 2`), simultaneous bidirectional audio triggers handset Acoustic Echo Cancellation (AEC) and non-linear speech ducking, corrupting or canceling demodulator inputs. Therefore, **TDD turn-taking applies to any modulation mode (including MCS 0 and MCS 1) whenever operating over half-duplex acoustic paths**. When deployed over full-duplex cabled dongles or digital PBX channels (`RTO_PROFILE = 1`), TDD is disabled and nodes operate in continuous full duplex.

```
 0s                  0.565s                 6.965s  7.115s      7.265s                 8.865s  9.015s     9.165s
 ┌───────────────────┬──────────────────────┬───────┬───────────┬──────────────────────┬───────┬──────────┐
 │ PLCP Control      │ Node A: Data Frame   │ EOT   │ Acoustic  │ Node B: CCF          │ EOT   │ Acoustic │
 │ Beacon (565 ms)   │ (64 Bytes, 512 bits) │ Tone  │ Decay     │ (16 Bytes, 128 bits) │ Tone  │ Decay    │
 └───────────────────┴──────────────────────┴───────┴───────────┴──────────────────────┴───────┴──────────┘
  ◄────────────── Node A Transmit Turn ────────────► ◄─ Guard ─► ◄── Node B Turn ─────► ◄─ Guard ─►

```

* **Acoustic Channel Turn Guard ($150\text{ ms}$):** Accommodates physical room reverberation, acoustic decay, and transceiver audio buffer flush before reversing direction.
* **EOT Dual-Tone Burst:** The $150.0\text{ ms}$ dual-tone burst ($1400\text{ Hz} + 1800\text{ Hz}$ at $-12.0\text{ dBFS}$) signals channel yield. Placing both frequencies within the telephone passband ($300\text{--}3400\text{ Hz}$) with high SNR ensures robust detection post-vocoder while preventing AEC from categorizing the burst as continuous echo.

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

Where $\hat{S}$ is the highest-likelihood candidate speech atom and $S_{\text{second}}$ is the runner-up candidate:
* **MCS 0 (Pitch Autocorrelation):** $\Lambda(S)$ is the normalized autocorrelation peak computed by the NCCF pitch estimator over the final 35.0 ms of the 50.0 ms symbol interval ($m \in [0 \dots 15]$). If the maximum correlation peak falls below the unvoiced noise floor threshold ($\Lambda(\hat{S}) < \Theta_{\text{unvoiced}} = 0.30$), the segment is classified as unvoiced noise: confidence is set to $C_i = 0.0$ and the byte is tagged as an unconditional erasure ($e$).
* **MCS 1 (Speech Atom Cross-Correlation):** $\Lambda(S)$ is the peak normalized cross-correlation between the received 20.0 ms audio segment $r[n]$ ($N_{\text{sym}} = 160$ samples) and the prototype synthesized speech atom $\hat{s}_S[n]$ loaded from the canonical Zero-Initial-State Prototype Bank:

$$\Lambda(S) = \max_{\tau \in [0 \dots 3]} \frac{\sum_{n=0}^{N_{\text{sym}}-1} r[n] \cdot \hat{s}_S[n - \tau]}{\sqrt{\sum_{n=0}^{N_{\text{sym}}-1} r[n]^2 \sum_{n=0}^{N_{\text{sym}}-1} \hat{s}_S[n - \tau]^2}}$$

* **Byte Confidence & Erasure Tagging Threshold ($\Theta_{\text{erase}}$):** $C_{\text{byte}} = \min(C_{\text{symbols\_in\_byte}})$. Bytes with $C_{\text{byte}} < \Theta_{\text{erase}} = 0.35$ (or unvoiced MCS 0 segments with $\Lambda < \Theta_{\text{unvoiced}} = 0.30$) are flagged as trial erasures for the GMD decoder. Threshold $\Theta_{\text{erase}} = 0.35$ provides an optimal operating point across 3GPP AMR error profiles: higher values trigger excessive trial erasures ($>16$), while lower values permit undetected symbol errors to consume twice the RS correction budget ($2s$).

### 6.1.1 $8 \times 8$ Byte Block Interleaver & Deinterleaver

To protect against time-localized speech coder transients, pitch pulse distortions, and frame boundary erasures, every 64-byte Canonical Data Frame is processed through a block interleaver that permutes bytes and their corresponding soft confidence values $C_{\text{byte}}$ simultaneously:

* **Two-Layer Framing & Interleaving Separation:**
  The $8 \times 8$ byte block interleaver operates strictly at Layer 1. The input to the interleaver is the full 64-byte Logical Canonical Frame $[D \mid P]$ constructed at Layer 2 (bytes $0 \dots 47$ containing `SYNC_WORD`, headers, data payload, and `PAYLOAD_CRC16`, followed by bytes $48 \dots 63$ containing the 16 bytes of RS parity). On transmission, the entire 64-byte block is permuted by $\pi(i)$ before physical symbol modulation. On reception, the demodulator gathers the 64 bytes, applies deinterleaver permutation $\pi^{-1} \equiv \pi$, and hands the reassembled 64-byte frame to Layer 2, where `SYNC_WORD` (`0xD391`) is validated as an in-band frame integrity delimiter. Raw physical burst acquisition is handled independently by the PLCP Barker-13 dual-chirp sequence and the 4th-power DLL.
* **Block Dimensions:** $8 \text{ rows} \times 8 \text{ columns} = 64 \text{ bytes}$ (indices $0 \dots 63$).
* **Interleaver Permutation (Transmitter):** Bytes are loaded into the $8 \times 8$ matrix in row-major order and extracted in column-major order:
  $$\pi(i) = (i \bmod 8) \cdot 8 + \left\lfloor \frac{i}{8} \right\rfloor, \quad \text{for } i \in [0 \dots 63]$$
  where $i$ is the linear byte index of the 64-byte frame $[D \mid P]$ (input bytes $0..47$ payload, $48..63$ RS parity).
* **Deinterleaver Permutation (Receiver):** Because transposition of a square matrix is self-inverting, the deinterleaver permutation is identical:
  $$\pi^{-1}(j) = (j \bmod 8) \cdot 8 + \left\lfloor \frac{j}{8} \right\rfloor, \quad \text{for } j \in [0 \dots 63]$$
* **Burst Dispersion:** Any contiguous burst error of up to 8 consecutive bytes on the acoustic channel is dispersed such that no more than 1 erroneous byte appears in any 8-byte RS codeword segment, well within the error-correcting capability of $\text{RS}(64, 48)$.

### 6.1.2 Reed-Solomon Codec Parameters & GMD Soft-Decision Decoding

V-RADM employs two systematic Reed-Solomon codes defined over the Galois field $\text{GF}(2^8)$:

1. **Galois Field Definition ($\text{GF}(2^8)$):**
   * Primitive polynomial: $p(x) = x^8 + x^4 + x^3 + x^2 + 1$ ($0x11D = 285$ decimal).
   * Primitive root / field generator: $\alpha = 0x02$ ($\alpha \in \text{GF}(2^8)$ where $\alpha^8 = \alpha^4 + \alpha^3 + \alpha^2 + 1$).
   * Polynomial Representation & Remainder Ordering: Polynomial coefficients are ordered from highest degree to lowest degree ($g_{2t} x^{2t} + g_{2t-1} x^{2t-1} + \dots + g_0$). When computing systematic parity via polynomial division $m(x) \cdot x^{2t} \pmod{g(x)}$, the resulting remainder polynomial $r(x) = p_0 x^{2t-1} + p_1 x^{2t-2} + \dots + p_{2t-1}$ appends parity bytes in descending degree order ($p_0 \dots p_{2t-1}$).
2. **Canonical Data Frame Code ($\text{RS}(64, 48)$):**
   * Block length: $N = 64$ bytes; Message length: $K = 48$ bytes; Parity length: $2t = 16$ bytes (error correction capability $t = 8$ bytes).
   * Generator polynomial: $g(x) = \prod_{i=0}^{15} (x - \alpha^i)$ (first root $b = 0$, roots $\alpha^0, \alpha^1, \dots, \alpha^{15}$).
   * Generator Polynomial Coefficients (descending powers $x^{16} \dots x^0$, hex):
     `0x01, 0x3B, 0x0D, 0x68, 0xBD, 0x44, 0xD1, 0x1E, 0x08, 0xA3, 0x41, 0x29, 0xE5, 0x62, 0x32, 0x24, 0x3B`
   * Systematic Codeword Layout: 48 information bytes ($m_0 \dots m_{47}$, spanning `SYNC_WORD` through `PAYLOAD_CRC16`) followed by 16 parity bytes ($p_0 \dots p_{15}$).
   * Generalized Minimum Distance (GMD) Errors-and-Erasures Decoding: Solves $2s + e \le 16$, where $s \le 8$ is corrected symbol errors and $e \le 16$ is tagged erasures. Bytes with deinterleaved confidence $C_{\text{byte}} < 0.35$ are flagged as trial erasures, testing $e \in \{16, 14, 12, \dots, 0\}$ until a valid codeword matching `PAYLOAD_CRC16` is found.
   * **Standard Test Vector:**
     * Information bytes (48 bytes): `56 52 41 44 4D 5F 54 45 53 54 5F 46 52 41 4D 45 00 01 02 03 04 05 06 07 08 09 0A 0B 0C 0D 0E 0F 10 11 12 13 14 15 16 17 18 19 1A 1B 1C 1D 1E 1F` (ASCII `"VRADM_TEST_FRAME"` followed by `0x00..0x1F`).
     * Parity bytes (16 bytes): `97 65 00 FE 8D 05 17 4F 63 7C FE 74 32 AF 3D EE`.
     * Codeword (64 bytes): Information bytes followed by parity bytes; syndromes evaluate to zero across all 16 roots $\alpha^0 \dots \alpha^{15}$.
3. **Compact Control Frame Code ($\text{RS}(16, 8)$):**
   * Block length: $N = 16$ bytes; Message length: $K = 8$ bytes; Parity length: $2t = 8$ bytes (error correction capability $t = 4$ bytes).
   * Generator polynomial: $g(x) = \prod_{i=0}^{7} (x - \alpha^i)$ (roots $\alpha^0 \dots \alpha^7$).
   * Generator Polynomial Coefficients (descending powers $x^8 \dots x^0$, hex):
     `0x01, 0xFF, 0x0B, 0x51, 0x36, 0xEF, 0xAD, 0xC8, 0x18`
   * Systematic Codeword Layout: 8 information bytes ($m_0 \dots m_7$, spanning `SYNC_WORD` through `CCF_MAC`) followed by 8 parity bytes ($p_0 \dots p_7$).
   * **Standard Test Vector:**
     * Information bytes (8 bytes): `D3 91 01 0A 00 29 B1 4F`.
     * Parity bytes (8 bytes): `EF 1D 1F 6C 64 B6 63 AE`.
     * Codeword (16 bytes): Information bytes followed by parity bytes; syndromes evaluate to zero across all 8 roots $\alpha^0 \dots \alpha^7$.

Decoder stress is normalized via the decoder burden metric $B \in [0.0, 1.0]$:

$$B = \begin{cases} \frac{2s + e}{16}, & \text{if decoding succeeds } (2s + e \le 16) \\ 1.0, & \text{if decoding fails } (2s + e > 16) \end{cases}$$

where $s \le 8$ is the number of corrected symbol errors and $e \le 16$ is the number of tagged erasures. When decoding fails, $P_{\text{FER}} = 1.0$ and $B = 1.0$. This strictly bounds $B \in [0.0, 1.0]$ and guarantees the Link Quality Metric $M \in [0.0, 1.0]$:

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
2. **MCS-Adaptive Window Clamping & Link Window Alignment:** Rather than using an unconstrained default TCP window or a static 1024-byte clamp—which at MCS 0 ($3.0\text{ B/s}$) accumulates over five and a half minutes ($1024\text{ B} / 3.0\text{ B/s} \approx 341\text{ seconds}$) of unacknowledged socket backlog before backpressure reaches the application—the PEP dynamically clamps the advertised window based on the active MCS ladder. By setting $W_{\text{clamp}} = 74\text{ bytes}$ at MCS 0, physical in-flight queue time is bounded to $\approx 24.7\text{ seconds}$ ($74\text{ B} / 3.0\text{ B/s}$, corresponding to ~2 TDD turn cycles), and to between $5.1\text{ and }7.8\text{ seconds}$ across MCS 1 through MCS 4 ($W_{\text{clamp}} / R_{\text{APP}}$). To prevent bufferbloat while strictly respecting the link-layer ARQ window ($W_{\text{ARQ}} = 8\text{ frames} = 296\text{ bytes}$), the PEP balances local staging capacity against over-the-air in-flight limits:

| Active MCS | Raw Rate | App Goodput | Target Window Clamp ($W_{\text{clamp}}$) | Local Staging Slices | Max Over-The-Air In-Flight ($W_{\text{ARQ}}$) | Max In-Flight Buffer Time ($W_{\text{clamp}} / R_{\text{APP}}$) |
|---|---|---|---|---|---|---|
| **MCS 0** | 80.0 bps | ~24.0 bps (3.0 B/s) | **74 bytes** | 2 frames | 2 frames (74 B) | ~24.7 s (~2 TDD burst cycles) |
| **MCS 1** | 400.0 bps | ~155.0 bps (19.4 B/s) | **148 bytes** | 4 frames | 4 frames (148 B) | ~7.6 s |
| **MCS 2** | 800.0 bps | ~330.0 bps (41.2 B/s) | **296 bytes** | 8 frames | 8 frames (296 B) | ~7.2 s |
| **MCS 3** | 3,200.0 bps | ~920.0 bps (115.0 B/s) | **592 bytes** | 16 frames | 8 frames (296 B) | ~5.1 s |
| **MCS 4** | 4,000.0 bps | ~1,450.0 bps (181.2 B/s) | **1,036 bytes** | 28 frames | 8 frames (296 B) | ~5.7 s |

   * TCP window scaling is suppressed across all modes.
   * **Staging vs. Over-The-Air In-Flight Separation:** For high-throughput modes (MCS 3 and MCS 4), $W_{\text{clamp}}$ allows up to 16 and 28 frames in the local PEP socket staging queue to prevent client application write stalls during bulk transfers. However, the link layer transmitter strictly paces transmission so that at most $W_{\text{ARQ}} = 8\text{ frames}$ ($296\text{ bytes}$) are in-flight over the acoustic medium at any time, exactly matching the 7-bit selective repeat coverage of `ACK_MAP`.
   * When the modem transitions MCS, the PEP dynamically updates the Advertised Window field in subsequent spoofed ACKs, bounding the client's unacknowledged queue to between $5.1\text{ and }24.7\text{ seconds}$ across the entire rate ladder.
3. **Link Slicing & Core Boundary:** Plaintext stream bytes are packed into standard IPv4 datagrams and sliced into 37-byte fragments (`BEST_EFFORT = 0`) handled by V-RADM's link-layer Selective Repeat ARQ.
   * **PEP ↔ Modem Core Architectural Boundary:** `vradm-core` is strictly an L2/L3 packet framing and physical DSP engine; it does not embed an internal TCP stack. The TCP-PEP is an upper-layer transport module residing within the platform adapter layer (`vradmd` daemon on Linux, `PacketTunnelProvider` on iOS). The PEP intercepts raw IP packets from the virtual TUN interface (`utun0` / `/dev/net/tun`), terminates the local TCP connection with OpenSSH/sshd, enforces window clamping, and injects standard IPv4 datagrams into `vradm_write_ip_packet()`. On the receiving side, `vradm_poll_ip_packet()` delivers reassembled IPv4 packets to the PEP, which extracts TCP payload bytes and delivers them to the local socket.
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
    * **TCP Option Stripping:** The PEP forces clean baseline negotiation by stripping `WSCALE` (Window Scale, RFC 7323), `TSopt` (TCP Timestamps, RFC 7323), and `SACK-Permitted` (RFC 2018) from both client and server SYN packets. *Note on SACK Stripping Intent:* Stripping `SACK-Permitted` on both local loopback sockets is intentional: because the local loopback path (`utun` to OpenSSH, and `vradmd` to `sshd`) has negligible latency ($<1\text{ ms}$) and zero channel packet loss, TCP selective acknowledgments are superfluous; stripping SACK minimizes TCP header overhead and ensures deterministic sequence tracking.
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

* **Packet Identity:** $\text{PKT\_ID} \equiv (\text{FRAME\_CLASS}, \text{SEQ}_{\text{initial}})$ (8 bits, modulo 256).
  * For reliable streams (`BEST_EFFORT = 0`), `PKT_ID` is the `REL_SEQ` assigned to Fragment 0 (`FRAG_IDX = 0`).
  * For best-effort datagrams (`BEST_EFFORT = 1`), `PKT_ID` is the `BE_SEQ` assigned to Fragment 0.
  * Because reliable and best-effort streams consume independent sequence counters, reassembly contexts are isolated by the tuple `(FRAME_CLASS, PKT_ID)`, preventing sequence collision between SSH and Mosh traffic.
* `URGENT_FLUSH` (Bit [7]): `1` = High-priority / out-of-band interactive flush (e.g., SSH break / Ctrl-C / SIGINT), instructing the receiving PEP to immediately flush intermediate reassembly buffers and deliver data to the local socket; `0` = Standard stream fragment.
* `FRAG_IDX` (Bits [6..4]): Fragment index ($0\text{--}7$, 3 bits, matching the 8-fragment maximum).
* `TOTAL_FRAGS_MINUS_ONE` (Bits [3..1]): Total fragments minus one ($0\text{--}7$, 3 bits, supporting up to 8 fragments $\implies 296\text{ bytes}$ max datagram).
* `BEST_EFFORT` (Bit [0]): `1` = Unreliable datagram (Mosh UDP); `0` = Reliable in-order delivery (TCP-PEP stream). Mirrors the physical `FRAME_CLASS` bit (Byte 0x02 Bit [2]).
  * Best-effort fragments consume the independent sequence counter `BE_SEQ` and do not participate in or stall link-layer ARQ (`ACK_BASE` / `ACK_MAP`).
  * Reliable fragments consume `REL_SEQ` and are protected by Selective Repeat ARQ.
* **Virtual MTU:** Standardized at **256 bytes** ($\lceil 256 / 37 \rceil = 7\text{ frames}$ per packet).
* **Fragment Reassembly Context Lifetime Bounds:**
  Because `PKT_ID` wraps every 256 frames, reassembly contexts enforce strict lifetime bounds to prevent stale fragments from colliding with new packets:
  1. **Adaptive Temporal Expiration:** Rather than a static 10-second timeout—which at MCS 0 ($T_{\text{turn}} = 9.165\text{ s}$ per frame in half-duplex interactive mode) would prematurely expire 7-fragment packets requiring $7 \times 9.165 = 64.155\text{ seconds}$—the reassembly timeout dynamically adapts to the active modulation ladder:
     $$T_{\text{reassembly\_timeout}} = \max\left(2 \times \text{RTO}, \; N_{\text{frags\_max}} \times T_{\text{turn}}(\text{active\_MCS}) + 15.0\text{ s}\right)$$
     For MCS 0 ($N_{\text{frags\_max}} = 8$, $T_{\text{turn}} = 9.165\text{ s}$), $T_{\text{reassembly\_timeout}} = \mathbf{90.0\text{ seconds}}$ ($8 \times 9.165\text{ s} + 15.0\text{ s} = 88.32\text{ s} \approx 90\text{ s}$), accommodating interactive single-frame turns with margin for up to 2 ARQ retransmissions. For high-speed modes (MCS 3 and MCS 4), $T_{\text{reassembly\_timeout}} \approx 16.0\text{--}20.0\text{ seconds}$.
  2. **Sequence Distance Expiration:** If the link sequence space advances by more than 64 frames beyond the initial fragment ($\operatorname{seq\_diff}(\text{current\_seq}, \text{PKT\_ID}) > 64$), the incomplete reassembly context MUST be immediately purged and discarded.
  3. **Wrap Collision Immunity & ARQ Window Invariance:** Across all modes, $T_{\text{reassembly\_timeout}}$ and the 64-frame sequence window strictly prevent 8-bit sequence wrap ambiguity. At MCS 0, a 256-frame wrap takes $256 \times 3.20\text{ s} = 819.2\text{ seconds} \gg 90.0\text{ s}$ (and 64 frames take $204.8\text{ s} > 90.0\text{ s}$). At MCS 4, a 256-frame wrap takes $33.8\text{ seconds}$, but 64 frames correspond to $8.45\text{ seconds}$.
      * *Mathematical Proof of Zero Premature Purge under Heavy ARQ:* Because the link-layer transmitter is strictly bounded by the selective repeat window ($W_{\text{ARQ}} \le 8\text{ frames} = 296\text{ bytes}$), the transmitter CANNOT advance `current_seq` more than 8 frames beyond the oldest unacknowledged sequence (`ACK_BASE`). For any active packet `PKT_ID` spanning up to $N_{\text{frags\_max}} \le 8$ frames, while any fragment of `PKT_ID` remains unacknowledged, $\text{ACK\_BASE} \le \text{PKT\_ID} + (N_{\text{frags\_max}} - 1) \le \text{PKT\_ID} + 7$. Consequently, while fragments of packet `PKT_ID` are undergoing active transmission or ARQ retransmission, $\operatorname{seq\_diff}(\text{current\_seq}, \text{PKT\_ID}) \le (N_{\text{frags\_max}} - 1) + W_{\text{ARQ}} \le 7 + 8 = 15 \ll 64$ is an immutable invariant. The 64-frame purge threshold can only be triggered after the link layer has completely abandoned the packet and successfully advanced past at least 8 subsequent, complete packet lifecycles. Premature purge during active ARQ recovery is mathematically impossible.


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

*Note on Topology C (iOS 18.2+ In-Call Audio Injection):* Apple's iOS 18.2 "Add Audio in Calls" API permits application audio streams to be mixed into active cellular phone calls; however, Apple strictly mandates that the user must manually enable the corresponding system Accessibility setting ("Live Speech" / "Add Audio in Calls" permission under *Settings > Accessibility*) before the operating system permits an application to inject synthesized PCM audio into live cellular calls. Implementations targeting Topology C MUST detect this permission state and instruct the user accordingly during onboarding.

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
; Hand off 8kHz linear PCM to vradmd TCP daemon (uuid,service) using dynamic RFC 4122 UUID.
; Note: ${UUID()} requires Asterisk func_uuid (built via menuselect under Dialplan Functions).
; If func_uuid is not built, fall back to ${CHANNEL(uniqueid)}:
same  => n,Set(SOCKET_UUID=${IF($[${ISNULL(${UUID()})}]?${CHANNEL(uniqueid)}:${UUID()})})
same  => n,AudioSocket(${SOCKET_UUID},127.0.0.1:9099)
same  => n,Hangup()
same  => n(reject),NoOp(Unauthorized Call Dropped)
same  => n,Hangup()

```

* **AudioSocket Protocol & Session UUID Dispatch:**
  Upon establishing the TCP connection to `127.0.0.1:9099`, the Asterisk AudioSocket application immediately transmits an initial identification packet:
  ```text
  [0x01 (Type: UUID), 0x00, 0x10 (16-byte payload), <16-byte RFC 4122 binary UUID>]
  ```
  The `vradmd` worker thread consumes this message, extracts the binary UUID to index its active session table, allocates a dedicated `vradm_engine_t` instance, and begins bi-directional streaming of 8 kHz 16-bit signed linear PCM audio chunks (`[0x10 (Audio Data), len_hi, len_lo, <PCM samples>]`). Audio chunks are transferred in standard 20 ms ($160\text{ samples} = 320\text{ bytes}$) or 40 ms ($320\text{ samples} = 640\text{ bytes}$) frames; the engine phase-locks its 5.0 ms symbol grid to sample index 0 of these chunks to maintain subframe alignment with the carrier vocoder.

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
   * **Audio Session Configuration:** The app initializes `AVAudioSession.sharedInstance()` with category `.playAndRecord`, mode `.measurement`, and options `[.allowBluetooth, .mixWithOthers]`. Mode `.measurement` requests the least-processed audio path available for the selected route; OS-level dynamics processing, automatic gain control (AGC), and acoustic echo cancellation (AEC) are suppressed on a best-effort basis. Because hardware audio pipelines across Apple devices and accessory routes (built-in speakerphone vs. wired Lightning/USB-C dongle) can exhibit device-dependent pre-emphasis or non-linear limiter behaviors, remaining route-specific hardware processing SHALL be characterized empirically per target device model. Background execution is preserved under `UIBackgroundModes = ["audio"]`, where continuous execution of the real-time audio render callback prevents iOS from suspending the Main App process when backgrounded or when the device screen is locked. *Note on App Store Policy:* Apple App Store Review Guidelines (Guideline 2.5.4) mandate that apps declaring the audio background mode provide audible sound playback or active voice communication audible to the user; headless or silent modem background streaming requires appropriate user-facing operational disclosure or foreground execution to maintain store compliance.
   * **NetworkExtension Lifecycle:** The NetworkExtension provider maintains packet handling while its tunnel session is active. Tunnel termination or provider restart is treated as a recoverable lifecycle event; the shared-memory protocol must tolerate producer/consumer disappearance and reattachment.

6. **Startup IPC Shared-Memory Self-Test:**
   Before establishing the network tunnel or passing production traffic, both the Main App and NetworkExtension execute an automated startup IPC self-test qualifying the lock-free SPSC shared memory queue across the App Group boundary:
   * **File & Mapping Qualification:** Validates creation, pre-allocation sizing, and `mmap` mapping of the backing file in `group.org.vradm`.
   * **Atomic Index Progression:** Validates release-acquire memory ordering and atomic sequence progression across producer and consumer.
   * **Cache-Line Alignment Verification:** Validates that atomic indices (`head`, `tail`) reside on distinct 64-byte hardware cache lines (`uintptr_t(tail) - uintptr_t(head) >= 64`), ensuring zero false sharing between CPU cores.
   * **Graceful Reattachment:** Validates that sudden termination and reattachment of either process cleanly recovers ring pointers without deadlock or buffer corruption.

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
    vradm_rate_t sample_rate;          // [0..3] Sampling rate (8000 or 16000 Hz)
    vradm_mcs_t  startup_mcs;          // [4] Initial active MCS ladder state (0..4)
    uint8_t      auto_rate_adaptation; // [5] Enable automatic link-metric rate adaptation (0/1)
    uint8_t      reserved[2];          // [6..7] Struct padding / future expansion
    float        tx_amplitude;         // [8..11] Target RMS ceiling (Default: 0.3535 = -9.0 dBFS RMS)
    uint8_t      reserved2[4];         // [12..15] Struct padding
    uint8_t      psk_key[16];          // [16..31] 128-bit Pre-Shared Key (16-Byte Naturally Aligned for SIMD)
} vradm_config_t; // Exactly 32 bytes, 16-byte aligned

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

typedef uint32_t vradm_cmd_type_t;
#define VRADM_CMD_NONE           0
#define VRADM_CMD_START_SOTP     1
#define VRADM_CMD_STOP_SOTP      2
#define VRADM_CMD_REQUEST_MCS    3
#define VRADM_CMD_RESET_SESSION  4
#define VRADM_CMD_SET_TX_PARAMS  5

typedef struct {
    uint32_t cmd_type;          // [0..3] Command opcode (VRADM_CMD_*)
    uint32_t cmd_id;            // [4..7] Monotonic command tracking sequence ID
    uint32_t param_u32;         // [8..11] Generic integer parameter (e.g. target_mcs, object_id)
    int32_t  param_i32;         // [12..15] Signed integer parameter (e.g. carrier offset, dB tuning)
    float    param_f32;         // [16..19] Floating-point parameter (e.g. tx_amplitude, redundancy_factor)
    uint8_t  inline_payload[12]; // [20..31] Fixed inline payload for short parameter strings/data
} vradm_cmd_t; // Exactly 32 bytes on all 32-bit and 64-bit architectures

#if defined(__STDC_VERSION__) && __STDC_VERSION__ >= 201112L
_Static_assert(sizeof(vradm_config_t) == 32, "vradm_config_t size mismatch: expected 32 bytes");
_Static_assert(sizeof(vradm_telemetry_t) == 40, "vradm_telemetry_t size mismatch: expected 40 bytes");
_Static_assert(sizeof(vradm_cmd_t) == 32, "vradm_cmd_t size mismatch: expected 32 bytes");
_Static_assert(offsetof(vradm_config_t, psk_key) == 16, "vradm_config_t: psk_key offset mismatch (must be 16-byte aligned)");
_Static_assert(offsetof(vradm_telemetry_t, security_tamper_detected) == 8, "vradm_telemetry_t: security_tamper_detected offset mismatch");
_Static_assert(offsetof(vradm_telemetry_t, sample_slip_accum) == 36, "vradm_telemetry_t: sample_slip_accum offset mismatch");
_Static_assert(offsetof(vradm_cmd_t, param_u32) == 8, "vradm_cmd_t: param_u32 offset mismatch");
_Static_assert(offsetof(vradm_cmd_t, inline_payload) == 20, "vradm_cmd_t: inline_payload offset mismatch");
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
 *    - Control mutations (SOTP start/stop, MCS commit requests, session resets,
 *      parameter tuning) are submitted via vradm_submit_cmd() into the host->audio
 *      SPSC queue using value-typed vradm_cmd_t structs (zero pointers across threads).
 *    - Payload Lifetime Safety: Large SOTP transmission payloads are staged
 *      synchronously into engine-owned memory via vradm_sotp_stage_tx_payload()
 *      on the host thread before enqueuing VRADM_CMD_START_SOTP with the assigned
 *      object_id, preventing any possibility of asynchronous use-after-free.
 *    - SOTP Reception Ownership & Handoff: Exclusively calls vradm_sotp_rx_poll()
 *      and vradm_sotp_rx_fetch(). The background RaptorQ decoder writes reconstructed
 *      symbols into engine-owned staging memory. Upon complete object decoding and
 *      successful BLAKE3-224 hash verification against the manifest, the engine atomically
 *      transitions the object state to VRADM_SOTP_READY via an atomic release store.
 *      The Host Network Thread samples this state via atomic acquire load in
 *      vradm_sotp_rx_poll() and copies the verified object out via vradm_sotp_rx_fetch().
 *      While in the VRADM_SOTP_READY state, the audio pipeline and decoder are strictly
 *      prohibited from mutating the staging memory until the host thread finishes
 *      fetching or releases the object via VRADM_CMD_STOP_SOTP.
 *
 * 3. Telemetry / Diagnostics Thread:
 *    - Calls vradm_get_telemetry() and vradm_get_active_mcs().
 *    - Safe to invoke concurrently from any thread at any time.
 *    - Race-Free Memory Model: Telemetry storage is internally double-buffered and
 *      protected by atomic sequence counters with C11/Rust acquire-release semantics.
 *      The writer updates the background slot, executes an atomic release store on
 *      the sequence counter, and flips active buffers. The reader in vradm_get_telemetry()
 *      samples seq1 (atomic acquire), copies the struct, and verifies seq2 (atomic acquire).
 *      Reads retry if (seq1 != seq2 || (seq1 & 1) != 0), guaranteeing tear-free atomic
 *      snapshots with zero undefined behavior or data races under language memory models.
 *    - Lightweight Active MCS Getter: vradm_get_active_mcs() directly executes an atomic
 *      load on engine->active_tx_mcs, bypassing the seqlock loop for zero-overhead
 *      window clamping decisions in the TCP-PEP.
 *
 * 4. Engine Lifecycle & Quiescence Contract:
 *    - vradm_create(), vradm_reset(), vradm_destroy().
 *    - Quiescence Invariant: vradm_reset() and vradm_destroy() are strictly prohibited while
 *      audio render callbacks or host polling threads are actively executing against the instance.
 *    - Standard Safe Reset Sequence (e.g. during iOS NetworkExtension restart or call renegotiation):
 *      1. Set a platform atomic flag (e.g. is_paused = true) instructing the real-time audio callback to
 *         synthesize silence/zero samples and immediately return without calling vradm_generate_audio()
 *         or vradm_process_audio().
 *      2. Wait for any in-flight host thread call (vradm_write_ip_packet, vradm_poll_ip_packet) to finish.
 *      3. Call vradm_reset(engine).
 *      4. Clear is_paused flag to resume normal real-time processing.
 * ========================================================================= */

/* --- Engine Lifecycle Management (Thread-Safe under Single-Owner Discipline) --- */
vradm_engine_t* vradm_create(const vradm_config_t* config);
void            vradm_destroy(vradm_engine_t* engine);
void            vradm_reset(vradm_engine_t* engine);

/* --- Asynchronous Host Command Queue (Lock-Free SPSC, Zero Pointers) --- */
int32_t vradm_submit_cmd(vradm_engine_t* engine, const vradm_cmd_t* cmd);

/* --- Real-Time Audio Streaming I/O (Zero Dynamic Allocations) --- */
void     vradm_process_audio(vradm_engine_t* engine, const int16_t* in_samples, uint32_t count);
uint32_t vradm_generate_audio(vradm_engine_t* engine, int16_t* out_samples, uint32_t max_count);

/* --- Mode A: IP Packet Datagram Stream (TUN / TCP-PEP Interface) --- */
int32_t vradm_write_ip_packet(vradm_engine_t* engine, const uint8_t* packet, uint32_t len);
int32_t vradm_poll_ip_packet(vradm_engine_t* engine, uint8_t* out_packet, uint32_t max_len);

/* --- Mode B: SOTP Simplex Object Transfer (Safe Engine-Owned Staging) --- */
int32_t vradm_sotp_stage_tx_payload(vradm_engine_t* engine, const uint8_t* payload, uint32_t len, float redundancy_factor, uint32_t* out_object_id);
int32_t vradm_sotp_rx_poll(vradm_engine_t* engine, uint32_t* out_collected_symbols, uint32_t* out_required_symbols);
int32_t vradm_sotp_rx_fetch(vradm_engine_t* engine, uint8_t* out_buf, uint32_t max_len, uint8_t out_hash[28]); // 28-byte BLAKE3_224 digest

/* --- Telemetry & Link Status (Race-Free Seqlock Snapshot & Lightweight Atomic Getters) --- */
void        vradm_get_telemetry(const vradm_engine_t* engine, vradm_telemetry_t* out_telem);
vradm_mcs_t vradm_get_active_mcs(const vradm_engine_t* engine); // Zero-overhead atomic load for PEP window clamping

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
| **TC-01** | Math Loopback & Codec Test Vectors | In-memory loopback | Zero noise, synchronous clock | Zero observed bit errors over $30 \times 10^6$ tested bits ($\implies P_e \le 1.0 \times 10^{-7}$ at 95% Clopper-Pearson confidence). Zero uncorrected RS errors.<br>• **Reed-Solomon Test Vectors:** Systematic $\text{RS}(64, 48)$ and $\text{RS}(16, 8)$ codecs verify bit-exact generator polynomial remainder parity and zero syndromes on standard test vectors; GMD errors-and-erasures decoder satisfies $2s + e \le 16$ and bounds decoder burden $B = \frac{2s + e}{16} \in [0.0, 1.0]$ with $B = 1.0$ on decoding failure ($2s + e > 16$).<br>• **Golay Test Vectors & Bit Partitioning:** Systematic Extended Golay $[24, 12, 8]$ codec verifies $G G^T \equiv 0 \pmod 2$, $P_{12} P_{12}^T \equiv I_{12} \pmod 2$, exact weight enumerator ($A_0=1, A_8=759, A_{12}=2576, A_{16}=759, A_{24}=1$), $d_{\text{min}} = 8$, encoding vector $\text{0x5A5} \to \mathbf{\text{0x5A51D9}}$, and 3-bit error correction $\mathbf{\text{0xFA5199}} \to \mathbf{\text{0x5A5}}$. Bit-exact packing across Codewords 1 & 2 ($m^{(1)} = \text{CUR\_MCS}[3] \mid \text{REQ\_MCS}[3] \mid \text{TX\_PWR}[2] \mid \text{BEAC\_SEQ}[7..4]$ and $m^{(2)} = \text{BEAC\_SEQ}[3..0] \mid \text{BEACON\_MAC8}[8]$) round-trips with zero bit displacement across all $2^{24}$ combinations.<br>• **MCS 1 Codec & Prototype Bank Verification:** Bit-exact parameter mapping round-trips all 256 symbol values ($S \in [0, 255]$) across formant pair table $\{F_1, F_2\}$, pitch lag table $P \in \{33 \dots 80\}$, and onset sample offset $\delta \in \{0 \dots 3\}$ with zero quantization mismatch. Demodulation cross-correlation against the canonical Zero-Initial-State Prototype Bank achieves $\Lambda(S) \ge 0.95 \gg \Theta_{\text{erase}} = 0.35$ on steady-state voiced atoms with characterized $\approx 0.03$ transient confidence pessimism.<br>• **MCS 3 Symbol Grid & AudioSocket Alignment:** AudioSocket frame alignment test verifies that PLCP interval ($520\text{ samples Barker-13} + 3,840\text{ samples 2-FSK} + 280\text{ samples guard} = 4,640\text{ samples} = 580.0\text{ ms}$) is an exact integer multiple of both 40-sample 5.0 ms subframes ($116 \times 40$) and 160-sample 20 ms AudioSocket/ACELP blocks ($29 \times 160$), guaranteeing zero phase misalignment across vocoder analysis frames.<br>• **MCS 0 Unvoiced / Noise Erasure Policy:** Acoustic noise loopback test verifies that segments with $\Lambda(\hat{S}) < \Theta_{\text{unvoiced}} = 0.30$ trigger zero-confidence erasure tagging ($C_i = 0.0$) feeding the RS GMD decoder as an explicit erasure ($1e$) rather than an undetected error ($2s$), with zero premature frame aborts.<br>• **Data Structure SIMD Alignment:** ABI layout test verifies $\text{sizeof}(\text{vradm\_config\_t}) = 32$, $\text{sizeof}(\text{vradm\_cmd\_t}) = 32$, $\text{sizeof}(\text{vradm\_telemetry\_t}) = 40$, and $\text{offsetof}(\text{vradm\_config\_t, psk\_key}) = 16$ ensuring 128-bit natural SIMD alignment for cryptographic and vectorized vector processing.<br>• **Preamble Waveform Limits:** Barker-13 dual-chirp verifies zero phase jump at mid-chip transition ($m=20$, $\Delta\theta = 0\text{ rad}$), chip boundary phase entry ($m=40$, $\theta \equiv 0\pmod{2\pi}$), continuous intra-chip sample step $|s[n] - s[n-1]| \le 1.20$, and inter-chip boundary step $|s[40k] - s[40k-1]| \le 0.30$. |
| **TC-02** | AMR-NB Robustness | AMR-NB @ 12.2 kbps | Injected channel erasure rate = 1.0%; AWGN $\text{SNR} = 18\text{ dB}$ | Zero unrecoverable frames over 30,000 frames ($\implies P_{\text{FER}} \le 1.0 \times 10^{-4}$ at 95% Clopper-Pearson confidence). Zero payload corruption. |
| **TC-03** | Dynamic Codec Adaptation | AMR-NB stepped down from 12.2k to 4.75k | Mode switch occurs at Frame 100 | Link metric $M$ initiates automatic downshift to MCS 1 within 4 frames. Zero dropped IP packets. |
| **TC-04a** | Balanced Cellular Gate (MCS 2) | AMR-NB @ 12.2 kbps | Injected channel $\text{SNR} = 18\text{ dB}$, clock drift $\pm 50\text{ PPM}$ | Multi-carrier 4th-power NDA DLL maintains phase-aligned carrier and timing lock. Measured Application Goodput $R_{\text{APP}} \ge 250\text{ bps}$ for SSH/TCP-PEP or $\ge 320\text{ bps}$ for UDP bulk stream (against theoretical L3 maximum $431.92\text{ bps}$). $P_{\text{FER}} \le 1.0 \times 10^{-3}$. Zero RS decode crashes. |
| **TC-04b** | Wideband Cellular Cabled Gate (MCS 3) | AMR-WB @ 12.65 kbps | Resampling $16\text{k} \to 8\text{k} \to 16\text{k}$; $\pm 80\text{ PPM}$ clock drift | DPLL and DLL maintain lock. Measured Application Goodput $R_{\text{APP}} \ge 850\text{ bps}$ for SSH/TCP-PEP or $\ge 1,100\text{ bps}$ for UDP bulk stream. $P_{\text{FER}} \le 1.0 \times 10^{-3}$. Zero RS decode crashes. |
| **TC-05** | VAD & AGC Verification | 3GPP VAD Model 1 & 2 + Smartphone AGC model | PRBS-7 phase dither enabled; voiced maintenance carrier active | Measured over 10,000 independent 1-second trials. False DTX entry $P_{\text{DTX}} \le 0.01$. Zero AGC signal-clamping events. Dither-induced FER degradation $\Delta P_{\text{FER}} \le 0.5\%$ ($\le 0.005$) compared to un-dithered transmission. |
| **TC-06** | Burst Erasure Recovery | MCS 3 (165 ms frames) | 3 consecutive physical frame drops ($495\text{ ms}$ drop) | Selective Repeat ARQ triggers fast retransmission on `REL_SEQ`. Independent `BE_SEQ` datagrams bypass ARQ and do not stall cumulative `ACK_BASE`. Complete IP packet stream recovery within $\le \mathbf{1,450\text{ ms}}$ of drop start. Zero application errors. |
| **TC-07** | SOTP Mid-Stream Entry | AMR-WB @ 12.65 kbps | Receiver attaches at Frame 40 of a 100-symbol broadcast | Receiver syncs via Metadata Manifest within 8 frames. Object reconstructs with matching BLAKE3_224 checksum. |
| **TC-08a** | Real VoLTE Cellular Call (MCS 3) | Commercial Mobile VoLTE Network | Active 15-minute phone call between iPhone and Asterisk server | Interactive OpenSSH/TCP-PEP session maintained continuously. Keystroke round-trip confirmation time $\le 550\text{ ms}$. |
| **TC-08b** | Real Degraded / Free-Air Link (MCS 0/1) | Acoustic Speaker-to-Mic Air Gap / Degraded 3G Call | High ambient acoustic noise and multi-second frame periods | Mosh UDP terminal session maintained continuously. Predictive local echo renders keystrokes with $< 50\text{ ms}$ UI latency; remote screen converges within $1.5 \times T_{\text{frame}}$ after burst recovery. |
| **TC-09** | Concurrency & Thread-Safety | 8 concurrent AudioSocket TCP threads | Multi-channel load test on Linux daemon | Zero cross-session cross-talk, race conditions, or memory corruption. CPU scaling linear across threads. |
| **TC-10a** | Symbol Timing & Phase-Slope Detector | MCS 2, MCS 3, and MCS 4 loopback | Injected random single-sample slips ($\pm 1$ sample every $500\text{ ms}$) across SNR sweep down to demotion threshold ($\text{SNR} \in [8\text{ dB}, 18\text{ dB}]$) | For MCS 2 and MCS 3: Reference-normalized 4th-power inter-carrier phase-slope timing detector ($\Delta\hat{\tau} \propto -\partial\angle r_k^{\text{clean}}/\partial f_k$, canceling transmit reference phase, static channel delay, and differential dither $4(\Delta\theta_m - \Delta\theta_0)$) and complex baseband transition detector resolve slips within $\le 2.5\text{ ms}$ ($< 1$ symbol interval) with zero static timing bias ($\tau_{\text{bias}} \equiv 0$). For MCS 4: Cyclic-prefix correlation detector resolves slips within $\le 2.5\text{ ms}$. Zero bit slips; Farrow fractional resampler tracks phase step without symbol decoding failure. |

| **TC-10b** | 4th-Power Carrier Tracking & MRC Threshold | MCS 2 and MCS 3 loopback | Static carrier frequency offsets up to $\pm 12.5\text{ Hz}$ across SNR sweep ($8\text{ dB} \dots 20\text{ dB}$) | Phase-aligned MRC coherent combining of derotated subcarrier residuals $\tilde{r}_k[n]$ achieves power-weighted squaring-loss mitigation gain: $\ge +5.74\text{ dB}$ for MCS 2 (4 carriers) and $\ge +7.56\text{ dB}$ for MCS 3 (8 carriers) calculated from profile amplitude weighting $(\sum A_k^2)^2 / \sum A_k^4$, or $+6.02\text{ dB}$ and $+9.03\text{ dB}$ under equal-power AWGN calibration ($10\log_{10} Z$). Coherent carrier phase tracking lock maintained without cycle slipping down to channel metric demotion threshold $M = 0.60$. |
| **TC-10c** | Limiter Activation & Multicarrier EVM | MCS 2, MCS 3, and MCS 4 composite multicarrier waveforms at nominal and worst-case crest-factor alignments | Nominal RMS targets with rare crest-factor transients | Peak-constrained normalization bounds soft-limiter activation rate to $< 0.05\%$ of samples. Measured composite multicarrier EVM degradation due to limiter non-linearity is $\le 0.5\text{ dB}$ (EVM $\le -22\text{ dB}$ across MCS 2 and MCS 3, EVM $\le -24\text{ dB}$ for MCS 4). |
| **TC-10d** | Continuous Sample Clock Drift Tracking | MCS 2, MCS 3, and MCS 4 cabled loopback | Continuous clock offset $\Delta F_s / F_s \in \{\pm 20, \pm 40, \pm 80, \pm 100\}\text{ PPM}$ | *CI Smoke Tier (2,000 frames):* Farrow 3rd-order resampler and DLL dynamic tracking maintain synchronization without buffer overflow/underflow or bit slips. $P_{\text{FER}} \le 1.0 \times 10^{-4}$. Zero unrecoverable frame loss.<br>*Hardware Qualification Tier (10,000 frames):* Continuous lock over physical streaming durations without cumulative phase drift or frame loss: $108.3\text{ minutes}$ ($6,500\text{ s}$) for MCS 2, $27.5\text{ minutes}$ ($1,650\text{ s}$) for MCS 3, and $22.0\text{ minutes}$ ($1,320\text{ s}$) for MCS 4. |
| **TC-11** | G.711 VoIP Codec-in-the-Loop Gate (MCS 4) | G.711 $\mu$-law / A-law companding over simulated VoIP network | 20 ms RTP packetization, $\sigma = 5.0\text{ ms}$ packet arrival jitter absorbed by $40\text{ ms}$ playout jitter buffer; 1.0% random packet loss triggering standard G.711 Appendix I Packet Loss Concealment (PLC) waveform synthesis distortion; $\pm 50\text{ PPM}$ clock skew | Reed-Solomon RS(64,48) with GMD soft-decision erasure tagging corrects symbol-level waveform distortions induced by G.711 Appendix I PLC across damaged physical frames in conjunction with Selective Repeat ARQ (repairing corrupted physical frames resulting from packet loss concealment rather than directly recovering raw RTP packets). Measured Application Goodput $R_{\text{APP}} \ge 1,200\text{ bps}$ for SSH/TCP-PEP or $\ge 1,450\text{ bps}$ for UDP bulk stream (against theoretical L3 maximum $1,769.14\text{ bps}$). $P_{\text{FER}} \le 1.0 \times 10^{-3}$. |

---

## 12. Direct Implementation Instructions

1. **Workspace Layout:** Scaffold a Cargo workspace with `crates/vradm-core` (`#![no_std]` core with `alloc` for initialization), `crates/vradm-server` (multi-threaded Asterisk daemon), `crates/vradm-cli` (diagnostics), and `tests/vocoder_bench`.
2. **Wire Format Verification:** Implement `crates/vradm-core/src/link/frame.rs` and verify bit-exact layout of both the 64-byte Canonical Data Frame and 16-byte Authenticated Compact Control Frame.
3. **PLCP Bootstrap & Authentication:** Implement the Barker-13 dual-chirp generator, dual Extended Golay $[24, 12, 8]$ codecs, SipHash-2-4 control plane MAC with DoS-immune token bucket rate limiting and silent drops, and 2-FSK modulator.
4. **Carrier & Timing Recovery (4th-Power NDA DLL):** Implement multi-carrier 4th-power phase extraction, reference-normalized phase-slope timing error detector ($\Delta\hat{\tau} \propto -\partial\angle r_k/\partial f_k$), phase-aligned MRC coherent combining ($\bar{r}[n]$), symbol-boundary transient exclusion zones, and fractional Farrow resampler in `crates/vradm-core/src/phy/dll.rs`.
5. **Soft-Decision RS Decoder:** Implement Chase/GMD soft-decision decoding in `crates/vradm-core/src/fec/rs.rs`, verifying that flagging 16 confidence-tagged erasures reconstructs corrupted frames under $2t + e \le 16$.
6. **Continuous Codec Validation:** Execute `tests/vocoder_bench` continuously against `libopencore-amr`, `vo-amrwbenc`, and G.711 before packaging C-ABI exports. Ensure zero regressions across tests TC-01 through TC-11 prior to platform deployment.