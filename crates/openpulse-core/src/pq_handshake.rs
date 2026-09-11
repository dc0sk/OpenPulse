use ed25519_dalek::SigningKey as EdSigningKey;
use ml_dsa::{
    signature::{Keypair as MlDsaKeypair, Signer as MlSigner, Verifier as MlVerifier},
    EncodedVerifyingKey, MlDsa44, SigningKey as MlDsaSigningKey,
};
use ml_kem::{
    kem::{Decapsulate, Encapsulate, KeyExport},
    Ciphertext, DecapsulationKey, EncapsulationKey, Key, MlKem768, Seed as MlKemSeed,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::error::ModemError;
use crate::handshake::{Freshness, TrustStore};
use crate::handshake_wire::{
    caps, signed_prefix_with_magic, split_frame_variable, BodyReader, BodyWriter, CONREQ_HASH_LEN,
    MAGIC_PQ_CONACK, MAGIC_PQ_CONREQ, PUBKEY_LEN, SIG_LEN,
};
use crate::signing_domain::SigningDomain;
use crate::trust::{
    evaluate_handshake, CertificateSource, HandshakeDecision, PolicyProfile, PublicKeyTrustLevel,
    SigningMode, TrustError,
};

// ------------------------------------------------------------------
// Size constants (verified against FIPS 203/204)
// ------------------------------------------------------------------

/// Byte length of an ML-DSA-44 verifying key.
pub const ML_DSA_44_PUBKEY_SIZE: usize = 1312;
/// Byte length of an ML-DSA-44 signature.
pub const ML_DSA_44_SIG_SIZE: usize = 2420;
/// Byte length of an ML-KEM-768 encapsulation key.
pub const ML_KEM_768_EK_SIZE: usize = 1184;
/// ML-KEM-768 decapsulation key in d||z seed form (64 bytes).
pub const ML_KEM_768_DK_SIZE: usize = 64;
/// Byte length of an ML-KEM-768 ciphertext.
pub const ML_KEM_768_CT_SIZE: usize = 1088;
/// Byte length of the ML-KEM-768 shared secret.
pub const ML_KEM_768_SS_SIZE: usize = 32;

// ------------------------------------------------------------------
// Error type
// ------------------------------------------------------------------

/// Errors returned during post-quantum handshake creation or verification.
#[derive(Debug, thiserror::Error)]
pub enum PqHandshakeError {
    #[error("invalid ML-DSA-44 public key")]
    InvalidPublicKey,
    #[error("invalid ML-DSA-44 or Ed25519 signature")]
    InvalidSignature,
    #[error("invalid ML-KEM-768 ciphertext or key")]
    InvalidCiphertext,
    #[error("trust policy rejected: {0:?}")]
    TrustPolicyRejected(TrustError),
    #[error("session ID mismatch: expected {expected}, got {got}")]
    SessionIdMismatch { expected: String, got: String },
    #[error("serialization error: {0}")]
    SerializationError(String),
    #[error("public key in frame does not match stored trusted key")]
    PublicKeyMismatch,
    #[error("selected mode was not offered by the initiator")]
    UnauthorizedMode,
}

impl From<TrustError> for PqHandshakeError {
    fn from(e: TrustError) -> Self {
        PqHandshakeError::TrustPolicyRejected(e)
    }
}

// ------------------------------------------------------------------
// Wire frame body structs (canonical — excludes signature fields)
// ------------------------------------------------------------------

/// Encode a PQ CONREQ body. Layout mirrors the classical frame: capped strings, positional
/// fixed-size keys, big-endian integers, and the classical-signature presence flag INSIDE the body
/// so it is covered by both signatures.
#[allow(clippy::too_many_arguments)]
fn encode_pq_conreq_body(
    station_id: &str,
    dst_station: &str,
    classical_pubkey: &[u8],
    pq_pubkey: &[u8],
    kem_pubkey: &[u8],
    signing_modes: &[SigningMode],
    session_id: u64,
    timestamp_ms: u64,
    has_classical_sig: bool,
) -> Result<Vec<u8>, ModemError> {
    let mut w = BodyWriter::new();
    w.str_capped("station_id", station_id, caps::STATION_ID)?;
    w.str_capped("dst_station", dst_station, caps::STATION_ID)?;
    w.fixed("classical_pubkey", classical_pubkey, PUBKEY_LEN)?;
    w.fixed("pq_pubkey", pq_pubkey, ML_DSA_44_PUBKEY_SIZE)?;
    w.fixed("kem_pubkey", kem_pubkey, ML_KEM_768_EK_SIZE)?;
    w.u8(signing_modes.len() as u8);
    for m in signing_modes {
        w.u8(m.to_wire());
    }
    w.u64(session_id);
    w.u64(timestamp_ms);
    w.u8(u8::from(has_classical_sig));
    Ok(w.finish())
}

// ------------------------------------------------------------------
// Wire frame structs
// ------------------------------------------------------------------

/// Post-quantum connection request (initiator → responder).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PqConReq {
    pub station_id: String,
    /// Who the request is addressed to; `"*"` is the explicit wildcard (#1178). Empty is invalid.
    pub dst_station: String,
    /// Unix-ms creation time, signed, for replay freshness.
    ///
    /// **The PQ bodies had NO timestamp at all**, so the PQ path had none of the replay protection
    /// the classical path gained in #615. Freezing that field-for-field into the new format would
    /// have baked a known hole into the wire and voided the stated reason for scoping PQ into this
    /// break — that the format be FINISHED, so wiring PQ later is not a second break.
    pub timestamp_ms: u64,
    /// Ed25519 verifying key (32 B).
    pub classical_pubkey: Vec<u8>,
    /// ML-DSA-44 verifying key (1312 B).
    pub pq_pubkey: Vec<u8>,
    /// ML-KEM-768 encapsulation key (1184 B).
    pub kem_pubkey: Vec<u8>,
    pub signing_modes: Vec<SigningMode>,
    pub session_id: u64,
    /// Ed25519 signature (64 B); empty when mode is `Pq`-only.
    pub classical_signature: Vec<u8>,
    /// ML-DSA-44 signature (2420 B).
    pub pq_signature: Vec<u8>,
}

