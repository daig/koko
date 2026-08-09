#!/usr/bin/env python3
"""Generate the checked-in typed builtin registry from its two source inputs.

Normal builds consume ``catalog_data.rs`` directly. Use ``--check`` in CI, or
pipe a fresh ``CALL show_functions()`` list capture to ``--refresh-from-stdin``
to replace ``catalog_rows.tsv`` before regenerating.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
FUNCTION_DIR = ROOT / "crates" / "koko-function"
ROWS_PATH = FUNCTION_DIR / "catalog_rows.tsv"
MANIFEST_PATH = FUNCTION_DIR / "builtins.toml"
OUTPUT_PATH = FUNCTION_DIR / "src" / "catalog_data.rs"

CATALOG_KINDS = {
    "SCALAR FUNCTION": "Scalar",
    "REWRITE FUNCTION": "Rewrite",
    "AGGREGATE FUNCTION": "Aggregate",
    "TABLE FUNCTION": "Table",
    "STANDALONE TABLE FUNCTION": "StandaloneTable",
    "ALGORITHM FUNCTION": "Algorithm",
}
MANIFEST_TO_CATALOG_KIND = {
    "scalar": "SCALAR FUNCTION",
    "rewrite": "REWRITE FUNCTION",
    "aggregate": "AGGREGATE FUNCTION",
    "table": "TABLE FUNCTION",
    "standalone_table": "STANDALONE TABLE FUNCTION",
    "algorithm": "ALGORITHM FUNCTION",
}
MANIFEST_KIND_VARIANT = {
    "scalar": "Scalar",
    "rewrite": "Rewrite",
    "aggregate": "Aggregate",
    "table": "Table",
    "standalone_table": "StandaloneTable",
    "algorithm": "Algorithm",
}
TYPE_IDS = {
    "ANY": "Any",
    "BOOL": "Bool",
    "INT8": "Int8",
    "INT16": "Int16",
    "INT32": "Int32",
    "INT64": "Int64",
    "INT128": "Int128",
    "UINT8": "UInt8",
    "UINT16": "UInt16",
    "UINT32": "UInt32",
    "UINT64": "UInt64",
    "UINT128": "UInt128",
    "SERIAL": "Serial",
    "DECIMAL": "Decimal",
    "DOUBLE": "Double",
    "FLOAT": "Float",
    "STRING": "String",
    "DATE": "Date",
    "TIMESTAMP": "Timestamp",
    "TIMESTAMP_NS": "TimestampNs",
    "TIMESTAMP_MS": "TimestampMs",
    "TIMESTAMP_SEC": "TimestampSec",
    "TIMESTAMP_TZ": "TimestampTz",
    "INTERVAL": "Interval",
    "UUID": "Uuid",
    "BLOB": "Blob",
    "LIST": "List",
    "ARRAY": "Array",
    "STRUCT": "Struct",
    "MAP": "Map",
    "UNION": "Union",
    "NODE": "Node",
    "REL": "Rel",
    "RECURSIVE_REL": "RecursiveRel",
    "INTERNAL_ID": "InternalId",
    "JSON": "Json",
}
CAST_SEMANTICS = {
    "ToInt8": "Int8",
    "ToInt16": "Int16",
    "ToInt32": "Int32",
    "ToInt64": "Int64",
    "ToInt128": "Int128",
    "ToUint8": "UInt8",
    "ToUint16": "UInt16",
    "ToUint32": "UInt32",
    "ToUint64": "UInt64",
    "ToUint128": "UInt128",
    "ToSerial": "Serial",
    "ToDouble": "Double",
    "ToFloat": "Float",
    "ToBool": "Bool",
    "ToString": "String",
    "ToBlob": "Blob",
    "ToUuid": "Uuid",
    "ToDate": "Date",
}
ROUND_SEMANTICS = {"Floor": "Floor", "Ceil": "Ceil", "Round": "Round"}

# Koko-only overloads live in the manifest rather than the Ladybug-derived
# catalog_rows.tsv capture. Each symbolic signature maps to the generated
# constraint family and its minimum accepted arity.
KOKO_OVERLOAD_SPECS = {
    "(NUMERIC,NUMERIC,...) -> NUMERIC": ("Numeric", 2),
}
DIGEST_SEMANTICS = {"Md5": "Md5", "Sha256": "Sha256"}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--check", action="store_true")
    mode.add_argument("--refresh-from-stdin", action="store_true")
    return parser.parse_args()


def parse_rows(text: str) -> list[tuple[str, str, str]]:
    rows: list[tuple[str, str, str]] = []
    for number, raw in enumerate(text.splitlines(), 1):
        if not raw:
            continue
        parts = raw.split("\t")
        if len(parts) != 3:
            raise ValueError(f"catalog row {number} must contain exactly three tab-separated fields")
        name, kind, signature = parts
        if not kind:
            raise ValueError(f"catalog row {number} has an empty kind")
        rows.append((name, kind, signature))
    return rows


def parse_value(raw: str):
    raw = raw.strip()
    if raw in ("true", "false"):
        return raw == "true"
    if raw.startswith("["):
        return json.loads(raw)
    return json.loads(raw)


def parse_manifest(text: str) -> list[dict[str, object]]:
    entries: list[dict[str, object]] = []
    current: dict[str, object] | None = None
    for number, raw in enumerate(text.splitlines(), 1):
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        if line == "[[builtin]]":
            current = {}
            entries.append(current)
            continue
        if current is None or "=" not in line:
            raise ValueError(f"manifest line {number} is not inside [[builtin]]")
        key, value = (part.strip() for part in line.split("=", 1))
        if key in current:
            raise ValueError(f"manifest line {number} duplicates key {key!r}")
        current[key] = parse_value(value)
    required = {"name", "kind", "semantic"}
    seen: set[str] = set()
    for entry in entries:
        missing = required - entry.keys()
        if missing:
            raise ValueError(f"manifest entry is missing {sorted(missing)}: {entry}")
        name = str(entry["name"])
        if name in seen:
            raise ValueError(f"duplicate manifest name {name!r}")
        if name != name.lower() or not name.isascii():
            raise ValueError(f"manifest name must be lower-case ASCII: {name!r}")
        if entry["kind"] not in MANIFEST_TO_CATALOG_KIND:
            raise ValueError(f"manifest name {name!r} has invalid kind {entry['kind']!r}")
        koko_overloads = entry.get("koko_overloads", [])
        if not isinstance(koko_overloads, list) or any(
            not isinstance(signature, str) or signature not in KOKO_OVERLOAD_SPECS
            for signature in koko_overloads
        ):
            raise ValueError(f"manifest name {name!r} has invalid koko_overloads")
        if koko_overloads and entry["kind"] != "scalar":
            raise ValueError(f"manifest name {name!r} has non-scalar Koko overloads")
        signatures = entry.get("signatures", [])
        if not isinstance(signatures, list) or any(
            not isinstance(signature, str) for signature in signatures
        ):
            raise ValueError(f"manifest name {name!r} has invalid signatures")
        catalog_signatures = entry.get("catalog_signatures", [])
        if not isinstance(catalog_signatures, list) or any(
            not isinstance(signature, str) for signature in catalog_signatures
        ):
            raise ValueError(f"manifest name {name!r} has invalid catalog_signatures")
        if catalog_signatures and "catalog_signature" in entry:
            raise ValueError(
                f"manifest name {name!r} has both catalog_signature and catalog_signatures"
            )
        if entry["kind"] == "algorithm" and not signatures:
            raise ValueError(f"algorithm manifest name {name!r} has no signatures")
        catalog_signatures = entry.get("catalog_signatures", [])
        if not isinstance(catalog_signatures, list) or any(
            not isinstance(signature, str) for signature in catalog_signatures
        ):
            raise ValueError(f"manifest name {name!r} has invalid catalog_signatures")
        if "catalog_signature" in entry and catalog_signatures:
            raise ValueError(
                f"manifest name {name!r} cannot define both catalog_signature and catalog_signatures"
            )
    if entries != sorted(entries, key=lambda entry: str(entry["name"]).lower()):
        raise ValueError("manifest entries must be sorted by ASCII-folded called name")
    return entries


def signature_parts(signature: str) -> tuple[list[str], str | None]:
    if not signature.startswith("(") or ")" not in signature:
        raise ValueError(f"malformed catalog signature {signature!r}")
    params_text, suffix = signature[1:].split(")", 1)
    params = [] if not params_text else params_text.split(",")
    result = suffix.removeprefix(" -> ") if suffix.startswith(" -> ") else None
    for type_name in params + ([result] if result else []):
        if type_name not in TYPE_IDS:
            raise ValueError(f"unknown catalog type ID {type_name!r} in {signature!r}")
    return params, result


def scalar_expression(semantic: str) -> str:
    if semantic in CAST_SEMANTICS:
        return f"BuiltinScalar::Cast(CastTarget::{CAST_SEMANTICS[semantic]})"
    if semantic in ROUND_SEMANTICS:
        return f"BuiltinScalar::Round(RoundMode::{ROUND_SEMANTICS[semantic]})"
    if semantic in DIGEST_SEMANTICS:
        return f"BuiltinScalar::Digest(DigestAlgorithm::{DIGEST_SEMANTICS[semantic]})"
    return f"BuiltinScalar::{semantic}"


def emit(rows: list[tuple[str, str, str]], entries: list[dict[str, object]]) -> str:
    manifest_names = {str(entry["name"]) for entry in entries}
    runtime_rows = [
        row
        for row in rows
        if row[1] in ("SCALAR FUNCTION", "REWRITE FUNCTION", "AGGREGATE FUNCTION")
        or row[0].lower() in manifest_names
    ]
    kind_variants = {**CATALOG_KINDS, "COPY FUNCTION": "Copy"}
    rows_by_name: dict[str, list[tuple[str, str, str]]] = {}
    for row in runtime_rows:
        rows_by_name.setdefault(row[0].lower(), []).append(row)

    entries_by_name = {str(entry["name"]): entry for entry in entries}
    missing = sorted(set(rows_by_name) - set(entries_by_name))
    if missing:
        raise ValueError(f"catalog builtin names missing from manifest: {', '.join(missing)}")
    for name, entry in entries_by_name.items():
        source = str(entry.get("overload_source", name))
        if not entry.get("signatures") and source not in rows_by_name:
            raise ValueError(f"manifest name {name!r} has no catalog rows and invalid overload_source {source!r}")
        canonical = str(entry.get("canonical_name", name))
        if canonical not in entries_by_name:
            raise ValueError(
                f"manifest name {name!r} has invalid canonical_name {canonical!r}"
            )
        if entries_by_name[canonical]["semantic"] != entry["semantic"]:
            raise ValueError(
                f"manifest alias {name!r} does not share {canonical!r}'s semantic ID"
            )
        expected_kind = MANIFEST_TO_CATALOG_KIND[str(entry["kind"])]
        row_kinds = (
            {expected_kind}
            if entry.get("signatures")
            else {row[1] for row in rows_by_name[source]}
        )
        if expected_kind not in row_kinds and not (
            entry["kind"] == "scalar" and "REWRITE FUNCTION" in row_kinds
        ):
            raise ValueError(f"manifest kind for {name!r} does not match overload source {source!r}")

    scalar_semantics = sorted(
        {
            str(entry["semantic"])
            for entry in entries
            if entry["kind"] in ("scalar", "rewrite")
            and str(entry["semantic"]) not in CAST_SEMANTICS
            and str(entry["semantic"]) not in ROUND_SEMANTICS
            and str(entry["semantic"]) not in DIGEST_SEMANTICS
        }
    )
    table_semantics = sorted(
        {
            str(entry["semantic"])
            for entry in entries
            if entry["kind"] in ("table", "standalone_table")
        }
    )
    algorithm_semantics = sorted(
        {
            str(entry["semantic"])
            for entry in entries
            if entry["kind"] == "algorithm"
        }
    )
    canonical_by_function: dict[str, str] = {}
    for entry in entries:
        if entry["kind"] not in ("scalar", "rewrite"):
            continue
        expression = scalar_expression(str(entry["semantic"]))
        canonical = str(entry.get("canonical_name", entry["name"]))
        prior = canonical_by_function.setdefault(expression, canonical)
        if prior != canonical:
            raise ValueError(
                f"semantic ID {expression} has conflicting canonical names "
                f"{prior!r} and {canonical!r}"
            )
    out: list[str] = [
        "//! Generated typed builtin registry and function display catalog.",
        "//! Ladybug-derived rows remain in `catalog_rows.tsv`; Koko-only overloads come from `builtins.toml`.",
        "",
        "use crate::AggOp;",
        "use koko_common::{IntKind, LogicalType};",
        "use std::cmp::Ordering;",
        "",
        "#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]",
        "pub enum FunctionCatalogKind { Scalar, Rewrite, Aggregate, Table, StandaloneTable, Algorithm, Copy }",
        "",
        "impl FunctionCatalogKind {",
        "    pub const fn as_str(self) -> &'static str {",
        "        match self { Self::Scalar => \"SCALAR FUNCTION\", Self::Rewrite => \"REWRITE FUNCTION\", Self::Aggregate => \"AGGREGATE FUNCTION\", Self::Table => \"TABLE FUNCTION\", Self::StandaloneTable => \"STANDALONE TABLE FUNCTION\", Self::Algorithm => \"ALGORITHM FUNCTION\", Self::Copy => \"COPY FUNCTION\" }",
        "    }",
        "}",
        "",
        "#[derive(Debug, Clone, Copy, PartialEq, Eq)]",
        "pub struct FunctionCatalogEntry { pub name: &'static str, pub kind: FunctionCatalogKind, pub signature: &'static str }",
        "",
        "#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]",
        "pub enum CastTarget { Int8, Int16, Int32, Int64, Int128, UInt8, UInt16, UInt32, UInt64, UInt128, Serial, Double, Float, Bool, String, Blob, Uuid, Date }",
        "",
        "#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]",
        "pub enum RoundMode { Floor, Ceil, Round }",
        "",
        "#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]",
        "pub enum DigestAlgorithm { Md5, Sha256 }",
        "",
        "#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]",
        "pub enum BuiltinScalar {",
        "    Cast(CastTarget),",
        "    Round(RoundMode),",
        "    Digest(DigestAlgorithm),",
    ]
    out.extend(f"    {semantic}," for semantic in scalar_semantics)
    out += [
        "}",
        "",
        "impl BuiltinScalar {",
        "    pub const fn canonical_name(self) -> &'static str {",
        "        match self {",
    ]
    out.extend(
        f"            {expression} => {json.dumps(canonical)},"
        for expression, canonical in sorted(canonical_by_function.items())
    )
    out += [
        "        }",
        "    }",
        "}",
        "",
        "#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]",
        "pub enum BuiltinTableFunction {",
    ]
    out.extend(f"    {semantic}," for semantic in table_semantics)
    out += [
        "}",
        "",
        "impl BuiltinTableFunction {",
        "    pub const fn canonical_name(self) -> &'static str {",
        "        match self {",
    ]
    out.extend(
        f"            Self::{entry['semantic']} => {json.dumps(str(entry['name']).upper())},"
        for entry in entries
        if entry["kind"] in ("table", "standalone_table")
    )
    out += [
        "        }",
        "    }",
        "}",
        "",
        "#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]",
        "pub enum BuiltinGraphAlgorithm {",
    ]
    out.extend(f"    {semantic}," for semantic in algorithm_semantics)
    out += [
        "}",
        "",
        "#[derive(Debug, Clone, Copy, PartialEq, Eq)]",
        "pub enum BuiltinFunction { Scalar(BuiltinScalar), Aggregate(AggOp), Table(BuiltinTableFunction), GraphAlgorithm(BuiltinGraphAlgorithm), CatalogOnly }",
        "#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]",
        "pub enum CatalogTypeId { Any, Bool, Int8, Int16, Int32, Int64, Int128, UInt8, UInt16, UInt32, UInt64, UInt128, Serial, Decimal, Double, Float, String, Date, Timestamp, TimestampNs, TimestampMs, TimestampSec, TimestampTz, Interval, Uuid, Blob, List, Array, Struct, Map, Union, Node, Rel, RecursiveRel, InternalId, Json }",
        "",
        "impl CatalogTypeId {",
        "    pub fn of(actual: &LogicalType) -> Self {",
        "        match actual {",
        "            LogicalType::Any => Self::Any, LogicalType::Bool => Self::Bool,",
        "            LogicalType::Int(IntKind::I8) => Self::Int8, LogicalType::Int(IntKind::I16) => Self::Int16, LogicalType::Int(IntKind::I32) => Self::Int32, LogicalType::Int(IntKind::I64) => Self::Int64, LogicalType::Int(IntKind::I128) => Self::Int128,",
        "            LogicalType::Int(IntKind::U8) => Self::UInt8, LogicalType::Int(IntKind::U16) => Self::UInt16, LogicalType::Int(IntKind::U32) => Self::UInt32, LogicalType::Int(IntKind::U64) => Self::UInt64, LogicalType::UInt128 => Self::UInt128, LogicalType::Serial => Self::Serial,",
        "            LogicalType::Decimal(_, _) => Self::Decimal, LogicalType::Double => Self::Double, LogicalType::Float => Self::Float, LogicalType::String => Self::String, LogicalType::Date => Self::Date, LogicalType::Timestamp => Self::Timestamp, LogicalType::TimestampNs => Self::TimestampNs, LogicalType::TimestampMs => Self::TimestampMs, LogicalType::TimestampSec => Self::TimestampSec, LogicalType::TimestampTz => Self::TimestampTz, LogicalType::Interval => Self::Interval, LogicalType::Uuid => Self::Uuid, LogicalType::Blob => Self::Blob, LogicalType::List(_) => Self::List, LogicalType::Array(_, _) => Self::Array, LogicalType::Struct(_) => Self::Struct, LogicalType::Map(_, _) => Self::Map, LogicalType::Union(_) => Self::Union, LogicalType::Node(_) => Self::Node, LogicalType::Rel(_) => Self::Rel, LogicalType::RecursiveRel => Self::RecursiveRel, LogicalType::InternalId => Self::InternalId, LogicalType::Json => Self::Json,",
        "        }",
        "    }",
        "}",
        "",
        "#[derive(Debug, Clone, Copy, PartialEq, Eq)]",
        "pub enum KokoOverloadFamily { Numeric }",
        "",
        "#[derive(Debug, Clone, Copy)]",
        "pub struct KokoOverloadDescriptor { pub family: KokoOverloadFamily, pub min_arity: usize, pub display_signature: &'static str }",
        "",
        "#[derive(Debug, Clone, Copy)]",
        "pub struct OverloadDescriptor { pub params: &'static [CatalogTypeId], pub result: Option<CatalogTypeId>, pub distinct: bool, pub display_signature: &'static str }",
        "",
        "#[derive(Debug, Clone, Copy)]",
        "pub struct BuiltinDescriptor { pub called_name: &'static str, pub function: BuiltinFunction, pub catalog_kind: FunctionCatalogKind, pub overloads: &'static [OverloadDescriptor], pub koko_overloads: &'static [KokoOverloadDescriptor], pub string_coerce: &'static [usize], pub variable_arity: bool, pub bindable: bool }",
        "",
    ]

    descriptor_rows: list[tuple[dict[str, object], list[tuple[str, str, str]]]] = []
    for index, entry in enumerate(entries):
        name = str(entry["name"])
        if entry.get("signatures"):
            source_rows = [
                (name, MANIFEST_TO_CATALOG_KIND[str(entry["kind"])], str(signature))
                for signature in entry["signatures"]
            ]
        else:
            source = str(entry.get("overload_source", name))
            source_rows = rows_by_name[source]
        overloads: list[tuple[str, str, str, bool]] = []
        if entry["kind"] == "aggregate":
            i = 0
            while i < len(source_rows):
                row = source_rows[i]
                paired = i + 1 < len(source_rows) and source_rows[i + 1][2] == row[2]
                if paired:
                    overloads.append((*row, True))
                    overloads.append((*source_rows[i + 1], False))
                    i += 2
                else:
                    overloads.append((*row, False))
                    i += 1
        else:
            overloads = [(*row, False) for row in source_rows if row[1] != "AGGREGATE FUNCTION"]
        out.append(f"static OVERLOADS_{index}: &[OverloadDescriptor] = &[")
        for _, _, signature, distinct in overloads:
            params, result = signature_parts(signature)
            params_code = ", ".join(f"CatalogTypeId::{TYPE_IDS[p]}" for p in params)
            result_code = "None" if result is None else f"Some(CatalogTypeId::{TYPE_IDS[result]})"
            out.append(
                "    OverloadDescriptor { params: &["
                + params_code
                + f"], result: {result_code}, distinct: {str(distinct).lower()}, display_signature: {json.dumps(signature)} }},"
            )
        out += [
            "];",
            "",
        ]
        extension_signatures = entry.get("koko_overloads", [])
        if extension_signatures:
            out.append(f"static KOKO_OVERLOADS_{index}: &[KokoOverloadDescriptor] = &[")
            for signature in extension_signatures:
                family, min_arity = KOKO_OVERLOAD_SPECS[str(signature)]
                out.append(
                    "    KokoOverloadDescriptor { family: KokoOverloadFamily::"
                    + family
                    + f", min_arity: {min_arity}, display_signature: {json.dumps(signature)} }},"
                )
            out += [
                "];",
                "",
            ]
        descriptor_rows.append((entry, source_rows))

    descriptor_entries: list[tuple[str, str]] = []
    for index, (entry, _) in enumerate(descriptor_rows):
        kind = str(entry["kind"])
        semantic = str(entry["semantic"])
        if kind == "aggregate":
            agg = {
                "Count": "AggOp::Count",
                "CountStar": "AggOp::CountStar",
                "Sum": "AggOp::Sum",
                "Avg": "AggOp::Avg",
                "Min": "AggOp::Min",
                "Max": "AggOp::Max",
                "Collect": "AggOp::Collect",
                "PercentileDisc": "AggOp::PercentileDisc(0)",
            }[semantic]
            function = f"BuiltinFunction::Aggregate({agg})"
        elif kind in ("table", "standalone_table"):
            function = f"BuiltinFunction::Table(BuiltinTableFunction::{semantic})"
        elif kind == "algorithm":
            function = f"BuiltinFunction::GraphAlgorithm(BuiltinGraphAlgorithm::{semantic})"
        else:
            function = f"BuiltinFunction::Scalar({scalar_expression(semantic)})"
        name = str(entry["name"])
        positions = ", ".join(str(value) for value in entry.get("string_coerce", []))
        koko_overloads = (
            f"KOKO_OVERLOADS_{index}" if entry.get("koko_overloads") else "&[]"
        )
        descriptor_entries.append((
            name,
            "    BuiltinDescriptor { called_name: "
            + json.dumps(name)
            + f", function: {function}, catalog_kind: FunctionCatalogKind::{MANIFEST_KIND_VARIANT[kind]}, overloads: OVERLOADS_{index}, koko_overloads: {koko_overloads}, string_coerce: &[{positions}], variable_arity: {str(bool(entry.get('variable_arity', False))).lower()}, bindable: {str(bool(entry.get('bindable', True))).lower()} }},",
        ))
    manifest_names = {str(entry["name"]).lower() for entry in entries}
    catalog_only: dict[str, tuple[str, str]] = {}
    for name, kind, _ in rows:
        if name.lower() in manifest_names:
            continue
        variant = kind_variants[kind]
        previous = catalog_only.setdefault(name.lower(), (name, variant))
        if previous[1] != variant:
            raise ValueError(
                f"catalog function {name!r} has inconsistent kinds: {previous[1]} and {variant}"
            )
    descriptor_entries.extend(
        (
            name,
            "    BuiltinDescriptor { called_name: "
            + json.dumps(name)
            + f", function: BuiltinFunction::CatalogOnly, catalog_kind: FunctionCatalogKind::{variant}, overloads: &[], koko_overloads: &[], string_coerce: &[], variable_arity: false, bindable: false }},",
        )
        for name, variant in catalog_only.values()
    )
    descriptor_entries.sort(key=lambda entry: entry[0].lower())
    koko_catalog_rows = [
        (str(entry["name"]).upper(), "SCALAR FUNCTION", str(signature))
        for entry in entries
        for signature in entry.get("koko_overloads", [])
    ]
    koko_catalog_rows.extend(
        (
            str(entry["name"]).upper(),
            MANIFEST_TO_CATALOG_KIND[str(entry["kind"])],
            str(signature),
        )
        for entry in entries
        for signature in (
            [entry["catalog_signature"]]
            if "catalog_signature" in entry
            else entry.get("catalog_signatures", [])
        )
    )
    out += [
        "#[rustfmt::skip]",
        "pub static BUILTIN_DESCRIPTORS: &[BuiltinDescriptor] = &[",
    ]
    out.extend(line for _, line in descriptor_entries)
    out += [
        "];",
        "",
        "fn ascii_fold(byte: u8) -> u8 { if byte.is_ascii_uppercase() { byte + (b'a' - b'A') } else { byte } }",
        "",
        "fn ascii_case_cmp(left: &str, right: &str) -> Ordering {",
        "    let mut left = left.bytes().map(ascii_fold);",
        "    let mut right = right.bytes().map(ascii_fold);",
        "    loop { match (left.next(), right.next()) { (Some(a), Some(b)) if a != b => return a.cmp(&b), (Some(_), Some(_)) => {}, (None, None) => return Ordering::Equal, (None, Some(_)) => return Ordering::Less, (Some(_), None) => return Ordering::Greater } }",
        "}",
        "",
        "pub fn resolve_builtin(called_name: &str) -> Option<&'static BuiltinDescriptor> {",
        "    BUILTIN_DESCRIPTORS.binary_search_by(|descriptor| ascii_case_cmp(descriptor.called_name, called_name)).ok().map(|index| &BUILTIN_DESCRIPTORS[index])",
        "}",
        "",
        "pub fn resolve_builtin_scalar(called_name: &str) -> Option<BuiltinScalar> {",
        "    let descriptor = resolve_builtin(called_name)?;",
        "    if !descriptor.bindable { return None; }",
        "    match descriptor.function { BuiltinFunction::Scalar(function) => Some(function), BuiltinFunction::Aggregate(_) | BuiltinFunction::Table(_) | BuiltinFunction::GraphAlgorithm(_) | BuiltinFunction::CatalogOnly => None }",
        "}",
        "",
        "/// `CALL show_functions()` rows: the Ladybug-derived source order followed by Koko-only overloads.",
        "#[rustfmt::skip]",
        f"pub static FUNCTION_CATALOG: [FunctionCatalogEntry; {len(rows) + len(koko_catalog_rows)}] = [",
    ]
    out.extend(
        "    FunctionCatalogEntry { name: "
        + json.dumps(name)
        + f", kind: FunctionCatalogKind::{kind_variants[kind]}, signature: "
        + json.dumps(signature)
        + " },"
        for name, kind, signature in rows + koko_catalog_rows
    )
    out += [
        "];",
        "",
    ]
    return "\n".join(out)


def format_rust(source: str) -> str:
    completed = subprocess.run(
        ["rustfmt", "--edition", "2024", "--emit", "stdout"],
        cwd=ROOT,
        input=source,
        text=True,
        capture_output=True,
        check=True,
    )
    return completed.stdout


def main() -> None:
    args = parse_args()
    if args.refresh_from_stdin:
        refreshed = sys.stdin.read()
        rows = []
        for number, raw in enumerate(refreshed.splitlines(), 1):
            if not raw:
                continue
            parts = raw.split("|")
            if len(parts) != 3:
                raise ValueError(f"stdin row {number} must contain exactly three pipe-separated fields")
            rows.append(tuple(parts))
        ROWS_PATH.write_text("".join("\t".join(row) + "\n" for row in rows))

    rows = parse_rows(ROWS_PATH.read_text())
    entries = parse_manifest(MANIFEST_PATH.read_text())
    generated = format_rust(emit(rows, entries))
    if args.check:
        current = OUTPUT_PATH.read_text() if OUTPUT_PATH.exists() else ""
        if current != generated:
            sys.exit(f"{OUTPUT_PATH.relative_to(ROOT)} is stale; run scripts/gen_fn_catalog.py")
        return
    OUTPUT_PATH.write_text(generated)


if __name__ == "__main__":
    main()
