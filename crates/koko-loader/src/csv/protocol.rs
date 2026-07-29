use super::*;

pub(super) fn read_records(
    path: &Path,
    arity: usize,
    options: &CsvOptions,
    warnings: &koko_common::warnings::WarningSink,
    worker_count: usize,
    memory: MemoryTracker,
) -> Result<RecordStream> {
    let dialect = detected_dialect(path, arity, options)?;
    // Unterminated/invalid records are isolated later under IGNORE_ERRORS.
    if !options.ignore_errors {
        koko_common::csv_dialect::validate_file_structure(path, options)?;
    }
    let compressed = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("gz") || ext.eq_ignore_ascii_case("gzip"));
    let parallel = options.parallel
        && worker_count > 1
        && !compressed
        && std::fs::metadata(path).is_ok_and(|metadata| metadata.len() >= 64 * 1024)
        && !koko_common::csv_dialect::has_quoted_newline(path, &dialect)?;
    if parallel {
        let ranges = split_record_ranges(path, worker_count)?;
        if ranges.len() > 1 {
            return Ok(RecordStream::Parallel(ParallelRecords::spawn(
                path, ranges, dialect, arity, memory,
            )?));
        }
    }
    let mut record_options = options.clone();
    if compressed {
        record_options.parallel = false;
    }
    Ok(RecordStream::Serial(Box::new(CheckedRecords {
        inner: koko_common::csv_dialect::open_reader(path, &dialect)?.into_records(),
        path: path.to_path_buf(),
        options: record_options,
        warnings: warnings.clone(),
        blank_lines: physical_blank_lines(path),
        arity,
        consumed: 0,
        pending: std::collections::VecDeque::new(),
    })))
}

pub(super) enum RecordStream {
    Serial(Box<CheckedRecords>),
    Parallel(ParallelRecords),
}

impl Iterator for RecordStream {
    type Item = Result<csv::StringRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Serial(records) => records.next(),
            Self::Parallel(records) => records.next(),
        }
    }
}

pub(super) struct TrackedRecordBatch {
    records: std::collections::VecDeque<csv::StringRecord>,
    _reservation: MemoryReservation,
}

pub(super) enum WorkerMessage {
    Batch(TrackedRecordBatch),
    Error(Error),
    Done,
}

pub(super) struct ParallelRecords {
    receivers: Vec<Receiver<WorkerMessage>>,
    handles: Vec<Option<JoinHandle<()>>>,
    worker: usize,
    batch: Option<TrackedRecordBatch>,
}

impl ParallelRecords {
    fn spawn(
        path: &Path,
        ranges: Vec<(u64, u64)>,
        dialect: Dialect,
        arity: usize,
        memory: MemoryTracker,
    ) -> Result<Self> {
        let mut receivers = Vec::with_capacity(ranges.len());
        let mut handles = Vec::with_capacity(ranges.len());
        for (start, end) in ranges {
            let (sender, receiver) = sync_channel(1);
            let path = path.to_path_buf();
            let memory = memory.clone();
            let handle = std::thread::Builder::new()
                .spawn(move || {
                    parse_record_range(&path, start, end, dialect, arity, &memory, &sender)
                })
                .map_err(|error| Error::Io(error.to_string()))?;
            receivers.push(receiver);
            handles.push(Some(handle));
        }
        Ok(Self {
            receivers,
            handles,
            worker: 0,
            batch: None,
        })
    }
}

impl Iterator for ParallelRecords {
    type Item = Result<csv::StringRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(batch) = &mut self.batch {
                if let Some(record) = batch.records.pop_front() {
                    return Some(Ok(record));
                }
                self.batch = None;
            }
            let receiver = self.receivers.get(self.worker)?;
            match receiver.recv() {
                Ok(WorkerMessage::Batch(batch)) => self.batch = Some(batch),
                Ok(WorkerMessage::Error(error)) => return Some(Err(error)),
                Ok(WorkerMessage::Done) => {
                    let worker_panicked = self.handles[self.worker]
                        .take()
                        .is_some_and(|handle| handle.join().is_err());
                    if worker_panicked {
                        return Some(Err(Error::runtime("CSV parsing worker panicked.")));
                    }
                    self.worker += 1;
                }
                Err(_) => return Some(Err(Error::runtime("CSV parsing worker stopped."))),
            }
        }
    }
}

impl Drop for ParallelRecords {
    fn drop(&mut self) {
        self.receivers.clear();
        for handle in &mut self.handles {
            if let Some(handle) = handle.take() {
                let _ = handle.join();
            }
        }
    }
}

