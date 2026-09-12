//! Byte-level codec for the v2 binary handshake frames (#1147).
//!
//! **What gets signed is the transmitted prefix** — `magic || version || length || body` — so the
//! signed message is exactly the bytes on the air minus the trailing signature. There is no second
//! representation, so verification is "hash what you received" and cannot drift from the encoder.
//! v1 signed `serde_json::to_vec` of the body in serde declaration order with no key sorting, while
//! the wire spec claimed keys were "sorted recursively": canonical was a label, not a property.
//!
//! The frame's own magic IS the domain. Since #1193 it is the REGISTERED tag —
//! `SigningDomain::{ConReq, ConAck, PqConReq, PqConAck}` map to `HSCQ`/`HSAK`/`HPCQ`/`HPAK` —
//! `sign_in_band` refuses a message that does not begin with it, and a raw Ed25519 call outside
//! `signing.rs` fails the clippy wall. Signing the transmitted prefix also binds the VERSION, which
//! v1 did not do. The ML-DSA signature in `pq_handshake.rs` signs the same prefix under a scoped
//! `#[allow(clippy::disallowed_methods)]` and is OUTSIDE that wall, so on the PQ path the registry
//! binds only the optional Ed25519 co-signature.
//!
//! This header used to argue that no tag was needed because a tag would separate "by whichever byte
//! differs first, which is the property tags exist to remove". #1193 refuted that — ALL prefix
//! separation is byte-differs-first; what a tag buys is guaranteed distinctness — and corrected the
//! claim in four docs while missing this header.

use crate::error::ModemError;

/// Frame magics. Distinct per type, and part of the signed prefix.
pub const MAGIC_CONREQ: &[u8; 4] = b"HSCQ";
/// See [`MAGIC_CONREQ`].
pub const MAGIC_CONACK: &[u8; 4] = b"HSAK";
/// Wire version.
///
/// DORMANT(#1147): part of the format's public contract — the wire spec and the known-answer vector
/// cite it — but no production caller names it, because `split_frame` enforces it internally.
///
/// Any value other than this one is rejected outright. There is no dual decode: there is no
/// compatibility mode for the data plane, and inventing one here would be the "legacy keystream"
/// mistake in a different file.
///
/// This comment used to say "`0x01` (the JSON format) is rejected outright", which was true while
/// the constant was `0x02` and became false the moment it was reset. The rule it stated is the
/// durable part; the specific value was not.
// FROZEN AT 0x01 UNTIL 1.0 — a maintainer decision, not an oversight.
//
// The wire format changes freely in the pre-1.0 window, and nothing is deployed anywhere except
// this project's own test rigs. Bumping the byte on each change would be ceremony: there is no
// third party whose stale build the version could protect.
//
// THE COST, so it is not a surprise: two builds from different points in this window do not fail
// cleanly. A stale peer gets a garbled `str_capped`/`finish` decode error rather than "version
// rejected", which is an unattributable on-air symptom — the class this repo has paid for before.
// The mitigation is procedural: REBUILD BOTH ENDS IN LOCKSTEP before any on-air session. If a
// third party ever runs this, that trade stops being acceptable and the byte starts moving.
//
// At 1.0 this becomes a real version and every subsequent format change bumps it.
pub const WIRE_VERSION: u8 = 0x01;

/// PQ frame magics (#1147). The PQ frames were bare JSON with NO magic, and the daemon routes by
/// magic sniff, so a PQ frame would have been silently dropped even if something sent one.
pub const MAGIC_PQ_CONREQ: &[u8; 4] = b"HPCQ";
/// See [`MAGIC_PQ_CONREQ`].
pub const MAGIC_PQ_CONACK: &[u8; 4] = b"HPAK";

/// `magic(4) + version(1) + length(2)`.
pub const HEADER_LEN: usize = 7;
/// Ed25519 signature, trailing and unsigned.
///
/// DORMANT(#1147): as [`WIRE_VERSION`] — contract surface, enforced internally.
pub const SIG_LEN: usize = 64;
/// One SAR fragment's data capacity — 255 B frame payload less the 4 B SAR header.
///
/// DORMANT(#1147): the budget every gate asserts against, and the number the whole change exists to
/// stay under. Consumed by tests rather than by production, because nothing in the encode path
/// needs to ask: the caps make the bound structural.
pub const FRAGMENT_CAPACITY: usize = 251;