/// Post-quantum connection acknowledgment (responder → initiator).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PqConAck {
    pub station_id: String,
    /// SHA-256 over the complete transmitted PQ CONREQ, replacing the session-id echo.
    pub conreq_hash: [u8; 32],
    /// Unix-ms creation time, signed, for replay freshness. See [`PqConReq::timestamp_ms`].
    pub timestamp_ms: u64,
    /// Ed25519 verifying key (32 B).
    pub classical_pubkey: Vec<u8>,
    /// ML-DSA-44 verifying key (1312 B).
    pub pq_pubkey: Vec<u8>,
    /// ML-KEM-768 ciphertext (1088 B).
    pub kem_ciphertext: Vec<u8>,
    pub selected_mode: SigningMode,
    /// Ed25519 signature (64 B); empty when mode is `Pq`.
    pub classical_signature: Vec<u8>,
    /// ML-DSA-44 signature (2420 B).
    pub pq_signature: Vec<u8>,
}

// ------------------------------------------------------------------
// Key generation
// ------------------------------------------------------------------

/// Returns (signing_key_bytes [32 B seed], verifying_key_bytes [1312 B]).
pub fn generate_ml_dsa_44_keypair() -> (Vec<u8>, Vec<u8>) {
    let mut seed_bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut seed_bytes);
    let sk = MlDsaSigningKey::<MlDsa44>::from_seed(&ml_dsa::Seed::from(seed_bytes));
    let vk_encoded = sk.verifying_key().encode();
    let vk_bytes: Vec<u8> = vk_encoded.to_vec();
    (seed_bytes.to_vec(), vk_bytes)
}

/// Returns (decapsulation_key_bytes [64 B seed], encapsulation_key_bytes [1184 B]).
pub fn generate_ml_kem_768_keypair() -> (Vec<u8>, Vec<u8>) {
    let mut seed_bytes = [0u8; 64];
    rand::rngs::OsRng.fill_bytes(&mut seed_bytes);
    let dk = DecapsulationKey::<MlKem768>::from_seed(MlKemSeed::from(seed_bytes));
    let ek_encoded = dk.encapsulation_key().to_bytes();
    let ek_bytes: Vec<u8> = ek_encoded.to_vec();
    let dk_bytes: Vec<u8> = seed_bytes.to_vec();
    (dk_bytes, ek_bytes)
}

