use std::collections::HashMap;

use ed25519_dalek::SigningKey;
use sha2::{Digest, Sha256};

use crate::error::ModemError;
use crate::handshake_wire::{
    caps, signed_prefix, split_frame, BodyReader, BodyWriter, CONREQ_HASH_LEN, KEX_PUBKEY_LEN,
    MAGIC_CONACK, MAGIC_CONREQ, PUBKEY_LEN,
};
use crate::signing_domain::SigningDomain;
use crate::trust::{
    evaluate_handshake, CertificateSource, HandshakeDecision, PolicyProfile, PublicKeyTrustLevel,
    SigningMode, TrustError,
};

// ------------------------------------------------------------------
// Errors
// ------------------------------------------------------------------

/// Errors returned when creating or verifying a handshake frame.
#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    #[error("invalid Ed25519 signature")]
    InvalidSignature,
    #[error("frame public key does not match the trusted key for this station")]
    PublicKeyMismatch,
    #[error("session ID mismatch: expected {expected}, got {got}")]
    SessionIdMismatch { expected: String, got: String },
    #[error("trust evaluation failed: {0:?}")]
    TrustFailure(TrustError),
    #[error("encoding error: {0}")]
    Encoding(String),
    /// The CONACK selected a signing mode the CONREQ never offered (F-1147-05).
    ///
    /// v1 evaluated `selected_mode` against LOCAL policy only, never against the offer, so a
    /// responder could select a mode the initiator never proposed. The PQ path already checked
    /// this and `protocol-wire-spec.md` claimed the classical path did too.
    #[error("responder selected a signing mode the initiator never offered")]
    UnofferedSigningMode,
    #[error("handshake timestamp is stale: {skew_ms} ms skew exceeds {max_skew_ms} ms")]
    StaleTimestamp { skew_ms: u64, max_skew_ms: u64 },
    #[error("handshake carries no timestamp but freshness is required")]
    MissingTimestamp,
}

/// Freshness bound for verifying a handshake's signed timestamp, closing the
/// capture-replay window. The verifier rejects a frame whose `timestamp_ms`
/// differs from `now_ms` by more than `max_skew_ms` (in either direction), and
/// rejects a frame that carries no timestamp at all (`timestamp_ms == 0`).
#[derive(Debug, Clone, Copy)]
pub struct Freshness {
    /// The verifier's current wall-clock time in Unix milliseconds.
    pub now_ms: u64,
    /// Maximum tolerated clock skew between the two stations, in milliseconds.
    pub max_skew_ms: u64,
}

impl Freshness {
    /// Reject a stale or future-dated frame.
    ///
    /// `0` is refused as [`HandshakeError::MissingTimestamp`]. In v1 that was a sentinel meaning
    /// "no timestamp advertised"; in v2 the field is mandatory and fixed-width, so zero is a value —
    /// but refusing it is still right (a station claiming the epoch), and the specific error is more
    /// informative than reporting ~54 years of skew.
    pub fn check(&self, timestamp_ms: u64) -> Result<(), HandshakeError> {
        if timestamp_ms == 0 {
            return Err(HandshakeError::MissingTimestamp);
        }
        let skew_ms = self.now_ms.abs_diff(timestamp_ms);
        if skew_ms > self.max_skew_ms {
            return Err(HandshakeError::StaleTimestamp {
                skew_ms,
                max_skew_ms: self.max_skew_ms,
            });
        }
        Ok(())
    }
}

impl From<TrustError> for HandshakeError {
    fn from(e: TrustError) -> Self {
        HandshakeError::TrustFailure(e)
    }
}

// ------------------------------------------------------------------
// TrustStore trait
// ------------------------------------------------------------------

/// Lookup table for peer public keys and their trust levels.
pub trait TrustStore {
    /// Returns the Ed25519 verifying-key bytes for the given station ID, if known.
    fn pubkey_for(&self, station_id: &str) -> Option<[u8; 32]>;

    /// Returns the trust level assigned to the given station ID.
    fn trust_level(&self, station_id: &str) -> PublicKeyTrustLevel;
}

/// In-memory trust store for testing and offline operation.
#[derive(Debug, Clone, Default)]
pub struct InMemoryTrustStore {
    entries: HashMap<String, ([u8; 32], PublicKeyTrustLevel)>,
}

impl InMemoryTrustStore {
    /// Create an empty trust store with no entries.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a station with a specific trust level.
    pub fn add_entry(
        &mut self,
        station_id: &str,
        pubkey: [u8; 32],
        trust_level: PublicKeyTrustLevel,
    ) {
        self.entries
            .insert(station_id.to_string(), (pubkey, trust_level));
    }
}

impl TrustStore for InMemoryTrustStore {
    fn pubkey_for(&self, station_id: &str) -> Option<[u8; 32]> {
        self.entries.get(station_id).map(|(k, _)| *k)
    }

    fn trust_level(&self, station_id: &str) -> PublicKeyTrustLevel {
        self.entries
            .get(station_id)
            .map(|(_, level)| *level)
            .unwrap_or(PublicKeyTrustLevel::Unknown)
    }
}

// ------------------------------------------------------------------
// ConReq — connection request frame
// ------------------------------------------------------------------