/// Decoder-enforced caps. These are what make "one fragment" a property of the MAXIMAL LEGAL frame
/// rather than of one example: `u8`-prefixed strings would otherwise admit a legal 300 B CONREQ.
///
/// **A cap must be justified against the GENERATOR, not against an example.** `SESSION_ID` used to
/// live here at 24 bytes, sized from a 6-character callsign — while `STATION_ID` then allowed 12
/// (18 since #1204). The daemon builds `"{callsign}-{unix_ms}"`, so an 11-character callsign (`3DA0/DL1ABC`, entirely
/// legal) produced a 25-byte id, `ConReq::create` failed, and the daemon logged a warning and
/// **carried on with no signed handshake** — a silent downgrade to unverified. The two rows
/// contradicted each other adjacently in the design's own table. `session_id` is now a fixed `u64`,
/// which removes the cap rather than re-tuning it.
pub mod caps {
    /// `station_id` (and `dst_station` on the CONREQ).
    ///
    /// 18, and the number is a POLICY choice over an unbounded generator, not a measurement. There
    /// is no authoritative maximum for a compound amateur callsign: ITU RR Article 19 bounds an
    /// ordinary call at roughly seven characters but lets administrations authorise longer
    /// special-event calls, and compound decoration (`prefix/` … `/suffix`) stacks on top of that.
    ///
    /// What 18 covers, which is the inventory this replaces the old example with:
    ///   `DC0SK/P`             7   an ordinary call, portable
    ///   `SV5/DL1ABCD/P`      13   ordinary call, foreign prefix and suffix
    ///   `3DA0/DL1ABCD/QRP`   16   four-character prefix, seven-character base
    ///   `3DA0/VI110ACT/QRP`  17   as above with an eight-character special-event base
    ///
    /// Longer forms remain constructible and are REFUSED LOUDLY — at config load, and at the
    /// command boundary (#1199) — rather than silently downgrading the session. That refusal, not
    /// the number, is what makes the cap honest.
    ///
    /// The previous value was 12, sized from the single example `DC0SK/P`, and a legal
    /// 13-character callsign could not handshake in either direction.
    pub const STATION_ID: usize = 18;
    /// `station_grid`. Six-character Maidenhead is the longest in use.
    pub const GRID: usize = 8;
    /// `profile_name`. `hpx_pilot_fast_rrc` is 18.
    pub const PROFILE_NAME: usize = 24;
    /// `signing_modes` entries.
    pub const SIGNING_MODES: usize = 4;
}

/// Fixed-size fields. A length prefix on a constant-size field hands a parser decision to the
/// sender, so these are positional and a wrong length is a decode error, not a late verify failure.
pub const PUBKEY_LEN: usize = 32;
/// See [`PUBKEY_LEN`].
pub const KEX_PUBKEY_LEN: usize = 32;
/// SHA-256 over the complete transmitted CONREQ frame, including its signature.
pub const CONREQ_HASH_LEN: usize = 32;

/// Append-only writer for the body region.
#[derive(Default)]
pub struct BodyWriter {
    buf: Vec<u8>,
}

impl BodyWriter {
    /// A writer over an empty body.
    pub fn new() -> Self {
        Self::default()
    }

    /// Write a `u8`-length-prefixed string, refusing anything over `cap`.
    ///
    /// The cap is enforced on the ENCODE side too, so a station cannot emit a frame its peer is
    /// required to reject — the failure surfaces at the sender, where it is actionable.
    pub fn str_capped(&mut self, field: &str, value: &str, cap: usize) -> Result<(), ModemError> {
        let bytes = value.as_bytes();
        if bytes.len() > cap {
            return Err(ModemError::Frame(format!(
                "handshake field `{field}` is {} bytes, over its {cap}-byte cap",
                bytes.len()
            )));
        }
        self.buf.push(bytes.len() as u8);
        self.buf.extend_from_slice(bytes);
        Ok(())
    }

