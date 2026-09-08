/// Configuration for the cross-band repeater.
#[derive(Debug, Clone)]
pub struct RepeaterConfig {
    /// Enable the repeater; `relay_one_frame()` is a no-op when `false`.
    pub enabled: bool,
    /// Modulation mode string used for both RX and TX (e.g. `"BPSK250"`).
    pub mode: String,
    /// Milliseconds to hold PTT after the last TX byte (half-duplex only).
    pub tx_hang_ms: u64,
    /// When true, PTT is held *across* relayed frames rather than dropped between them: the key is
    /// taken on the first frame, re-stamped by each subsequent one, and released by the watchdog
    /// after [`openpulse_radio::DEFAULT_PTT_MAX`] of silence. `tx_hang_ms` is ignored.
    ///
    /// It is NOT held from session start (changed in #1260) — the watchdog is in-process, so an
    /// eager unbounded hold means a dead daemon leaves rig_b keyed with nothing to release it.
    pub full_duplex: bool,
    /// Station callsign transmitted for §97.119 identification of the *transmitting* rig (rig_b). Empty
    /// disables auto-ID (the repeater then never keys an ID — the operator is responsible).
    pub callsign: String,
    /// Auto-ID interval in seconds (Part-97 §97.119 = 600 = 10 min). `0` disables auto-ID.
    pub id_interval_secs: u64,
}

impl Default for RepeaterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: "BPSK250".into(),
            tx_hang_ms: 0,
            full_duplex: false,
            callsign: String::new(),
            id_interval_secs: 600,
        }
    }
}