// ------------------------------------------------------------------
// Internal signing/verification helpers
// ------------------------------------------------------------------

// ML-DSA, not Ed25519: a different algorithm, so the Ed25519 wall does not apply. It
// signs the SAME domain-tagged message as its classical partner — the domain is the
// frame type, not the key, so the hybrid needs one tag rather than one per algorithm.
#[allow(clippy::disallowed_methods)]
fn ml_dsa_sign(signing_key_seed: &[u8], message: &[u8]) -> Result<Vec<u8>, PqHandshakeError> {
    let seed_arr: [u8; 32] = signing_key_seed
        .try_into()
        .map_err(|_| PqHandshakeError::InvalidPublicKey)?;
    let sk = MlDsaSigningKey::<MlDsa44>::from_seed(&ml_dsa::Seed::from(seed_arr));
    let sig: ml_dsa::Signature<MlDsa44> = sk.sign(message);
    let sig_encoded = sig.encode();
    Ok(sig_encoded.to_vec())
}

// ML-DSA, not Ed25519: a different algorithm, so the Ed25519 wall does not apply. It
// verifies the SAME domain-tagged message as its classical partner — the domain is the
// frame type, not the key, so the hybrid needs one tag rather than one per algorithm.
#[allow(clippy::disallowed_methods)]
fn ml_dsa_verify(
    vk_bytes: &[u8],
    message: &[u8],
    sig_bytes: &[u8],
) -> Result<(), PqHandshakeError> {
    let vk_arr = EncodedVerifyingKey::<MlDsa44>::try_from(vk_bytes)
        .map_err(|_| PqHandshakeError::InvalidPublicKey)?;
    let vk = ml_dsa::VerifyingKey::<MlDsa44>::decode(&vk_arr);
    let sig = ml_dsa::Signature::<MlDsa44>::try_from(sig_bytes)
        .map_err(|_| PqHandshakeError::InvalidSignature)?;
    vk.verify(message, &sig)
        .map_err(|_| PqHandshakeError::InvalidSignature)
}

fn ed25519_sign(domain: SigningDomain, classical_seed: &[u8; 32], message: &[u8]) -> Vec<u8> {
    crate::signing::sign_in_band(domain, classical_seed, message)
        .map(|s| s.to_vec())
        .unwrap_or_default()
}

fn ed25519_verify(
    domain: SigningDomain,
    vk_bytes: &[u8],
    message: &[u8],
    sig_bytes: &[u8],
) -> bool {
    let Ok(vk_arr): Result<[u8; 32], _> = vk_bytes.try_into() else {
        return false;
    };
    let Ok(sig_arr): Result<[u8; 64], _> = sig_bytes.try_into() else {
        return false;
    };
    crate::signing::verify_in_band(domain, &vk_arr, message, &sig_arr)
}

fn cert_source_for_trust(trust_level: PublicKeyTrustLevel) -> CertificateSource {
    match trust_level {
        PublicKeyTrustLevel::Full => CertificateSource::OutOfBand,
        _ => CertificateSource::OverAir,
    }
}

/// Returns true if the given mode list represents Pq-only (no Hybrid).
fn is_pq_only(modes: &[SigningMode]) -> bool {
    modes.contains(&SigningMode::Pq) && !modes.contains(&SigningMode::Hybrid)
}

// ------------------------------------------------------------------
// Handshake creation
// ------------------------------------------------------------------

