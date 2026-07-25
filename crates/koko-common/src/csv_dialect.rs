//! CSV dialect detection — the "parse correctness" layer.
//!
//! A CSV file is two things fused together: a *physical encoding* (bytes → a
//! rectangular grid of raw string cells) and a *semantic interpretation* (raw
//! strings → typed values). This module owns the first layer only. It never
//! looks at what a cell *means*; it answers only "where do fields and records
//! begin and end?" — i.e. the delimiter, quote char, and escape style. Type
//! inference (the second layer) is deliberately out of scope: declare types
//! explicitly (`LOAD WITH HEADERS`, or a target table for `COPY`).
//!
//! [`resolve_dialect`] is the single entry point shared by every read path
//! (`COPY`, typed `LOAD`, bare `LOAD`): given a file and the user's pinned
//! options, it samples the file and picks the dialect that splits it most
//! consistently — anchored to a known column count when the caller has one
//! (COPY / typed LOAD), and by pure self-consistency otherwise (bare LOAD).
//! [`open_reader`] then builds a BOM-skipping streaming reader for that dialect,
//! and [`looks_like_header`] decides whether row 1 is a header using only the
//! *declared* names/types (never inferred ones).

use crate::{Error, LogicalType, Result};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// The UTF-8 byte-order mark, stripped from the front of a file before parsing.
const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// Byte budget read from the file for detection. Big enough to cover plenty of
/// rows of any real CSV, small enough to stay cheap on a huge file; trimmed back
/// to a record boundary so a candidate is never wrongly rejected on a half row.
const SAMPLE_BYTES: usize = 1 << 20; // 1 MiB

/// Cap on records scored per candidate during detection — bounds the work on a
/// large sample while staying a representative consistency check.
const MAX_SAMPLE_ROWS: usize = 1024;
const QUOTED_NEWLINE_MSG: &str = "Quoted newlines are not supported in parallel CSV reader. \
                                  Please specify PARALLEL=FALSE in the options.";
const QUOTED_NEWLINE_CONTEXT_CHARS: usize = 4;

/// Candidate delimiters tried when the delimiter is not pinned, in priority order
/// (comma first, so a genuinely single-column file is not spuriously split).
const DELIMITERS: [u8; 4] = [b',', b';', b'\t', b'|'];
/// Candidate quote characters tried when the quote is not pinned.
const QUOTES: [u8; 2] = [b'"', b'\''];

/// A resolved CSV dialect: exactly the bytes needed to split a file into cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dialect {
    pub delimiter: u8,
    /// The quote char, or `None` when the file does no quoting (a stray quote
    /// byte is then a literal — mirrors the engine disabling quoting when it
    /// detects the chosen quote was never actually used).
    pub quote: Option<u8>,
    /// The escape char inside quoted fields. `None` is RFC-4180 doubling (`""`)
    /// only; `Some(c)` additionally honors `c`-escapes. Doubling stays enabled
    /// either way, so a `Some(b'\\')` reader parses both `\"` and `""` — a
    /// lenient superset that needs no separate escape-detection axis.
    pub escape: Option<u8>,
}

impl Default for Dialect {
    /// The non-auto-detect fallback: comma / double-quote, with backslash *and*
    /// doubled escapes honored — matching the loader's historical reader.
    fn default() -> Self {
        Dialect {
            delimiter: b',',
            quote: Some(b'"'),
            escape: Some(b'\\'),
        }
    }
}

/// User-facing CSV reader options shared by `COPY` and typed `LOAD FROM`.
///
/// Dialect pins are `None` when the user did not set that axis; `resolve_dialect`
/// then auto-detects them unless `auto_detect=false`. `file_format` is retained so
/// the binder can validate C++ file-type semantics (`.csv` by extension, or
/// explicit `file_format='csv'`) before the reader opens the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvOptions {
    pub delimiter: Option<u8>,
    pub quote: Option<u8>,
    pub escape: Option<u8>,
    pub header: Option<bool>,
    pub skip: usize,
    pub auto_detect: bool,
    pub parallel: bool,
    pub null_strings: Vec<String>,
    pub file_format: Option<String>,
    /// Number of data rows used for dialect/header/type sniffing. Zero is
    /// normalized by the binder to the C++ default of 256.
    pub sample_size: usize,
    /// Permit semicolon-separated list elements without outer brackets.
    pub list_unbraced: bool,
    /// Relationship-group endpoint selectors. They are retained with the
    /// reader options but do not affect file resolution or CSV dialects.
    pub from: Option<String>,
    pub to: Option<String>,
    /// `IGNORE_ERRORS=true` — skip constraint-violating rows (honored by the
    /// query-source COPY; the CSV path still rejects it).
    pub ignore_errors: bool,
}

impl Default for CsvOptions {
    fn default() -> Self {
        Self {
            delimiter: None,
            quote: None,
            escape: None,
            header: None,
            skip: 0,
            auto_detect: true,
            parallel: true,
            // C++ defaults `null_strings` to exactly `[""]` for STRING columns.
            null_strings: vec![String::new()],
            file_format: None,
            sample_size: 256,
            list_unbraced: false,
            from: None,
            to: None,
            ignore_errors: false,
        }
    }
}

/// Resolve the dialect of `path` from the user's pinned axes. A `Some` pin is
/// fixed; a `None` pin is detected from a file sample (or, when `auto_detect` is
/// false, taken from [`Dialect::default`]). `known_arity` is the caller's column
/// count when it has one (the table schema for `COPY`, the type list for typed
/// `LOAD`); `None` for bare `LOAD`, where the delimiter is chosen by pure
/// row-width consistency instead.
pub fn resolve_dialect(
    path: &Path,
    delimiter: Option<u8>,
    quote: Option<u8>,
    escape: Option<u8>,
    auto_detect: bool,
    known_arity: Option<usize>,
) -> Result<Dialect> {
    // Escape is not a detection axis: backslash-plus-doubling is a superset that
    // reads every escape style in the corpus, so we only honor an explicit pin.
    let escape = escape.or(Some(b'\\'));
    if !auto_detect {
        return Ok(Dialect {
            delimiter: delimiter.unwrap_or(b','),
            quote: Some(quote.unwrap_or(b'"')),
            escape,
        });
    }
    let sample = read_sample(path, SAMPLE_BYTES)?;
    Ok(detect_from_sample(
        &sample,
        delimiter,
        quote,
        escape,
        known_arity,
    ))
}

/// The path to actually read. Gzip input is validated and fully decompressed
/// before a cache file is published, so corrupt archives cannot leave a
/// readable partial file behind. Both `.gz` and `.gzip` are accepted.
fn readable_path(path: &Path) -> Result<PathBuf> {
    let is_gzip = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("gz") || ext.eq_ignore_ascii_case("gzip"));
    if !is_gzip {
        return Ok(path.to_path_buf());
    }

    let mut source =
        File::open(path).map_err(|error| Error::Io(format!("{}: {error}", path.display())))?;
    let mut header = [0u8; 10];
    if source.read_exact(&mut header).is_err() || header[0..2] != [0x1f, 0x8b] {
        return Err(Error::Io("Input is not a GZIP stream.".to_string()));
    }
    if header[2] != 8 {
        return Err(Error::Io(
            "Unsupported GZIP compression method.".to_string(),
        ));
    }
    if header[3] & 0b1110_0000 != 0 {
        return Err(Error::Io("Unsupported GZIP archive.".to_string()));
    }

    let metadata = std::fs::metadata(path)
        .map_err(|error| Error::Io(format!("{}: {error}", path.display())))?;
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0u128, |duration| duration.as_nanos());
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path
        .to_string_lossy()
        .bytes()
        .chain(mtime.to_le_bytes())
        .chain(metadata.len().to_le_bytes())
    {
        hash = (hash ^ byte as u64).wrapping_mul(0x0100_0000_01b3);
    }
    let cached = std::env::temp_dir().join(format!("koko_gzip_{hash:016x}.csv"));
    if cached.exists() {
        return Ok(cached);
    }

    source
        .seek(SeekFrom::Start(0))
        .map_err(|error| Error::Io(format!("{}: {error}", path.display())))?;
    let mut decoder = flate2::read::MultiGzDecoder::new(source);
    let staging =
        std::env::temp_dir().join(format!("koko_gzip_{hash:016x}_{}.tmp", std::process::id()));
    let mut output = File::create(&staging)
        .map_err(|error| Error::Io(format!("{}: {error}", path.display())))?;
    if let Err(error) = std::io::copy(&mut decoder, &mut output) {
        drop(output);
        let _ = std::fs::remove_file(&staging);
        return Err(Error::Io(error.to_string()));
    }
    drop(output);
    match std::fs::rename(&staging, &cached) {
        Ok(()) => {}
        Err(_) if cached.exists() => {
            let _ = std::fs::remove_file(&staging);
        }
        Err(error) => {
            let _ = std::fs::remove_file(&staging);
            return Err(Error::Io(format!("{}: {error}", path.display())));
        }
    }
    Ok(cached)
}

