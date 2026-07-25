//! Query parameter admission, origin tracking, and redacted storage.

use crate::value_codec::{CodecError, decode_parameter_object, decode_value};
use koko::{LogicalType, QueryParameter, SyntaxStatus, TokenKind, Value, analyze_cypher};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParameterOrigin {
    File(PathBuf),
    CommandLine,
    Interactive,
}

#[derive(Debug, Clone)]
pub struct ParameterEntry {
    name: String,
    value: Value,
    logical_type: LogicalType,
    origin: ParameterOrigin,
}

impl ParameterEntry {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn value(&self) -> &Value {
        &self.value
    }

    pub const fn logical_type(&self) -> &LogicalType {
        &self.logical_type
    }

    pub const fn origin(&self) -> &ParameterOrigin {
        &self.origin
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ParameterError {
    #[error("parameter must use NAME=JSON syntax")]
    Assignment,
    #[error("invalid parameter name `{0}`")]
    Name(String),
    #[error("duplicate command-line parameter `{0}`")]
    Duplicate(String),
    #[error(transparent)]
    Value(#[from] CodecError),
}

#[derive(Debug, Clone, Default)]
pub struct ParameterStore {
    entries: BTreeMap<String, ParameterEntry>,
}

impl ParameterStore {
    pub fn entries(&self) -> impl ExactSizeIterator<Item = &ParameterEntry> {
        self.entries.values()
    }

    pub fn get(&self, name: &str) -> Option<&ParameterEntry> {
        self.entries.get(name)
    }

    pub fn insert_file_object(
        &mut self,
        source: &str,
        path: PathBuf,
    ) -> Result<(), ParameterError> {
        for (name, value) in decode_parameter_object(source)? {
            validate_name(&name)?;
            let logical_type = value.logical_type();
            self.entries.insert(
                name.clone(),
                ParameterEntry {
                    name,
                    value,
                    logical_type,
                    origin: ParameterOrigin::File(path.clone()),
                },
            );
        }
        Ok(())
    }

    pub fn insert_command_line(&mut self, assignment: &str) -> Result<(), ParameterError> {
        let (name, source) = assignment
            .split_once('=')
            .ok_or(ParameterError::Assignment)?;
        validate_name(name)?;
        if self
            .entries
            .get(name)
            .is_some_and(|entry| entry.origin == ParameterOrigin::CommandLine)
        {
            return Err(ParameterError::Duplicate(name.to_string()));
        }
        let value = decode_value(source)?;
        let logical_type = value.logical_type();
        self.entries.insert(
            name.to_string(),
            ParameterEntry {
                name: name.to_string(),
                value,
                logical_type,
                origin: ParameterOrigin::CommandLine,
            },
        );
        Ok(())
    }

    pub fn insert_interactive(&mut self, name: &str, source: &str) -> Result<(), ParameterError> {
        validate_name(name)?;
        let value = decode_value(source)?;
        let logical_type = value.logical_type();
        self.entries.insert(
            name.to_string(),
            ParameterEntry {
                name: name.to_string(),
                value,
                logical_type,
                origin: ParameterOrigin::Interactive,
            },
        );
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> bool {
        self.entries.remove(name).is_some()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn query_parameters(&self) -> Vec<QueryParameter<'_>> {
        self.entries
            .values()
            .map(|entry| QueryParameter::typed(&entry.name, &entry.value, &entry.logical_type))
            .collect()
    }
}

fn validate_name(name: &str) -> Result<(), ParameterError> {
    if name.is_empty() {
        return Err(ParameterError::Name(name.to_string()));
    }
    let source = format!("RETURN ${name}");
    let analysis = analyze_cypher(&source, None);
    let parameter_count = analysis
        .tokens()
        .iter()
        .filter(|token| token.kind() == TokenKind::Parameter)
        .count();
    if analysis.status() != SyntaxStatus::Complete || parameter_count != 1 {
        return Err(ParameterError::Name(name.to_string()));
    }
    let parameter = analysis
        .tokens()
        .iter()
        .find(|token| token.kind() == TokenKind::Parameter)
        .expect("counted one parameter");
    if source[parameter.span().start()..parameter.span().end()] != format!("${name}") {
        return Err(ParameterError::Name(name.to_string()));
    }
    Ok(())
}
