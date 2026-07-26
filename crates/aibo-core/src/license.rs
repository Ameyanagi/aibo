//! Offline licence verification (§19).
//!
//! The licence is a single opaque string the user pastes in:
//!
//! ```text
//! license = base64( ed25519_sig(64) || cbor({ v, id, kid, tier, email,
//!                                             seats, issued_at,
//!                                             expires_at?, max_version?,
//!                                             edition?, product?, features? }) )
//! ```
//!
//! Three properties of §19 are load-bearing and are the reason this module
//! carries its own CBOR codec instead of pulling in a general one:
//!
//! 1. **Strict, canonical CBOR.** A permissive decoder is a forgery surface.
//!    Definite lengths only, shortest-form integer heads only, map keys in
//!    canonical (length-then-bytewise) order, no duplicates, no unknown keys,
//!    absent optionals *omitted* rather than encoded as `null`, and not one
//!    trailing byte after the top-level map. Exactly one byte sequence encodes
//!    any given [`License`], and [`License::to_canonical_cbor`] produces it.
//! 2. **Signature before parse.** The signature is checked against every
//!    embedded key *before* the payload is decoded, so the decoder never runs
//!    on unauthenticated bytes. The key id inside the payload is then required
//!    to match the key that actually verified — it is covered by the signature,
//!    so it cannot be swapped to point at a different key.
//! 3. **Grace only after a previously valid licence.** A signature or schema
//!    failure returns `Err` and never so much as looks at [`LicenseState`];
//!    if a validation failure could open the 14-day offline grace, the grace
//!    period *is* the crack.
//!
//! Backwards clock movement is tolerated within a bound by evaluating expiry
//! against a stored high-water mark rather than the raw wall clock, so winding
//! the machine's clock back cannot resurrect an expired licence.
//!
//! Per §19 this is friction, not DRM: a closed-source Rust binary can be
//! patched and offline seat limits are contractual, not enforceable.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// Schema version this build understands (the `v` field).
pub const SCHEMA_VERSION: u64 = 1;

/// Offline grace after expiry, in seconds — §19's 14 days.
pub const OFFLINE_GRACE_SECS: u64 = 14 * 24 * 60 * 60;

/// How far the wall clock may run *behind* the stored high-water mark before
/// it is reported as a rollback. NTP corrections and timezone/DST bugs live
/// well inside a day; deliberate tampering does not.
pub const MAX_BACKWARD_CLOCK_SKEW_SECS: u64 = 24 * 60 * 60;

/// Upper bound on a decoded licence blob. A licence is a few hundred bytes;
/// anything larger is not a licence and should not be parsed.
pub const MAX_BLOB_BYTES: usize = 8 * 1024;

const SIGNATURE_LEN: usize = 64;

// ---------------------------------------------------------------------------
// Embedded trust anchors
// ---------------------------------------------------------------------------

/// One trusted signing key: its rotation id and its raw Ed25519 public key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SigningKeyEntry {
    /// Rotation id. Appears in the signed payload as `kid`.
    pub id: u32,
    /// Raw 32-byte Ed25519 public key.
    pub public_key: [u8; 32],
}

/// The keys this build trusts.
///
/// **Placeholder.** The entry below was generated locally and its private half
/// was destroyed without ever being stored, so no licence can be signed for it
/// and none will verify. That is deliberate: a checked-in key derived from a
/// known seed would be a forging oracle. Replace this with the real release
/// public key when the signing key is created (§19, P8), and *add* rather than
/// replace when rotating, so licences issued under the old key keep working.
pub const EMBEDDED_KEYS: &[SigningKeyEntry] = &[SigningKeyEntry {
    id: 1,
    public_key: [
        0x81, 0x78, 0xd2, 0x00, 0xfd, 0xfd, 0xea, 0x60, 0xda, 0xe5, 0x04, 0x17, 0x59, 0x03, 0x91,
        0x6c, 0x60, 0x3b, 0x4b, 0x52, 0x30, 0x8d, 0x38, 0x19, 0x85, 0x27, 0x13, 0xc8, 0xe2, 0x21,
        0x5a, 0x76,
    ],
}];

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a licence blob was rejected.
///
/// Every variant is a *refusal*: none of them grants grace, and none of them
/// mutates the stored [`LicenseState`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LicenseError {
    /// The outer base64 was not strict, canonically padded standard base64.
    #[error("licence is not valid base64")]
    Base64,
    /// The blob is larger than [`MAX_BLOB_BYTES`].
    #[error("licence blob is too large ({0} bytes)")]
    TooLarge(usize),
    /// The blob ended before the structure it announced was complete.
    #[error("licence blob is truncated")]
    Truncated,
    /// Bytes remain after the top-level CBOR map.
    #[error("licence blob has trailing bytes")]
    TrailingBytes,
    /// The CBOR is decodable in some permissive reading but is not the one
    /// canonical encoding: an indefinite length, a non-shortest integer head,
    /// out-of-order or duplicated map keys, an unknown key, or an optional
    /// field written as `null` instead of being omitted.
    #[error("licence CBOR is not canonical: {0}")]
    NonCanonicalCbor(&'static str),
    /// The payload decoded but does not satisfy the schema.
    #[error("licence schema violation: {0}")]
    Schema(&'static str),
    /// The `v` field names a schema this build does not implement.
    #[error("licence schema version {0} is not supported")]
    UnsupportedVersion(u64),
    /// No embedded key verifies the signature.
    #[error("licence signature is not valid")]
    BadSignature,
    /// The signature verified, but the payload's `kid` names a different key
    /// than the one that verified it.
    #[error("licence key id {claimed} does not match the signing key {signed_by}")]
    WrongKeyId {
        /// The `kid` in the payload.
        claimed: u32,
        /// The id of the key whose signature actually verified.
        signed_by: u32,
    },
}

// ---------------------------------------------------------------------------
// The licence payload
// ---------------------------------------------------------------------------

/// The tier a licence grants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// Single user.
    Personal,
    /// Multiple seats. Per §19 the seat count is contractual, not enforced.
    Team,
}

impl Tier {
    /// The wire spelling used in the CBOR payload.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Tier::Personal => "personal",
            Tier::Team => "team",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "personal" => Some(Tier::Personal),
            "team" => Some(Tier::Team),
            _ => None,
        }
    }
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The signed contents of a licence.
///
/// §19 asks for product/edition/feature fields "so tiers don't need a schema
/// change" — they are here from v1, optional, and omitted when unset so the
/// canonical encoding stays bijective.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct License {
    /// Schema version. Always [`SCHEMA_VERSION`] for this build.
    pub v: u64,
    /// Licence id — also the grace anchor, so grace earned by one licence
    /// cannot be spent by another.
    pub id: Uuid,
    /// Id of the signing key, for rotation.
    pub kid: u32,
    /// Which tier.
    pub tier: Tier,
    /// Purchaser email, shown in the about screen.
    pub email: String,
    /// Seat count. Contractual only (§19).
    pub seats: u8,
    /// Issue time, Unix seconds.
    pub issued_at: u64,
    /// Expiry, Unix seconds. `None` is the one-time-purchase model.
    pub expires_at: Option<u64>,
    /// Highest product version this licence unlocks, `"major.minor.patch"`.
    /// `None` means no cutoff.
    pub max_version: Option<String>,
    /// Optional edition discriminator, for tiers added after v1.
    pub edition: Option<String>,
    /// Optional product discriminator, if this key ever signs for more than
    /// one product.
    pub product: Option<String>,
    /// Optional feature flags, for tiers added after v1. Omitted when empty.
    pub features: Vec<String>,
}