/// Open a BOM-skipping streaming CSV reader for `path`, configured for `dialect`.
/// `has_headers(false)` because header handling is the caller's (it needs the
/// declared schema); `flexible(true)` so a ragged row reaches the caller's own
/// field-count check rather than aborting the read.
pub fn open_reader(path: &Path, dialect: &Dialect) -> Result<csv::Reader<File>> {
    let read_path = readable_path(path)?;
    let mut file =
        File::open(&read_path).map_err(|e| Error::Io(format!("{}: {e}", path.display())))?;
    skip_bom(&mut file).map_err(|e| Error::Io(format!("{}: {e}", path.display())))?;
    Ok(reader_from(file, dialect))
}

/// Build a CSV reader over an already-positioned bounded input. Parallel local
/// readers use this after seeking to independent record-boundary ranges.
pub fn reader_from<R: Read>(reader: R, dialect: &Dialect) -> csv::Reader<R> {
    let mut builder = csv::ReaderBuilder::new();
    configure(&mut builder, dialect);
    builder.from_reader(reader)
}

/// Whether the decoded file contains a physical newline while a quoted field
/// is open. Such files stay on the serial reader so record boundaries are not
/// split across workers (and the existing PARALLEL diagnostic remains exact).
pub fn has_quoted_newline(path: &Path, dialect: &Dialect) -> Result<bool> {
    let Some(quote) = dialect.quote else {
        return Ok(false);
    };
    let read_path = readable_path(path)?;
    let file =
        File::open(read_path).map_err(|error| Error::Io(format!("{}: {error}", path.display())))?;
    let mut reader = BufReader::new(file);
    let mut buffer = [0u8; 64 * 1024];
    let mut in_quote = false;
    let mut escaped = false;
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| Error::Io(format!("{}: {error}", path.display())))?;
        if count == 0 {
            return Ok(false);
        }
        for &byte in &buffer[..count] {
            if in_quote && escaped {
                escaped = false;
                continue;
            }
            if in_quote && dialect.escape == Some(byte) {
                escaped = true;
            } else if byte == quote {
                in_quote = !in_quote;
            } else if in_quote && matches!(byte, b'\n' | b'\r') {
                return Ok(true);
            }
        }
    }
}

/// Read one decoded record after a caller has already validated the file's
/// structural quote/escape rules.
pub fn read_prevalidated_record(
    reader: &mut csv::Reader<File>,
    rec: &mut csv::StringRecord,
    path: &Path,
    options: &CsvOptions,
) -> Result<bool> {
    reader.read_record(rec).map_err(|e| match e.kind() {
        csv::ErrorKind::Utf8 { pos, .. } => {
            let line = pos.as_ref().map(|p| p.line() as usize).unwrap_or(1);
            let dialect = resolve_dialect(
                path,
                options.delimiter,
                options.quote,
                options.escape,
                options.auto_detect,
                None,
            )
            .unwrap_or_default();
            wrap_row_error(path, line, "Invalid UTF8-encoded string.", None, &dialect)
        }
        _ => Error::Io(format!("{}: {e}", path.display())),
    })
}

/// Read one CSV record and enforce option-sensitive record-boundary semantics.
pub fn read_record(
    reader: &mut csv::Reader<File>,
    rec: &mut csv::StringRecord,
    path: &Path,
    options: &CsvOptions,
) -> Result<bool> {
    let has_row = read_prevalidated_record(reader, rec, path, options)?;
    if has_row {
        validate_record(path, rec, options)?;
    } else {
        // At EOF the C++ reader surfaces a trailing malformed quoted field the
        // `csv` crate silently accepts: a quote that never closed, or an escape
        // char immediately before EOF.
        if !options.ignore_errors {
            validate_file_structure(path, options)?;
        }
    }
    Ok(has_row)
}
fn read_sniff_record(
    reader: &mut csv::Reader<File>,
    record: &mut csv::StringRecord,
    path: &Path,
    options: &CsvOptions,
) -> Result<bool> {
    loop {
        match read_record(reader, record, path, options) {
            Ok(has_record) => return Ok(has_record),
            Err(error) if options.ignore_errors && !matches!(error, Error::Parser(_)) => {
                record.clear();
            }
            Err(error) => return Err(error),
        }
    }
}

/// Validate a file's quoted-field escape structure the way the C++ reader does
/// but the `csv` crate does not: scan the physical bytes tracking quote/escape
/// state and reject
/// * a quoted field left open at EOF (`unterminated quotes.`),
/// * a dangling escape char right before EOF (`escape at end of file.`), and
/// * an escape char followed by anything other than the quote or escape char
///   (`neither QUOTE nor ESCAPE is proceeded by ESCAPE.` — the C++ typo).
///
/// The record text is the offending physical line, cut after the failing byte
/// with `...` when more of the line follows.
pub fn validate_file_structure(path: &Path, options: &CsvOptions) -> Result<()> {
    let dialect = resolve_dialect(
        path,
        options.delimiter,
        options.quote,
        options.escape,
        options.auto_detect,
        None,
    )?;
    let Some(quote) = dialect.quote else {
        return Ok(());
    };
    let read_path = readable_path(path)?;
    let file =
        File::open(read_path).map_err(|error| Error::Io(format!("{}: {error}", path.display())))?;
    let mut reader = BufReader::new(file);
    // A record-text renderer that cuts physical `line` after byte offset `col`
    // (inclusive), appending `...` when the line has more bytes.
    let record_upto = |line: usize, col: usize| -> String {
        let full = physical_line_text(path, line as u64);
        let fb = full.as_bytes();
        let end = (col + 1).min(fb.len());
        let mut s = String::from_utf8_lossy(&fb[..end]).into_owned();
        if end < fb.len() {
            s.push_str("...");
        }
        s
    };
    let mut in_quote = false;
    let mut pending_escape = false;
    let mut line = 1usize;
    let mut col = 0usize;
    let mut field_line = 1usize;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| Error::Io(format!("{}: {error}", path.display())))?;
        if count == 0 {
            break;
        }
        for &byte in &buffer[..count] {
            if in_quote {
                if pending_escape {
                    pending_escape = false;
                    if dialect.escape == Some(quote) {
                        if byte != quote {
                            if !matches!(byte, b'\n' | b'\r') && byte != dialect.delimiter {
                                let raw = record_upto(field_line, col);
                                return Err(Error::copy(format!(
                                    "Error in file {} on line {field_line}: quote should be followed \
                                     by end of file, end of value, end of row or another quote. \
                                     Line/record containing the error: '{raw}'",
                                    path.display(),
                                )));
                            }
                            in_quote = false;
                        }
                    } else if byte != quote
                        && dialect.escape != Some(byte)
                        && options.escape.is_some()
                    {
                        let raw = record_upto(field_line, col);
                        return Err(Error::copy(format!(
                            "Error in file {} on line {field_line}: neither QUOTE nor ESCAPE is \
                             proceeded by ESCAPE. Line/record containing the error: '{raw}'",
                            path.display(),
                        )));
                    }
                } else if dialect.escape == Some(byte) {
                    pending_escape = true;
                } else if byte == quote {
                    in_quote = false;
                }
            } else if byte == quote {
                in_quote = true;
                field_line = line;
            }
            if byte == b'\n' {
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
    }
    if pending_escape && dialect.escape == Some(quote) {
        in_quote = false;
    }
    if in_quote {
        let inner = if pending_escape {
            "escape at end of file."
        } else {
            "unterminated quotes."
        };
        return Err(wrap_row_error(path, field_line, inner, None, &dialect));
    }
    Ok(())
}