/// Everything a CONREQ carries, so the constructor does not take eleven positional arguments.
#[derive(Debug, Clone)]
pub struct ConReqParams<'a> {
    /// This station's callsign (cap 12).
    pub station_id: &'a str,
    /// Who the request is addressed to; `"*"` is the explicit wildcard (#1178).
    ///
    /// **Empty is invalid**, deliberately: "unaddressed" must not be spellable by omission, or the
    /// field decays back into the state it was added to fix — where every daemon in range answers
    /// and the RF is spent by every listener before the initiator filters.
    pub dst_station: &'a str,
    /// Signing modes offered, in preference order (cap 4 entries).
    pub signing_modes: Vec<SigningMode>,
    /// Session identifier.
    ///
    /// A fixed `u64`, not a string: the string form was capped at 24 bytes while the daemon built
    /// it as `"{callsign}-{unix_ms}"` from a callsign capped at 12, so a legal 11-character
    /// compound callsign overflowed the cap and silently dropped the station to an unverified
    /// session. A fixed-width id removes the cap instead of re-tuning it.
    pub session_id: u64,
    /// Maidenhead grid, empty if not advertised (cap 8).
    pub station_grid: &'a str,
    /// Active OTA ladder name, empty if none (cap 24).
    pub profile_name: &'a str,
    /// Fingerprint of the active ladder mapping (0 = none).
    pub profile_fingerprint: u64,
    /// Unix-ms creation time. **Mandatory** in v2 — the `0` sentinel and its
    /// `skip_serializing_if` contortions existed only to keep v1 signatures byte-identical to
    /// pre-#615 frames, and this break discards that.
    pub timestamp_ms: u64,
    /// Ephemeral X25519 public key (exactly 32 bytes) for OTA-ACK key agreement (E7).
    pub kex_pubkey: &'a [u8],
}

/// Connection request sent by the initiating station during Discovery.
///
/// v2 binary layout. The `signature` covers the **transmitted prefix**
/// (`magic || version || length || body`) — see [`crate::handshake_wire`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConReq {
    /// Initiator callsign.
    pub station_id: String,
    /// Addressee callsign, or `"*"` for a broadcast request (#1178).
    pub dst_station: String,
    /// Ed25519 verifying-key bytes (32).
    pub pubkey: Vec<u8>,
    /// Ephemeral X25519 public key (32) for OTA-ACK key agreement.
    pub kex_pubkey: Vec<u8>,
    /// Signing modes offered, in preference order.
    pub signing_modes: Vec<SigningMode>,
    /// Session identifier.
    pub session_id: u64,
    /// Maidenhead grid (empty = not advertised).
    pub station_grid: String,
    /// Active OTA ladder name (empty = none advertised).
    pub profile_name: String,
    /// Fingerprint of the active ladder mapping (0 = none).
    pub profile_fingerprint: u64,
    /// Unix-ms creation time, signed, for replay freshness.
    pub timestamp_ms: u64,
    /// Ed25519 signature (64) over the transmitted prefix.
    pub signature: Vec<u8>,
}

impl ConReq {
    /// Build and sign a CONREQ, returning the frame **bytes**.
    ///
    /// Returns the wire frame rather than a struct because the signature covers the transmitted
    /// prefix: handing back a struct would invite a caller to re-encode and re-derive the signed
    /// span, which is the drift this format exists to remove.
    pub fn create(
        params: &ConReqParams<'_>,
        signing_key_seed: &[u8; 32],
    ) -> Result<Vec<u8>, ModemError> {
        if params.dst_station.is_empty() {
            return Err(ModemError::Frame(
                "CONREQ dst_station is empty; use \"*\" for a broadcast request (#1178)".into(),
            ));
        }
        if params.station_id.is_empty() {
            return Err(ModemError::Frame(
                "CONREQ station_id is empty; an unnamed station cannot be a verified peer".into(),
            ));
        }
        if params.signing_modes.len() > caps::SIGNING_MODES {
            return Err(ModemError::Frame(format!(
                "CONREQ offers {} signing modes, over the {} cap",
                params.signing_modes.len(),
                caps::SIGNING_MODES
            )));
        }
        let signing_key = SigningKey::from_bytes(signing_key_seed);
        let pubkey = signing_key.verifying_key().to_bytes();

        let mut w = BodyWriter::new();
        w.str_capped("station_id", params.station_id, caps::STATION_ID)?;
        w.str_capped("dst_station", params.dst_station, caps::STATION_ID)?;
        w.fixed("pubkey", &pubkey, PUBKEY_LEN)?;
        w.fixed("kex_pubkey", params.kex_pubkey, KEX_PUBKEY_LEN)?;
        w.u8(params.signing_modes.len() as u8);
        for m in &params.signing_modes {
            w.u8(m.to_wire());
        }
        w.u64(params.session_id);
        w.str_capped("station_grid", params.station_grid, caps::GRID)?;
        w.str_capped("profile_name", params.profile_name, caps::PROFILE_NAME)?;
        w.u64(params.profile_fingerprint);
        w.u64(params.timestamp_ms);

        let prefix = signed_prefix(MAGIC_CONREQ, &w.finish())?;
        let sig = crate::signing::sign_in_band(SigningDomain::ConReq, signing_key_seed, &prefix)
            .map_err(|e: crate::signing::SigningError| ModemError::Frame(e.to_string()))?;
        let mut frame = prefix;
        frame.extend_from_slice(&sig);
        Ok(frame)
    }