/// Parameters for a PQ CONREQ.
///
/// DORMANT(#1147): the PQ path has zero production callers — nothing constructs or dispatches a PQ
/// frame. That is the premise of scoping PQ into this format break rather than a gap: at ~5 kB
/// (~2.7 min at BPSK250) a PQ handshake is not deployable on this link, so what the work buys is a
/// FINISHED format, so that wiring PQ later is not a second wire break.
pub struct PqConReqParams<'a> {
    /// This station's callsign (cap 12).
    pub station_id: &'a str,
    /// Addressee, or `"*"` (#1178). Empty is invalid.
    pub dst_station: &'a str,
    /// ML-DSA-44 signing seed.
    pub pq_signing_key: &'a [u8],
    /// ML-KEM-768 encapsulation key.
    pub kem_ek: &'a [u8],
    /// Modes offered, in preference order.
    pub signing_modes: Vec<SigningMode>,
    /// Session identifier (fixed-width; see `ConReqParams::session_id`).
    pub session_id: u64,
    /// Unix-ms creation time; mandatory.
    pub timestamp_ms: u64,
}

/// Build and sign a PQ CONREQ, returning the frame **bytes**.
///
/// Both signatures cover the same transmitted prefix as the classical frames. The ML-DSA signature
/// is always present; the Ed25519 one is present unless the offered modes are PQ-only, and its
/// presence is a flag INSIDE the signed body — outside it, one flipped bit would change how many
/// trailing bytes a verifier reads as the classical signature, and that bit would be unsigned.
pub fn create_pq_conreq(
    params: &PqConReqParams<'_>,
    classical_seed: &[u8; 32],
) -> Result<Vec<u8>, PqHandshakeError> {
    if params.dst_station.is_empty() {
        return Err(PqHandshakeError::SerializationError(
            "PQ CONREQ dst_station is empty; use \"*\" for a broadcast request (#1178)".into(),
        ));
    }
    let ed_sk = EdSigningKey::from_bytes(classical_seed);
    let classical_pubkey = ed_sk.verifying_key().to_bytes().to_vec();

    let seed_arr: [u8; 32] = params
        .pq_signing_key
        .try_into()
        .map_err(|_| PqHandshakeError::InvalidPublicKey)?;
    let mldsa_sk = MlDsaSigningKey::<MlDsa44>::from_seed(&ml_dsa::Seed::from(seed_arr));
    let pq_pubkey: Vec<u8> = mldsa_sk.verifying_key().encode().to_vec();

    let has_classical = !is_pq_only(&params.signing_modes);
    let body = encode_pq_conreq_body(
        params.station_id,
        params.dst_station,
        &classical_pubkey,
        &pq_pubkey,
        params.kem_ek,
        &params.signing_modes,
        params.session_id,
        params.timestamp_ms,
        has_classical,
    )
    .map_err(|e| PqHandshakeError::SerializationError(e.to_string()))?;

    let prefix = signed_prefix_with_magic(MAGIC_PQ_CONREQ, &body)
        .map_err(|e| PqHandshakeError::SerializationError(e.to_string()))?;

    let pq_signature = ml_dsa_sign(params.pq_signing_key, &prefix)?;
    let mut frame = prefix.clone();
    if has_classical {
        frame.extend_from_slice(&ed25519_sign(
            SigningDomain::PqConReq,
            classical_seed,
            &prefix,
        ));
    }
    frame.extend_from_slice(&pq_signature);
    Ok(frame)
}