impl License {
    /// Whether this licence unlocks `version` (`"major.minor.patch"`).
    ///
    /// Conservative by construction: a `max_version` or `version` that does not
    /// parse as a numeric triple denies rather than allows. Pre-release and
    /// build suffixes are ignored, so `1.2.0-rc1` is treated as `1.2.0`.
    #[must_use]
    pub fn permits_version(&self, version: &str) -> bool {
        let Some(max) = self.max_version.as_deref() else {
            return true;
        };
        match (parse_version(max), parse_version(version)) {
            (Some(max), Some(v)) => v <= max,
            _ => false,
        }
    }

    /// Encode this licence as the one canonical CBOR byte string.
    ///
    /// This is the exact byte sequence the signature is computed over, and the
    /// only one [`verify`] accepts.
    #[must_use]
    pub fn to_canonical_cbor(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(160);

        let mut n = 7; // v, id, kid, tier, email, seats, issued_at
        n += usize::from(self.edition.is_some());
        n += usize::from(self.product.is_some());
        n += usize::from(!self.features.is_empty());
        n += usize::from(self.expires_at.is_some());
        n += usize::from(self.max_version.is_some());
        put_head(&mut out, MAJOR_MAP, n as u64);

        // Canonical map order: keys sorted by encoded length, then bytewise.
        put_text(&mut out, "v");
        put_head(&mut out, MAJOR_UINT, self.v);
        put_text(&mut out, "id");
        put_bytes(&mut out, self.id.as_bytes());
        put_text(&mut out, "kid");
        put_head(&mut out, MAJOR_UINT, u64::from(self.kid));
        put_text(&mut out, "tier");
        put_text(&mut out, self.tier.as_str());
        put_text(&mut out, "email");
        put_text(&mut out, &self.email);
        put_text(&mut out, "seats");
        put_head(&mut out, MAJOR_UINT, u64::from(self.seats));
        if let Some(edition) = &self.edition {
            put_text(&mut out, "edition");
            put_text(&mut out, edition);
        }
        if let Some(product) = &self.product {
            put_text(&mut out, "product");
            put_text(&mut out, product);
        }
        if !self.features.is_empty() {
            put_text(&mut out, "features");
            put_head(&mut out, MAJOR_ARRAY, self.features.len() as u64);
            for f in &self.features {
                put_text(&mut out, f);
            }
        }
        put_text(&mut out, "issued_at");
        put_head(&mut out, MAJOR_UINT, self.issued_at);
        if let Some(expires_at) = self.expires_at {
            put_text(&mut out, "expires_at");
            put_head(&mut out, MAJOR_UINT, expires_at);
        }
        if let Some(max_version) = &self.max_version {
            put_text(&mut out, "max_version");
            put_text(&mut out, max_version);
        }
        out
    }

    /// Decode a canonical CBOR payload.
    ///
    /// Callers should not need this — [`verify`] checks the signature first.
    /// It is public so the licence-issuing tool can round-trip what it signed.
    ///
    /// # Errors
    ///
    /// Returns [`LicenseError`] if `payload` is not the canonical encoding of a
    /// schema-valid licence.
    pub fn from_canonical_cbor(payload: &[u8]) -> Result<Self, LicenseError> {
        let mut cur = Cursor::new(payload);
        let license = decode_license(&mut cur)?;
        if !cur.is_empty() {
            return Err(LicenseError::TrailingBytes);
        }
        Ok(license)
    }
}

fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let core = s.split(['-', '+']).next()?;
    let mut it = core.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next().unwrap_or("0").parse().ok()?;
    let patch = it.next().unwrap_or("0").parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

// ---------------------------------------------------------------------------
// Persisted state
// ---------------------------------------------------------------------------

/// The small amount of state licence checking has to remember across runs.
///
/// It is only ever advanced by a licence whose signature already verified, so
/// it cannot be seeded by a forgery. Losing it is safe: it costs the user the
/// offline grace window, nothing more.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LicenseState {
    /// Highest wall-clock time ever observed, Unix seconds. Expiry is
    /// evaluated against `max(now, this)`, so winding the clock back does not
    /// un-expire a licence.
    pub clock_high_water: u64,
    /// When a licence was last seen *inside its term*, Unix seconds. Grace is
    /// only available once this is set.
    pub last_valid_at: Option<u64>,
    /// Which licence earned that. Grace is not transferable between licences.
    pub last_valid_id: Option<Uuid>,
}

/// The verdict for a licence that verified cryptographically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LicenseStatus {
    /// Inside its term (or perpetual).
    Valid,
    /// Past `expires_at`, but this licence was previously seen valid on this
    /// machine and the 14-day offline grace has not run out.
    Grace {
        /// When grace runs out, Unix seconds.
        until: u64,
    },
    /// Past `expires_at` with no grace left, or with no grace ever earned.
    Expired {
        /// The `expires_at` that has passed, Unix seconds.
        expired_at: u64,
    },
}

impl LicenseStatus {
    /// Whether the product should run.
    #[must_use]
    pub const fn is_entitled(self) -> bool {
        matches!(self, LicenseStatus::Valid | LicenseStatus::Grace { .. })
    }
}

/// A licence that passed signature and schema checks, plus its time verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// The signed contents.
    pub license: License,
    /// Whether it is in term, in grace, or expired.
    pub status: LicenseStatus,
    /// The wall clock is behind the stored high-water mark by more than
    /// [`MAX_BACKWARD_CLOCK_SKEW_SECS`]. Expiry was evaluated against the
    /// high-water mark; surface this so the user is told why (§13/§17) rather
    /// than left guessing.
    pub clock_rolled_back: bool,
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

/// Verify a licence blob against the [`EMBEDDED_KEYS`].
///
/// `now` is Unix seconds. `state` is advanced *only* on success, and only
/// forwards.
///
/// # Errors
///
/// Returns [`LicenseError`] if the blob is malformed, non-canonical, schema
/// invalid, or not signed by a trusted key. In every one of those cases
/// `state` is left untouched and no grace is granted.
pub fn verify(blob: &str, now: u64, state: &mut LicenseState) -> Result<Verified, LicenseError> {
    verify_with(blob, EMBEDDED_KEYS, now, state)
}