    /// Parse a CONREQ frame. Does **not** verify the signature — see [`verify_conreq`].
    pub fn decode(bytes: &[u8]) -> Result<Self, ModemError> {
        let spans = split_frame(bytes, MAGIC_CONREQ, "CONREQ")?;
        let (body, signature) = (spans.body, spans.signature);
        let mut r = BodyReader::new(body);
        let station_id = r.str_capped("station_id", caps::STATION_ID)?;
        let dst_station = r.str_capped("dst_station", caps::STATION_ID)?;
        let pubkey = r.fixed("pubkey", PUBKEY_LEN)?;
        let kex_pubkey = r.fixed("kex_pubkey", KEX_PUBKEY_LEN)?;
        let n = r.u8("signing_modes count")? as usize;
        if n > caps::SIGNING_MODES {
            return Err(ModemError::Frame(format!(
                "CONREQ declares {n} signing modes, over the {} cap",
                caps::SIGNING_MODES
            )));
        }
        // An unknown mode in the OFFER list is skipped, not fatal — `SigningMode::from_wire`'s own
        // contract says "a negotiation outcome, not a parse error to guess at", and rejecting the
        // frame did the opposite. It also made the one negotiable enum in the format un-extendable:
        // the day mode 0x07 exists, a station offering [0x07, Normal] would be unreadable by every
        // deployed v2 peer — a de-facto wire break, which is exactly what a "finished format" is
        // supposed to prevent.
        //
        // Only the STRUCT VIEW drops the byte. The signature covers the body, so the unknown
        // discriminant is still signed and still bound; a peer simply cannot negotiate on it.
        // (The CONACK's SELECTED mode keeps its hard error — a mode we cannot honour is not a
        // negotiation outcome, it is an unusable session.)
        let mut signing_modes = Vec::with_capacity(n);
        for _ in 0..n {
            let b = r.u8("signing_mode")?;
            if let Some(m) = SigningMode::from_wire(b) {
                signing_modes.push(m);
            }
        }
        let session_id = r.u64("session_id")?;
        let station_grid = r.str_capped("station_grid", caps::GRID)?;
        let profile_name = r.str_capped("profile_name", caps::PROFILE_NAME)?;
        let profile_fingerprint = r.u64("profile_fingerprint")?;
        let timestamp_ms = r.u64("timestamp_ms")?;
        r.finish("CONREQ")?;

        if dst_station.is_empty() {
            return Err(ModemError::Frame(
                "CONREQ dst_station is empty; unaddressed cannot be spelled by omission".into(),
            ));
        }
        // F5: an empty station_id verifies under a permissive policy (there is no stored key to
        // bind against) and would then be recorded as a verified peer with an empty callsign — an
        // identity that cannot be revoked, looked up, or logged usefully. `dst_station` was already
        // refused for the sibling reason; this closes the pair.
        if station_id.is_empty() {
            return Err(ModemError::Frame(
                "CONREQ station_id is empty; an unnamed station cannot be a verified peer".into(),
            ));
        }
        Ok(Self {
            station_id,
            dst_station,
            pubkey,
            kex_pubkey,
            signing_modes,
            session_id,
            station_grid,
            profile_name,
            profile_fingerprint,
            timestamp_ms,
            signature: signature.to_vec(),
        })
    }

    /// Whether this request is addressed to `callsign` (or broadcast).
    pub fn is_addressed_to(&self, callsign: &str) -> bool {
        self.dst_station == "*" || self.dst_station == callsign
    }
}

// ------------------------------------------------------------------
// ConAck — connection acknowledgment frame
// ------------------------------------------------------------------

/// Parameters for a CONACK.
#[derive(Debug, Clone)]
pub struct ConAckParams<'a> {
    /// Responder callsign (cap 12).
    pub station_id: &'a str,
    /// The mode chosen from those the CONREQ offered.
    pub selected_mode: SigningMode,
    /// SHA-256 over the complete transmitted CONREQ frame; see [`conreq_hash`].
    pub conreq_hash: [u8; 32],
    /// Maidenhead grid, empty if not advertised (cap 8).
    pub station_grid: &'a str,
    /// Active OTA ladder name, empty if none (cap 24).
    pub profile_name: &'a str,
    /// Fingerprint of the active ladder mapping (0 = none).
    pub profile_fingerprint: u64,
    /// Unix-ms creation time; mandatory in v2.
    pub timestamp_ms: u64,
    /// Ephemeral X25519 public key (exactly 32 bytes).
    pub kex_pubkey: &'a [u8],
}