/// Return the C++ structural CSV diagnostic for one decoded record, if its
/// opening physical line contains an invalid escape or a closing quote followed
/// by a non-boundary byte. The `csv` crate accepts both forms permissively.
pub fn invalid_record_parts(
    path: &Path,
    rec: &csv::StringRecord,
    options: &CsvOptions,
) -> Option<(String, u64, String)> {
    let mut dialect = resolve_dialect(
        path,
        options.delimiter,
        options.quote,
        options.escape,
        options.auto_detect,
        Some(rec.len()),
    )
    .ok()?;
    if rec.len() == 1
        && options.quote.is_none()
        && std::fs::read(path).is_ok_and(|bytes| {
            bytes
                .split(|&byte| byte == b'\n')
                .skip(1)
                .any(|line| line.first() == Some(&b'"'))
        })
    {
        dialect.quote = Some(b'"');
    }
    invalid_record_parts_with_dialect(path, rec, options, dialect)
}

/// The cached-dialect form used by COPY's row loop.
pub fn invalid_record_parts_with_dialect(
    path: &Path,
    rec: &csv::StringRecord,
    options: &CsvOptions,
    dialect: Dialect,
) -> Option<(String, u64, String)> {
    let quote = dialect.quote?;
    let line = rec.position().map_or(1, |position| position.line());
    let raw = physical_record_text(path, rec);
    let bytes = raw.as_bytes();
    let fragment = |end: usize| {
        let end = end.min(bytes.len());
        let mut value = String::from_utf8_lossy(&bytes[..end]).into_owned();
        if end < bytes.len() {
            value.push_str("...");
        }
        value
    };
    let mut index = 0;
    let mut field_start = true;
    let mut in_quote = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_quote {
            if dialect.escape == Some(byte) && byte != quote {
                let Some(&next) = bytes.get(index + 1) else {
                    return Some((
                        "escape at end of file.".to_string(),
                        line,
                        fragment(index + 1),
                    ));
                };
                if next != quote && dialect.escape != Some(next) {
                    if options.escape.is_some() {
                        return Some((
                            "neither QUOTE nor ESCAPE is proceeded by ESCAPE.".to_string(),
                            line,
                            fragment(index + 2),
                        ));
                    }
                    index += 1;
                    continue;
                }
                index += 2;
                continue;
            }
            if byte == quote {
                match bytes.get(index + 1).copied() {
                    Some(next) if next == quote => {
                        index += 2;
                        continue;
                    }
                    Some(next) if next == dialect.delimiter => {
                        in_quote = false;
                        field_start = true;
                    }
                    None => {
                        in_quote = false;
                    }
                    Some(_) => {
                        return Some((
                            "quote should be followed by end of file, end of value, end of row or another quote."
                                .to_string(),
                            line,
                            fragment(index + 1),
                        ));
                    }
                }
            }
        } else if field_start && byte == quote {
            in_quote = true;
            field_start = false;
        } else {
            field_start = byte == dialect.delimiter;
        }
        index += 1;
    }
    if in_quote && !options.parallel {
        let decoded = std::fs::read(readable_path(path).ok()?).ok()?;
        let mut physical_line = 1u64;
        let mut tail_start = None;
        for (offset, byte) in decoded.iter().enumerate() {
            if *byte == b'\n' {
                if physical_line == line {
                    tail_start = Some(offset + 1);
                    break;
                }
                physical_line += 1;
            }
        }
        let mut continuation = tail_start.unwrap_or(decoded.len());
        let mut pending_escape = false;
        while continuation < decoded.len() {
            let byte = decoded[continuation];
            if pending_escape {
                pending_escape = false;
            } else if dialect.escape == Some(byte) {
                pending_escape = true;
            } else if byte == quote {
                if decoded.get(continuation + 1) == Some(&quote) {
                    continuation += 1;
                } else {
                    in_quote = false;
                    break;
                }
            }
            continuation += 1;
        }
        if in_quote {
            return Some(("unterminated quotes.".to_string(), line, raw));
        }
    }
    None
}

/// Validate a decoded record against options that the `csv` crate itself accepts
/// more broadly than the C++ reader. In the C++ boundary, embedded newlines are
/// legal only in the serial CSV reader selected by `PARALLEL=FALSE`.
pub fn validate_record(path: &Path, rec: &csv::StringRecord, options: &CsvOptions) -> Result<()> {
    if let Some((message, line, fragment)) = invalid_record_parts(path, rec, options) {
        return Err(Error::copy(format!(
            "Error in file {} on line {line}: {message} Line/record containing the error: '{fragment}'",
            path.display()
        )));
    }
    if options.parallel && record_contains_newline(rec) {
        return Err(quoted_newline_error(path, rec));
    }
    Ok(())
}

/// The quoted-newline violation's warning parts (inner message, physical
/// line, record fragment), when `rec` violates the parallel-reader rule.
pub fn quoted_newline_parts(
    path: &Path,
    rec: &csv::StringRecord,
    options: &CsvOptions,
) -> Option<(String, u64, String)> {
    if options.parallel && record_contains_newline(rec) {
        let line = rec.position().map_or(1, |pos| pos.line());
        let fragment = physical_line_text(path, line);
        Some((QUOTED_NEWLINE_MSG.to_string(), line, fragment))
    } else {
        None
    }
}

fn record_contains_newline(rec: &csv::StringRecord) -> bool {
    rec.iter()
        .any(|field| field.as_bytes().iter().any(|&b| b == b'\n' || b == b'\r'))
}

fn quoted_newline_error(path: &Path, rec: &csv::StringRecord) -> Error {
    let line = rec.position().map_or(1, |pos| pos.line());
    let fragment = physical_line_prefix(path, line).unwrap_or_default();
    Error::copy(format!(
        "Error in file {} on line {}: {} Line/record containing the error: '{}'",
        path.display(),
        line,
        QUOTED_NEWLINE_MSG,
        fragment
    ))
}

pub fn physical_record_text(path: &Path, rec: &csv::StringRecord) -> String {
    let Some(position) = rec.position() else {
        return physical_line_text(path, 1);
    };
    let read_path = match readable_path(path) {
        Ok(path) => path,
        Err(_) => return String::new(),
    };
    let mut file = match File::open(read_path) {
        Ok(file) => file,
        Err(_) => return String::new(),
    };
    if file.seek(SeekFrom::Start(position.byte())).is_err() {
        return String::new();
    }
    let mut buffer = Vec::new();
    if BufReader::new(file).read_until(b'\n', &mut buffer).is_err() {
        return String::new();
    }
    if position.byte() == 0 && buffer.starts_with(&BOM) {
        buffer.drain(0..BOM.len());
    }
    while matches!(buffer.last(), Some(b'\n' | b'\r')) {
        buffer.pop();
    }
    String::from_utf8_lossy(&buffer).into_owned()
}

