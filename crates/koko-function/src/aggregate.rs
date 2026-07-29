use super::*;

// ---- aggregates ----

/// An aggregate function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggOp {
    /// `count(*)` — counts rows (including those with null args).
    CountStar,
    /// `count(x)` — counts non-null values.
    Count,
    Sum,
    Avg,
    Min,
    Max,
    /// `collect(x)` — gather non-null values into a list.
    Collect,
    /// `percentileDisc(x, p)` — the discrete percentile: the smallest sorted
    /// value whose cumulative fraction reaches `p` (stored as `f64` bits so
    /// the op stays `Copy`/`Eq`).
    PercentileDisc(u64),
}

/// The result type of an aggregate given its argument type. C++ does not register
/// SUM/AVG overloads for DECIMAL, so reject those at bind time.
pub fn agg_result_type(op: AggOp, arg: &LogicalType) -> Result<LogicalType> {
    match op {
        AggOp::CountStar | AggOp::Count => Ok(LogicalType::Int64),
        AggOp::Sum | AggOp::Avg if matches!(arg, LogicalType::Decimal(_, _)) => {
            Err(Error::binder(format!(
                "Function {} did not receive correct arguments:\nActual:   ({})\nExpected: numeric type excluding DECIMAL",
                if op == AggOp::Sum { "SUM" } else { "AVG" },
                arg
            )))
        }
        AggOp::Avg => Ok(LogicalType::Double),
        AggOp::PercentileDisc(_) => Ok(arg.clone()),
        // C++ SUM widens: INT*/SERIAL -> INT128, UINT* -> UINT128, FLOAT -> DOUBLE
        // (audit V3; the overload table in review_regressions.test is the spec).
        AggOp::Sum => Ok(match arg {
            LogicalType::Int(k) if k.is_signed() => LogicalType::Int(IntKind::I128),
            LogicalType::Serial => LogicalType::Int(IntKind::I128),
            LogicalType::Int(_) => LogicalType::UInt128,
            LogicalType::UInt128 => LogicalType::UInt128,
            LogicalType::Float | LogicalType::Double => LogicalType::Double,
            other => other.clone(),
        }),
        AggOp::Min | AggOp::Max => Ok(arg.clone()),
        AggOp::Collect => Ok(LogicalType::List(Box::new(arg.clone()))),
    }
}

/// Accumulator for one aggregate within one group.
#[derive(Debug, Clone)]
pub struct AggState {
    op: AggOp,
    n_rows: i64,
    n_nonnull: i64,
    sum_i: i128,
    sum_u: u128,
    sum_f: f64,
    is_float: bool,
    is_unsigned: bool,
    extreme: Option<Value>,
    collected: Vec<Value>,
    seen: Option<HashSet<ValueKey>>,
}

impl AggState {
    pub fn new(op: AggOp, distinct: bool) -> Self {
        Self {
            op,
            n_rows: 0,
            n_nonnull: 0,
            sum_i: 0,
            sum_u: 0,
            sum_f: 0.0,
            is_float: false,
            is_unsigned: false,
            extreme: None,
            collected: Vec::new(),
            seen: if distinct { Some(HashSet::new()) } else { None },
        }
    }
    /// Heap bytes retained by variable-width aggregate state.
    pub fn heap_bytes(&self) -> u64 {
        let extreme = self.extreme.as_ref().map(value_payload_bytes).unwrap_or(0);
        let collected = (self.collected.capacity() * std::mem::size_of::<Value>()) as u64
            + self.collected.iter().map(value_payload_bytes).sum::<u64>();
        let seen = self.seen.as_ref().map_or(0, |seen| {
            (seen.capacity() * (std::mem::size_of::<ValueKey>() + std::mem::size_of::<usize>()))
                as u64
                + seen.iter().map(ValueKey::heap_bytes).sum::<u64>()
        });
        extreme.saturating_add(collected).saturating_add(seen)
    }
    /// Conservative bytes to reserve before [`Self::update_n`] may retain `v`.
    ///
    /// The estimate intentionally charges each retained element rather than relying
    /// on allocator-specific `Vec`/`HashSet` growth factors.
    pub fn reservation_bytes_for_update(&self, v: &Value, n: u64) -> u64 {
        if self.op == AggOp::CountStar || v.is_null() {
            return 0;
        }
        let mut bytes = 0u64;
        let mut effective_n = n;
        if let Some(seen) = &self.seen {
            let is_nan = matches!(v, Value::Double(x) if x.is_nan())
                || matches!(v, Value::Float(x) if x.is_nan());
            let key = ValueKey::from_value(v);
            if !is_nan && seen.contains(&key) {
                return 0;
            }
            bytes = bytes
                .saturating_add(std::mem::size_of::<ValueKey>() as u64)
                .saturating_add(key.heap_bytes())
                .saturating_add((2 * std::mem::size_of::<usize>()) as u64);
            effective_n = 1;
        }
        let retained_value =
            (std::mem::size_of::<Value>() as u64).saturating_add(value_payload_bytes(v));
        match self.op {
            AggOp::Collect | AggOp::PercentileDisc(_) => {
                bytes.saturating_add(effective_n.saturating_mul(retained_value))
            }
            AggOp::Min => {
                if self
                    .extreme
                    .as_ref()
                    .is_none_or(|extreme| order_cmp(v, extreme) == Ordering::Less)
                {
                    bytes.saturating_add(value_payload_bytes(v))
                } else {
                    bytes
                }
            }
            AggOp::Max => {
                if self
                    .extreme
                    .as_ref()
                    .is_none_or(|extreme| order_cmp(v, extreme) == Ordering::Greater)
                {
                    bytes.saturating_add(value_payload_bytes(v))
                } else {
                    bytes
                }
            }
            AggOp::CountStar | AggOp::Count | AggOp::Sum | AggOp::Avg => bytes,
        }
    }