/// Connection acknowledgement sent by the responding station.
///
/// **Carries no `session_id`.** `conreq_hash` subsumes the echo and binds harder — the session id is
/// cleartext and time-based, hence guessable inside the handshake window, whereas the hash covers
/// the whole transmitted CONREQ including the initiator's `kex_pubkey`. Both endpoints already hold
/// the id: the responder from the CONREQ it verified, the initiator from the CONREQ it sent.
#[derive(Debug, Clone, PartialEq, Eq)]
/// A signed CONACK.
///
/// THERE IS NO `dst_station` HERE, deliberately (#1191). A CONACK has exactly one consumer and it
/// SELF-SELECTS by `conreq_hash`: the initiator matches it against the CONREQ it sent, which is a
/// SHA-256 over the whole transmitted frame and therefore a strictly stronger filter than a
/// callsign echo — it rejects a replayed CONACK between the SAME two stations, which a callsign
/// cannot. #1178's spent-RF argument for addressing the CONREQ does not transfer: nobody transmits
/// in response to a CONACK, so there is no wasted RF for an addressee to prevent. The field had no
/// production reader, and removing it is what buys `caps::STATION_ID` the room to reach 18.
pub struct ConAck {
    /// Responder callsign.
    pub station_id: String,
    /// Ed25519 verifying-key bytes (32).
    pub pubkey: Vec<u8>,
    /// Ephemeral X25519 public key (32).
    pub kex_pubkey: Vec<u8>,
    /// Signing mode selected for this session.
    pub selected_mode: SigningMode,
    /// SHA-256 of the CONREQ this answers, as transmitted.
    pub conreq_hash: [u8; 32],
    /// Maidenhead grid (empty = not advertised).
    pub station_grid: String,
    /// Responder's active OTA ladder name (empty = none).
    pub profile_name: String,
    /// Fingerprint of the responder's ladder mapping (0 = none).
    pub profile_fingerprint: u64,
    /// Unix-ms creation time, signed, for replay freshness.
    pub timestamp_ms: u64,
    /// Ed25519 signature (64) over the transmitted prefix.
    pub signature: Vec<u8>,
}

impl ConAck {
    /// Build and sign a CONACK, returning the frame **bytes**. See [`ConReq::create`] for why bytes.
    pub fn create(
        params: &ConAckParams<'_>,
        signing_key_seed: &[u8; 32],
    ) -> Result<Vec<u8>, ModemError> {
        let signing_key = SigningKey::from_bytes(signing_key_seed);
        let pubkey = signing_key.verifying_key().to_bytes();

        let mut w = BodyWriter::new();
        w.str_capped("station_id", params.station_id, caps::STATION_ID)?;
        w.fixed("pubkey", &pubkey, PUBKEY_LEN)?;
        w.fixed("kex_pubkey", params.kex_pubkey, KEX_PUBKEY_LEN)?;
        w.u8(params.selected_mode.to_wire());
        w.fixed("conreq_hash", &params.conreq_hash, CONREQ_HASH_LEN)?;
        w.str_capped("station_grid", params.station_grid, caps::GRID)?;
        w.str_capped("profile_name", params.profile_name, caps::PROFILE_NAME)?;
        w.u64(params.profile_fingerprint);
        w.u64(params.timestamp_ms);

        let prefix = signed_prefix(MAGIC_CONACK, &w.finish())?;
        let sig = crate::signing::sign_in_band(SigningDomain::ConAck, signing_key_seed, &prefix)
            .map_err(|e| ModemError::Frame(e.to_string()))?;
        let mut frame = prefix;
        frame.extend_from_slice(&sig);
        Ok(frame)
    }

    /// Parse a CONACK frame. Does **not** verify the signature — see [`verify_conack`].
    pub fn decode(bytes: &[u8]) -> Result<Self, ModemError> {
        let spans = split_frame(bytes, MAGIC_CONACK, "CONACK")?;
        let (body, signature) = (spans.body, spans.signature);
        let mut r = BodyReader::new(body);
        let station_id = r.str_capped("station_id", caps::STATION_ID)?;
        let pubkey = r.fixed("pubkey", PUBKEY_LEN)?;
        let kex_pubkey = r.fixed("kex_pubkey", KEX_PUBKEY_LEN)?;
        let mode_byte = r.u8("selected_mode")?;
        let selected_mode = SigningMode::from_wire(mode_byte).ok_or_else(|| {
            ModemError::Frame(format!(
                "CONACK selects unknown signing mode {mode_byte:#04x}"
            ))
        })?;
        let hash_vec = r.fixed("conreq_hash", CONREQ_HASH_LEN)?;
        let station_grid = r.str_capped("station_grid", caps::GRID)?;
        let profile_name = r.str_capped("profile_name", caps::PROFILE_NAME)?;
        let profile_fingerprint = r.u64("profile_fingerprint")?;
        let timestamp_ms = r.u64("timestamp_ms")?;
        r.finish("CONACK")?;

        let mut conreq_hash = [0u8; CONREQ_HASH_LEN];
        conreq_hash.copy_from_slice(&hash_vec);
        Ok(Self {
            station_id,
            pubkey,
            kex_pubkey,
            selected_mode,
            conreq_hash,
            station_grid,
            profile_name,
            profile_fingerprint,
            timestamp_ms,
            signature: signature.to_vec(),
        })
    }
}

/// Verify a CONREQ from its transmitted **bytes**.
///
/// Takes bytes, not a decoded struct. The signature covers the transmitted prefix, so verifying a
/// struct would mean re-encoding it to recover the signed span — a second representation that can
/// drift from the first, which is exactly the v1 defect ("canonical" JSON that was not canonical).
/// Here the verified span is the received bytes themselves.
pub fn verify_conreq(
    bytes: &[u8],
    trust_store: &dyn TrustStore,
    policy: PolicyProfile,
    local_min_mode: SigningMode,
    freshness: Option<Freshness>,
) -> Result<(ConReq, HandshakeDecision), HandshakeError> {
    let spans = split_frame(bytes, MAGIC_CONREQ, "CONREQ")
        .map_err(|e| HandshakeError::Encoding(e.to_string()))?;
    let req = ConReq::decode(bytes).map_err(|e| HandshakeError::Encoding(e.to_string()))?;

    if !verify_ed25519(
        SigningDomain::ConReq,
        &req.pubkey,
        spans.signed_prefix,
        spans.signature,
    ) {
        return Err(HandshakeError::InvalidSignature);
    }

    // Replay-freshness: the timestamp is inside the signed prefix, so this runs after signature
    // verification (an attacker cannot alter it without breaking the signature).
    if let Some(f) = freshness {
        f.check(req.timestamp_ms)?;
    }

    // Bind the in-frame key to the trusted key for this station. The signature above only proves
    // possession of the *frame's own* key; without this bind, an attacker self-signs a CONREQ
    // claiming a trusted callsign with their own key and is classified at that callsign's level.
    bind_frame_key(trust_store, &req.station_id, &req.pubkey)?;

    let key_trust = trust_store.trust_level(&req.station_id);
    let cert_source = cert_source_for_trust(key_trust);
    let decision = evaluate_handshake(
        policy,
        local_min_mode,
        &req.signing_modes,
        key_trust,
        cert_source,
        false,
    )?;
    Ok((req, decision))
}