/// The full raw text of a file's `target_line` (1-based) for diagnostics.
pub fn physical_line_text(path: &Path, target_line: u64) -> String {
    read_physical_line(path, target_line)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

fn read_physical_line(path: &Path, target_line: u64) -> std::io::Result<Vec<u8>> {
    let read_path =
        readable_path(path).map_err(|error| std::io::Error::other(error.to_string()))?;
    let file = File::open(read_path)?;
    let mut reader = BufReader::new(file);
    let mut buffer = Vec::new();
    for _ in 0..target_line {
        buffer.clear();
        if reader.read_until(b'\n', &mut buffer)? == 0 {
            return Ok(Vec::new());
        }
    }
    if target_line == 1 && buffer.starts_with(&BOM) {
        buffer.drain(0..BOM.len());
    }
    while matches!(buffer.last(), Some(b'\n' | b'\r')) {
        buffer.pop();
    }
    Ok(buffer)
}

pub fn physical_line_prefix(path: &Path, target_line: u64) -> std::io::Result<String> {
    Ok(
        String::from_utf8_lossy(&read_physical_line(path, target_line)?)
            .chars()
            .take(QUOTED_NEWLINE_CONTEXT_CHARS)
            .collect(),
    )
}

/// Per decoded physical line, whether it is empty. This is intentionally a
/// compact side index rather than row materialization, and works for gzip too.
pub fn blank_physical_lines(path: &Path) -> Result<Vec<bool>> {
    let read_path = readable_path(path)?;
    let mut file =
        File::open(read_path).map_err(|error| Error::Io(format!("{}: {error}", path.display())))?;
    skip_bom(&mut file).map_err(|error| Error::Io(format!("{}: {error}", path.display())))?;
    let mut reader = BufReader::new(file);
    let mut result = Vec::new();
    let mut has_content = false;
    let mut have_line = false;
    let mut previous_cr = false;
    loop {
        let available = reader
            .fill_buf()
            .map_err(|error| Error::Io(format!("{}: {error}", path.display())))?;
        if available.is_empty() {
            break;
        }
        let consumed = available.len();
        for &byte in available {
            match byte {
                b'\n' if previous_cr => previous_cr = false,
                b'\n' => {
                    result.push(!has_content);
                    has_content = false;
                    have_line = false;
                }
                b'\r' => {
                    result.push(!has_content);
                    has_content = false;
                    have_line = false;
                    previous_cr = true;
                }
                _ => {
                    has_content = true;
                    have_line = true;
                    previous_cr = false;
                }
            }
        }
        reader.consume(consumed);
    }
    if have_line {
        result.push(!has_content);
    }
    Ok(result)
}
/// Normalize a `LIST_UNBRACED=true` cell into the ordinary bracket/comma
/// literal syntax consumed by the shared value caster. Semicolons separate
/// list elements, and nested list targets recursively wrap scalar elements.
/// Non-list targets and disabled mode borrow the original text.
pub fn normalize_unbraced_list<'a>(
    value: &'a str,
    logical_type: &LogicalType,
    enabled: bool,
) -> std::borrow::Cow<'a, str> {
    if !enabled {
        return std::borrow::Cow::Borrowed(value);
    }
    fn strip_outer(value: &str) -> Option<&str> {
        let bytes = value.as_bytes();
        if bytes.first() != Some(&b'[') || bytes.last() != Some(&b']') {
            return None;
        }
        let mut depth = 0usize;
        for (index, byte) in bytes.iter().copied().enumerate() {
            match byte {
                b'[' => depth += 1,
                b']' => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 && index + 1 != bytes.len() {
                        return None;
                    }
                }
                _ => {}
            }
        }
        (depth == 0).then_some(&value[1..value.len() - 1])
    }

    fn normalize(value: &str, logical_type: &LogicalType) -> Option<String> {
        let child = match logical_type {
            LogicalType::List(child) | LogicalType::Array(child, _) => child.as_ref(),
            _ => return None,
        };
        let trimmed = value.trim();
        let inner = strip_outer(trimmed).unwrap_or(trimmed);
        let semicolon = crate::types::split_top_level(inner, ';');
        let parts = if semicolon.len() > 1 {
            semicolon
        } else if strip_outer(trimmed).is_some() {
            crate::types::split_top_level(inner, ',')
        } else {
            vec![inner]
        };
        let mut result = String::from("[");
        for (index, part) in parts.into_iter().enumerate() {
            if index > 0 {
                result.push(',');
            }
            let part = part.trim();
            if let Some(nested) = normalize(part, child) {
                result.push_str(&nested);
            } else {
                result.push_str(part);
            }
        }
        result.push(']');
        Some(result)
    }

    normalize(value, logical_type)
        .map(std::borrow::Cow::Owned)
        .unwrap_or(std::borrow::Cow::Borrowed(value))
}

/// Whether `rec` is the file's header row, judged against the *declared* schema
/// (never inferred types): same arity, and either every field matches a declared
/// column name (case-insensitively) or some field fails to parse as its declared
/// type (e.g. a `id:ID(Comment)` label over an `INT64` column). Empty cells parse
/// as null, so they never falsely flag a data row as a header.
pub fn looks_like_header(
    rec: &csv::StringRecord,
    col_names: &[String],
    col_types: &[LogicalType],
) -> bool {
    rec.len() == col_names.len()
        && rec.iter().zip(col_types).any(|(f, t)| {
            // Only *structural* parse failures mark a header row (C++
            // sniffs leniently by SHAPE): a bracket-shaped cell for a
            // LIST/ARRAY column, or a brace-shaped one for STRUCT/MAP, is
            // data even when its content fails to parse.
            let mut f_trim = f.trim();
            for q in ['"', '\''] {
                if f_trim.len() >= 2 && f_trim.starts_with(q) && f_trim.ends_with(q) {
                    f_trim = f_trim[1..f_trim.len() - 1].trim();
                }
            }
            let shape_matches = match t {
                LogicalType::List(_) | LogicalType::Array(_, _) => f_trim.starts_with('['),
                LogicalType::Struct(_) | LogicalType::Map(_, _) => f_trim.starts_with('{'),
                LogicalType::Union(_) => true,
                _ => false,
            };
            !shape_matches && crate::literal::parse_value_literal(f, t).is_err()
        })
}

/// Sniff a bare `LOAD FROM`'s column *names* (not types) from the file: resolve
/// the dialect, then read row 1. With `has_header`, the row's cells are the names
/// (blanks become `columnN`); otherwise the columns are named `column0..columnN`.
/// The arity is the returned length. Types are the caller's concern — bare `LOAD`
/// treats every column as `STRING` (value-type inference is out of scope).
pub fn sniff_columns(
    path: &Path,
    delimiter: Option<u8>,
    quote: Option<u8>,
    escape: Option<u8>,
    auto_detect: bool,
    parallel: bool,
    has_header: bool,
) -> Result<Vec<String>> {
    let dialect = resolve_dialect(path, delimiter, quote, escape, auto_detect, None)?;
    let mut reader = open_reader(path, &dialect)?;
    let mut rec = csv::StringRecord::new();
    let read_options = CsvOptions {
        delimiter,
        quote,
        escape,
        auto_detect,
        parallel,
        ..CsvOptions::default()
    };
    let has_row = read_record(&mut reader, &mut rec, path, &read_options)?;
    if !has_row {
        return Err(Error::runtime(format!(
            "LOAD FROM \"{}\": file has no rows to determine columns from.",
            path.display()
        )));
    }
    Ok(if has_header {
        rec.iter()
            .enumerate()
            .map(|(i, f)| {
                let n = f.trim();
                if n.is_empty() {
                    format!("column{i}")
                } else {
                    n.to_string()
                }
            })
            .collect()
    } else {
        (0..rec.len()).map(|i| format!("column{i}")).collect()
    })
}