/// Parse a PQ CONREQ frame. Does **not** verify signatures — see [`verify_pq_conreq`].
pub fn decode_pq_conreq(bytes: &[u8]) -> Result<PqConReq, PqHandshakeError> {
    let (spans, trailer) = split_frame_variable(bytes, MAGIC_PQ_CONREQ, "PQ CONREQ")
        .map_err(|e| PqHandshakeError::SerializationError(e.to_string()))?;
    let mut r = BodyReader::new(spans.body);
    let station_id = rd(r.str_capped("station_id", caps::STATION_ID))?;
    let dst_station = rd(r.str_capped("dst_station", caps::STATION_ID))?;
    let classical_pubkey = rd(r.fixed("classical_pubkey", PUBKEY_LEN))?;
    let pq_pubkey = rd(r.fixed("pq_pubkey", ML_DSA_44_PUBKEY_SIZE))?;
    let kem_pubkey = rd(r.fixed("kem_pubkey", ML_KEM_768_EK_SIZE))?;
    let n = rd(r.u8("signing_modes count"))? as usize;
    if n > caps::SIGNING_MODES {
        return Err(PqHandshakeError::SerializationError(format!(
            "PQ CONREQ declares {n} signing modes, over the {} cap",
            caps::SIGNING_MODES
        )));
    }
    // Unknown modes in the OFFER list are skipped, not fatal — see `ConReq::decode` for why.
    // Note `is_pq_only` is unaffected by this: it inspects only `Pq` and `Hybrid`, both known, so
    // dropping unknown discriminants cannot change whether a classical signature is expected.
    let mut signing_modes = Vec::with_capacity(n);
    for _ in 0..n {
        let b = rd(r.u8("signing_mode"))?;
        if let Some(m) = SigningMode::from_wire(b) {
            signing_modes.push(m);
        }
    }
    let session_id = rd(r.u64("session_id"))?;
    let timestamp_ms = rd(r.u64("timestamp_ms"))?;
    let has_classical = rd(r.u8("has_classical_sig"))? != 0;
    rd(r.finish("PQ CONREQ"))?;

    if dst_station.is_empty() {
        return Err(PqHandshakeError::SerializationError(
            "PQ CONREQ dst_station is empty".into(),
        ));
    }
    // A1: the flag must agree with what the signed modes imply. `create` derives it exactly this
    // way, so any frame where they disagree was hand-built — and the disagreement matters: the
    // VERIFIER decides whether to check the classical signature from `is_pq_only(modes)`, not from
    // the flag, so `flag = 1` with PQ-only modes would carry 64 trailing bytes that nothing ever
    // verifies. That is a covert channel inside a frame the receiver calls verified, and it makes
    // the frame malleable, which breaks `conreq_hash` determinism and hence CONACK binding.
    if has_classical != !is_pq_only(&signing_modes) {
        return Err(PqHandshakeError::SerializationError(format!(
            "PQ CONREQ says classical signature {} but its signed modes imply {}",
            if has_classical { "present" } else { "absent" },
            if is_pq_only(&signing_modes) {
                "absent"
            } else {
                "present"
            }
        )));
    }
    let (classical_signature, pq_signature) = split_pq_trailer(trailer, has_classical)?;
    Ok(PqConReq {
        station_id,
        dst_station,
        timestamp_ms,
        classical_pubkey,
        pq_pubkey,
        kem_pubkey,
        signing_modes,
        session_id,
        classical_signature,
        pq_signature,
    })
}

/// Map a codec error into the PQ error type.
fn rd<T>(r: Result<T, ModemError>) -> Result<T, PqHandshakeError> {
    r.map_err(|e| PqHandshakeError::SerializationError(e.to_string()))
}

/// Split the trailing signature region, cross-checking it against the body's presence flag.
///
/// The flag is signed; the trailer length is not. Checking them against each other is what stops a
/// truncated or padded frame from being reinterpreted with a different signature split.
fn split_pq_trailer(
    trailer: &[u8],
    has_classical: bool,
) -> Result<(Vec<u8>, Vec<u8>), PqHandshakeError> {
    let expected = if has_classical {
        SIG_LEN + ML_DSA_44_SIG_SIZE
    } else {
        ML_DSA_44_SIG_SIZE
    };
    if trailer.len() != expected {
        return Err(PqHandshakeError::SerializationError(format!(
            "PQ frame carries {} trailing signature bytes; the signed body says it should carry \
             {expected} (classical signature {})",
            trailer.len(),
            if has_classical { "present" } else { "absent" }
        )));
    }
    Ok(if has_classical {
        (trailer[..SIG_LEN].to_vec(), trailer[SIG_LEN..].to_vec())
    } else {
        (Vec::new(), trailer.to_vec())
    })
}

/// Parameters for a PQ CONACK.
///
/// DORMANT(#1147): see [`PqConReqParams`].
pub struct PqConAckParams<'a> {
    /// Responder callsign (cap 12).
    pub station_id: &'a str,
    /// ML-DSA-44 signing seed.
    pub pq_signing_key: &'a [u8],
    /// The initiator's ML-KEM encapsulation key, from its CONREQ.
    pub req_kem_ek: &'a [u8],
    /// Mode chosen from those the CONREQ offered.
    pub selected_mode: SigningMode,
    /// SHA-256 over the complete transmitted PQ CONREQ.
    pub conreq_hash: [u8; 32],
    /// Unix-ms creation time; mandatory.
    pub timestamp_ms: u64,
}