/// SHA-256 over the complete transmitted CONREQ frame, **including its signature**.
///
/// This is what a CONACK binds to. Binding to the whole frame — rather than echoing the session id —
/// closes mix-and-match shapes generically: the daemon concedes the session id is cleartext and
/// time-based, so guessable inside the handshake window, while this covers the initiator's
/// `kex_pubkey` and every other field exactly as transmitted.
pub fn conreq_hash(conreq_bytes: &[u8]) -> [u8; 32] {
    sha256_bytes(conreq_bytes)
}

/// Verify a CONACK against the CONREQ it answers.
///
/// `conreq_bytes` is the CONREQ **as transmitted**; the binding is a hash over exactly those bytes.
pub fn verify_conack(
    bytes: &[u8],
    conreq_bytes: &[u8],
    offered_modes: &[SigningMode],
    trust_store: &dyn TrustStore,
    policy: PolicyProfile,
    local_min_mode: SigningMode,
    freshness: Option<Freshness>,
) -> Result<(ConAck, HandshakeDecision), HandshakeError> {
    let spans = split_frame(bytes, MAGIC_CONACK, "CONACK")
        .map_err(|e| HandshakeError::Encoding(e.to_string()))?;
    let ack = ConAck::decode(bytes).map_err(|e| HandshakeError::Encoding(e.to_string()))?;

    if !verify_ed25519(
        SigningDomain::ConAck,
        &ack.pubkey,
        spans.signed_prefix,
        spans.signature,
    ) {
        return Err(HandshakeError::InvalidSignature);
    }

    // Transcript binding replaces v1's session-id echo. The daemon concedes the session id is
    // cleartext and time-based, so guessable inside the handshake window; hashing the whole
    // transmitted CONREQ — including the initiator's `kex_pubkey` and the signature — closes
    // mix-and-match shapes generically rather than one at a time.
    let expected = conreq_hash(conreq_bytes);
    if ack.conreq_hash != expected {
        return Err(HandshakeError::SessionIdMismatch {
            expected: hex_short(&expected),
            got: hex_short(&ack.conreq_hash),
        });
    }

    // F-1147-05: the selected mode must be one the CONREQ actually offered. v1 evaluated it against
    // LOCAL policy only and never against the offer, so a responder could select a mode the
    // initiator never proposed — the PQ path already checked this and the wire spec claimed the
    // classical path did too.
    if !offered_modes.contains(&ack.selected_mode) {
        return Err(HandshakeError::UnofferedSigningMode);
    }

    if let Some(f) = freshness {
        f.check(ack.timestamp_ms)?;
    }

    bind_frame_key(trust_store, &ack.station_id, &ack.pubkey)?;

    let key_trust = trust_store.trust_level(&ack.station_id);
    let cert_source = cert_source_for_trust(key_trust);
    let decision = evaluate_handshake(
        policy,
        local_min_mode,
        &[ack.selected_mode],
        key_trust,
        cert_source,
        false,
    )?;
    Ok((ack, decision))
}

fn verify_ed25519(
    domain: SigningDomain,
    pubkey_bytes: &[u8],
    message: &[u8],
    sig_bytes: &[u8],
) -> bool {
    let Ok(pubkey_arr): Result<[u8; 32], _> = pubkey_bytes.try_into() else {
        return false;
    };
    let Ok(sig_arr): Result<[u8; 64], _> = sig_bytes.try_into() else {
        return false;
    };
    crate::signing::verify_in_band(domain, &pubkey_arr, message, &sig_arr)
}
fn cert_source_for_trust(trust_level: PublicKeyTrustLevel) -> CertificateSource {
    match trust_level {
        PublicKeyTrustLevel::Full => CertificateSource::OutOfBand,
        _ => CertificateSource::OverAir,
    }
}
/// Require the frame's public key to equal the trust-store key bound to `station_id`, if any. An unknown
/// station has no stored key, so it proceeds at `Unknown` trust (over-air TOFU) — the bind only rejects a
/// frame that *claims a trusted callsign* under a key the operator did not trust for it.
fn bind_frame_key(
    trust_store: &dyn TrustStore,
    station_id: &str,
    frame_pubkey: &[u8],
) -> Result<(), HandshakeError> {
    if let Some(stored) = trust_store.pubkey_for(station_id) {
        let frame_key: [u8; 32] = frame_pubkey
            .try_into()
            .map_err(|_| HandshakeError::InvalidSignature)?;
        if frame_key != stored {
            return Err(HandshakeError::PublicKeyMismatch);
        }
    }
    Ok(())
}