/// Infer per-column types for a bare `LOAD FROM` the C++ way: sample up to the
/// configured number of data rows (past the header when present) and type each column from the
/// shapes its non-empty cells all share — INT64, DECIMAL(int+frac, frac)
/// (DOUBLE once the needed precision exceeds 38, or for exponent forms),
/// BOOL, DATE, TIMESTAMP, UUID; everything else (bracketed list text
/// included — kept permissive) stays STRING.
pub fn sniff_column_types(
    path: &Path,
    options: &CsvOptions,
    has_header: bool,
    num_cols: usize,
) -> Result<Vec<crate::LogicalType>> {
    let mut dialect = resolve_dialect(
        path,
        options.delimiter,
        options.quote,
        options.escape,
        options.auto_detect,
        None,
    )?;
    if num_cols == 1
        && options.quote.is_none()
        && std::fs::read(path).is_ok_and(|bytes| {
            bytes
                .split(|&byte| byte == b'\n')
                .skip(1)
                .any(|line| line.first() == Some(&b'"'))
        })
    {
        dialect.quote = Some(b'"');
    }
    let mut reader = open_reader(path, &dialect)?;
    let mut rec = csv::StringRecord::new();
    let mut tokens = vec![Vec::<String>::new(); num_cols];
    let mut first = true;
    let mut sampled = 0;
    while sampled < options.sample_size && read_sniff_record(&mut reader, &mut rec, path, options)?
    {
        if first && has_header {
            first = false;
            continue;
        }
        first = false;
        for (i, field) in rec.iter().enumerate().take(num_cols) {
            let field = field.trim();
            if !field.is_empty() {
                tokens[i].push(field.to_string());
            }
        }
        sampled += 1;
    }
    Ok(tokens
        .iter()
        .map(|column| infer_tokens(column.iter().map(String::as_str).collect()))
        .collect())
}

fn infer_tokens(tokens: Vec<&str>) -> crate::LogicalType {
    use crate::LogicalType as LT;
    let tokens: Vec<&str> = tokens
        .into_iter()
        .map(str::trim)
        .filter(|token| !token.is_empty() && !token.eq_ignore_ascii_case("null"))
        .collect();
    if tokens.is_empty() {
        return LT::String;
    }
    if tokens
        .iter()
        .all(|token| token.starts_with('[') && token.ends_with(']'))
    {
        let mut children = Vec::new();
        for token in &tokens {
            children.extend(
                crate::types::split_top_level(&token[1..token.len() - 1], ',')
                    .into_iter()
                    .map(str::trim)
                    .filter(|child| !child.is_empty()),
            );
        }
        return LT::List(Box::new(infer_tokens(children)));
    }
    if tokens
        .iter()
        .all(|token| token.starts_with('{') && token.ends_with('}'))
    {
        let mut entries = Vec::new();
        for token in &tokens {
            entries.extend(
                crate::types::split_top_level(&token[1..token.len() - 1], ',')
                    .into_iter()
                    .map(str::trim)
                    .filter(|entry| !entry.is_empty()),
            );
        }
        let is_struct = entries
            .iter()
            .all(|entry| split_once_nested(entry, ':').is_some());
        if is_struct {
            let mut fields: Vec<(String, Vec<&str>)> = Vec::new();
            for entry in entries {
                let (name, value) = split_once_nested(entry, ':').expect("checked above");
                let name = name.trim().trim_matches(['\'', '"']).to_string();
                if let Some((_, values)) = fields.iter_mut().find(|(field, _)| field == &name) {
                    values.push(value);
                } else {
                    fields.push((name, vec![value]));
                }
            }
            return LT::Struct(
                fields
                    .into_iter()
                    .map(|(name, values)| (name, infer_tokens(values)))
                    .collect(),
            );
        }
        if entries
            .iter()
            .all(|entry| split_once_nested(entry, '=').is_some())
        {
            let mut keys = Vec::new();
            let mut values = Vec::new();
            for entry in entries {
                let (key, value) = split_once_nested(entry, '=').expect("checked above");
                if !key.trim().is_empty() {
                    keys.push(key);
                }
                if !value.trim().is_empty() {
                    values.push(value);
                }
            }
            return LT::Map(Box::new(infer_tokens(keys)), Box::new(infer_tokens(values)));
        }
    }
    let mut shape = ColShape::new();
    for token in tokens {
        shape.saw = true;
        shape.update(token);
    }
    shape.finalize()
}

