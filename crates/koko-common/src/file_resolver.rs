//! Deterministic local-file expansion shared by every ingest path.

use crate::{Error, Result};
use std::path::{Path, PathBuf};

/// A file format understood by the IM3 local readers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileFormat {
    Csv,
    Parquet,
    Npy,
}

impl FileFormat {
    /// Parse an explicit `file_format` option.
    pub fn parse(value: &str) -> Result<Self> {
        if value.eq_ignore_ascii_case("csv") {
            Ok(Self::Csv)
        } else if value.eq_ignore_ascii_case("parquet") {
            Ok(Self::Parquet)
        } else if value.eq_ignore_ascii_case("npy") {
            Ok(Self::Npy)
        } else {
            Err(Error::binder(format!(
                "Cannot load from file type {value}. If this file type is part of a koko extension please load the extension then try again."
            )))
        }
    }

    /// Infer a format from a concrete local path. Compressed CSV commonly uses
    /// a bare `.gz`/`.gzip` suffix; compression around a columnar format is
    /// rejected explicitly instead of feeding compressed bytes to that reader.
    pub fn infer(path: &Path) -> Result<Self> {
        let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
        if extension.eq_ignore_ascii_case("gz") || extension.eq_ignore_ascii_case("gzip") {
            let inner = Path::new(path.file_stem().unwrap_or_default())
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("");
            if inner.eq_ignore_ascii_case("parquet") || inner.eq_ignore_ascii_case("npy") {
                return Err(Error::Io(
                    "Koko currently only supports reading from compressed csv files.".to_string(),
                ));
            }
            return Ok(Self::Csv);
        }
        if extension.eq_ignore_ascii_case("csv") || extension.is_empty() {
            Ok(Self::Csv)
        } else if extension.eq_ignore_ascii_case("parquet") {
            Ok(Self::Parquet)
        } else if extension.eq_ignore_ascii_case("npy") {
            Ok(Self::Npy)
        } else {
            Err(Error::binder(format!(
                "Cannot load from file type {extension}. If this file type is part of a koko extension please load the extension then try again."
            )))
        }
    }
}

/// Statement-scoped local path settings. `file_search_path` is intentionally
/// kept in its user-facing comma-separated form so entry order is preserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileResolverConfig {
    pub base_dir: PathBuf,
    pub home_directory: Option<PathBuf>,
    pub file_search_path: String,
}

impl Default for FileResolverConfig {
    fn default() -> Self {
        Self {
            base_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            home_directory: std::env::var_os("HOME").map(PathBuf::from),
            file_search_path: String::new(),
        }
    }
}

/// One concrete result. `original` is the spelling supplied by the user and is
/// retained for diagnostics even when `path` came from a glob or search path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFile {
    pub original: String,
    pub path: PathBuf,
    pub format: FileFormat,
}

/// Expand an ordered list of local source spellings and resolve one format.
///
/// Each spelling is independent: matches are sorted within that spelling,
/// spelling order and duplicates are preserved, and a relative base-directory
/// match suppresses search-path probing. When the base has no match, every
/// search-path entry contributes matches in configured entry order.
pub fn resolve_files(
    spellings: &[String],
    config: &FileResolverConfig,
    explicit_format: Option<&str>,
) -> Result<Vec<ResolvedFile>> {
    let mut concrete = Vec::<(String, PathBuf)>::new();

    // Remote rejection precedes every filesystem probe, including probes for
    // earlier local spellings in the same list.
    for spelling in spellings {
        if has_uri_scheme(spelling) {
            return Err(Error::not_implemented(format!(
                "Remote file sources are not supported until IM5: {spelling}."
            )));
        }
    }

    for spelling in spellings {
        let matches = expand_spelling(spelling, config)?;
        if matches.is_empty() {
            return Err(Error::binder(format!(
                "No file found that matches the pattern: {spelling}."
            )));
        }
        for path in matches {
            let canonical = std::fs::canonicalize(&path).map_err(|_| {
                Error::binder(format!(
                    "No file found that matches the pattern: {spelling}."
                ))
            })?;
            concrete.push((spelling.clone(), canonical));
        }
    }

    // Existence/glob expansion wins over directory and format validation.
    for (_, path) in &concrete {
        if !path.is_file() {
            return Err(Error::binder(format!(
                "Provided path is not a file: {}.",
                path.display()
            )));
        }
    }
    let explicit = explicit_format.map(FileFormat::parse).transpose()?;

    let mut resolved = Vec::with_capacity(concrete.len());
    let mut inferred = None;
    for (original, path) in concrete {
        let format = match explicit {
            Some(format) => format,
            None => {
                let format = FileFormat::infer(&path)?;
                if inferred.is_some_and(|expected| expected != format) {
                    return Err(Error::copy(
                        "Loading files with different types is not currently supported.",
                    ));
                }
                inferred = Some(format);
                format
            }
        };
        resolved.push(ResolvedFile {
            original,
            path,
            format,
        });
    }
    Ok(resolved)
}

