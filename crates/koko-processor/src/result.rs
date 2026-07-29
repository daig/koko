use super::*;

/// The materialized typed result of a query.
#[derive(Debug, Clone, Default)]
pub struct ExecResult {
    pub column_names: Vec<String>,
    pub column_types: Vec<LogicalType>,
    pub batches: Vec<DataChunk>,
}

impl ExecResult {
    pub fn num_rows(&self) -> usize {
        self.batches.iter().map(DataChunk::size).sum()
    }

    pub fn into_rows(self) -> Vec<Vec<Value>> {
        let mut rows = Vec::with_capacity(self.num_rows());
        for batch in self.batches {
            for position in batch.sel.iter() {
                rows.push(
                    batch
                        .columns
                        .iter()
                        .map(|column| column.get_value(position))
                        .collect(),
                );
            }
        }
        rows
    }

    pub fn map_values_mut(&mut self, mut map: impl FnMut(&mut Value)) {
        for batch in &mut self.batches {
            let positions: Vec<_> = batch.sel.iter().collect();
            for position in positions {
                for column in &mut batch.columns {
                    let mut value = column.get_value(position);
                    map(&mut value);
                    column.set_value_owned(position, value);
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutputPosition {
    pub(crate) batch: usize,
    pub(crate) physical: usize,
    pub(crate) ordinal: usize,
}

/// Columnar final-result builder. Projection emits directly into typed chunks;
/// only ORDER BY keys and DISTINCT hashes use row-shaped temporary metadata.
pub(crate) struct OutputBuffer {
    pub(crate) column_types: Vec<LogicalType>,
    pub(crate) batches: Vec<DataChunk>,
    pub(crate) order_keys: Option<Vec<Vec<Value>>>,
    pub(crate) num_rows: usize,
}

impl OutputBuffer {
    pub(crate) fn new(column_types: Vec<LogicalType>, has_order: bool) -> Self {
        Self {
            column_types,
            batches: Vec::new(),
            order_keys: has_order.then(Vec::new),
            num_rows: 0,
        }
    }

    pub(crate) fn from_exec(result: ExecResult) -> Self {
        let num_rows = result.num_rows();
        Self {
            column_types: result.column_types,
            batches: result.batches,
            order_keys: None,
            num_rows,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.num_rows
    }

    /// Append one projected row and return newly retained heap bytes.
    pub(crate) fn push(&mut self, values: Vec<Value>, order_keys: Vec<Value>) -> u64 {
        debug_assert_eq!(values.len(), self.column_types.len());
        let mut allocated = values.iter().map(value_payload_bytes).sum::<u64>();
        if self
            .batches
            .last()
            .is_none_or(|batch| batch.size() == VECTOR_CAPACITY)
        {
            let batch = DataChunk::new(&self.column_types);
            allocated = allocated.saturating_add(batch.allocated_bytes());
            self.batches.push(batch);
        }
        let batch = self.batches.last_mut().expect("created above");
        let position = batch.size();
        for (column, value) in batch.columns.iter_mut().zip(values) {
            column.set_value_owned(position, value);
        }
        batch.set_flat(position + 1);
        if let Some(keys) = &mut self.order_keys {
            allocated = allocated
                .saturating_add(std::mem::size_of::<Vec<Value>>() as u64)
                .saturating_add((order_keys.capacity() * std::mem::size_of::<Value>()) as u64)
                .saturating_add(order_keys.iter().map(value_payload_bytes).sum::<u64>());
            keys.push(order_keys);
        } else {
            debug_assert!(order_keys.is_empty());
        }
        self.num_rows += 1;
        allocated
    }

    pub(crate) fn append(&mut self, mut other: Self, memory: &QueryMemory) -> Result<()> {
        other.normalize_to(&self.column_types, memory)?;
        self.batches.append(&mut other.batches);
        match (&mut self.order_keys, other.order_keys) {
            (Some(keys), Some(mut other_keys)) => keys.append(&mut other_keys),
            (None, None) => {}
            _ => {
                return Err(Error::runtime(
                    "internal UNION output order-key mismatch".to_string(),
                ));
            }
        }
        self.num_rows += other.num_rows;
        Ok(())
    }

    fn normalize_to(&mut self, target_types: &[LogicalType], memory: &QueryMemory) -> Result<()> {
        if self.column_types.len() != target_types.len() {
            return Err(Error::runtime(format!(
                "internal UNION normalization width mismatch: operand has {} columns, canonical \
                 result has {}",
                self.column_types.len(),
                target_types.len()
            )));
        }

        for (column_index, (source, target)) in
            self.column_types.iter().zip(target_types).enumerate()
        {
            if source == target || *target == LogicalType::Any {
                continue;
            }
            if *source != LogicalType::Any {
                return Err(Error::runtime(format!(
                    "internal UNION normalization cannot convert column {column_index} from \
                     {source} to {target}"
                )));
            }
            for batch in &self.batches {
                for position in batch.sel.iter() {
                    if !batch.columns[column_index].nulls.is_null(position) {
                        let actual = batch.columns[column_index]
                            .get_value(position)
                            .logical_type();
                        return Err(Error::runtime(format!(
                            "internal UNION normalization expected a null-only ANY column at \
                             position {column_index}, found {actual}"
                        )));
                    }
                }
            }
        }

        for (column_index, target) in target_types.iter().enumerate() {
            if self.column_types[column_index] == *target {
                continue;
            }
            for batch in &mut self.batches {
                memory.charge(ColumnData::allocation_bytes(target.physical_type()))?;
                let selection = &batch.sel;
                let source = &mut batch.columns[column_index];
                let mut normalized = ValueVector::new_null(target.clone());
                if *target == LogicalType::Any {
                    for position in selection.iter() {
                        normalized.set_value_owned(position, source.take_value(position));
                    }
                }
                *source = normalized;
            }
        }
        self.column_types.clone_from_slice(target_types);
        Ok(())
    }

    pub(crate) fn positions(&self) -> Vec<OutputPosition> {
        let mut positions = Vec::with_capacity(self.num_rows);
        let mut ordinal = 0;
        for (batch_index, batch) in self.batches.iter().enumerate() {
            for physical in batch.sel.iter() {
                positions.push(OutputPosition {
                    batch: batch_index,
                    physical,
                    ordinal,
                });
                ordinal += 1;
            }
        }
        positions
    }

    pub(crate) fn value_at(&self, position: OutputPosition, column: usize) -> Value {
        self.batches[position.batch].columns[column].get_value(position.physical)
    }

    pub(crate) fn compact(&self, positions: &[OutputPosition]) -> Vec<DataChunk> {
        let mut batches = Vec::with_capacity(positions.len().div_ceil(VECTOR_CAPACITY));
        for selected in positions.chunks(VECTOR_CAPACITY) {
            let mut batch = DataChunk::new(&self.column_types);
            for (column_index, output) in batch.columns.iter_mut().enumerate() {
                for (output_position, source) in selected.iter().copied().enumerate() {
                    output.set_value_owned(output_position, self.value_at(source, column_index));
                }
            }
            batch.set_flat(selected.len());
            batches.push(batch);
        }
        batches
    }

    pub(crate) fn finish(
        self,
        column_names: Vec<String>,
        distinct: bool,
        order_ascending: &[bool],
        skip: usize,
        limit: Option<usize>,
        memory: &QueryMemory,
    ) -> Result<ExecResult> {
        let position_bytes = (self.num_rows * std::mem::size_of::<OutputPosition>()) as u64;
        let distinct_bytes = if distinct {
            (self.num_rows
                * (std::mem::size_of::<Vec<ValueKey>>()
                    + self.column_types.len() * std::mem::size_of::<ValueKey>()
                    + 2 * std::mem::size_of::<usize>())) as u64
        } else {
            0
        };
        memory.charge(position_bytes.saturating_add(distinct_bytes))?;
        let mut positions = self.positions();
        if distinct {
            let mut seen = HashSet::new();
            positions.retain(|position| {
                let key = (0..self.column_types.len())
                    .map(|column| ValueKey::from_value(&self.value_at(*position, column)))
                    .collect::<Vec<_>>();
                seen.insert(key)
            });
        }
        if !order_ascending.is_empty() {
            let order_keys = self
                .order_keys
                .as_ref()
                .expect("ORDER BY projection records sort keys");
            positions.sort_by(|left, right| {
                for (index, ascending) in order_ascending.iter().copied().enumerate() {
                    let ordering = order_cmp(
                        &order_keys[left.ordinal][index],
                        &order_keys[right.ordinal][index],
                    );
                    let ordering = if ascending {
                        ordering
                    } else {
                        ordering.reverse()
                    };
                    if ordering != Ordering::Equal {
                        return ordering;
                    }
                }
                Ordering::Equal
            });
        }
        let positions = positions
            .into_iter()
            .skip(skip)
            .take(limit.unwrap_or(usize::MAX))
            .collect::<Vec<_>>();
        let identity = positions.len() == self.num_rows
            && positions
                .iter()
                .enumerate()
                .all(|(ordinal, position)| position.ordinal == ordinal);
        let batches = if identity {
            self.batches
        } else {
            let batches = self.compact(&positions);
            memory.charge(batches.iter().map(DataChunk::allocated_bytes).sum())?;
            batches
        };
        Ok(ExecResult {
            column_names,
            column_types: self.column_types,
            batches,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(types: Vec<LogicalType>, rows: Vec<Vec<Value>>) -> OutputBuffer {
        let mut output = OutputBuffer::new(types, false);
        for row in rows {
            output.push(row, Vec::new());
        }
        output
    }

    #[test]
    fn union_normalization_permits_only_exact_null_and_dynamic_conversions() {
        let tracker = MemoryTracker::default();
        let memory = QueryMemory::new(&tracker).unwrap();

        let mut exact = OutputBuffer::new(vec![LogicalType::Int64], false);
        exact
            .append(
                buffer(vec![LogicalType::Int64], vec![vec![Value::Int64(1)]]),
                &memory,
            )
            .unwrap();
        assert_eq!(exact.value_at(exact.positions()[0], 0), Value::Int64(1));
        assert_eq!(memory.bytes(), 0);

        let mut inferred = OutputBuffer::new(vec![LogicalType::Int64], false);
        inferred
            .append(
                buffer(vec![LogicalType::Any], vec![vec![Value::Null]]),
                &memory,
            )
            .unwrap();
        let inferred_position = inferred.positions()[0];
        assert_eq!(
            inferred.batches[inferred_position.batch].columns[0].logical_type,
            LogicalType::Int64
        );
        assert_eq!(inferred.value_at(inferred_position, 0), Value::Null);

        let mut dynamic = OutputBuffer::new(vec![LogicalType::Any], false);
        dynamic
            .append(
                buffer(
                    vec![LogicalType::String],
                    vec![vec![Value::String("value".to_string())]],
                ),
                &memory,
            )
            .unwrap();
        let dynamic_position = dynamic.positions()[0];
        assert_eq!(
            dynamic.batches[dynamic_position.batch].columns[0].logical_type,
            LogicalType::Any
        );
        assert_eq!(
            dynamic.value_at(dynamic_position, 0),
            Value::String("value".to_string())
        );
        assert!(memory.bytes() > 0);
    }

    #[test]
    fn union_normalization_returns_errors_for_impossible_payloads() {
        let tracker = MemoryTracker::default();
        let memory = QueryMemory::new(&tracker).unwrap();

        let mut concrete = OutputBuffer::new(vec![LogicalType::Int64], false);
        let error = concrete
            .append(
                buffer(
                    vec![LogicalType::String],
                    vec![vec![Value::String("value".to_string())]],
                ),
                &memory,
            )
            .unwrap_err();
        assert!(matches!(error, Error::Runtime(_)));
        assert!(
            error
                .to_string()
                .contains("cannot convert column 0 from STRING to INT64")
        );

        let mut null_only = OutputBuffer::new(vec![LogicalType::Int64], false);
        let error = null_only
            .append(
                buffer(vec![LogicalType::Any], vec![vec![Value::Int64(1)]]),
                &memory,
            )
            .unwrap_err();
        assert!(matches!(error, Error::Runtime(_)));
        assert!(
            error
                .to_string()
                .contains("expected a null-only ANY column")
        );
    }
}