    /// Feed one input value (`Value::Null` for the `count(*)` placeholder).
    pub fn update(&mut self, v: &Value) {
        self.update_n(v, 1);
    }

    /// Feed one input value with **factorization multiplicity** `n`: the value
    /// stands for `n` identical logical tuples (a collapsed pattern suffix folded
    /// into a count; see [`koko_common::DataChunk::multiplicity`]). `n == 1` is the
    /// ordinary path. `count(*)` counts `n` rows; `count(x)`/`sum`/`avg` scale by
    /// `n`; `min`/`max` are idempotent in `n`; `collect` gathers `n` copies. Under
    /// `DISTINCT`, `n` is ignored — the same value repeated adds no new distinct
    /// value (so `count(DISTINCT head)` over a fan-out is still 1).
    pub fn update_n(&mut self, v: &Value, n: u64) {
        self.n_rows += n as i64;
        if self.op == AggOp::CountStar {
            return;
        }
        if v.is_null() {
            return;
        }
        if let Some(seen) = &mut self.seen {
            // C++ counts NaNs as distinct from each other (audit V9): a NaN never
            // deduplicates, so count(DISTINCT [nan, nan, 1.0]) is 3.
            let is_nan = matches!(v, Value::Double(x) if x.is_nan())
                || matches!(v, Value::Float(x) if x.is_nan());
            if !is_nan && !seen.insert(ValueKey::from_value(v)) {
                return; // duplicate under DISTINCT
            }
        }
        // A DISTINCT value contributes exactly once regardless of multiplicity.
        let n = if self.seen.is_some() { 1 } else { n };
        let ni = n as i64;
        self.n_nonnull += ni;
        match self.op {
            AggOp::Count => {}
            AggOp::Sum | AggOp::Avg => match v {
                Value::Double(x) => {
                    self.is_float = true;
                    self.sum_f += *x * n as f64;
                }
                Value::Float(x) => {
                    self.is_float = true;
                    self.sum_f += *x as f64 * n as f64;
                }
                // Unsigned widths accumulate exactly in u128 (C++ SUM(UINT*) ->
                // UINT128 — audit V3; the old i128 path silently wrapped).
                Value::UInt128(_)
                | Value::IntX {
                    kind: IntKind::U8 | IntKind::U16 | IntKind::U32 | IntKind::U64,
                    ..
                } => {
                    self.is_unsigned = true;
                    let val = v.as_u128().unwrap_or(0);
                    self.sum_u = self.sum_u.wrapping_add(val.wrapping_mul(n as u128));
                    self.sum_f += val as f64 * n as f64;
                }
                // Exact signed accumulation in i128 (C++ SUM(INT*) -> INT128).
                _ if v.as_int128().is_some() => {
                    let val = v.as_int128().unwrap();
                    self.sum_i = self.sum_i.wrapping_add(val.wrapping_mul(n as i128));
                    self.sum_f += val as f64 * n as f64;
                }
                // DECIMAL SUM/AVG is rejected by the binder to match C++; do
                // not silently accumulate it through f64 if a caller bypasses
                // binding and feeds AggState directly.
                Value::Decimal { .. } => {}
                _ if v.as_f64().is_some() => {
                    self.is_float = true;
                    self.sum_f += v.as_f64().unwrap() * n as f64;
                }
                _ => {}
            },
            AggOp::Min => {
                if self
                    .extreme
                    .as_ref()
                    .is_none_or(|e| order_cmp(v, e) == Ordering::Less)
                {
                    self.extreme = Some(v.clone());
                }
            }
            AggOp::Max => {
                if self
                    .extreme
                    .as_ref()
                    .is_none_or(|e| order_cmp(v, e) == Ordering::Greater)
                {
                    self.extreme = Some(v.clone());
                }
            }
            AggOp::Collect | AggOp::PercentileDisc(_) => {
                for _ in 0..n {
                    self.collected.push(v.clone());
                }
            }
            AggOp::CountStar => unreachable!(),
        }
    }