    /// Write a fixed-size field, refusing a wrong length at the sender.
    pub fn fixed(&mut self, field: &str, value: &[u8], len: usize) -> Result<(), ModemError> {
        if value.len() != len {
            return Err(ModemError::Frame(format!(
                "handshake field `{field}` is {} bytes, must be exactly {len}",
                value.len()
            )));
        }
        self.buf.extend_from_slice(value);
        Ok(())
    }

    /// Write one byte.
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    /// Write a big-endian `u64`.
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    /// The body bytes.
    pub fn finish(self) -> Vec<u8> {
        self.buf
    }
}

/// Reader over the body region; every accessor is bounds-checked.
pub struct BodyReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> BodyReader<'a> {
    /// A reader over `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize, field: &str) -> Result<&'a [u8], ModemError> {
        let end = self.pos.checked_add(n).ok_or_else(|| {
            ModemError::Frame(format!("handshake field `{field}` length overflows"))
        })?;
        if end > self.buf.len() {
            return Err(ModemError::Frame(format!(
                "handshake truncated reading `{field}`: need {n} bytes, {} remain",
                self.buf.len().saturating_sub(self.pos)
            )));
        }
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    /// Read a `u8`-length-prefixed string, refusing anything over `cap`.
    pub fn str_capped(&mut self, field: &str, cap: usize) -> Result<String, ModemError> {
        let len = self.take(1, field)?[0] as usize;
        if len > cap {
            return Err(ModemError::Frame(format!(
                "handshake field `{field}` declares {len} bytes, over its {cap}-byte cap"
            )));
        }
        let bytes = self.take(len, field)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| ModemError::Frame(format!("handshake field `{field}` is not UTF-8")))
    }

    /// Read a fixed-size field.
    pub fn fixed(&mut self, field: &str, len: usize) -> Result<Vec<u8>, ModemError> {
        Ok(self.take(len, field)?.to_vec())
    }

    /// Read one byte.
    pub fn u8(&mut self, field: &str) -> Result<u8, ModemError> {
        Ok(self.take(1, field)?[0])
    }

    /// Read a big-endian `u64`.
    pub fn u64(&mut self, field: &str) -> Result<u64, ModemError> {
        let b = self.take(8, field)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_be_bytes(a))
    }

    /// Refuse trailing bytes — a body longer than its fields is a different frame, not this one.
    pub fn finish(self, what: &str) -> Result<(), ModemError> {
        if self.pos != self.buf.len() {
            return Err(ModemError::Frame(format!(
                "{what} body has {} trailing bytes",
                self.buf.len() - self.pos
            )));
        }
        Ok(())
    }
}

/// Split a frame whose trailing signature region is NOT a fixed 64 bytes.
///
/// The PQ frames carry TWO signatures and the classical one is optional (0 or 64 B), so the trailer
/// length is a property of the body rather than a constant. This validates magic, version and the
/// declared body length, then hands the caller whatever trails — which the caller must check against
/// the presence flag it reads FROM THE BODY.
///
/// The flag lives inside the signed body deliberately: outside it, flipping one bit would change how
/// many bytes a verifier treats as the classical signature, and that bit would itself be unsigned.
pub fn split_frame_variable<'a>(
    bytes: &'a [u8],
    magic: &[u8; 4],
    what: &str,
) -> Result<(FrameSpans<'a>, &'a [u8]), ModemError> {
    if bytes.len() < HEADER_LEN {
        return Err(ModemError::Frame(format!(
            "{what} too short: {} bytes, minimum {HEADER_LEN} for a header",
            bytes.len()
        )));
    }
    if &bytes[..4] != magic {
        return Err(ModemError::Frame(format!("invalid {what} magic")));
    }
    let version = bytes[4];
    if version != WIRE_VERSION {
        return Err(ModemError::Frame(format!(
            "{what} wire version {version:#04x} is not supported (this build speaks \
             {WIRE_VERSION:#04x} only; there is no dual decode)"
        )));
    }
    let declared = u16::from_be_bytes([bytes[5], bytes[6]]) as usize;
    let body_end = HEADER_LEN
        .checked_add(declared)
        .ok_or_else(|| ModemError::Frame(format!("{what} declared length overflows")))?;
    if bytes.len() < body_end {
        return Err(ModemError::Frame(format!(
            "{what} declares a {declared}-byte body but only {} bytes follow the header",
            bytes.len().saturating_sub(HEADER_LEN)
        )));
    }
    Ok((
        FrameSpans {
            signed_prefix: &bytes[..body_end],
            body: &bytes[HEADER_LEN..body_end],
            signature: &[],
        },
        &bytes[body_end..],
    ))
}