/// [`verify`] against a caller-supplied keyring.
///
/// # Errors
///
/// As [`verify`].
pub fn verify_with(
    blob: &str,
    keys: &[SigningKeyEntry],
    now: u64,
    state: &mut LicenseState,
) -> Result<Verified, LicenseError> {
    // Bound the work before doing any of it.
    if blob.len() > MAX_BLOB_BYTES * 2 {
        return Err(LicenseError::TooLarge(blob.len()));
    }
    let raw = BASE64
        .decode(blob.trim())
        .map_err(|_| LicenseError::Base64)?;
    if raw.len() > MAX_BLOB_BYTES {
        return Err(LicenseError::TooLarge(raw.len()));
    }
    if raw.len() <= SIGNATURE_LEN {
        return Err(LicenseError::Truncated);
    }
    let (sig_bytes, payload) = raw.split_at(SIGNATURE_LEN);
    let sig_bytes: [u8; SIGNATURE_LEN] = sig_bytes.try_into().expect("split at 64");
    let signature = Signature::from_bytes(&sig_bytes);

    // Signature first: the CBOR decoder must never run on unauthenticated
    // bytes. `verify_strict` rejects small-order public keys and the
    // malleability that plain `verify` tolerates.
    let signer = keys
        .iter()
        .find(|entry| {
            VerifyingKey::from_bytes(&entry.public_key)
                .is_ok_and(|vk| vk.verify_strict(payload, &signature).is_ok())
        })
        .ok_or(LicenseError::BadSignature)?;

    let license = License::from_canonical_cbor(payload)?;

    // `kid` is inside the signed payload, so this cannot be forged — it is a
    // rotation cross-check, not a trust decision.
    if license.kid != signer.id {
        return Err(LicenseError::WrongKeyId {
            claimed: license.kid,
            signed_by: signer.id,
        });
    }

    // Only now, with an authentic licence in hand, is stored state consulted.
    let clock_rolled_back =
        state.clock_high_water.saturating_sub(now) > MAX_BACKWARD_CLOCK_SKEW_SECS;
    let effective_now = now.max(state.clock_high_water);
    let status = evaluate(&license, effective_now, state);

    state.clock_high_water = effective_now;
    if status == LicenseStatus::Valid {
        // Deliberately not updated during grace: grace that renews itself is
        // not a grace period.
        state.last_valid_at = Some(effective_now);
        state.last_valid_id = Some(license.id);
    }

    Ok(Verified {
        license,
        status,
        clock_rolled_back,
    })
}

fn evaluate(license: &License, effective_now: u64, state: &LicenseState) -> LicenseStatus {
    let Some(expires_at) = license.expires_at else {
        return LicenseStatus::Valid;
    };
    if effective_now <= expires_at {
        return LicenseStatus::Valid;
    }
    let earned_grace = state.last_valid_at.is_some() && state.last_valid_id == Some(license.id);
    if earned_grace {
        let until = expires_at.saturating_add(OFFLINE_GRACE_SECS);
        if effective_now <= until {
            return LicenseStatus::Grace { until };
        }
    }
    LicenseStatus::Expired {
        expired_at: expires_at,
    }
}

/// Assemble a licence blob from a canonical payload and its signature.
///
/// The signing half lives in the issuing tool, never in the product: this
/// crate holds public keys only.
#[must_use]
pub fn encode_blob(payload: &[u8], signature: &[u8; SIGNATURE_LEN]) -> String {
    let mut raw = Vec::with_capacity(SIGNATURE_LEN + payload.len());
    raw.extend_from_slice(signature);
    raw.extend_from_slice(payload);
    BASE64.encode(raw)
}

// ---------------------------------------------------------------------------
// Canonical CBOR encoder
// ---------------------------------------------------------------------------

const MAJOR_UINT: u8 = 0;
const MAJOR_BYTES: u8 = 2;
const MAJOR_TEXT: u8 = 3;
const MAJOR_ARRAY: u8 = 4;
const MAJOR_MAP: u8 = 5;