/// Build and sign a PQ CONACK, returning `(frame_bytes, shared_secret)`.
///
/// Encapsulates the KEM key from `req_kem_ek`.
pub fn create_pq_conack(
    params: &PqConAckParams<'_>,
    classical_seed: &[u8; 32],
) -> Result<(Vec<u8>, Vec<u8>), PqHandshakeError> {
    let ed_sk = EdSigningKey::from_bytes(classical_seed);
    let classical_pubkey = ed_sk.verifying_key().to_bytes().to_vec();

    let seed_arr: [u8; 32] = params
        .pq_signing_key
        .try_into()
        .map_err(|_| PqHandshakeError::InvalidPublicKey)?;
    let mldsa_sk = MlDsaSigningKey::<MlDsa44>::from_seed(&ml_dsa::Seed::from(seed_arr));
    let pq_pubkey: Vec<u8> = mldsa_sk.verifying_key().encode().to_vec();

    let ek_arr = Key::<EncapsulationKey<MlKem768>>::try_from(params.req_kem_ek)
        .map_err(|_| PqHandshakeError::InvalidPublicKey)?;
    let ek = EncapsulationKey::<MlKem768>::new(&ek_arr)
        .map_err(|_| PqHandshakeError::InvalidPublicKey)?;
    let (ct, ss) = ek.encapsulate();
    let kem_ciphertext: Vec<u8> = ct.to_vec();
    let shared_secret: Vec<u8> = ss.to_vec();

    let has_classical = params.selected_mode != SigningMode::Pq;
    let mut w = BodyWriter::new();
    let e = |x: Result<(), ModemError>| {
        x.map_err(|e| PqHandshakeError::SerializationError(e.to_string()))
    };
    e(w.str_capped("station_id", params.station_id, caps::STATION_ID))?;
    e(w.fixed("classical_pubkey", &classical_pubkey, PUBKEY_LEN))?;
    e(w.fixed("pq_pubkey", &pq_pubkey, ML_DSA_44_PUBKEY_SIZE))?;
    e(w.fixed("kem_ciphertext", &kem_ciphertext, ML_KEM_768_CT_SIZE))?;
    w.u8(params.selected_mode.to_wire());
    e(w.fixed("conreq_hash", &params.conreq_hash, CONREQ_HASH_LEN))?;
    w.u64(params.timestamp_ms);
    w.u8(u8::from(has_classical));

    let prefix = signed_prefix_with_magic(MAGIC_PQ_CONACK, &w.finish())
        .map_err(|e| PqHandshakeError::SerializationError(e.to_string()))?;
    let pq_signature = ml_dsa_sign(params.pq_signing_key, &prefix)?;
    let mut frame = prefix.clone();
    if has_classical {
        frame.extend_from_slice(&ed25519_sign(
            SigningDomain::PqConAck,
            classical_seed,
            &prefix,
        ));
    }
    frame.extend_from_slice(&pq_signature);
    Ok((frame, shared_secret))
}