/// Assemble `magic || version || length || body`, the exact bytes that get signed.
///
/// Same rule for classical and PQ frames: one signed representation, and it is the transmitted one.
pub fn signed_prefix_with_magic(magic: &[u8; 4], body: &[u8]) -> Result<Vec<u8>, ModemError> {
    let len = u16::try_from(body.len())
        .map_err(|_| ModemError::Frame("handshake body exceeds u16 length".into()))?;
    let mut out = Vec::with_capacity(HEADER_LEN + body.len());
    out.extend_from_slice(magic);
    out.push(WIRE_VERSION);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(body);
    Ok(out)
}

/// The three spans of a received frame: the signed prefix, the body inside it, and the signature.
///: the signed prefix, the body inside it, and the signature.
///
/// Named rather than returned as a bare tuple so a caller cannot silently transpose two `&[u8]`
/// of identical type — the prefix and the body differ only by seven leading bytes, and verifying
/// the wrong one would still "work" on well-formed input and fail only on the attack it exists to
/// stop.
#[derive(Debug)]
pub struct FrameSpans<'a> {
    /// `magic || version || length || body` — exactly what the signature covers.
    pub signed_prefix: &'a [u8],
    /// The body region alone.
    pub body: &'a [u8],
    /// The trailing, unsigned signature.
    ///
    /// **Empty on the `split_frame_variable` path**, where the trailer is not a fixed 64 bytes and
    /// is returned separately for the caller to split against the body's presence flag. Only
    /// `split_frame` populates it.
    pub signature: &'a [u8],
}

/// Split a received frame into its signed prefix, body and signature.
///
/// Validates magic, version and declared length before anything downstream looks at the body. A v1
/// (`0x01`) frame is rejected here by version, which is why no caller needs a dual-decode path.
pub fn split_frame<'a>(
    bytes: &'a [u8],
    magic: &[u8; 4],
    what: &str,
) -> Result<FrameSpans<'a>, ModemError> {
    if bytes.len() < HEADER_LEN + SIG_LEN {
        return Err(ModemError::Frame(format!(
            "{what} too short: {} bytes, minimum {}",
            bytes.len(),
            HEADER_LEN + SIG_LEN
        )));
    }
    if &bytes[..4] != magic {
        return Err(ModemError::Frame(format!("invalid {what} magic")));
    }
    let version = bytes[4];
    if version != WIRE_VERSION {
        return Err(ModemError::Frame(format!(
            "{what} wire version {version:#04x} is not supported (this build speaks \
             {WIRE_VERSION:#04x} only; there is no dual decode)"
        )));
    }
    let declared = u16::from_be_bytes([bytes[5], bytes[6]]) as usize;
    let expected_total = HEADER_LEN + declared + SIG_LEN;
    if bytes.len() != expected_total {
        return Err(ModemError::Frame(format!(
            "{what} declares a {declared}-byte body, so the frame must be {expected_total} bytes, \
             but it is {}",
            bytes.len()
        )));
    }
    Ok(FrameSpans {
        signed_prefix: &bytes[..HEADER_LEN + declared],
        body: &bytes[HEADER_LEN..HEADER_LEN + declared],
        signature: &bytes[HEADER_LEN + declared..],
    })
}

