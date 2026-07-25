//! Ports of the C++ engine's `hash()` (murmurhash64 over physical values,
//! `hash_functions.h`) and its PCG32 random engine (`random_engine.h` /
//! `rand_function.cpp`), so `hash()`/`random()`/`setseed()` reproduce the
//! oracle bit-for-bit (the corpus pins post-seed `random()` digits).

use koko_common::{Error, Result, Value};

/// `NULL_HASH` — the hash of a NULL value.
const NULL_HASH: u64 = u64::MAX;

fn murmurhash64(mut x: u64) -> u64 {
    x ^= x >> 32;
    x = x.wrapping_mul(0xd6e8feb86659fd93);
    x ^= x >> 32;
    x = x.wrapping_mul(0xd6e8feb86659fd93);
    x ^= x >> 32;
    x
}

fn combine(a: u64, b: u64) -> u64 {
    a.wrapping_mul(0xbf58476d1ce4e5b9) ^ b
}

fn hash_i128(v: i128) -> u64 {
    let low = v as u64;
    let high = (v >> 64) as u64;
    murmurhash64(low) ^ murmurhash64(high)
}

/// Hash a value exactly like the C++ engine (physical-type dispatch).
pub fn hash_value(v: &Value) -> Result<u64> {
    Ok(match v {
        Value::Null => NULL_HASH,
        Value::Bool(b) => murmurhash64(*b as u64),
        Value::Int64(n) => murmurhash64(*n as u64),
        // Narrow ints hash via their native width, sign-extended to u64 like
        // the C++ integral conversions; UINT128/INT128 as low^high.
        Value::IntX { value, kind } => {
            if matches!(kind, koko_common::IntKind::I128) {
                hash_i128(*value)
            } else {
                murmurhash64(*value as i64 as u64)
            }
        }
        Value::UInt128(u) => murmurhash64(*u as u64) ^ murmurhash64((*u >> 64) as u64),
        Value::Decimal { value, .. } => hash_i128(*value),
        Value::Double(d) => {
            // 0 and -0 are not byte-equivalent but hash the same.
            if *d == 0.0 {
                murmurhash64(0)
            } else {
                murmurhash64(d.to_bits())
            }
        }
        Value::Float(f) => {
            if *f == 0.0 {
                murmurhash64(0)
            } else {
                murmurhash64(f.to_bits() as u64)
            }
        }
        Value::String(s) => hash_bytes(s.as_bytes()),
        Value::Blob(b) => hash_bytes(b),
        Value::Date(d) => murmurhash64(*d as i64 as u64),
        Value::Timestamp(t) | Value::TimestampTz(t) => murmurhash64(*t as u64),
        Value::Interval(iv) => combine(
            murmurhash64(iv.months as i64 as u64),
            combine(
                murmurhash64(iv.days as i64 as u64),
                murmurhash64(iv.micros as u64),
            ),
        ),
        Value::Uuid(u) => murmurhash64(*u as u64) ^ murmurhash64((*u >> 64) as u64),
        Value::InternalId(id) => murmurhash64(id.offset.0) ^ murmurhash64(id.table_id.0),
        // List: fold elements into NULL_HASH with combine(acc, h(elem)).
        Value::List(items) => {
            let mut acc = NULL_HASH;
            for it in items {
                acc = combine(acc, hash_value(it)?);
            }
            acc
        }
        // Struct/union: h(field0), then combine(h(field_i), acc) per later
        // field (the new field hash is the *left* operand).
        Value::Struct(fields) => {
            let mut it = fields.iter();
            let mut acc = match it.next() {
                Some((_, first)) => hash_value(first)?,
                None => NULL_HASH,
            };
            for (_, f) in it {
                acc = combine(hash_value(f)?, acc);
            }
            acc
        }
        Value::Map(entries) => {
            // A map is physically a LIST of {key, value} structs.
            let mut acc = NULL_HASH;
            for (k, val) in entries {
                let kh = hash_value(k)?;
                let vh = hash_value(val)?;
                acc = combine(acc, combine(vh, kh));
            }
            acc
        }
        // Node/rel hash by their internal ID.
        Value::Node(n) => murmurhash64(n.id.offset.0) ^ murmurhash64(n.id.table_id.0),
        Value::Rel(r) => murmurhash64(r.id.offset.0) ^ murmurhash64(r.id.table_id.0),
        other => {
            return Err(Error::runtime(format!(
                "Cannot hash data type {}",
                other.logical_type().name()
            )));
        }
    })
}

fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = 0u64;
    for block in b.chunks_exact(8) {
        h = combine(
            h,
            murmurhash64(u64::from_le_bytes(block.try_into().unwrap())),
        );
    }
    let rem = &b[b.len() / 8 * 8..];
    let mut last = 0u64;
    for (i, byte) in rem.iter().enumerate() {
        last |= (*byte as u64) << (i * 8);
    }
    combine(h, murmurhash64(last))
}

/// PCG32 (`setseq_xsh_rr_64_32`) with the pcg_random.hpp default stream —
/// the exact engine behind the oracle's `random()`.
#[derive(Debug, Clone)]
struct Pcg32 {
    state: u64,
}

const PCG_MULT: u64 = 6364136223846793005;
const PCG_INC: u64 = 1442695040888963407;

impl Pcg32 {
    fn seeded(seed: u64) -> Self {
        // engine(itype state): state_ = bump(state + increment()).
        let mut p = Pcg32 { state: 0 };
        p.state = p.bump(seed.wrapping_add(PCG_INC));
        p
    }

    fn bump(&self, s: u64) -> u64 {
        s.wrapping_mul(PCG_MULT).wrapping_add(PCG_INC)
    }

    fn next32(&mut self) -> u32 {
        let old = self.state;
        self.state = self.bump(old);
        let xorshifted = (((old >> 18) ^ old) >> 27) as u32;
        let rot = (old >> 59) as u32;
        xorshifted.rotate_right(rot)
    }
}

/// Connection-owned nondeterministic-function state. Clones share one stream,
/// allowing a statement context to advance its owning connection's sequence
/// without process-global state.
#[derive(Debug, Clone)]
pub struct RandomState {
    rng: std::sync::Arc<std::sync::Mutex<Pcg32>>,
}

impl Default for RandomState {
    fn default() -> Self {
        let seed = std::time::UNIX_EPOCH
            .elapsed()
            .map_or(0x853c49e6748fea9b, |d| d.as_nanos() as u64);
        Self {
            rng: std::sync::Arc::new(std::sync::Mutex::new(Pcg32::seeded(seed))),
        }
    }
}

impl RandomState {
    /// `setseed(s)`: seed this connection's engine with
    /// `uint64(s * 2^64)` like the C++ cast.
    pub fn set_seed(&self, seed: f64) {
        let scaled = (seed * u64::MAX as f64) as u64;
        *self.rng.lock().unwrap_or_else(|e| e.into_inner()) = Pcg32::seeded(scaled);
    }

    /// `random()`: next u32 / UINT32_MAX.
    pub fn next_random(&self) -> f64 {
        self.rng.lock().unwrap_or_else(|e| e.into_inner()).next32() as f64 / u32::MAX as f64
    }

    /// A connection-local random UUID payload.
    pub fn next_uuid(&self) -> u128 {
        let mut rng = self.rng.lock().unwrap_or_else(|e| e.into_inner());
        let high = ((rng.next32() as u64) << 32) | rng.next32() as u64;
        let low = ((rng.next32() as u64) << 32) | rng.next32() as u64;
        ((high as u128) << 64) | low as u128
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn murmur_matches_oracle() {
        assert_eq!(murmurhash64(1), 4717996019076358352);
        assert_eq!(hash_bytes(b"abc"), 13131266495343169797);
        assert_eq!(murmurhash64(1.5f64.to_bits()), 1706605666616485939);
        // list [1,2]: fold into NULL_HASH
        let l = Value::List(vec![Value::Int64(1), Value::Int64(2)]);
        assert_eq!(hash_value(&l).unwrap(), 16499939440198084685);
    }

    #[test]
    fn pcg_matches_corpus() {
        // The corpus pins random() after setseed(0.2): 0.910543, 0.650728, …
        let mut p = Pcg32::seeded((0.2f64 * u64::MAX as f64) as u64);
        let r1 = p.next32() as f64 / u32::MAX as f64;
        assert!((r1 - 0.910543).abs() < 5e-7, "got {r1}");
        let r2 = p.next32() as f64 / u32::MAX as f64;
        assert!((r2 - 0.650728).abs() < 5e-7, "got {r2}");
    }
}
