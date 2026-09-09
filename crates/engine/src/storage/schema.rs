//! Relation schemas.
//!
//! Identifier folding rule: an unquoted SQL identifier is matched
//! case-insensitively against the stored field name, while a `"quoted"` one
//! must match exactly. Stored names keep whatever case the source gave them,
//! so a CSV header of `TripDistance` prints as `TripDistance` but is reachable
//! as `tripdistance`.

use crate::types::DataType;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
}

impl Field {
    pub fn new(name: impl Into<String>, data_type: DataType, nullable: bool) -> Field {
        Field {
            name: name.into(),
            data_type,
            nullable,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Schema {
    pub fields: Vec<Field>,
}

/// Outcome of resolving a column name against a schema.
pub enum Resolution {
    Found(usize),
    NotFound,
    /// Two fields differ only by case and the reference was unquoted.
    Ambiguous(Vec<usize>),
}

impl Schema {
    pub fn new(fields: Vec<Field>) -> Schema {
        Schema { fields }
    }

    pub fn empty() -> Schema {
        Schema { fields: Vec::new() }
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    pub fn field(&self, i: usize) -> &Field {
        &self.fields[i]
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.fields.iter().map(|f| f.name.as_str())
    }

    /// `name` must already be normalized by `ast::Ident::normalized`, i.e.
    /// lowercased if it was unquoted.
    pub fn resolve(&self, name: &str, quoted: bool) -> Resolution {
        let matches: Vec<usize> = self
            .fields
            .iter()
            .enumerate()
            .filter(|(_, f)| {
                if quoted {
                    f.name == name
                } else {
                    f.name.eq_ignore_ascii_case(name)
                }
            })
            .map(|(i, _)| i)
            .collect();
        match matches.len() {
            0 => Resolution::NotFound,
            1 => Resolution::Found(matches[0]),
            _ => Resolution::Ambiguous(matches),
        }
    }
}
