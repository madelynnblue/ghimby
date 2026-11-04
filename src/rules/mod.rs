//! The filter rules language: one rule per line, evaluated top to bottom.
//!
//! ```text
//! hide "finished" if state != "open"
//! tag "draft" if draft
//! show "storage" if owned_by("@acme/storage", max_owners: 2)
//! default hide
//! ```

mod eval;
mod lex;
mod parse;

use std::collections::HashMap;
use std::sync::Mutex;

use regex::Regex;

use crate::item::Value;

pub use eval::Decision;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Show,
    Hide,
    Tag,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Show => "show",
            Action::Hide => "hide",
            Action::Tag => "tag",
        }
    }

    pub fn parse(s: &str) -> Option<Action> {
        match s {
            "show" => Some(Action::Show),
            "hide" => Some(Action::Hide),
            "tag" => Some(Action::Tag),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum Expr {
    Lit(Value),
    Field(String),
    List(Vec<Expr>),
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Cmp(&'static str, Box<Expr>, Box<Expr>),
    Call {
        name: String,
        args: Vec<Expr>,
        named: Vec<(String, Expr)>,
    },
}

#[derive(Debug)]
pub struct Rule {
    pub action: Action,
    pub name: String,
    pub cond: Expr,
    /// The condition as written, for the form editor.
    pub cond_src: String,
}

#[derive(Debug)]
pub struct Program {
    pub rules: Vec<Rule>,
    pub default: Action,
}

#[derive(Clone)]
pub struct FieldDoc {
    pub name: &'static str,
    pub help: &'static str,
}

/// The fields sources provide, used to reject typos at parse time.
#[derive(Clone, Default)]
pub struct Schema {
    pub fields: Vec<FieldDoc>,
}

impl Schema {
    pub fn extend(&mut self, fields: &[FieldDoc]) {
        for f in fields {
            if !self.fields.iter().any(|g| g.name == f.name) {
                self.fields.push(f.clone());
            }
        }
    }
}

pub struct FuncDoc {
    pub name: &'static str,
    pub args: usize,
    pub named: &'static [&'static str],
    pub help: &'static str,
}

pub const FUNCTIONS: &[FuncDoc] = &[
    FuncDoc {
        name: "any",
        args: 2,
        named: &[],
        help: "any(list, \"glob\"): some element matches the glob",
    },
    FuncDoc {
        name: "all",
        args: 2,
        named: &[],
        help: "all(list, \"glob\"): every element matches the glob (false if empty)",
    },
    FuncDoc {
        name: "age",
        args: 1,
        named: &[],
        help: "age(time): how long ago, compare with durations like 14d, 3h",
    },
    FuncDoc {
        name: "len",
        args: 1,
        named: &[],
        help: "len(list): number of elements",
    },
    FuncDoc {
        name: "lower",
        args: 1,
        named: &[],
        help: "lower(string): lowercased",
    },
    FuncDoc {
        name: "owned_by",
        args: 1,
        named: &["max_owners"],
        help: "owned_by(\"@org/team\", max_owners: N): a changed file's CODEOWNERS line includes the team and has at most N owners",
    },
    FuncDoc {
        name: "requested",
        args: 1,
        named: &[],
        help: "requested(\"login\" or \"org/team\"): a review is currently requested from them",
    },
];

pub struct Compiled {
    pub program: Program,
    globs: Mutex<HashMap<String, Regex>>,
}

pub fn compile(text: &str, schema: &Schema) -> Result<Compiled, Vec<(usize, String)>> {
    Ok(Compiled {
        program: parse::parse(text, schema)?,
        globs: Mutex::new(HashMap::new()),
    })
}

pub fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn format_rule(action: Action, name: &str, cond: &str) -> String {
    format!("{} {} if {}", action.as_str(), quote(name), cond.trim())
}

#[cfg(test)]
mod tests;