fn put_head(out: &mut Vec<u8>, major: u8, arg: u64) {
    let m = major << 5;
    match arg {
        0..=23 => out.push(m | arg as u8),
        24..=0xff => {
            out.push(m | 24);
            out.push(arg as u8);
        }
        0x100..=0xffff => {
            out.push(m | 25);
            out.extend_from_slice(&(arg as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(m | 26);
            out.extend_from_slice(&(arg as u32).to_be_bytes());
        }
        _ => {
            out.push(m | 27);
            out.extend_from_slice(&arg.to_be_bytes());
        }
    }
}

fn put_text(out: &mut Vec<u8>, s: &str) {
    put_head(out, MAJOR_TEXT, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_head(out, MAJOR_BYTES, b.len() as u64);
    out.extend_from_slice(b);
}

// ---------------------------------------------------------------------------
// Strict CBOR decoder
// ---------------------------------------------------------------------------

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], LicenseError> {
        let end = self.pos.checked_add(n).ok_or(LicenseError::Truncated)?;
        let slice = self.buf.get(self.pos..end).ok_or(LicenseError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    /// Read one CBOR head, rejecting every non-canonical spelling of it:
    /// indefinite lengths, the reserved additional-information values, and any
    /// argument that could have been written in fewer bytes.
    fn head(&mut self) -> Result<(u8, u64), LicenseError> {
        let b = *self.take(1)?.first().expect("one byte");
        let major = b >> 5;
        let ai = b & 0x1f;
        let arg = match ai {
            0..=23 => u64::from(ai),
            24 => {
                let v = u64::from(*self.take(1)?.first().expect("one byte"));
                if v < 24 {
                    return Err(LicenseError::NonCanonicalCbor("integer not shortest form"));
                }
                v
            }
            25 => {
                let raw: [u8; 2] = self.take(2)?.try_into().expect("two bytes");
                let v = u64::from(u16::from_be_bytes(raw));
                if v <= 0xff {
                    return Err(LicenseError::NonCanonicalCbor("integer not shortest form"));
                }
                v
            }
            26 => {
                let raw: [u8; 4] = self.take(4)?.try_into().expect("four bytes");
                let v = u64::from(u32::from_be_bytes(raw));
                if v <= 0xffff {
                    return Err(LicenseError::NonCanonicalCbor("integer not shortest form"));
                }
                v
            }
            27 => {
                let raw: [u8; 8] = self.take(8)?.try_into().expect("eight bytes");
                let v = u64::from_be_bytes(raw);
                if v <= 0xffff_ffff {
                    return Err(LicenseError::NonCanonicalCbor("integer not shortest form"));
                }
                v
            }
            31 => return Err(LicenseError::NonCanonicalCbor("indefinite length")),
            _ => return Err(LicenseError::NonCanonicalCbor("reserved head")),
        };
        Ok((major, arg))
    }

    fn expect(&mut self, major: u8, what: &'static str) -> Result<u64, LicenseError> {
        let (m, arg) = self.head()?;
        if m != major {
            return Err(LicenseError::Schema(what));
        }
        Ok(arg)
    }

    fn uint(&mut self, what: &'static str) -> Result<u64, LicenseError> {
        self.expect(MAJOR_UINT, what)
    }

    fn text(&mut self, what: &'static str) -> Result<&'a str, LicenseError> {
        let len = self.expect(MAJOR_TEXT, what)?;
        let len = usize::try_from(len).map_err(|_| LicenseError::Truncated)?;
        let raw = self.take(len)?;
        std::str::from_utf8(raw).map_err(|_| LicenseError::Schema("text is not UTF-8"))
    }
}

/// Map keys in canonical CBOR order: by encoded length first, then bytewise.
/// The decoder walks this list monotonically, which rejects out-of-order keys,
/// duplicate keys and unknown keys with one comparison.
const KEYS: [&str; 12] = [
    "v",
    "id",
    "kid",
    "tier",
    "email",
    "seats",
    "edition",
    "product",
    "features",
    "issued_at",
    "expires_at",
    "max_version",
];

fn decode_license(cur: &mut Cursor<'_>) -> Result<License, LicenseError> {
    let entries = cur.expect(MAJOR_MAP, "top level is not a map")?;
    let entries = usize::try_from(entries).map_err(|_| LicenseError::Truncated)?;
    if entries > KEYS.len() {
        return Err(LicenseError::NonCanonicalCbor("too many map entries"));
    }

    let mut v = None;
    let mut id = None;
    let mut kid = None;
    let mut tier = None;
    let mut email = None;
    let mut seats = None;
    let mut edition = None;
    let mut product = None;
    let mut features: Vec<String> = Vec::new();
    let mut issued_at = None;
    let mut expires_at = None;
    let mut max_version = None;

    // `next` is the lowest key rank still acceptable; it only ever increases.
    let mut next = 0usize;
    for _ in 0..entries {
        let key = cur.text("map key is not a text string")?;
        let rank = KEYS
            .iter()
            .position(|k| *k == key)
            .ok_or(LicenseError::NonCanonicalCbor("unknown map key"))?;
        if rank < next {
            return Err(LicenseError::NonCanonicalCbor(
                "map keys out of canonical order or duplicated",
            ));
        }
        next = rank + 1;

        match rank {
            0 => v = Some(cur.uint("v")?),
            1 => {
                let len = cur.expect(MAJOR_BYTES, "id")?;
                if len != 16 {
                    return Err(LicenseError::Schema("id is not 16 bytes"));
                }
                let raw: [u8; 16] = cur.take(16)?.try_into().expect("16 bytes");
                id = Some(Uuid::from_bytes(raw));
            }
            2 => {
                let raw = cur.uint("kid")?;
                kid = Some(u32::try_from(raw).map_err(|_| LicenseError::Schema("kid too large"))?);
            }
            3 => {
                tier = Some(
                    Tier::parse(cur.text("tier")?).ok_or(LicenseError::Schema("unknown tier"))?,
                );
            }
            4 => {
                let s = cur.text("email")?;
                if s.is_empty() || s.len() > 254 {
                    return Err(LicenseError::Schema("email length out of range"));
                }
                email = Some(s.to_owned());
            }
            5 => {
                let raw = cur.uint("seats")?;
                let n =
                    u8::try_from(raw).map_err(|_| LicenseError::Schema("seats out of range"))?;
                if n == 0 {
                    return Err(LicenseError::Schema("seats out of range"));
                }
                seats = Some(n);
            }
            6 => edition = Some(bounded(cur.text("edition")?, 64, "edition")?),
            7 => product = Some(bounded(cur.text("product")?, 64, "product")?),
            8 => {
                let n = cur.expect(MAJOR_ARRAY, "features")?;
                let n = usize::try_from(n).map_err(|_| LicenseError::Truncated)?;
                if n == 0 {
                    return Err(LicenseError::NonCanonicalCbor(
                        "empty features must be omitted",
                    ));
                }
                if n > 32 {
                    return Err(LicenseError::Schema("too many features"));
                }
                for _ in 0..n {
                    features.push(bounded(cur.text("feature")?, 64, "feature")?);
                }
            }
            9 => issued_at = Some(cur.uint("issued_at")?),
            10 => expires_at = Some(cur.uint("expires_at")?),
            11 => max_version = Some(bounded(cur.text("max_version")?, 32, "max_version")?),
            _ => unreachable!("rank is an index into KEYS"),
        }
    }

    let v = v.ok_or(LicenseError::Schema("missing v"))?;
    if v != SCHEMA_VERSION {
        return Err(LicenseError::UnsupportedVersion(v));
    }
    let license = License {
        v,
        id: id.ok_or(LicenseError::Schema("missing id"))?,
        kid: kid.ok_or(LicenseError::Schema("missing kid"))?,
        tier: tier.ok_or(LicenseError::Schema("missing tier"))?,
        email: email.ok_or(LicenseError::Schema("missing email"))?,
        seats: seats.ok_or(LicenseError::Schema("missing seats"))?,
        issued_at: issued_at.ok_or(LicenseError::Schema("missing issued_at"))?,
        expires_at,
        max_version,
        edition,
        product,
        features,
    };
    if let Some(expires_at) = license.expires_at
        && expires_at < license.issued_at
    {
        return Err(LicenseError::Schema("expires_at precedes issued_at"));
    }
    if license.tier == Tier::Personal && license.seats != 1 {
        return Err(LicenseError::Schema("personal tier is single seat"));
    }
    Ok(license)
}

fn bounded(s: &str, max: usize, what: &'static str) -> Result<String, LicenseError> {
    if s.is_empty() || s.len() > max {
        return Err(LicenseError::Schema(what));
    }
    Ok(s.to_owned())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    const DAY: u64 = 24 * 60 * 60;
    const ISSUED: u64 = 1_700_000_000;
    const EXPIRES: u64 = ISSUED + 365 * DAY;

    /// A throwaway keypair, derived in-test from a fixed seed. This is not a
    /// real signing key and never signs a real licence — the shipped trust
    /// anchor is [`EMBEDDED_KEYS`], whose private half does not exist.
    fn test_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn keyring(id: u32, key: &SigningKey) -> Vec<SigningKeyEntry> {
        vec![SigningKeyEntry {
            id,
            public_key: key.verifying_key().to_bytes(),
        }]
    }

    fn sample(expires_at: Option<u64>) -> License {
        License {
            v: 1,
            id: Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef),
            kid: 1,
            tier: Tier::Team,
            email: "user@example.com".to_owned(),
            seats: 5,
            issued_at: ISSUED,
            expires_at,
            max_version: Some("2.0.0".to_owned()),
            edition: None,
            product: Some("aibo".to_owned()),
            features: vec!["autocomplete".to_owned()],
        }
    }

    fn sign_payload(key: &SigningKey, payload: &[u8]) -> String {
        encode_blob(payload, &key.sign(payload).to_bytes())
    }

    fn blob(key: &SigningKey, license: &License) -> String {
        sign_payload(key, &license.to_canonical_cbor())
    }

    /// State for a licence that has previously been seen inside its term.
    fn earned_grace(license: &License, at: u64) -> LicenseState {
        LicenseState {
            clock_high_water: at,
            last_valid_at: Some(at),
            last_valid_id: Some(license.id),
        }
    }

    // --- happy path --------------------------------------------------------

    #[test]
    fn valid_licence_verifies_and_records_state() {
        let key = test_key(1);
        let license = sample(Some(EXPIRES));
        let mut state = LicenseState::default();

        let now = ISSUED + DAY;
        let out = verify_with(&blob(&key, &license), &keyring(1, &key), now, &mut state).unwrap();

        assert_eq!(out.status, LicenseStatus::Valid);
        assert!(out.status.is_entitled());
        assert!(!out.clock_rolled_back);
        assert_eq!(out.license, license);
        assert_eq!(state.last_valid_at, Some(now));
        assert_eq!(state.last_valid_id, Some(license.id));
        assert_eq!(state.clock_high_water, now);
    }

    #[test]
    fn perpetual_licence_never_expires() {
        let key = test_key(1);
        let license = sample(None);
        let mut state = LicenseState::default();
        let out = verify_with(
            &blob(&key, &license),
            &keyring(1, &key),
            ISSUED + 100 * 365 * DAY,
            &mut state,
        )
        .unwrap();
        assert_eq!(out.status, LicenseStatus::Valid);
    }

    #[test]
    fn cbor_round_trips_and_is_bijective() {
        for expires_at in [None, Some(EXPIRES)] {
            let license = sample(expires_at);
            let bytes = license.to_canonical_cbor();
            let back = License::from_canonical_cbor(&bytes).unwrap();
            assert_eq!(back, license);
            assert_eq!(back.to_canonical_cbor(), bytes);
        }
    }

    #[test]
    fn max_version_gates_updates() {
        let license = sample(Some(EXPIRES));
        assert!(license.permits_version("1.9.3"));
        assert!(license.permits_version("2.0.0"));
        assert!(license.permits_version("2.0.0-rc1"));
        assert!(!license.permits_version("2.0.1"));
        assert!(!license.permits_version("3.0.0"));
        assert!(!license.permits_version("garbage"));

        let mut unlimited = license;
        unlimited.max_version = None;
        assert!(unlimited.permits_version("99.0.0"));
    }

    // --- grace -------------------------------------------------------------

    #[test]
    fn expired_within_grace_is_still_entitled() {
        let key = test_key(1);
        let license = sample(Some(EXPIRES));
        let mut state = earned_grace(&license, EXPIRES - DAY);

        let now = EXPIRES + 3 * DAY;
        let out = verify_with(&blob(&key, &license), &keyring(1, &key), now, &mut state).unwrap();

        assert_eq!(
            out.status,
            LicenseStatus::Grace {
                until: EXPIRES + OFFLINE_GRACE_SECS
            }
        );
        assert!(out.status.is_entitled());
        // Grace must not renew itself.
        assert_eq!(state.last_valid_at, Some(EXPIRES - DAY));
    }

    #[test]
    fn expired_beyond_grace_is_not_entitled() {
        let key = test_key(1);
        let license = sample(Some(EXPIRES));
        let mut state = earned_grace(&license, EXPIRES - DAY);

        let now = EXPIRES + OFFLINE_GRACE_SECS + 1;
        let out = verify_with(&blob(&key, &license), &keyring(1, &key), now, &mut state).unwrap();

        assert_eq!(
            out.status,
            LicenseStatus::Expired {
                expired_at: EXPIRES
            }
        );
        assert!(!out.status.is_entitled());
    }

    #[test]
    fn expired_licence_never_seen_valid_gets_no_grace() {
        let key = test_key(1);
        let license = sample(Some(EXPIRES));
        let mut state = LicenseState::default();

        let out = verify_with(
            &blob(&key, &license),
            &keyring(1, &key),
            EXPIRES + DAY,
            &mut state,
        )
        .unwrap();

        assert_eq!(
            out.status,
            LicenseStatus::Expired {
                expired_at: EXPIRES
            }
        );
    }

    #[test]
    fn grace_is_not_transferable_between_licences() {
        let key = test_key(1);
        let mut other = sample(Some(EXPIRES));
        other.id = Uuid::from_u128(9);
        let license = sample(Some(EXPIRES));
        // Grace was earned by a *different* licence id.
        let mut state = earned_grace(&other, EXPIRES - DAY);

        let out = verify_with(
            &blob(&key, &license),
            &keyring(1, &key),
            EXPIRES + DAY,
            &mut state,
        )
        .unwrap();

        assert_eq!(
            out.status,
            LicenseStatus::Expired {
                expired_at: EXPIRES
            }
        );
    }

    // --- the crack that must stay closed ----------------------------------

    #[test]
    fn bad_signature_is_rejected_and_never_grants_grace() {
        let key = test_key(1);
        let attacker = test_key(2);
        let license = sample(Some(EXPIRES));
        // The machine has a rich history of valid checks: if a signature
        // failure could fall through to grace, this is where it would.
        let good = earned_grace(&license, EXPIRES - DAY);

        // (a) signed by a key we do not trust
        let mut state = good.clone();
        assert_eq!(
            verify_with(
                &blob(&attacker, &license),
                &keyring(1, &key),
                EXPIRES + DAY,
                &mut state
            ),
            Err(LicenseError::BadSignature)
        );
        assert_eq!(state, good, "state must not move on a signature failure");

        // (b) authentic signature, payload edited afterwards
        let mut raw = BASE64.decode(blob(&key, &license)).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0x01;
        let mut state = good.clone();
        assert_eq!(
            verify_with(
                &BASE64.encode(&raw),
                &keyring(1, &key),
                EXPIRES + DAY,
                &mut state
            ),
            Err(LicenseError::BadSignature)
        );
        assert_eq!(state, good);

        // (c) signature bits flipped
        let mut raw = BASE64.decode(blob(&key, &license)).unwrap();
        raw[0] ^= 0x80;
        let mut state = good.clone();
        assert!(matches!(
            verify_with(
                &BASE64.encode(&raw),
                &keyring(1, &key),
                EXPIRES + DAY,
                &mut state
            ),
            Err(LicenseError::BadSignature)
        ));
        assert_eq!(state, good);
    }

    #[test]
    fn schema_failure_never_grants_grace() {
        let key = test_key(1);
        let license = sample(Some(EXPIRES));
        let good = earned_grace(&license, EXPIRES - DAY);

        // A properly signed payload that is not a schema-valid licence.
        let mut payload = license.to_canonical_cbor();
        payload.truncate(payload.len() - 4);
        let mut state = good.clone();
        let err = verify_with(
            &sign_payload(&key, &payload),
            &keyring(1, &key),
            EXPIRES + DAY,
            &mut state,
        )
        .unwrap_err();
        assert_eq!(err, LicenseError::Truncated);
        assert_eq!(state, good, "state must not move on a schema failure");
    }

    // --- decoder strictness ------------------------------------------------

    #[test]
    fn truncated_cbor_is_rejected() {
        let key = test_key(1);
        let license = sample(Some(EXPIRES));
        let full = license.to_canonical_cbor();
        // Every prefix must be refused, never partially accepted.
        for cut in 1..full.len() {
            let mut state = LicenseState::default();
            let err = verify_with(
                &sign_payload(&key, &full[..cut]),
                &keyring(1, &key),
                ISSUED + DAY,
                &mut state,
            )
            .unwrap_err();
            assert!(
                matches!(
                    err,
                    LicenseError::Truncated
                        | LicenseError::Schema(_)
                        | LicenseError::NonCanonicalCbor(_)
                        | LicenseError::UnsupportedVersion(_)
                ),
                "prefix of {cut} bytes produced {err:?}"
            );
            assert_eq!(state, LicenseState::default());
        }
    }

    #[test]
    fn empty_and_signature_only_blobs_are_rejected() {
        let key = test_key(1);
        let mut state = LicenseState::default();
        assert_eq!(
            verify_with("", &keyring(1, &key), ISSUED, &mut state),
            Err(LicenseError::Truncated)
        );
        assert_eq!(
            verify_with(
                &BASE64.encode([0u8; 64]),
                &keyring(1, &key),
                ISSUED,
                &mut state
            ),
            Err(LicenseError::Truncated)
        );
        assert_eq!(
            verify_with("not base64!!", &keyring(1, &key), ISSUED, &mut state),
            Err(LicenseError::Base64)
        );
    }

    #[test]
    fn trailing_bytes_after_the_map_are_rejected() {
        let key = test_key(1);
        let license = sample(Some(EXPIRES));
        let mut payload = license.to_canonical_cbor();
        payload.push(0xf6); // a well-formed CBOR `null`, correctly signed
        let mut state = LicenseState::default();
        assert_eq!(
            verify_with(
                &sign_payload(&key, &payload),
                &keyring(1, &key),
                ISSUED + DAY,
                &mut state
            ),
            Err(LicenseError::TrailingBytes)
        );
        assert_eq!(state, LicenseState::default());
    }

    // --- non-canonical CBOR ------------------------------------------------
    //
    // These build *complete* maps and deviate from canonical in exactly one
    // way, so a rejection cannot be credited to an unrelated defect such as a
    // missing required field. Each case asserts the specific error, and the
    // helpers are checked against `to_canonical_cbor` first.

    fn head(major: u8, arg: u64) -> Vec<u8> {
        let mut v = Vec::new();
        put_head(&mut v, major, arg);
        v
    }

    fn text(s: &str) -> Vec<u8> {
        let mut v = Vec::new();
        put_text(&mut v, s);
        v
    }

    /// Build a CBOR map from `(key, value_bytes)` in the order given, with the
    /// map count taken from the entry count. Nothing is sorted: the caller
    /// controls the order so mis-ordering can be tested.
    fn map(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut out = head(MAJOR_MAP, entries.len() as u64);
        for (k, v) in entries {
            out.extend(text(k));
            out.extend(v);
        }
        out
    }

    /// The canonical entry list for [`sample`], as `(key, value)` pairs in
    /// canonical order. `map(&baseline())` must equal
    /// `sample(..).to_canonical_cbor()`.
    fn baseline() -> Vec<(&'static str, Vec<u8>)> {
        let license = sample(Some(EXPIRES));
        let mut id = Vec::new();
        put_bytes(&mut id, license.id.as_bytes());
        let mut features = head(MAJOR_ARRAY, 1);
        features.extend(text("autocomplete"));
        vec![
            ("v", head(MAJOR_UINT, 1)),
            ("id", id),
            ("kid", head(MAJOR_UINT, 1)),
            ("tier", text("team")),
            ("email", text("user@example.com")),
            ("seats", head(MAJOR_UINT, 5)),
            ("product", text("aibo")),
            ("features", features),
            ("issued_at", head(MAJOR_UINT, ISSUED)),
            ("expires_at", head(MAJOR_UINT, EXPIRES)),
            ("max_version", text("2.0.0")),
        ]
    }

    #[test]
    fn keys_are_in_canonical_cbor_order() {
        // RFC 8949 §4.2.1: map keys sort by their *encoded* bytes, which for
        // short text strings means length first, then bytewise. The decoder's
        // ordering check is only a canonicality check if this holds.
        let encoded: Vec<Vec<u8>> = KEYS.iter().map(|k| text(k)).collect();
        let mut sorted = encoded.clone();
        sorted.sort();
        assert_eq!(encoded, sorted);
        sorted.dedup();
        assert_eq!(sorted.len(), KEYS.len(), "duplicate key name in KEYS");
    }

    #[test]
    fn hand_built_baseline_matches_the_encoder() {
        // If this drifts, every case below is testing the wrong bytes.
        assert_eq!(map(&baseline()), sample(Some(EXPIRES)).to_canonical_cbor());
        assert_eq!(
            License::from_canonical_cbor(&map(&baseline())).unwrap(),
            sample(Some(EXPIRES))
        );
    }

    #[test]
    fn indefinite_lengths_are_rejected() {
        // Same entries, indefinite-length map (0xbf .. 0xff).
        let mut bytes = vec![0xbf];
        for (k, v) in baseline() {
            bytes.extend(text(k));
            bytes.extend(v);
        }
        bytes.push(0xff);
        assert_eq!(
            License::from_canonical_cbor(&bytes),
            Err(LicenseError::NonCanonicalCbor("indefinite length"))
        );

        // Indefinite-length text string as a value.
        let mut entries = baseline();
        entries[4].1 = vec![0x7f, 0x61, b'a', 0xff];
        assert_eq!(
            License::from_canonical_cbor(&map(&entries)),
            Err(LicenseError::NonCanonicalCbor("indefinite length"))
        );
    }

    #[test]
    fn non_shortest_integer_heads_are_rejected() {
        // `v: 1` written with a one-byte argument instead of inline.
        let mut entries = baseline();
        entries[0].1 = vec![0x18, 0x01];
        assert_eq!(
            License::from_canonical_cbor(&map(&entries)),
            Err(LicenseError::NonCanonicalCbor("integer not shortest form"))
        );

        // `seats: 5` written as a u16.
        let mut entries = baseline();
        entries[5].1 = vec![0x19, 0x00, 0x05];
        assert_eq!(
            License::from_canonical_cbor(&map(&entries)),
            Err(LicenseError::NonCanonicalCbor("integer not shortest form"))
        );

        // `issued_at` written as a u64 when a u32 would do.
        let mut entries = baseline();
        let mut wide = vec![0x1b];
        wide.extend_from_slice(&ISSUED.to_be_bytes());
        entries[8].1 = wide;
        assert_eq!(
            License::from_canonical_cbor(&map(&entries)),
            Err(LicenseError::NonCanonicalCbor("integer not shortest form"))
        );

        // A map count written non-shortest.
        let mut bytes = vec![0xb8, 0x0b];
        for (k, v) in baseline() {
            bytes.extend(text(k));
            bytes.extend(v);
        }
        assert_eq!(
            License::from_canonical_cbor(&bytes),
            Err(LicenseError::NonCanonicalCbor("integer not shortest form"))
        );

        // A text-string length written non-shortest.
        let mut entries = baseline();
        let mut long_head = vec![0x78, 0x04];
        long_head.extend_from_slice(b"team");
        entries[3].1 = long_head;
        assert_eq!(
            License::from_canonical_cbor(&map(&entries)),
            Err(LicenseError::NonCanonicalCbor("integer not shortest form"))
        );
    }

    #[test]
    fn out_of_order_and_duplicate_keys_are_rejected() {
        // Swap two adjacent, otherwise complete entries.
        let mut entries = baseline();
        entries.swap(0, 1);
        assert_eq!(
            License::from_canonical_cbor(&map(&entries)),
            Err(LicenseError::NonCanonicalCbor(
                "map keys out of canonical order or duplicated"
            ))
        );

        // A key that sorts late moved to the front.
        let mut entries = baseline();
        let last = entries.pop().expect("max_version");
        entries.insert(0, last);
        assert_eq!(
            License::from_canonical_cbor(&map(&entries)),
            Err(LicenseError::NonCanonicalCbor(
                "map keys out of canonical order or duplicated"
            ))
        );

        // A complete licence with one entry repeated — the classic
        // last-one-wins smuggling trick.
        let mut entries = baseline();
        entries.insert(6, ("seats", head(MAJOR_UINT, 250)));
        assert_eq!(
            License::from_canonical_cbor(&map(&entries)),
            Err(LicenseError::NonCanonicalCbor(
                "map keys out of canonical order or duplicated"
            ))
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        // Otherwise valid, plus one extra field. A permissive decoder would
        // ignore it; that is the forgery surface §19 warns about.
        let mut entries = baseline();
        entries.push(("seats_override", head(MAJOR_UINT, 99)));
        assert_eq!(
            License::from_canonical_cbor(&map(&entries)),
            Err(LicenseError::NonCanonicalCbor("unknown map key"))
        );
    }

    #[test]
    fn absent_optionals_must_be_omitted_not_null() {
        // `max_version: null` instead of the field being absent.
        let mut entries = baseline();
        entries[10].1 = vec![0xf6];
        assert_eq!(
            License::from_canonical_cbor(&map(&entries)),
            Err(LicenseError::Schema("max_version"))
        );

        // An empty `features` array instead of the field being absent.
        let mut entries = baseline();
        entries[7].1 = head(MAJOR_ARRAY, 0);
        assert_eq!(
            License::from_canonical_cbor(&map(&entries)),
            Err(LicenseError::NonCanonicalCbor(
                "empty features must be omitted"
            ))
        );
    }

    #[test]
    fn wrong_major_types_are_rejected() {
        // `v: -1` — a negative integer where an unsigned one is required.
        let mut entries = baseline();
        entries[0].1 = vec![0x20];
        assert_eq!(
            License::from_canonical_cbor(&map(&entries)),
            Err(LicenseError::Schema("v"))
        );

        // `seats: "5"`.
        let mut entries = baseline();
        entries[5].1 = text("5");
        assert_eq!(
            License::from_canonical_cbor(&map(&entries)),
            Err(LicenseError::Schema("seats"))
        );

        // `id` as a text string rather than a 16-byte string.
        let mut entries = baseline();
        entries[1].1 = text("0123456789abcdef");
        assert_eq!(
            License::from_canonical_cbor(&map(&entries)),
            Err(LicenseError::Schema("id"))
        );

        // `id` as a byte string of the wrong length.
        let mut entries = baseline();
        let mut short = Vec::new();
        put_bytes(&mut short, &[0u8; 8]);
        entries[1].1 = short;
        assert_eq!(
            License::from_canonical_cbor(&map(&entries)),
            Err(LicenseError::Schema("id is not 16 bytes"))
        );

        // A top level that is not a map at all.
        assert_eq!(
            License::from_canonical_cbor(&text("nope")),
            Err(LicenseError::Schema("top level is not a map"))
        );
    }

    #[test]
    fn missing_required_fields_are_rejected() {
        for (i, (name, _)) in baseline().into_iter().enumerate().take(6) {
            let mut entries = baseline();
            entries.remove(i);
            let err = License::from_canonical_cbor(&map(&entries)).unwrap_err();
            assert!(
                matches!(err, LicenseError::Schema(_)),
                "dropping {name} produced {err:?}"
            );
        }
        // `issued_at` sits at index 8.
        let mut entries = baseline();
        entries.remove(8);
        assert_eq!(
            License::from_canonical_cbor(&map(&entries)),
            Err(LicenseError::Schema("missing issued_at"))
        );
    }

    #[test]
    fn declared_map_count_must_match_the_content() {
        // Count says 11, only 10 entries follow.
        let entries = baseline();
        let mut bytes = head(MAJOR_MAP, 11);
        for (k, v) in entries.iter().take(10) {
            bytes.extend(text(k));
            bytes.extend(v);
        }
        assert_eq!(
            License::from_canonical_cbor(&bytes),
            Err(LicenseError::Truncated)
        );

        // Count says 10, 11 entries follow: the eleventh becomes trailing.
        let mut bytes = head(MAJOR_MAP, 10);
        for (k, v) in &entries {
            bytes.extend(text(k));
            bytes.extend(v);
        }
        assert_eq!(
            License::from_canonical_cbor(&bytes),
            Err(LicenseError::TrailingBytes)
        );

        // A count no schema-valid licence could have.
        let mut bytes = head(MAJOR_MAP, 99);
        for (k, v) in &entries {
            bytes.extend(text(k));
            bytes.extend(v);
        }
        assert_eq!(
            License::from_canonical_cbor(&bytes),
            Err(LicenseError::NonCanonicalCbor("too many map entries"))
        );
    }

    #[test]
    fn invalid_utf8_in_a_text_field_is_rejected() {
        let mut entries = baseline();
        let mut bad = head(MAJOR_TEXT, 4);
        bad.extend_from_slice(&[0xff, 0xfe, 0xfd, 0xfc]);
        entries[4].1 = bad;
        assert_eq!(
            License::from_canonical_cbor(&map(&entries)),
            Err(LicenseError::Schema("text is not UTF-8"))
        );
    }

    #[test]
    fn an_unknown_tier_is_rejected() {
        let mut entries = baseline();
        entries[3].1 = text("enterprise");
        assert_eq!(
            License::from_canonical_cbor(&map(&entries)),
            Err(LicenseError::Schema("unknown tier"))
        );
    }

    #[test]
    fn schema_bounds_are_enforced() {
        let key = test_key(1);
        let keys = keyring(1, &key);

        // expires_at before issued_at
        let mut bad = sample(Some(ISSUED - 1));
        bad.expires_at = Some(ISSUED - 1);
        let mut state = LicenseState::default();
        assert_eq!(
            verify_with(&blob(&key, &bad), &keys, ISSUED, &mut state),
            Err(LicenseError::Schema("expires_at precedes issued_at"))
        );

        // personal tier with more than one seat
        let mut bad = sample(Some(EXPIRES));
        bad.tier = Tier::Personal;
        assert_eq!(
            verify_with(&blob(&key, &bad), &keys, ISSUED, &mut state),
            Err(LicenseError::Schema("personal tier is single seat"))
        );

        // zero seats
        let mut bad = sample(Some(EXPIRES));
        bad.seats = 0;
        assert_eq!(
            verify_with(&blob(&key, &bad), &keys, ISSUED, &mut state),
            Err(LicenseError::Schema("seats out of range"))
        );

        // a future schema version
        let mut bad = sample(Some(EXPIRES));
        bad.v = 2;
        assert_eq!(
            verify_with(&blob(&key, &bad), &keys, ISSUED, &mut state),
            Err(LicenseError::UnsupportedVersion(2))
        );

        assert_eq!(state, LicenseState::default());
    }

    #[test]
    fn oversized_blobs_are_refused_before_parsing() {
        let key = test_key(1);
        let mut state = LicenseState::default();
        let huge = "A".repeat(MAX_BLOB_BYTES * 2 + 4);
        assert!(matches!(
            verify_with(&huge, &keyring(1, &key), ISSUED, &mut state),
            Err(LicenseError::TooLarge(_))
        ));
    }

    // --- key rotation ------------------------------------------------------

    #[test]
    fn wrong_key_id_is_rejected() {
        let key = test_key(1);
        let mut license = sample(Some(EXPIRES));
        license.kid = 9; // claims a key that did not sign it
        let mut state = LicenseState::default();

        assert_eq!(
            verify_with(
                &blob(&key, &license),
                &keyring(1, &key),
                ISSUED + DAY,
                &mut state
            ),
            Err(LicenseError::WrongKeyId {
                claimed: 9,
                signed_by: 1
            })
        );
        assert_eq!(state, LicenseState::default());
    }

    #[test]
    fn licence_signed_by_a_key_outside_the_keyring_is_rejected() {
        let old = test_key(3);
        let current = test_key(4);
        let license = sample(Some(EXPIRES));
        let mut state = LicenseState::default();
        assert_eq!(
            verify_with(
                &blob(&old, &license),
                &keyring(1, &current),
                ISSUED + DAY,
                &mut state
            ),
            Err(LicenseError::BadSignature)
        );
    }

    #[test]
    fn rotation_keeps_both_keys_working() {
        let old = test_key(5);
        let new = test_key(6);
        let ring = vec![
            SigningKeyEntry {
                id: 1,
                public_key: old.verifying_key().to_bytes(),
            },
            SigningKeyEntry {
                id: 2,
                public_key: new.verifying_key().to_bytes(),
            },
        ];

        let mut old_license = sample(Some(EXPIRES));
        old_license.kid = 1;
        let mut new_license = sample(Some(EXPIRES));
        new_license.kid = 2;
        new_license.id = Uuid::from_u128(11);

        for (key, license) in [(&old, &old_license), (&new, &new_license)] {
            let mut state = LicenseState::default();
            let out = verify_with(&blob(key, license), &ring, ISSUED + DAY, &mut state).unwrap();
            assert_eq!(out.status, LicenseStatus::Valid);
        }
    }

    // --- clocks ------------------------------------------------------------

    #[test]
    fn clock_rolled_back_cannot_resurrect_an_expired_licence() {
        let key = test_key(1);
        let license = sample(Some(EXPIRES));
        // The machine has already seen a time well past expiry plus grace.
        let mut state = LicenseState {
            clock_high_water: EXPIRES + OFFLINE_GRACE_SECS + 30 * DAY,
            last_valid_at: Some(EXPIRES - DAY),
            last_valid_id: Some(license.id),
        };
        let before = state.clone();

        // The user winds the clock back to the middle of the term.
        let now = ISSUED + DAY;
        let out = verify_with(&blob(&key, &license), &keyring(1, &key), now, &mut state).unwrap();

        assert_eq!(
            out.status,
            LicenseStatus::Expired {
                expired_at: EXPIRES
            }
        );
        assert!(out.clock_rolled_back);
        // The high-water mark never moves backwards, so this stays closed.
        assert_eq!(state.clock_high_water, before.clock_high_water);
        assert_eq!(state.last_valid_at, before.last_valid_at);
    }

    #[test]
    fn small_backwards_clock_movement_is_tolerated_silently() {
        let key = test_key(1);
        let license = sample(Some(EXPIRES));
        let seen = ISSUED + 10 * DAY;
        let mut state = LicenseState {
            clock_high_water: seen,
            last_valid_at: Some(seen),
            last_valid_id: Some(license.id),
        };

        // An NTP correction of an hour: still valid, no warning, and the
        // high-water mark holds.
        let now = seen - 3600;
        let out = verify_with(&blob(&key, &license), &keyring(1, &key), now, &mut state).unwrap();
        assert_eq!(out.status, LicenseStatus::Valid);
        assert!(!out.clock_rolled_back);
        assert_eq!(state.clock_high_water, seen);
        assert_eq!(state.last_valid_at, Some(seen));
    }

    #[test]
    fn clock_high_water_advances_with_time() {
        let key = test_key(1);
        let license = sample(Some(EXPIRES));
        let mut state = LicenseState::default();
        let keys = keyring(1, &key);
        for day in 1..5 {
            let now = ISSUED + day * DAY;
            verify_with(&blob(&key, &license), &keys, now, &mut state).unwrap();
            assert_eq!(state.clock_high_water, now);
        }
    }

    // --- the shipped keyring ----------------------------------------------

    #[test]
    fn embedded_keys_are_well_formed_and_unforgeable_here() {
        assert!(!EMBEDDED_KEYS.is_empty());
        for entry in EMBEDDED_KEYS {
            VerifyingKey::from_bytes(&entry.public_key)
                .expect("embedded key is a valid Ed25519 point");
        }
        // No test key may collide with a shipped key.
        for seed in 0u8..8 {
            let vk = test_key(seed).verifying_key().to_bytes();
            assert!(EMBEDDED_KEYS.iter().all(|e| e.public_key != vk));
        }
    }
}
