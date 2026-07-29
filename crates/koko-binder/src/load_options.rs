use koko_common::{Error, Result, file_resolver::FileFormat};
use koko_parser::ast;

pub(crate) fn option_type_name(value: &ast::LoadOptVal) -> &'static str {
    match value {
        ast::LoadOptVal::Bool(_) => "BOOL",
        ast::LoadOptVal::Int(_) => "INT64",
        ast::LoadOptVal::Float(_) => "DOUBLE",
        ast::LoadOptVal::Str(_) => "STRING",
        ast::LoadOptVal::List(_) => "ANY[]",
    }
}

pub(crate) fn bool_value(key: &str, value: &ast::LoadOptVal) -> Result<bool> {
    match value {
        ast::LoadOptVal::Bool(value) => Ok(*value),
        ast::LoadOptVal::Int(1) => Ok(true),
        ast::LoadOptVal::Int(0) => Ok(false),
        ast::LoadOptVal::Str(value) if value.eq_ignore_ascii_case("true") || value == "1" => {
            Ok(true)
        }
        ast::LoadOptVal::Str(value) if value.eq_ignore_ascii_case("false") || value == "0" => {
            Ok(false)
        }
        _ => Err(Error::binder(format!(
            "The type of csv parsing option {key} must be a boolean."
        ))),
    }
}

pub(crate) fn int_value(
    key: &str,
    value: &ast::LoadOptVal,
    negative_message: &str,
) -> Result<usize> {
    match value {
        ast::LoadOptVal::Int(value) if *value >= 0 => Ok(*value as usize),
        ast::LoadOptVal::Int(_) => Err(Error::runtime(negative_message.to_string())),
        _ => Err(Error::binder(format!(
            "The type of csv parsing option {key} must be a INT64."
        ))),
    }
}

pub(crate) fn string_value(key: &str, value: &ast::LoadOptVal) -> Result<String> {
    match value {
        ast::LoadOptVal::Str(value) => Ok(value.clone()),
        _ => Err(Error::binder(format!(
            "The type of csv parsing option {key} must be a string."
        ))),
    }
}

pub(crate) fn char_value(key: &str, value: &ast::LoadOptVal) -> Result<u8> {
    let value = string_value(key, value)?;
    match value.as_bytes() {
        [byte] => Ok(*byte),
        [b'\\', b't'] => Ok(b'\t'),
        [b'\\', byte] => Ok(*byte),
        _ => Err(Error::binder(
            "Copy csv option value must be a single character with an optional escape character."
                .to_string(),
        )),
    }
}

pub(crate) fn string_list_value(key: &str, value: &ast::LoadOptVal) -> Result<Vec<String>> {
    match value {
        ast::LoadOptVal::List(items) => items
            .iter()
            .map(|item| match item {
                ast::LoadOptVal::Str(value) => Ok(value.clone()),
                _ => Err(Error::binder(format!(
                    "The type of csv parsing option {key} must be a STRING[]."
                ))),
            })
            .collect(),
        _ => Err(Error::binder(format!(
            "The type of csv parsing option {key} must be a STRING[]."
        ))),
    }
}

pub(crate) fn validate_file_format(value: &str) -> Result<()> {
    FileFormat::parse(value).map(|_| ())
}