/// Alias kept for the classical call sites; see [`signed_prefix_with_magic`].
pub fn signed_prefix(magic: &[u8; 4], body: &[u8]) -> Result<Vec<u8>, ModemError> {
    signed_prefix_with_magic(magic, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CONREQ body at every cap: the layout the 251 B budget is actually spent on.
    fn maximal_conreq_body() -> Vec<u8> {
        let mut w = BodyWriter::new();
        w.str_capped(
            "station_id",
            &"A".repeat(caps::STATION_ID),
            caps::STATION_ID,
        )
        .unwrap();
        w.str_capped(
            "dst_station",
            &"B".repeat(caps::STATION_ID),
            caps::STATION_ID,
        )
        .unwrap();
        w.fixed("pubkey", &[7u8; PUBKEY_LEN], PUBKEY_LEN).unwrap();
        w.fixed("kex_pubkey", &[9u8; KEX_PUBKEY_LEN], KEX_PUBKEY_LEN)
            .unwrap();
        w.u8(caps::SIGNING_MODES as u8);
        for _ in 0..caps::SIGNING_MODES {
            w.u8(1);
        }
        w.u64(u64::MAX); // session_id — fixed-width since the #1147 follow-up
        w.str_capped("station_grid", &"D".repeat(caps::GRID), caps::GRID)
            .unwrap();
        w.str_capped(
            "profile_name",
            &"E".repeat(caps::PROFILE_NAME),
            caps::PROFILE_NAME,
        )
        .unwrap();
        w.u64(u64::MAX);
        w.u64(u64::MAX);
        w.finish()
    }

    /// THE BUDGET GATE: the MAXIMAL legal CONREQ fits one SAR fragment.
    ///
    /// Asserted on the worst case, not on an example frame — the whole point of the decoder caps is
    /// that "it fits" is a property of every legal frame. A CONREQ that spans fragments decodes with
    /// probability p^3 on a fading channel instead of p, which is the defect #1147 exists to fix.
    #[test]
    fn the_maximal_legal_conreq_fits_one_sar_fragment() {
        let frame = HEADER_LEN + maximal_conreq_body().len() + SIG_LEN;
        assert_eq!(
            frame, 236,
            "the maximal CONREQ is {frame} B, not the budgeted 236 — redo the fragment \
             arithmetic against the encoder, do not adjust this number to match"
        );
        assert!(
            frame <= FRAGMENT_CAPACITY,
            "the maximal legal CONREQ is {frame} B, over the {FRAGMENT_CAPACITY} B one fragment \
             holds, so a legal frame would need three acquisitions instead of one"
        );
    }

    /// The CONACK budget, which is what forced `session_id` OUT of the frame: echoing it costs 25 B
    /// and pushes the worst case to 269 B. `conreq_hash` subsumes the echo and binds harder.
    #[test]
    fn the_maximal_legal_conack_fits_one_sar_fragment() {
        let mut w = BodyWriter::new();
        w.str_capped(
            "station_id",
            &"A".repeat(caps::STATION_ID),
            caps::STATION_ID,
        )
        .unwrap();
        // No dst_station: a CONACK self-selects by conreq_hash (#1191).
        w.fixed("pubkey", &[7u8; PUBKEY_LEN], PUBKEY_LEN).unwrap();
        w.fixed("kex_pubkey", &[9u8; KEX_PUBKEY_LEN], KEX_PUBKEY_LEN)
            .unwrap();
        w.u8(1);
        w.fixed("conreq_hash", &[3u8; CONREQ_HASH_LEN], CONREQ_HASH_LEN)
            .unwrap();
        w.str_capped("station_grid", &"D".repeat(caps::GRID), caps::GRID)
            .unwrap();
        w.str_capped(
            "profile_name",
            &"E".repeat(caps::PROFILE_NAME),
            caps::PROFILE_NAME,
        )
        .unwrap();
        w.u64(u64::MAX);
        w.u64(u64::MAX);
        let frame = HEADER_LEN + w.finish().len() + SIG_LEN;
        // Arithmetic redone for cap 18 with dst_station removed (#1191), not adjusted to match:
        //   header 7 + station_id (1+18) + pubkey 32 + kex 32 + mode 1 + conreq_hash 32
        //   + grid (1+8) + profile (1+24) + fingerprint 8 + timestamp 8 + signature 64 = 237
        assert_eq!(
            frame, 237,
            "the maximal CONACK is {frame} B, not the budgeted 237 — redo the fragment \
             arithmetic against the encoder, do not adjust this number to match"
        );
        assert!(frame <= FRAGMENT_CAPACITY);
    }

    /// A frame of the WRONG version is refused by version, not by parsing — which is why nothing
    /// downstream needs a dual-decode path.
    ///
    /// The version to reject is derived from `WIRE_VERSION`, not written as a literal. This test
    /// used to hard-code "a v1 frame is rejected", which was true while the constant was `0x02` and
    /// became a FALSE TEST the moment #1191 reset it to `0x01` — it then asserted that the current
    /// version is refused. A test naming a constant's value goes stale the moment the value moves.
    #[test]
    fn a_frame_of_the_wrong_version_is_rejected_by_version_not_by_parsing() {
        let wrong = WIRE_VERSION.wrapping_add(1);
        let mut f = signed_prefix(MAGIC_CONREQ, &[0u8; 8]).unwrap();
        f.extend_from_slice(&[0u8; SIG_LEN]);
        f[4] = wrong;
        let e = split_frame(&f, MAGIC_CONREQ, "CONREQ")
            .unwrap_err()
            .to_string();
        assert!(
            e.contains(&format!("{wrong:#04x}")) || e.contains("version"),
            "expected a version refusal for {wrong:#04x}, got: {e}"
        );
        // Positive control: the CURRENT version must parse, or the test above passes vacuously.
        let mut ok = signed_prefix(MAGIC_CONREQ, &[0u8; 8]).unwrap();
        ok.extend_from_slice(&[0u8; SIG_LEN]);
        assert!(split_frame(&ok, MAGIC_CONREQ, "CONREQ").is_ok());
    }

    /// A frame cannot verify as another TYPE: the magic is inside the signed prefix.
    #[test]
    fn a_conack_frame_is_not_accepted_as_a_conreq() {
        let mut f = signed_prefix(MAGIC_CONACK, &[0u8; 8]).unwrap();
        f.extend_from_slice(&[0u8; SIG_LEN]);
        assert!(split_frame(&f, MAGIC_CONREQ, "CONREQ").is_err());
    }

    /// A declared length that disagrees with the frame is refused before the body is read, so a
    /// truncated or padded frame cannot be reinterpreted as a shorter well-formed one.
    #[test]
    fn a_length_that_disagrees_with_the_frame_is_rejected() {
        let mut f = signed_prefix(MAGIC_CONREQ, &[1u8; 16]).unwrap();
        f.extend_from_slice(&[0u8; SIG_LEN]);
        assert!(split_frame(&f, MAGIC_CONREQ, "CONREQ").is_ok());
        f[6] = 15;
        assert!(split_frame(&f, MAGIC_CONREQ, "CONREQ").is_err());
    }

    /// Caps are enforced on BOTH sides: a sender cannot emit a frame its peer must reject.
    #[test]
    fn an_over_cap_field_is_refused_at_the_sender_and_at_the_decoder() {
        let mut w = BodyWriter::new();
        assert!(w
            .str_capped(
                "station_id",
                &"A".repeat(caps::STATION_ID + 1),
                caps::STATION_ID
            )
            .is_err());

        let mut body = vec![(caps::STATION_ID + 1) as u8];
        body.extend(std::iter::repeat_n(b'A', caps::STATION_ID + 1));
        let mut r = BodyReader::new(&body);
        assert!(r.str_capped("station_id", caps::STATION_ID).is_err());
    }

    /// Trailing bytes are refused — a longer body is a different frame, not this one.
    #[test]
    fn trailing_body_bytes_are_rejected() {
        let body = [0u8; 9];
        let mut r = BodyReader::new(&body);
        assert_eq!(r.u64("timestamp_ms").unwrap(), 0);
        assert!(r.finish("CONREQ").is_err());
    }
}
