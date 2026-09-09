/// Configuration for the cross-band repeater.
#[derive(Debug, Clone)]
pub struct RepeaterConfig {
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
    /// Carrier-sense rig_b's band before ACQUIRING the key (#1325).
    ///
    /// Sensing governs channel acquisition, not continuation: while `full_duplex` holds the key
    /// across frames this station already owns the channel, and a sense there would read its own
    /// carrier as busy and never relay again. That is the hole that disqualified the CAT S-meter
    /// design, and it is why the check is placed at `acquire_key`, not at every transmit.
    pub carrier_sense: bool,
}

impl Default for RepeaterConfig {
    fn default() -> Self {
        Self {
            mode: "BPSK250".into(),
            tx_hang_ms: 0,
            full_duplex: false,
            callsign: String::new(),
            id_interval_secs: 600,
            carrier_sense: true,
        }
    }
}