    /// Fold a partial accumulator for the **same group and op** into this one — the
    /// merge step of the partitioned parallel hash aggregate (P3 step 9), where each
    /// morsel built a local partial. Merging the morsel partials in **morsel order**
    /// reproduces the serial accumulation exactly:
    /// - `count(*)`/`count`/`sum`/`avg` add the row/value counts and the **`i128`**
    ///   integer sum (associative ⇒ bit-identical regardless of partition);
    /// - `min`/`max` take the combined extreme (order-independent);
    /// - `collect` concatenates (the caller merges in morsel order ⇒ serial list order).
    ///
    /// The float sum (`sum_f`) is added too, but the parallel path never reaches this
    /// for a float-typed `SUM`/`AVG` argument (f64 addition is non-associative, so the
    /// optimizer keeps those serial); likewise `DISTINCT` aggregates are never
    /// parallelized, so both states are non-distinct here.
    pub fn merge(&mut self, other: AggState) {
        debug_assert_eq!(self.op, other.op, "merge of mismatched aggregate ops");
        debug_assert!(
            self.seen.is_none() && other.seen.is_none(),
            "DISTINCT aggregates are not parallelized, so are never merged"
        );
        self.n_rows += other.n_rows;
        self.n_nonnull += other.n_nonnull;
        self.sum_i = self.sum_i.wrapping_add(other.sum_i);
        self.sum_u = self.sum_u.wrapping_add(other.sum_u);
        self.sum_f += other.sum_f;
        self.is_float |= other.is_float;
        self.is_unsigned |= other.is_unsigned;
        match self.op {
            AggOp::Min => {
                if let Some(o) = other.extreme {
                    if self
                        .extreme
                        .as_ref()
                        .is_none_or(|e| order_cmp(&o, e) == Ordering::Less)
                    {
                        self.extreme = Some(o);
                    }
                }
            }
            AggOp::Max => {
                if let Some(o) = other.extreme {
                    if self
                        .extreme
                        .as_ref()
                        .is_none_or(|e| order_cmp(&o, e) == Ordering::Greater)
                    {
                        self.extreme = Some(o);
                    }
                }
            }
            AggOp::Collect | AggOp::PercentileDisc(_) => self.collected.extend(other.collected),
            AggOp::CountStar | AggOp::Count | AggOp::Sum | AggOp::Avg => {}
        }
    }

    /// Produce the aggregate's final value for the group.
    ///
    /// Returns `Err` if an INT64 `SUM` overflowed — matching the checked
    /// overflow contract of scalar integer arithmetic (rather than silently
    /// wrapping the `i128` accumulator down to `i64`).
    pub fn finalize(self) -> Result<Value> {
        Ok(match self.op {
            AggOp::CountStar => Value::Int64(self.n_rows),
            AggOp::Count => Value::Int64(self.n_nonnull),
            // SUM widens like C++ (audit V3): INT* -> INT128, UINT* -> UINT128,
            // FLOAT -> DOUBLE — the old INT64 "overflow" error was invented.
            AggOp::Sum => {
                if self.n_nonnull == 0 {
                    Value::Null
                } else if self.is_float {
                    Value::Double(self.sum_f)
                } else if self.is_unsigned {
                    Value::UInt128(self.sum_u)
                } else {
                    Value::IntX {
                        value: self.sum_i,
                        kind: IntKind::I128,
                    }
                }
            }
            AggOp::Avg => {
                if self.n_nonnull == 0 {
                    Value::Null
                } else if self.is_float {
                    Value::Double(self.sum_f / self.n_nonnull as f64)
                } else if self.is_unsigned {
                    Value::Double(self.sum_u as f64 / self.n_nonnull as f64)
                } else {
                    // Divide the exact integer sum (one rounding) rather than the
                    // lossily-accumulated f64, which loses precision past 2^53.
                    Value::Double(self.sum_i as f64 / self.n_nonnull as f64)
                }
            }
            AggOp::Min | AggOp::Max => self.extreme.unwrap_or(Value::Null),
            // `collect` ignores NULL inputs, so an empty accumulator is a known
            // empty list rather than an unknown value.
            AggOp::Collect => Value::List(self.collected),
            AggOp::PercentileDisc(_) if self.collected.is_empty() => Value::Null,
            AggOp::PercentileDisc(bits) => {
                let p = f64::from_bits(bits).clamp(0.0, 1.0);
                let mut vals = self.collected;
                vals.sort_by(order_cmp);
                let n = vals.len();
                let idx = ((p * n as f64).ceil() as usize).clamp(1, n) - 1;
                vals[idx].clone()
            }
        })
    }
}