fn split_once_nested(value: &str, separator: char) -> Option<(&str, &str)> {
    let mut stack = Vec::new();
    let mut quote = None;
    for (index, ch) in value.char_indices() {
        if let Some(active) = quote {
            if ch == active {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '[' | '{' | '(' => stack.push(ch),
            ']' | '}' | ')' => {
                stack.pop();
            }
            _ if ch == separator && stack.is_empty() => {
                return Some((&value[..index], &value[index + ch.len_utf8()..]));
            }
            _ => {}
        }
    }
    None
}

/// Accumulated value-shape evidence for one sniffed column (or the elements of
/// a LIST column).
#[derive(Clone)]
struct ColShape {
    saw: bool,
    all_int: bool,
    all_i128: bool,
    all_dec: bool,
    all_f64: bool,
    all_bool: bool,
    all_date: bool,
    all_ts: bool,
    all_uuid: bool,
    all_interval: bool,
    max_int_digits: u32,
    max_frac_digits: u32,
}

impl ColShape {
    fn new() -> Self {
        ColShape {
            saw: false,
            all_int: true,
            all_i128: true,
            all_dec: true,
            all_f64: true,
            all_bool: true,
            all_date: true,
            all_ts: true,
            all_uuid: true,
            all_interval: true,
            max_int_digits: 0,
            max_frac_digits: 0,
        }
    }

    /// Fold one non-empty token into the shape.
    fn update(&mut self, t: &str) {
        // Plain decimal shape `[+-]?digits[.digits]` — no exponent.
        fn dec_shape(t: &str) -> Option<(u32, u32)> {
            let t = t.strip_prefix(['+', '-']).unwrap_or(t);
            let (int_part, frac_part) = match t.split_once('.') {
                Some((i, f)) => (i, f),
                None => (t, ""),
            };
            if int_part.is_empty() && frac_part.is_empty() {
                return None;
            }
            if !int_part.bytes().all(|b| b.is_ascii_digit())
                || !frac_part.bytes().all(|b| b.is_ascii_digit())
            {
                return None;
            }
            // A redundant leading zero is not decimal-shaped ("00.5" → STRING),
            // and a bare "0" still counts as one integer digit (DECIMAL(3, 2)
            // for "0.25").
            if int_part.len() > 1 && int_part.starts_with('0') {
                return None;
            }
            Some((int_part.len() as u32, frac_part.len() as u32))
        }
        let integer_shape = t.strip_prefix(['+', '-']).is_some_and(|digits| {
            !digits.is_empty()
                && digits.bytes().all(|byte| byte.is_ascii_digit())
                && (digits.len() == 1 || !digits.starts_with('0'))
        }) || (!t.starts_with(['+', '-'])
            && !t.is_empty()
            && t.bytes().all(|byte| byte.is_ascii_digit())
            && (t.len() == 1 || !t.starts_with('0')));
        self.all_int &= integer_shape && t.parse::<i64>().is_ok();
        self.all_i128 &= integer_shape && t.parse::<i128>().is_ok();
        let unsigned = t.strip_prefix(['+', '-']).unwrap_or(t);
        let integer_part = unsigned
            .split_once('.')
            .map_or(unsigned, |(integer, _)| integer);
        let redundant_leading_zero = integer_part.len() > 1
            && integer_part.starts_with('0')
            && integer_part.bytes().all(|byte| byte.is_ascii_digit());
        match dec_shape(t) {
            Some((ints, fracs)) => {
                self.max_int_digits = self.max_int_digits.max(ints);
                self.max_frac_digits = self.max_frac_digits.max(fracs);
            }
            None => self.all_dec = false,
        }
        self.all_f64 &= !redundant_leading_zero && t.parse::<f64>().is_ok();
        self.all_bool &= t.eq_ignore_ascii_case("true") || t.eq_ignore_ascii_case("false");
        let normalized_date = t.replace(['/', '\\', ' '], "-");
        self.all_date &= crate::temporal::parse_date(&normalized_date).is_some();
        self.all_ts &= crate::temporal::parse_timestamp(t).is_some();
        // The C++ sniffer only recognizes the canonical dashed UUID form
        // (a dashless hex string stays STRING).
        self.all_uuid &= t.len() == 36
            && t.as_bytes().get(8) == Some(&b'-')
            && crate::scalar::parse_uuid(t).is_some();
        self.all_interval &= crate::temporal::parse_interval(t).is_ok();
    }

    /// The narrowest type consistent with everything folded in.
    fn finalize(&self) -> crate::LogicalType {
        use crate::LogicalType as LT;
        if self.all_int {
            return LT::Int64;
        }
        if self.all_i128 {
            return LT::Int(crate::IntKind::I128);
        }
        if self.all_dec {
            let precision = self.max_int_digits + self.max_frac_digits;
            if precision <= 38 {
                return LT::Decimal(precision.max(1) as u8, self.max_frac_digits as u8);
            }
            return LT::Double;
        }
        if self.all_f64 {
            return LT::Double;
        }
        if self.all_bool {
            return LT::Bool;
        }
        if self.all_date {
            return LT::Date;
        }
        if self.all_ts {
            return LT::Timestamp;
        }
        if self.all_uuid {
            return LT::Uuid;
        }
        if self.all_interval {
            return LT::Interval;
        }
        LT::String
    }
}

/// Decide whether a bare `LOAD FROM`'s first row is a header the C++ way
/// (audit W3): a header exists iff some column's data rows all parse as numbers
/// while the first row's cell does not (C++ sniffs data types and detects the
/// header by type conflict; numeric conflict covers the practical cases since
/// bare LOAD otherwise types everything STRING). Returns the decision plus the
/// column names — row-1 cells when a header, else `column0..N`. Samples up to
/// `sample_size` data rows.
pub fn sniff_bare_columns(path: &Path, options: &CsvOptions) -> Result<(bool, Vec<String>)> {
    let dialect = resolve_dialect(
        path,
        options.delimiter,
        options.quote,
        options.escape,
        options.auto_detect,
        None,
    )?;
    let mut reader = open_reader(path, &dialect)?;
    let mut rec = csv::StringRecord::new();
    if !read_sniff_record(&mut reader, &mut rec, path, options)? {
        return Err(Error::runtime(format!(
            "LOAD FROM \"{}\": file has no rows to determine columns from.",
            path.display()
        )));
    }
    let first: Vec<String> = rec.iter().map(|f| f.trim().to_string()).collect();
    let first_len = first.len();
    let mut max_cols = first_len;
    // Bitmask of sniffable cell classes: numeric, bool, bracketed list, date,
    // timestamp, uuid (the shapes C++'s sniffer types beyond STRING).
    fn cell_classes(t: &str) -> u8 {
        let mut m = 0u8;
        if t.parse::<f64>().is_ok() {
            m |= 1;
        }
        if t.eq_ignore_ascii_case("true") || t.eq_ignore_ascii_case("false") {
            m |= 2;
        }
        if t.starts_with('[') && t.ends_with(']') {
            m |= 4;
        }
        let normalized_date = t.replace(['/', '\\', ' '], "-");
        if crate::temporal::parse_date(&normalized_date).is_some() {
            m |= 8;
        }
        if crate::temporal::parse_timestamp(t).is_some() {
            m |= 16;
        }
        if crate::scalar::parse_uuid(t).is_some() {
            m |= 32;
        }
        if t.starts_with('{') && t.ends_with('}') {
            m |= 64;
        }
        m
    }
    // Per column: the class intersection over non-empty data cells (0xFF until a
    // cell is seen; 0 once cells disagree ⇒ the column is plain STRING).
    let mut col_classes = vec![0xFFu8; first.len()];
    let mut saw_data = vec![false; first.len()];
    let mut sampled = 0;
    while sampled < options.sample_size && read_sniff_record(&mut reader, &mut rec, path, options)?
    {
        max_cols = max_cols.max(rec.len());
        if col_classes.len() < max_cols {
            col_classes.resize(max_cols, 0xFF);
            saw_data.resize(max_cols, false);
        }
        for (i, f) in rec.iter().enumerate().take(first.len()) {
            let t = f.trim();
            if t.is_empty() {
                continue;
            }
            saw_data[i] = true;
            col_classes[i] &= cell_classes(t);
        }
        sampled += 1;
    }
    // Header iff some column's data is uniformly typed while the first row's
    // cell does not fit that type (C++'s type-conflict detection).
    let has_header = max_cols == first_len
        && first
            .iter()
            .zip(col_classes.iter().zip(&saw_data))
            .any(|(cell, (&classes, &saw))| {
                saw && classes != 0 && !cell.is_empty() && (cell_classes(cell) & classes) == 0
            });
    let names = if has_header {
        first
            .iter()
            .enumerate()
            .map(|(i, n)| {
                if n.is_empty() {
                    format!("column{i}")
                } else {
                    n.clone()
                }
            })
            .collect()
    } else {
        (0..max_cols).map(|i| format!("column{i}")).collect()
    };
    Ok((has_header, names))
}

/// Configure a reader builder for a dialect. Shared by [`open_reader`] and the
/// detection scorer so both parse a candidate identically.
fn configure(builder: &mut csv::ReaderBuilder, dialect: &Dialect) {
    builder
        .has_headers(false)
        .flexible(true)
        .delimiter(dialect.delimiter)
        .double_quote(true)
        .escape(dialect.escape);
    match dialect.quote {
        Some(q) => {
            builder.quoting(true).quote(q);
        }
        None => {
            builder.quoting(false);
        }
    }
}

/// Read up to `max` bytes of `path` for detection: strip a leading BOM, and if
/// the file was larger than the budget drop the trailing partial line so a
/// candidate is scored only over whole records.
fn read_sample(path: &Path, max: usize) -> Result<Vec<u8>> {
    let read_path = readable_path(path)?;
    let mut file =
        File::open(read_path).map_err(|e| Error::Io(format!("{}: {e}", path.display())))?;
    let mut buf = Vec::new();
    file.by_ref()
        .take(max as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| Error::Io(format!("{}: {e}", path.display())))?;
    let truncated = buf.len() > max;
    if truncated {
        buf.truncate(max);
    }
    if buf.starts_with(&BOM) {
        buf.drain(0..BOM.len());
    }
    if truncated {
        if let Some(pos) = buf.iter().rposition(|&b| b == b'\n') {
            let tail = &buf[pos + 1..];
            if let Some(&quote @ (b'"' | b'\'')) = tail.first() {
                buf.push(quote);
                buf.push(b'\n');
            } else {
                buf.truncate(pos + 1);
            }
        }
    }
    Ok(buf)
}

/// Seek the file past a leading UTF-8 BOM if present, else back to the start.
fn skip_bom(file: &mut File) -> std::io::Result<()> {
    let mut head = [0u8; 3];
    let mut read = 0;
    while read < head.len() {
        match file.read(&mut head[read..])? {
            0 => break,
            n => read += n,
        }
    }
    if !(read == BOM.len() && head == BOM) {
        file.seek(SeekFrom::Start(0))?;
    }
    Ok(())
}

/// A candidate's score, compared field-by-field in declaration order (derived
/// `Ord`): a clean parse beats a parse error; structurally used quoting beats
/// treating quoted delimiters as data; a delimiter that actually splits records
/// beats an unused candidate; then modal arity proximity, any exact row, consistency, and width.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Score {
    passes: bool,
    ever_quoted: bool,
    splits: bool,
    arity_closeness: usize,
    arity_match: bool,
    consistent: usize,
    columns: usize,
}