pub(super) fn parse_record_range(
    path: &Path,
    start: u64,
    end: u64,
    dialect: Dialect,
    arity: usize,
    memory: &MemoryTracker,
    sender: &SyncSender<WorkerMessage>,
) {
    #[cfg(test)]
    PARALLEL_WORKERS_STARTED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let result = (|| -> Result<()> {
        let mut file = std::fs::File::open(path)
            .map_err(|error| Error::Io(format!("{}: {error}", path.display())))?;
        file.seek(SeekFrom::Start(start))
            .map_err(|error| Error::Io(format!("{}: {error}", path.display())))?;
        let mut input = BufReader::new(file.take(end.saturating_sub(start)));
        let mut line = Vec::new();
        let mut records = Vec::with_capacity(VECTOR_CAPACITY);
        let mut reservation = memory.try_reserve(0)?;
        let mut line_reservation = memory.try_reserve(0)?;
        let mut first_line = start == 0;
        loop {
            line.clear();
            let bytes = read_tracked_line(&mut input, &mut line, &mut line_reservation)?;
            if bytes == 0 {
                break;
            }
            while matches!(line.last(), Some(b'\n' | b'\r')) {
                line.pop();
            }
            let leading_file_line = first_line;
            first_line = false;
            if leading_file_line && line.starts_with(&[0xEF, 0xBB, 0xBF]) {
                line.drain(..3);
            }
            if line.is_empty() && (arity != 1 || leading_file_line) {
                continue;
            }
            // Parsing duplicates the raw line into StringRecord storage. Keep this reservation at
            // the reused Vec's high-water capacity instead of issuing two atomics for every row.
            line_reservation.resize((line.capacity() as u64).saturating_mul(2))?;
            let record = if line.is_empty() {
                csv::StringRecord::from(vec![""])
            } else {
                let mut reader = koko_common::csv_dialect::reader_from(line.as_slice(), &dialect);
                match reader.records().next() {
                    Some(Ok(record)) => record,
                    Some(Err(error)) => {
                        return Err(match error.kind() {
                            csv::ErrorKind::Utf8 { .. } => {
                                Error::copy("Invalid UTF8-encoded string.")
                            }
                            _ => Error::Io(error.to_string()),
                        });
                    }
                    None => csv::StringRecord::from(vec![""]),
                }
            };
            let bytes = record.as_byte_record().as_slice().len() as u64
                + std::mem::size_of::<csv::StringRecord>() as u64;
            reservation.resize(reservation.bytes().saturating_add(bytes))?;
            records.push(record);
            if records.len() == VECTOR_CAPACITY {
                let batch = TrackedRecordBatch {
                    records: std::collections::VecDeque::from(records),
                    _reservation: reservation,
                };
                if sender.send(WorkerMessage::Batch(batch)).is_err() {
                    return Ok(());
                }
                records = Vec::with_capacity(VECTOR_CAPACITY);
                reservation = memory.try_reserve(0)?;
            }
        }
        if !records.is_empty()
            && sender
                .send(WorkerMessage::Batch(TrackedRecordBatch {
                    records: std::collections::VecDeque::from(records),
                    _reservation: reservation,
                }))
                .is_err()
        {
            return Ok(());
        }
        Ok(())
    })();
    if let Err(error) = result {
        let _ = sender.send(WorkerMessage::Error(error));
    }
    let _ = sender.send(WorkerMessage::Done);
}
pub(super) fn read_tracked_line<R: BufRead>(
    reader: &mut R,
    line: &mut Vec<u8>,
    reservation: &mut MemoryReservation,
) -> Result<usize> {
    let mut total = 0usize;
    loop {
        let (take, done) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                return Ok(total);
            }
            match available.iter().position(|&byte| byte == b'\n') {
                Some(index) => (index + 1, true),
                None => (available.len(), false),
            }
        };
        let required = (line.len() + take) as u64;
        if required > reservation.bytes() {
            reservation.resize(required)?;
        }
        {
            let available = reader.fill_buf()?;
            line.extend_from_slice(&available[..take]);
        }
        reader.consume(take);
        total += take;
        if done {
            return Ok(total);
        }
    }
}