// ------------------------------------------------------------------
// SHA-256 helper (shared with manifest.rs via pub(crate))
// ------------------------------------------------------------------

pub(crate) fn sha256_bytes(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// First 8 bytes of a hash, for error messages.
fn hex_short(b: &[u8]) -> String {
    b.iter().take(8).map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_key(seed: u8) -> [u8; 32] {
        [seed; 32]
    }

    fn pubkey_for_seed(seed: u8) -> Vec<u8> {
        SigningKey::from_bytes(&make_key(seed))
            .verifying_key()
            .to_bytes()
            .to_vec()
    }

    fn req_params<'a>(ts: u64) -> ConReqParams<'a> {
        ConReqParams {
            station_id: "W1AW",
            dst_station: "DL1ABC",
            signing_modes: vec![SigningMode::Normal],
            session_id: 0x0000_0000_0000_0001,
            station_grid: "FN31pr",
            profile_name: "hpx_hf",
            profile_fingerprint: 42,
            timestamp_ms: ts,
            kex_pubkey: &[1u8; 32],
        }
    }

    fn trusted(station: &str, seed: u8) -> InMemoryTrustStore {
        let mut s = InMemoryTrustStore::new();
        let mut k = [0u8; 32];
        k.copy_from_slice(&pubkey_for_seed(seed));
        s.add_entry(station, k, PublicKeyTrustLevel::Full);
        s
    }

    #[test]
    fn conreq_round_trip() {
        let f = ConReq::create(&req_params(1_700_000_000_000), &make_key(1)).unwrap();
        let d = ConReq::decode(&f).unwrap();
        assert_eq!(d.station_id, "W1AW");
        assert_eq!(d.station_grid, "FN31pr");
        assert_eq!(d.pubkey, pubkey_for_seed(1));
        assert_eq!(d.signature.len(), 64);
    }

    #[test]
    fn conack_round_trip_and_binds_its_conreq() {
        let req_bytes = ConReq::create(&req_params(1_700_000_000_000), &make_key(1)).unwrap();
        let ack_bytes = ConAck::create(
            &ConAckParams {
                station_id: "DL1ABC",
                selected_mode: SigningMode::Normal,
                conreq_hash: conreq_hash(&req_bytes),
                station_grid: "JO62qm",
                profile_name: "hpx_hf",
                profile_fingerprint: 42,
                timestamp_ms: 1_700_000_000_100,
                kex_pubkey: &[2u8; 32],
            },
            &make_key(2),
        )
        .unwrap();
        let d = ConAck::decode(&ack_bytes).unwrap();
        assert_eq!(d.station_id, "DL1ABC");
        assert_eq!(d.selected_mode, SigningMode::Normal);
        assert_eq!(d.conreq_hash, conreq_hash(&req_bytes));

        let st = trusted("DL1ABC", 2);
        assert!(verify_conack(
            &ack_bytes,
            &req_bytes,
            &[SigningMode::Normal],
            &st,
            PolicyProfile::Balanced,
            SigningMode::Normal,
            None
        )
        .is_ok());
    }

    /// A CONACK bound to a DIFFERENT CONREQ is refused — this is what replaces v1's session-id echo.
    #[test]
    fn a_conack_bound_to_another_conreq_is_rejected() {
        let req_a = ConReq::create(&req_params(1_700_000_000_000), &make_key(1)).unwrap();
        let mut other = req_params(1_700_000_000_000);
        other.session_id = 0x0000_0000_0000_0002;
        let req_b = ConReq::create(&other, &make_key(1)).unwrap();
        assert_ne!(conreq_hash(&req_a), conreq_hash(&req_b));

        let ack = ConAck::create(
            &ConAckParams {
                station_id: "DL1ABC",
                selected_mode: SigningMode::Normal,
                conreq_hash: conreq_hash(&req_b),
                station_grid: "",
                profile_name: "",
                profile_fingerprint: 0,
                timestamp_ms: 1_700_000_000_100,
                kex_pubkey: &[2u8; 32],
            },
            &make_key(2),
        )
        .unwrap();
        let st = trusted("DL1ABC", 2);
        let e = verify_conack(
            &ack,
            &req_a,
            &[SigningMode::Normal],
            &st,
            PolicyProfile::Balanced,
            SigningMode::Normal,
            None,
        );
        assert!(matches!(e, Err(HandshakeError::SessionIdMismatch { .. })));
    }

    /// F-1147-05: a mode the CONREQ never offered is refused. v1 checked LOCAL policy only.
    #[test]
    fn a_conack_selecting_an_unoffered_mode_is_rejected() {
        let req = ConReq::create(&req_params(1_700_000_000_000), &make_key(1)).unwrap();
        let ack = ConAck::create(
            &ConAckParams {
                station_id: "DL1ABC",
                selected_mode: SigningMode::Paranoid,
                conreq_hash: conreq_hash(&req),
                station_grid: "",
                profile_name: "",
                profile_fingerprint: 0,
                timestamp_ms: 1_700_000_000_100,
                kex_pubkey: &[2u8; 32],
            },
            &make_key(2),
        )
        .unwrap();
        let st = trusted("DL1ABC", 2);
        let e = verify_conack(
            &ack,
            &req,
            &[SigningMode::Normal, SigningMode::Psk],
            &st,
            PolicyProfile::Balanced,
            SigningMode::Normal,
            None,
        );
        assert!(
            matches!(e, Err(HandshakeError::UnofferedSigningMode)),
            "expected the unoffered-mode refusal, got {e:?}"
        );
    }

    #[test]
    fn verify_conreq_rejects_a_key_that_is_not_the_stations_trusted_key() {
        let f = ConReq::create(&req_params(1_700_000_000_000), &make_key(1)).unwrap();
        // The station is trusted, but under a DIFFERENT key than the frame carries.
        let st = trusted("W1AW", 9);
        assert!(
            verify_conreq(&f, &st, PolicyProfile::Balanced, SigningMode::Normal, None).is_err()
        );
    }

    fn freshness_at(now: u64) -> Freshness {
        Freshness {
            now_ms: now,
            max_skew_ms: 120_000,
        }
    }

    #[test]
    fn fresh_conreq_within_window_is_accepted() {
        let t = 1_700_000_000_000;
        let f = ConReq::create(&req_params(t), &make_key(1)).unwrap();
        let st = trusted("W1AW", 1);
        assert!(verify_conreq(
            &f,
            &st,
            PolicyProfile::Balanced,
            SigningMode::Normal,
            Some(freshness_at(t + 60_000))
        )
        .is_ok());
    }

    #[test]
    fn stale_conreq_is_rejected() {
        let t = 1_700_000_000_000;
        let f = ConReq::create(&req_params(t), &make_key(1)).unwrap();
        let st = trusted("W1AW", 1);
        let e = verify_conreq(
            &f,
            &st,
            PolicyProfile::Balanced,
            SigningMode::Normal,
            Some(freshness_at(t + 500_000)),
        );
        assert!(matches!(e, Err(HandshakeError::StaleTimestamp { .. })));
    }

    #[test]
    fn future_dated_conreq_is_rejected() {
        let t = 1_700_000_000_000;
        let f = ConReq::create(&req_params(t), &make_key(1)).unwrap();
        let st = trusted("W1AW", 1);
        let e = verify_conreq(
            &f,
            &st,
            PolicyProfile::Balanced,
            SigningMode::Normal,
            Some(freshness_at(t - 500_000)),
        );
        assert!(matches!(e, Err(HandshakeError::StaleTimestamp { .. })));
    }

    #[test]
    fn none_freshness_skips_the_check() {
        let f = ConReq::create(&req_params(1), &make_key(1)).unwrap();
        let st = trusted("W1AW", 1);
        assert!(verify_conreq(&f, &st, PolicyProfile::Balanced, SigningMode::Normal, None).is_ok());
    }

    /// v2 makes `timestamp_ms` a mandatory fixed-width field, so a frame carrying NO timestamp is
    /// unconstructible — the v1 test for that asserted a state the format no longer admits.
    ///
    /// Zero is now a VALUE rather than a sentinel, and `Freshness::check` still refuses it as
    /// `MissingTimestamp`. That is kept deliberately: a station claiming the epoch is refused
    /// either way, and the specific error says more than "stale by 1.7e12 ms" would. Written after
    /// this test asserted `StaleTimestamp` and failed — the assumption was mine, not a defect.
    #[test]
    fn a_zero_timestamp_is_refused_as_missing_not_merely_stale() {
        let f = ConReq::create(&req_params(0), &make_key(1)).unwrap();
        assert_eq!(ConReq::decode(&f).unwrap().timestamp_ms, 0);
        let st = trusted("W1AW", 1);
        let e = verify_conreq(
            &f,
            &st,
            PolicyProfile::Balanced,
            SigningMode::Normal,
            Some(freshness_at(1_700_000_000_000)),
        );
        assert!(
            matches!(e, Err(HandshakeError::MissingTimestamp)),
            "expected MissingTimestamp for a zero timestamp, got {e:?}"
        );
    }
}

#[cfg(test)]
mod conreq_v2_tests {
    use super::*;
    use crate::handshake_wire::{HEADER_LEN, SIG_LEN, WIRE_VERSION};

    const SEED: [u8; 32] = [7u8; 32];

    fn params<'a>(dst: &'a str) -> ConReqParams<'a> {
        ConReqParams {
            station_id: "DC0SK",
            dst_station: dst,
            signing_modes: vec![SigningMode::Normal, SigningMode::Psk],
            session_id: 1_755_000_000,
            station_grid: "JO62qm",
            profile_name: "hpx_hf",
            profile_fingerprint: 0xDEAD_BEEF_CAFE_F00D,
            timestamp_ms: 1_755_000_000_000,
            kex_pubkey: &[3u8; 32],
        }
    }

    fn store(req: &ConReq) -> InMemoryTrustStore {
        let mut s = InMemoryTrustStore::new();
        let mut k = [0u8; 32];
        k.copy_from_slice(&req.pubkey);
        s.add_entry("DC0SK", k, PublicKeyTrustLevel::Full);
        s
    }

    fn verify(
        bytes: &[u8],
        st: &InMemoryTrustStore,
    ) -> Result<(ConReq, HandshakeDecision), HandshakeError> {
        verify_conreq(
            bytes,
            st,
            PolicyProfile::Balanced,
            SigningMode::Normal,
            None,
        )
    }

    #[test]
    fn a_signed_conreq_round_trips_and_verifies() {
        let f = ConReq::create(&params("*"), &SEED).unwrap();
        let decoded = ConReq::decode(&f).unwrap();
        assert_eq!(decoded.station_id, "DC0SK");
        assert_eq!(decoded.dst_station, "*");
        assert_eq!(
            decoded.signing_modes,
            vec![SigningMode::Normal, SigningMode::Psk]
        );
        assert_eq!(decoded.profile_fingerprint, 0xDEAD_BEEF_CAFE_F00D);
        assert_eq!(decoded.timestamp_ms, 1_755_000_000_000);
        let st = store(&decoded);
        assert!(verify(&f, &st).is_ok());
    }

    /// A REAL CONREQ FITS ONE FRAGMENT — the whole point of #1147. The v1 JSON frame was 752 B on
    /// the wire = 3 SAR fragments = three acquisitions, decoding at ~p^3 on a fading channel.
    #[test]
    fn a_realistic_conreq_fits_one_sar_fragment() {
        let f = ConReq::create(&params("DL1ABC"), &SEED).unwrap();
        assert!(
            f.len() <= crate::handshake_wire::FRAGMENT_CAPACITY,
            "a realistic CONREQ is {} B, over one fragment",
            f.len()
        );
    }

    /// BYTE TAMPER, one per mutable region. The old tests mutated STRUCT FIELDS and re-verified;
    /// under a sign-the-bytes format that is vacuous, because re-encoding recomputes the signed span.
    /// Each region here is mutated in the transmitted bytes and must break verification.
    #[test]
    fn tampering_with_any_signed_region_breaks_verification() {
        let f = ConReq::create(&params("DL1ABC"), &SEED).unwrap();
        let st = store(&ConReq::decode(&f).unwrap());
        assert!(
            verify(&f, &st).is_ok(),
            "control: the untampered frame must verify"
        );

        // The magic, the version, the length field, and several body offsets. Body offsets are
        // chosen across distinct fields rather than at one spot, so a signature that happened to
        // cover only a prefix would still be caught.
        let body_start = HEADER_LEN;
        let body_len = f.len() - HEADER_LEN - SIG_LEN;
        let mut regions = vec![("magic", 0usize), ("version", 4), ("length", 6)];
        for (i, off) in [0usize, body_len / 4, body_len / 2, body_len - 1]
            .iter()
            .enumerate()
        {
            regions.push((
                ["body@start", "body@quarter", "body@half", "body@end"][i],
                body_start + off,
            ));
        }
        for (what, idx) in regions {
            let mut bad = f.clone();
            bad[idx] ^= 0xFF;
            assert!(
                verify(&bad, &st).is_err(),
                "flipping `{what}` (byte {idx}) still verified — that region is not covered by the \
                 signature, so the signed span is not the transmitted prefix"
            );
        }
    }

    /// A frame cannot verify as another TYPE or VERSION: both are inside the signed prefix.
    #[test]
    fn a_frame_cannot_be_reinterpreted_as_another_type_or_version() {
        let f = ConReq::create(&params("DL1ABC"), &SEED).unwrap();
        let st = store(&ConReq::decode(&f).unwrap());

        let mut wrong_version = f.clone();
        wrong_version[4] = WIRE_VERSION + 1;
        assert!(verify(&wrong_version, &st).is_err());

        let mut as_conack = f.clone();
        as_conack[..4].copy_from_slice(b"HSAK");
        assert!(
            verify(&as_conack, &st).is_err(),
            "a CONACK-magicked frame verified as a CONREQ"
        );
    }

    /// #1178: unaddressed must not be spellable by omission, at either end.
    #[test]
    fn an_empty_dst_station_is_refused_at_both_ends() {
        let e = ConReq::create(&params(""), &SEED).unwrap_err().to_string();
        assert!(
            e.contains("dst_station"),
            "expected a dst_station refusal, got: {e}"
        );

        // And a hand-built frame with an empty dst cannot be decoded either, so the encoder-side
        // check is not the only thing standing between the wire and an unaddressed request.
        let f = ConReq::create(&params("*"), &SEED).unwrap();
        let mut hand = f.clone();
        let dst_len_idx = HEADER_LEN + 1 + "DC0SK".len();
        assert_eq!(
            hand[dst_len_idx], 1,
            "expected the 1-byte \"*\" length here"
        );
        hand[dst_len_idx] = 0;
        assert!(ConReq::decode(&hand).is_err());
    }

    /// Addressing is what the daemon filters on (#1178).
    #[test]
    fn addressing_matches_the_wildcard_and_the_exact_callsign_only() {
        let b = ConReq::decode(&ConReq::create(&params("*"), &SEED).unwrap()).unwrap();
        assert!(b.is_addressed_to("DL1ABC") && b.is_addressed_to("DC0SK"));
        let d = ConReq::decode(&ConReq::create(&params("DL1ABC"), &SEED).unwrap()).unwrap();
        assert!(d.is_addressed_to("DL1ABC"));
        assert!(!d.is_addressed_to("DL2XYZ"));
    }

    /// The hash a CONACK binds to covers the signature too, so two frames differing only there
    /// bind differently.
    #[test]
    fn the_conreq_hash_covers_the_whole_transmitted_frame() {
        let f = ConReq::create(&params("DL1ABC"), &SEED).unwrap();
        let h = conreq_hash(&f);
        let mut other = f.clone();
        let last = other.len() - 1;
        other[last] ^= 0x01;
        assert_ne!(
            h,
            conreq_hash(&other),
            "the hash ignores the signature region"
        );
    }
}