/// The pure core of detection: pick the best `(delimiter, quote)` over a byte
/// sample. Exposed for unit tests; [`resolve_dialect`] is the file-backed entry.
fn detect_from_sample(
    sample: &[u8],
    pin_delim: Option<u8>,
    pin_quote: Option<u8>,
    escape: Option<u8>,
    known_arity: Option<usize>,
) -> Dialect {
    let delims: Vec<u8> = pin_delim.map_or_else(|| DELIMITERS.to_vec(), |d| vec![d]);
    // Unpinned: try both quote chars, plus "no quoting" so a file with stray
    // unpaired quotes still parses cleanly.
    let quotes: Vec<Option<u8>> = match pin_quote {
        Some(q) => vec![Some(q)],
        None => QUOTES.iter().map(|&q| Some(q)).chain([None]).collect(),
    };

    let mut best: Option<(Score, Dialect)> = None;
    for &delimiter in &delims {
        for &quote in &quotes {
            let dialect = Dialect {
                delimiter,
                quote,
                escape,
            };
            let score = score_candidate(sample, &dialect, known_arity);
            if best.as_ref().is_none_or(|(b, _)| score > *b) {
                best = Some((score, dialect));
            }
        }
    }

    // `delims`/`quotes` are non-empty, so a best candidate always exists.
    let (_, mut dialect) = best.expect("at least one dialect candidate");
    // Normalize: a quote char that never actually opened a field is disabled (so a
    // literal quote in the data is preserved), unless the user pinned it.
    if pin_quote.is_none() {
        if let Some(q) = dialect.quote {
            if !ever_quoted(sample, dialect.delimiter, q) {
                dialect.quote = None;
            }
        }
    }
    if pin_quote.is_none() && dialect.quote.is_none() {
        for quote in QUOTES {
            if ever_quoted(sample, dialect.delimiter, quote) {
                let quoted = Dialect {
                    quote: Some(quote),
                    ..dialect
                };
                let score = score_candidate(sample, &quoted, known_arity);
                if score.passes && score.arity_match {
                    dialect = quoted;
                    break;
                }
            }
        }
    }
    dialect
}

/// Score one candidate dialect over the sample.
fn score_candidate(sample: &[u8], dialect: &Dialect, known_arity: Option<usize>) -> Score {
    let mut builder = csv::ReaderBuilder::new();
    configure(&mut builder, dialect);
    let mut reader = builder.from_reader(sample);

    let mut counts: Vec<usize> = Vec::new();
    let mut rec = csv::StringRecord::new();
    let mut passes = true;
    while counts.len() < MAX_SAMPLE_ROWS {
        match reader.read_record(&mut rec) {
            Ok(true) => counts.push(rec.len()),
            Ok(false) => break,
            // A hard parse error (e.g. an unterminated quote) disqualifies the
            // candidate — this is how a wrong quote char is pruned.
            Err(_) => {
                passes = false;
                break;
            }
        }
    }
    if counts.is_empty() {
        return Score {
            passes: false,
            splits: false,
            arity_match: false,
            arity_closeness: 0,
            ever_quoted: false,
            consistent: 0,
            columns: 0,
        };
    }

    let modal = modal_count(&counts);
    let target = known_arity.unwrap_or(modal);
    let consistent = counts.iter().filter(|&&c| c == target).count();
    // With a known arity, "matching" means the target actually occurred.
    let arity_match = known_arity.is_none() || consistent > 0;
    let arity_closeness = known_arity.map_or(0, |arity| usize::MAX - modal.abs_diff(arity));
    let ever_quoted = dialect
        .quote
        .is_some_and(|q| ever_quoted(sample, dialect.delimiter, q));
    Score {
        passes,
        splits: counts.iter().any(|&count| count > 1),
        arity_match,
        ever_quoted,
        arity_closeness,
        consistent,
        columns: modal,
    }
}

/// The most frequent field count (ties broken toward the wider row).
fn modal_count(counts: &[usize]) -> usize {
    let mut freq: HashMap<usize, usize> = HashMap::new();
    for &c in counts {
        *freq.entry(c).or_default() += 1;
    }
    freq.into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)))
        .map_or(0, |(c, _)| c)
}

/// Whether `quote` is ever used to open a field under `delim` — i.e. appears at a
/// field start (file start, or right after a delimiter or line break). Drives the
/// tie-break and the disable-unused-quoting normalization.
fn ever_quoted(sample: &[u8], delim: u8, quote: u8) -> bool {
    let mut at_field_start = true;
    for &b in sample {
        if b == quote && at_field_start {
            return true;
        }
        at_field_start = b == delim || b == b'\n' || b == b'\r';
    }
    false
}

/// Wrap a row-level COPY/LOAD failure the way the C++ CSV reader does:
/// `Copy exception: Error in file <path> on line <n>: <inner> Line/record
/// containing the error: '<raw>'` — `inner` keeps its own class prefix and
/// final period; `raw` is the physical line, cut after the failing field with
/// `...` appended when more of the record follows.
pub fn wrap_row_error(
    path: &Path,
    line_no: usize,
    inner: &str,
    upto_field: Option<usize>,
    dialect: &Dialect,
) -> crate::Error {
    // A Parser-class inner error escapes the row context entirely — C++
    // raises it from the struct-string parser before the CSV wrapper sees it.
    if let Some(msg) = inner.strip_prefix("Parser exception: ") {
        return crate::Error::Parser(msg.to_string());
    }
    let raw = error_record_text(path, line_no, upto_field, dialect);
    crate::Error::copy(format!(
        "Error in file {} on line {}: {} Line/record containing the error: '{}'",
        path.display(),
        line_no,
        inner,
        raw
    ))
}

/// The raw text of physical line `line_no` (1-based), cut after field
/// `upto_field` (quote-aware) with `...` when the record continues.
fn error_record_text(
    path: &Path,
    line_no: usize,
    upto_field: Option<usize>,
    dialect: &Dialect,
) -> String {
    let Ok(read_path) = readable_path(path) else {
        return String::new();
    };
    let content = match std::fs::read(read_path) {
        // Lossy: an invalid-UTF-8 record still renders (as U+FFFD), matching
        // the C++ error context.
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => return String::new(),
    };
    let Some(line) = content.lines().nth(line_no.saturating_sub(1)) else {
        return String::new();
    };
    let Some(upto) = upto_field else {
        return line.to_string();
    };
    let delim = dialect.delimiter as char;
    let quote = dialect.quote.unwrap_or(b'"') as char;
    let mut field = 0usize;
    let mut in_quotes = false;
    for (i, c) in line.char_indices() {
        if c == quote {
            in_quotes = !in_quotes;
        } else if c == delim && !in_quotes {
            if field == upto {
                return format!("{}...", &line[..i]);
            }
            field += 1;
        }
    }
    line.to_string()
}