pub(super) fn split_record_ranges(path: &Path, worker_count: usize) -> Result<Vec<(u64, u64)>> {
    let length = std::fs::metadata(path)
        .map_err(|error| Error::Io(format!("{}: {error}", path.display())))?
        .len();
    let mut first = std::fs::File::open(path)
        .map_err(|error| Error::Io(format!("{}: {error}", path.display())))?;
    let mut bom = [0u8; 3];
    let bom_len = if first.read(&mut bom).unwrap_or(0) == 3 && bom == [0xEF, 0xBB, 0xBF] {
        3
    } else {
        0
    };
    let workers = worker_count
        .max(1)
        .min(length.saturating_sub(bom_len).div_ceil(64 * 1024) as usize)
        .max(1);
    let mut boundaries = vec![bom_len];
    for worker in 1..workers {
        let target = bom_len + (length - bom_len) * worker as u64 / workers as u64;
        let mut file = std::fs::File::open(path)
            .map_err(|error| Error::Io(format!("{}: {error}", path.display())))?;
        file.seek(SeekFrom::Start(target.saturating_sub(1)))
            .map_err(|error| Error::Io(format!("{}: {error}", path.display())))?;
        let mut reader = BufReader::new(file);
        let mut skipped = Vec::new();
        let previous = reader
            .fill_buf()
            .map_err(|error| Error::Io(format!("{}: {error}", path.display())))?
            .first()
            .copied();
        let boundary = if previous == Some(b'\n') {
            target
        } else {
            reader
                .read_until(b'\n', &mut skipped)
                .map_err(|error| Error::Io(format!("{}: {error}", path.display())))?;
            target.saturating_sub(1) + skipped.len() as u64
        };
        if boundary > *boundaries.last().expect("initial boundary") && boundary < length {
            boundaries.push(boundary);
        }
    }
    boundaries.push(length);
    Ok(boundaries
        .windows(2)
        .filter_map(|pair| (pair[0] < pair[1]).then_some((pair[0], pair[1])))
        .collect())
}

/// Per physical line of `path`, whether it is a blank line (empty after the
/// line terminator / a leading BOM). Used to re-inject the interior blank
/// records the `csv` crate drops.
pub(super) fn physical_blank_lines(path: &Path) -> Vec<bool> {
    koko_common::csv_dialect::blank_physical_lines(path).unwrap_or_default()
}

pub(super) struct CheckedRecords {
    inner: csv::StringRecordsIntoIter<std::fs::File>,
    path: PathBuf,
    options: CsvOptions,
    warnings: koko_common::warnings::WarningSink,
    /// Per physical line, whether it is blank (the `csv` crate skips these
    /// without counting them; C++ reads each as a single-empty-field record —
    /// the `alice\n\nbob` null-PK case). Indexed by the count of physical
    /// lines consumed so far.
    blank_lines: Vec<bool>,
    arity: usize,
    /// How many physical lines (blank or record) have been consumed.
    consumed: usize,
    /// Blank records queued for the interior lines before the next record,
    /// plus the real record that follows them (emitted in order).
    pending: std::collections::VecDeque<csv::StringRecord>,
}

impl Iterator for CheckedRecords {
    type Item = Result<csv::StringRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        // Drain any queued blank / held records first.
        if let Some(rec) = self.pending.pop_front() {
            return Some(Ok(rec));
        }
        loop {
            // Queue the blank physical lines standing before the next record.
            while self
                .blank_lines
                .get(self.consumed)
                .copied()
                .unwrap_or(false)
            {
                if self.arity == 1 && self.consumed > 0 {
                    self.pending.push_back(csv::StringRecord::from(vec![""]));
                }
                self.consumed += 1;
            }
            // No more records: any queued trailing blanks are NOT records (a
            // trailing newline never yields one), so drop them via `?`.
            let record = self.inner.next()?;
            let record = match record.map_err(|e| match e.kind() {
                // Invalid UTF-8 is a Copy exception with the file/line/record
                // context (the record text renders lossily).
                csv::ErrorKind::Utf8 { pos, .. } => {
                    let line = pos.as_ref().map(|p| p.line() as usize).unwrap_or(1);
                    wrap_row_error(
                        &self.path,
                        line,
                        "Invalid UTF8-encoded string.",
                        None,
                        &self.options,
                    )
                }
                _ => Error::Io(e.to_string()),
            }) {
                Ok(r) => r,
                Err(e) => return Some(Err(e)),
            };
            // The parallel-reader quoted-newline rule: under IGNORE_ERRORS the
            // row records a warning and is skipped; otherwise it is fatal.
            if let Some((inner, line, fragment)) =
                koko_common::csv_dialect::quoted_newline_parts(&self.path, &record, &self.options)
            {
                if self.options.ignore_errors {
                    self.warnings.push(
                        inner,
                        self.path.to_string_lossy().into_owned(),
                        line,
                        fragment,
                    );
                    continue;
                }
                return Some(Err(koko_common::csv_dialect::validate_record(
                    &self.path,
                    &record,
                    &self.options,
                )
                .expect_err("violation detected above")));
            }
            // Interior blank lines the `csv` crate skipped between the last
            // record and this one are single-empty-field records (C++ reads
            // them so — a 1-column table then hits the NULL-PK constraint).
            // Interior blank physical lines the `csv` crate silently dropped
            // (it doesn't even count them in positions) are single-empty-field
            // records in C++ — a 1-column table then hits the NULL-PK
            // constraint (`alice\n\nbob`). Emit the blanks queued for the
            // lines before this record, then the record.
            self.consumed += 1; // this record consumed one non-blank line
            if !self.pending.is_empty() {
                self.pending.push_back(record);
                return Some(Ok(self.pending.pop_front().expect("queued above")));
            }
            return Some(Ok(record));
        }
    }
}