/// Parse a PQ CONACK frame. Does **not** verify signatures — see [`verify_pq_conack`].
pub fn decode_pq_conack(bytes: &[u8]) -> Result<PqConAck, PqHandshakeError> {
    let (spans, trailer) = split_frame_variable(bytes, MAGIC_PQ_CONACK, "PQ CONACK")
        .map_err(|e| PqHandshakeError::SerializationError(e.to_string()))?;
    let mut r = BodyReader::new(spans.body);
    let station_id = rd(r.str_capped("station_id", caps::STATION_ID))?;
    let classical_pubkey = rd(r.fixed("classical_pubkey", PUBKEY_LEN))?;
    let pq_pubkey = rd(r.fixed("pq_pubkey", ML_DSA_44_PUBKEY_SIZE))?;
    let kem_ciphertext = rd(r.fixed("kem_ciphertext", ML_KEM_768_CT_SIZE))?;
    let mode_byte = rd(r.u8("selected_mode"))?;
    let selected_mode = SigningMode::from_wire(mode_byte).ok_or_else(|| {
        PqHandshakeError::SerializationError(format!("unknown signing mode {mode_byte:#04x}"))
    })?;
    let hash_vec = rd(r.fixed("conreq_hash", CONREQ_HASH_LEN))?;
    let timestamp_ms = rd(r.u64("timestamp_ms"))?;
    let has_classical = rd(r.u8("has_classical_sig"))? != 0;
    rd(r.finish("PQ CONACK"))?;

    // A1, CONACK side: `create` sets the flag as `selected_mode != Pq`; enforce the same here.
    if has_classical != (selected_mode != SigningMode::Pq) {
        return Err(PqHandshakeError::SerializationError(format!(
            "PQ CONACK says classical signature {} but its signed mode {selected_mode:?} implies \
             otherwise",
            if has_classical { "present" } else { "absent" }
        )));
    }
    let (classical_signature, pq_signature) = split_pq_trailer(trailer, has_classical)?;
    let mut conreq_hash = [0u8; CONREQ_HASH_LEN];
    conreq_hash.copy_from_slice(&hash_vec);
    Ok(PqConAck {
        station_id,
        conreq_hash,
        timestamp_ms,
        classical_pubkey,
        pq_pubkey,
        kem_ciphertext,
        selected_mode,
        classical_signature,
        pq_signature,
    })
}

/// Decapsulate the ML-KEM-768 ciphertext from `PqConAck` to recover the shared secret.
pub fn kem_decapsulate(dk: &[u8], ct: &[u8]) -> Result<Vec<u8>, PqHandshakeError> {
    let dk_seed: [u8; 64] = dk
        .try_into()
        .map_err(|_| PqHandshakeError::InvalidCiphertext)?;
    let kem_dk = DecapsulationKey::<MlKem768>::from_seed(MlKemSeed::from(dk_seed));
    let ct_arr =
        Ciphertext::<MlKem768>::try_from(ct).map_err(|_| PqHandshakeError::InvalidCiphertext)?;
    let ss = kem_dk.decapsulate(&ct_arr);
    Ok(ss.to_vec())
}

// ------------------------------------------------------------------
// Verification
// ------------------------------------------------------------------