/// The field count of a file's first record under the resolved dialect, for the
/// bind-time `LOAD WITH HEADERS` column-count check. `None` when the file cannot
/// be read (existence errors are reported elsewhere).
pub fn sniffed_arity(
    path: &str,
    options: &CsvOptions,
    known_arity: Option<usize>,
) -> Option<usize> {
    let p = std::path::Path::new(path);
    if read_sample(p, BOM.len() + 1).ok()?.is_empty() {
        return None;
    }
    let dialect = resolve_dialect(
        p,
        options.delimiter,
        options.quote,
        options.escape,
        options.auto_detect,
        known_arity,
    )
    .ok()?;
    let mut reader = open_reader(p, &dialect).ok()?;
    let mut rec = csv::StringRecord::new();
    match reader.read_record(&mut rec) {
        Ok(true) => Some(rec.len()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn det(sample: &str, known: Option<usize>) -> Dialect {
        detect_from_sample(sample.as_bytes(), None, None, Some(b'\\'), known)
    }

    fn temp_path(extension: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "koko-csv-dialect-{}-{id}.{extension}",
            std::process::id()
        ))
    }

    #[test]
    fn detects_comma_by_default() {
        let d = det("a,b,c\n1,2,3\n", Some(3));
        assert_eq!(d.delimiter, b',');
    }

    #[test]
    fn detects_pipe_delimiter_unknown_arity() {
        // LDBC-style: a pipe file with a typed header, no arity hint.
        let s = "id:ID|created:LONG|ip:STRING\n618|2011|46.16\n42|2020|10.0\n";
        let d = det(s, None);
        assert_eq!(d.delimiter, b'|');
    }

    #[test]
    fn detects_semicolon_with_known_arity() {
        let s = "from;to;year\n0;1;2021\n2;1;2020\n";
        assert_eq!(det(s, Some(3)).delimiter, b';');
    }

    #[test]
    fn quoted_delimiter_does_not_oversplit() {
        // The comma inside the quoted field must not count as a separator.
        let s = "a;b\n1;\"x,y\"\n2;\"p,q\"\n";
        let d = det(s, Some(2));
        assert_eq!(d.delimiter, b';');
        assert_eq!(d.quote, Some(b'"'));
    }

    #[test]
    fn sniffed_arity_prefers_structurally_used_quotes_over_wrong_target_width() {
        let path = temp_path("csv");
        std::fs::write(
            &path,
            concat!(
                r#"0,2,2021-06-30,1986-10-21 21:08:31.521,10 years 5 months,"[rnme,m8]","{locations:['toronto','waterloo'], amount: [100, 200]}",1,{a=b}"#,
                "\n",
                r#"0,3,2021-06-30,1946-08-25 19:07:22,20 years,"[n,j]","{}",2020-10-10,"{c=d, e=f, 1=2}""#,
                "\n",
                r#"0,5,2021-06-30,2012-12-11 20:07:22,10 days,"[i,j]","{locations:['shanghai'], amount: [10]}","nice weather","#,
                "\n",
                r#"2,0,2021-06-30,1946-08-25 19:07:22,10 years,"[a,b]","{locations:['paris'], amount: [20, 5000]}",4,"#,
                "\n",
                r#"2,3,1950-05-14,1946-08-25 19:07:22,23 minutes,"[f,g]","{locations:['paris'], amount: [2000, 5340]}","cool stuff found","#,
                "\n",
            ),
        )
        .unwrap();
        assert_eq!(
            sniffed_arity(path.to_str().unwrap(), &CsvOptions::default(), Some(7)),
            Some(9)
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn single_quote_wins_tie_via_ever_quoted() {
        // Both quote chars yield the same arity; the one actually used wins, and
        // is kept (not normalized away).
        let s = "from;to;v\n0;1;'[a,b]'\n2;3;'[c,d]'\n";
        let d = detect_from_sample(s.as_bytes(), None, None, Some(b'\\'), None);
        assert_eq!(d.delimiter, b';');
        assert_eq!(d.quote, Some(b'\''));
    }

    #[test]
    fn unused_quote_is_disabled() {
        // No field is ever quoted, so quoting is turned off.
        let d = det("a,b\n1,2\n3,4\n", Some(2));
        assert_eq!(d.quote, None);
    }

    #[test]
    fn single_column_not_spuriously_split() {
        let d = det("127\n65537\n4294967295\n", None);
        assert_eq!(d.delimiter, b','); // comma (priority) → one column
    }

    #[test]
    fn pinned_delimiter_is_respected() {
        let d = detect_from_sample(b"a;b;c\n1;2;3\n", Some(b','), None, Some(b'\\'), None);
        assert_eq!(d.delimiter, b','); // honored even though ';' fits better
    }

    #[test]
    fn tab_delimited() {
        let s = "a\tb\tc\n1\t2\t3\n4\t5\t6\n";
        assert_eq!(det(s, None).delimiter, b'\t');
    }

    #[test]
    fn skips_utf8_bom() {
        let path = temp_path("csv");
        std::fs::write(&path, b"\xEF\xBB\xBFid,name\n1,Alice\n").unwrap();
        let dialect = resolve_dialect(&path, None, None, None, true, Some(2)).unwrap();
        let mut reader = open_reader(&path, &dialect).unwrap();
        let first = reader.records().next().unwrap().unwrap();
        assert_eq!(first.get(0), Some("id"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn quoted_newline_is_serial_only_and_unterminated_quote_is_preflighted() {
        let path = temp_path("csv");
        std::fs::write(&path, b"\"hello\nworld\",1\n").unwrap();
        let dialect = resolve_dialect(&path, Some(b','), Some(b'"'), None, true, Some(2)).unwrap();
        assert!(has_quoted_newline(&path, &dialect).unwrap());
        let mut serial = open_reader(&path, &dialect).unwrap();
        let mut record = csv::StringRecord::new();
        assert!(
            read_record(
                &mut serial,
                &mut record,
                &path,
                &CsvOptions {
                    parallel: false,
                    ..CsvOptions::default()
                },
            )
            .unwrap()
        );
        assert_eq!(record.get(0), Some("hello\nworld"));

        let mut parallel = open_reader(&path, &dialect).unwrap();
        let error = read_record(
            &mut parallel,
            &mut record,
            &path,
            &CsvOptions {
                parallel: true,
                ..CsvOptions::default()
            },
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Quoted newlines are not supported")
        );

        std::fs::write(&path, b"\"unterminated").unwrap();
        let error = validate_file_structure(&path, &CsvOptions::default()).unwrap_err();
        assert!(error.to_string().contains("unterminated quotes"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn sample_size_changes_type_sniffing() {
        let path = temp_path("csv");
        std::fs::write(&path, b"1\n2\nnot-an-int\n").unwrap();
        let small = sniff_column_types(
            &path,
            &CsvOptions {
                sample_size: 2,
                ..CsvOptions::default()
            },
            false,
            1,
        )
        .unwrap();
        let full = sniff_column_types(
            &path,
            &CsvOptions {
                sample_size: 3,
                ..CsvOptions::default()
            },
            false,
            1,
        )
        .unwrap();
        assert_eq!(small, vec![LogicalType::Int64]);
        assert_eq!(full, vec![LogicalType::String]);
        std::fs::write(&path, b"\"[1,2]\"\n\"[3,4]\"\n").unwrap();
        assert_eq!(
            sniff_column_types(&path, &CsvOptions::default(), false, 1).unwrap(),
            vec![LogicalType::List(Box::new(LogicalType::Int64))]
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reads_gzip_and_gzip_extensions_and_rejects_bad_magic() {
        use std::io::Write;

        for extension in ["csv.gz", "csv.gzip"] {
            let path = temp_path(extension);
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(b"id,name\n1,Alice\n").unwrap();
            std::fs::write(&path, encoder.finish().unwrap()).unwrap();
            let dialect = resolve_dialect(&path, None, None, None, true, Some(2)).unwrap();
            let first = open_reader(&path, &dialect)
                .unwrap()
                .records()
                .next()
                .unwrap()
                .unwrap();
            assert_eq!(first.iter().collect::<Vec<_>>(), ["id", "name"]);
            let _ = std::fs::remove_file(path);
        }

        let corrupt = temp_path("csv.gz");
        std::fs::write(&corrupt, b"not gzip").unwrap();
        assert_eq!(
            resolve_dialect(&corrupt, None, None, None, true, None)
                .unwrap_err()
                .to_string(),
            "IO exception: Input is not a GZIP stream."
        );
        let _ = std::fs::remove_file(corrupt);
    }
    #[test]
    fn quoted_single_column_beats_inner_delimiters() {
        let d = detect_from_sample(
            b"p\n\"{a:1,b:2,nested:[3,4]}\"\n",
            None,
            None,
            Some(b'\\'),
            Some(1),
        );
        assert_eq!(d.delimiter, b',');
        assert_eq!(d.quote, Some(b'"'));
    }
    #[test]
    fn bom_only_has_no_records() {
        let path = temp_path("csv");
        std::fs::write(&path, BOM).unwrap();
        let dialect = resolve_dialect(&path, None, None, None, true, Some(2)).unwrap();
        let mut reader = open_reader(&path, &dialect).unwrap();
        let mut record = csv::StringRecord::new();
        assert!(!reader.read_record(&mut record).unwrap());
        let _ = std::fs::remove_file(path);
    }
}
