//! Source-agnostic items that rules evaluate against.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Str(String),
    /// Unix seconds.
    Time(i64),
    /// Seconds.
    Duration(i64),
    List(Vec<Value>),
    /// A list of string sets, e.g. CODEOWNERS owners per changed file.
    Sets(Vec<Vec<String>>),
}

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Str(_) => "string",
            Value::Time(_) => "time",
            Value::Duration(_) => "duration",
            Value::List(_) => "list",
            Value::Sets(_) => "sets",
        }
    }

    pub fn strs<I: IntoIterator<Item = S>, S: Into<String>>(it: I) -> Value {
        Value::List(it.into_iter().map(|s| Value::Str(s.into())).collect())
    }
}

#[derive(Debug, Clone)]
pub struct Item {
    pub id: String,
    pub url: String,
    pub title: String,
    /// Short label for display, e.g. "monorepo#1234".
    pub label: String,
    pub fields: BTreeMap<String, Value>,
}

impl Item {
    pub fn get(&self, name: &str) -> &Value {
        self.fields.get(name).unwrap_or(&Value::Null)
    }

    pub fn set(&mut self, name: &str, v: Value) {
        self.fields.insert(name.to_string(), v);
    }
}