fn has_uri_scheme(spelling: &str) -> bool {
    let Some((scheme, _)) = spelling.split_once("://") else {
        return false;
    };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

fn expand_spelling(spelling: &str, config: &FileResolverConfig) -> Result<Vec<PathBuf>> {
    if Path::new(spelling).is_absolute() {
        return glob_paths(Path::new(spelling));
    }
    if spelling == "~" || spelling.starts_with("~/") {
        let Some(home) = config.home_directory.as_deref() else {
            return Ok(Vec::new());
        };
        let suffix = spelling.strip_prefix("~/").unwrap_or("");
        return glob_paths(&home.join(suffix));
    }

    let base_matches = glob_paths(&config.base_dir.join(spelling))?;
    if !base_matches.is_empty() {
        return Ok(base_matches);
    }

    let mut matches = Vec::new();
    for entry in config.file_search_path.split(',').map(str::trim) {
        if entry.is_empty() {
            continue;
        }
        let root = expand_search_root(entry, config);
        matches.extend(glob_paths(&root.join(spelling))?);
    }
    Ok(matches)
}

fn expand_search_root(entry: &str, config: &FileResolverConfig) -> PathBuf {
    if entry == "~" || entry.starts_with("~/") {
        if let Some(home) = config.home_directory.as_deref() {
            return home.join(entry.strip_prefix("~/").unwrap_or(""));
        }
    }
    let path = Path::new(entry);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        config.base_dir.join(path)
    }
}

fn exact_basename_exists(path: &Path) -> bool {
    let (Some(parent), Some(file_name)) = (path.parent(), path.file_name()) else {
        return path.exists();
    };
    std::fs::read_dir(parent).is_ok_and(|entries| {
        entries
            .filter_map(std::result::Result::ok)
            .any(|entry| entry.file_name() == file_name)
    })
}