/// Verify a received PqConReq and evaluate trust.
/// Verify a PQ CONREQ.
///
/// DORMANT(#1147): the PQ path has **zero production callers** — nothing constructs or dispatches a
/// PQ frame yet, which is why #1147 scopes PQ into the format break (so wiring it later is not a
/// second wire break). It was previously counted reachable only because `handshake.rs` mentioned it
/// in a doc comment; the reachability scan treats a prose mention as a reference, and rewriting that
/// comment exposed the truth. Its sibling `verify_pq_conack` has been in the baseline all along.
pub fn verify_pq_conreq(
    bytes: &[u8],
    trust_store: &dyn TrustStore,
    policy: PolicyProfile,
    local_min_mode: SigningMode,
    freshness: Option<Freshness>,
) -> Result<(PqConReq, HandshakeDecision), PqHandshakeError> {
    // Verified from the transmitted bytes, exactly as the classical path is.
    let (spans, _trailer) = split_frame_variable(bytes, MAGIC_PQ_CONREQ, "PQ CONREQ")
        .map_err(|e| PqHandshakeError::SerializationError(e.to_string()))?;
    let req = decode_pq_conreq(bytes)?;
    let canonical = spans.signed_prefix;

    // ML-DSA-44 signature always required
    ml_dsa_verify(&req.pq_pubkey, canonical, &req.pq_signature)?;

    // Ed25519 signature required unless Pq-only
    if !is_pq_only(&req.signing_modes)
        && !ed25519_verify(
            SigningDomain::PqConReq,
            &req.classical_pubkey,
            canonical,
            &req.classical_signature,
        )
    {
        return Err(PqHandshakeError::InvalidSignature);
    }

    // Replay freshness — the PQ bodies had NO timestamp before #1147, so this check had nothing to
    // run on and the PQ path carried none of the protection the classical path gained in #615.
    if let Some(f) = freshness {
        f.check(req.timestamp_ms)
            .map_err(|e| PqHandshakeError::SerializationError(e.to_string()))?;
    }

    // Bind the in-frame classical key to the stored trusted key, if any.
    if let Some(stored_key) = trust_store.pubkey_for(&req.station_id) {
        let frame_key: [u8; 32] = req
            .classical_pubkey
            .as_slice()
            .try_into()
            .map_err(|_| PqHandshakeError::PublicKeyMismatch)?;
        if frame_key != stored_key {
            return Err(PqHandshakeError::PublicKeyMismatch);
        }
    }

    // Validate the KEM encapsulation key is syntactically well-formed.
    let ek_arr = Key::<EncapsulationKey<MlKem768>>::try_from(req.kem_pubkey.as_slice())
        .map_err(|_| PqHandshakeError::InvalidPublicKey)?;
    EncapsulationKey::<MlKem768>::new(&ek_arr).map_err(|_| PqHandshakeError::InvalidPublicKey)?;

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

/// Verify a received PqConAck, checking session_id, mode, pubkey binding, and signatures.
pub fn verify_pq_conack(
    bytes: &[u8],
    conreq_bytes: &[u8],
    req_signing_modes: &[SigningMode],
    trust_store: &dyn TrustStore,
    policy: PolicyProfile,
    local_min_mode: SigningMode,
    freshness: Option<Freshness>,
) -> Result<(PqConAck, HandshakeDecision), PqHandshakeError> {
    let (spans, _trailer) = split_frame_variable(bytes, MAGIC_PQ_CONACK, "PQ CONACK")
        .map_err(|e| PqHandshakeError::SerializationError(e.to_string()))?;
    let ack = decode_pq_conack(bytes)?;

    // Transcript binding, as on the classical path: a hash over the whole transmitted CONREQ rather
    // than an echoed session id.
    let expected = crate::handshake::conreq_hash(conreq_bytes);
    if ack.conreq_hash != expected {
        return Err(PqHandshakeError::SessionIdMismatch {
            expected: expected
                .iter()
                .take(8)
                .map(|b| format!("{b:02x}"))
                .collect(),
            got: ack
                .conreq_hash
                .iter()
                .take(8)
                .map(|b| format!("{b:02x}"))
                .collect(),
        });
    }

    if !req_signing_modes.contains(&ack.selected_mode) {
        return Err(PqHandshakeError::UnauthorizedMode);
    }

    let canonical = spans.signed_prefix;

    ml_dsa_verify(&ack.pq_pubkey, canonical, &ack.pq_signature)?;

    if ack.selected_mode != SigningMode::Pq
        && !ed25519_verify(
            SigningDomain::PqConAck,
            &ack.classical_pubkey,
            canonical,
            &ack.classical_signature,
        )
    {
        return Err(PqHandshakeError::InvalidSignature);
    }

    if let Some(f) = freshness {
        f.check(ack.timestamp_ms)
            .map_err(|e| PqHandshakeError::SerializationError(e.to_string()))?;
    }

    // Bind the in-frame classical key to the stored trusted key, if any.
    if let Some(stored_key) = trust_store.pubkey_for(&ack.station_id) {
        let frame_key: [u8; 32] = ack
            .classical_pubkey
            .as_slice()
            .try_into()
            .map_err(|_| PqHandshakeError::PublicKeyMismatch)?;
        if frame_key != stored_key {
            return Err(PqHandshakeError::PublicKeyMismatch);
        }
    }

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

// ------------------------------------------------------------------
// SAR serialization helpers
// ------------------------------------------------------------------

#[cfg(test)]
mod decode_error_tests {
    use super::*;

    /// Audit H4: the decode entry points must return `SerializationError` (not panic) on malformed
    /// SAR-reassembled bytes — the module previously had no inline tests for its error branches.
    #[test]
    fn decode_rejects_malformed_bytes() {
        for bad in [&b""[..], b"not json", b"{", b"{\"x\":1}"] {
            assert!(matches!(
                decode_pq_conreq(bad),
                Err(PqHandshakeError::SerializationError(_))
            ));
            assert!(matches!(
                decode_pq_conack(bad),
                Err(PqHandshakeError::SerializationError(_))
            ));
        }
    }
}