fn glob_paths(pattern: &Path) -> Result<Vec<PathBuf>> {
    let pattern_text = pattern.to_string_lossy();
    let entries = match glob::glob_with(
        &pattern_text,
        glob::MatchOptions {
            case_sensitive: true,
            require_literal_separator: false,
            require_literal_leading_dot: false,
        },
    ) {
        Ok(entries) => entries,
        // A malformed glob has no concrete match and follows the normal empty
        // expansion diagnostic at the public boundary.
        Err(_) => return Ok(Vec::new()),
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(std::result::Result::ok)
        .filter(|path| exact_basename_exists(path))
        .collect();
    paths.sort();
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("koko-resolver-{}-{id}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn file(&self, relative: &str) -> PathBuf {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"x").unwrap();
            path
        }

        fn config(&self) -> FileResolverConfig {
            FileResolverConfig {
                base_dir: self.0.clone(),
                home_directory: Some(self.0.join("home")),
                file_search_path: String::new(),
            }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn names(files: &[ResolvedFile]) -> Vec<String> {
        files
            .iter()
            .map(|file| {
                file.path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn resolves_literal_and_ordered_duplicate_list() {
        let dir = TempDir::new();
        dir.file("a.csv");
        dir.file("b.csv");
        let files = resolve_files(
            &["b.csv".into(), "a.csv".into(), "b.csv".into()],
            &dir.config(),
            None,
        )
        .unwrap();
        assert_eq!(names(&files), ["b.csv", "a.csv", "b.csv"]);
        assert_eq!(files[0].original, "b.csv");
    }

    #[test]
    fn literal_resolution_is_case_sensitive() {
        let dir = TempDir::new();
        dir.file("City.parquet");
        let error = resolve_files(&["city.parquet".into()], &dir.config(), None).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Binder exception: No file found that matches the pattern: city.parquet."
        );
    }

    #[test]
    fn expands_full_globs_sorted_with_spelling_order() {
        let dir = TempDir::new();
        dir.file("nested/z1.csv");
        dir.file("nested/a2.csv");
        dir.file("other.csv");
        let files = resolve_files(
            &["nested/?[12].csv".into(), "other.csv".into()],
            &dir.config(),
            None,
        )
        .unwrap();
        assert_eq!(names(&files), ["a2.csv", "z1.csv", "other.csv"]);
    }

    #[test]
    fn expands_home_directory() {
        let dir = TempDir::new();
        dir.file("home/home.csv");
        let files = resolve_files(&["~/home.csv".into()], &dir.config(), None).unwrap();
        assert_eq!(names(&files), ["home.csv"]);
    }

    #[test]
    fn base_match_wins_over_search_paths() {
        let dir = TempDir::new();
        dir.file("pick.csv");
        dir.file("s1/pick.csv");
        dir.file("s2/pick.csv");
        let mut config = dir.config();
        config.file_search_path = "s1,s2".into();
        let files = resolve_files(&["pick.csv".into()], &config, None).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].path.parent().unwrap(),
            dir.0.canonicalize().unwrap()
        );
    }

    #[test]
    fn search_paths_accumulate_in_entry_order() {
        let dir = TempDir::new();
        dir.file("s1/b.csv");
        dir.file("s1/a.csv");
        dir.file("s2/c.csv");
        let mut config = dir.config();
        config.file_search_path = "s2,s1".into();
        let files = resolve_files(&["*.csv".into()], &config, None).unwrap();
        assert_eq!(names(&files), ["c.csv", "a.csv", "b.csv"]);
    }

    #[test]
    fn supports_paths_longer_than_255_bytes() {
        let dir = TempDir::new();
        let component = "a".repeat(120);
        let relative = format!("{component}/{component}/long.csv");
        dir.file(&relative);
        let files = resolve_files(&[relative], &dir.config(), None).unwrap();
        assert!(files[0].path.as_os_str().len() > 255);
    }

    #[test]
    fn missing_spelling_reports_original() {
        let dir = TempDir::new();
        let error = resolve_files(&["missing?.csv".into()], &dir.config(), None).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Binder exception: No file found that matches the pattern: missing?.csv."
        );
    }

    #[test]
    fn directory_precedes_format_dispatch() {
        let dir = TempDir::new();
        std::fs::create_dir_all(dir.0.join("folder.csv")).unwrap();
        let error =
            resolve_files(&["folder.csv".into()], &dir.config(), Some("unknown")).unwrap_err();
        assert!(error.to_string().contains("Provided path is not a file"));
    }

    #[test]
    fn infers_csv_gzip_and_rejects_mixed_formats() {
        let dir = TempDir::new();
        dir.file("a.csv.gz");
        dir.file("b.parquet");
        let csv = resolve_files(&["a.csv.gz".into()], &dir.config(), None).unwrap();
        assert_eq!(csv[0].format, FileFormat::Csv);
        let error = resolve_files(
            &["a.csv.gz".into(), "b.parquet".into()],
            &dir.config(),
            None,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Copy exception: Loading files with different types is not currently supported."
        );
    }

    #[test]
    fn explicit_format_overrides_extensions() {
        let dir = TempDir::new();
        dir.file("data.unknown");
        let files = resolve_files(&["data.unknown".into()], &dir.config(), Some("csv")).unwrap();
        assert_eq!(files[0].format, FileFormat::Csv);
    }

    #[test]
    fn remote_is_rejected_before_local_filesystem_errors() {
        let dir = TempDir::new();
        let error = resolve_files(
            &["missing.csv".into(), "https://example.test/a.csv".into()],
            &dir.config(),
            None,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Not implemented exception: Remote file sources are not supported until IM5: https://example.test/a.csv."
        );
    }
}
