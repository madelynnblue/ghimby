use anyhow::{Result, anyhow, bail};
use regex::Regex;

use super::{Action, Compiled, Expr};
use crate::codeowners::normalize_owner;
use crate::item::{Item, Value};

#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub show: bool,
    /// The rule that decided, or "default".
    pub rule: String,
    pub tags: Vec<String>,
    pub errors: Vec<String>,
}

impl Compiled {
    pub fn decide(&self, item: &Item, now: i64) -> Decision {
        let mut tags = Vec::new();
        let mut errors = Vec::new();
        for r in &self.program.rules {
            let matched = match self.eval(&r.cond, item, now) {
                Ok(Value::Bool(b)) => b,
                Ok(Value::Null) => false,
                Ok(v) => {
                    errors.push(format!("{}: condition is a {}, not a bool", r.name, v.type_name()));
                    false
                }
                Err(e) => {
                    errors.push(format!("{}: {e}", r.name));
                    false
                }
            };
            if !matched {
                continue;
            }
            match r.action {
                Action::Tag => tags.push(r.name.clone()),
                a => {
                    return Decision {
                        show: a == Action::Show,
                        rule: r.name.clone(),
                        tags,
                        errors,
                    };
                }
            }
        }
        Decision {
            show: self.program.default == Action::Show,
            rule: "default".into(),
            tags,
            errors,
        }
    }

    fn glob(&self, g: &str) -> Result<Regex> {
        let mut cache = self.globs.lock().unwrap();
        if let Some(re) = cache.get(g) {
            return Ok(re.clone());
        }
        let re = crate::glob::compile(g)?;
        cache.insert(g.to_string(), re.clone());
        Ok(re)
    }

    fn eval(&self, e: &Expr, item: &Item, now: i64) -> Result<Value> {
        Ok(match e {
            Expr::Lit(v) => v.clone(),
            Expr::Field(f) => item.get(f).clone(),
            Expr::List(xs) => Value::List(
                xs.iter()
                    .map(|x| self.eval(x, item, now))
                    .collect::<Result<_>>()?,
            ),
            Expr::Not(x) => match self.eval(x, item, now)? {
                Value::Bool(b) => Value::Bool(!b),
                Value::Null => Value::Bool(true),
                v => bail!("'not' needs a bool, got {}", v.type_name()),
            },
            Expr::And(a, b) => {
                Value::Bool(truthy(self.eval(a, item, now)?)? && truthy(self.eval(b, item, now)?)?)
            }
            Expr::Or(a, b) => {
                Value::Bool(truthy(self.eval(a, item, now)?)? || truthy(self.eval(b, item, now)?)?)
            }
            Expr::Cmp(op, a, b) => {
                let a = self.eval(a, item, now)?;
                let b = self.eval(b, item, now)?;
                Value::Bool(self.cmp(op, &a, &b)?)
            }
            Expr::Call { name, args, named } => {
                let args: Vec<Value> = args
                    .iter()
                    .map(|x| self.eval(x, item, now))
                    .collect::<Result<_>>()?;
                let named: Vec<(&str, Value)> = named
                    .iter()
                    .map(|(k, x)| Ok((k.as_str(), self.eval(x, item, now)?)))
                    .collect::<Result<_>>()?;
                self.call(name, &args, &named, item, now)?
            }
        })
    }

    fn cmp(&self, op: &str, a: &Value, b: &Value) -> Result<bool> {
        // Missing fields never match, whatever the operator.
        if matches!(a, Value::Null) || matches!(b, Value::Null) {
            return Ok(false);
        }
        Ok(match op {
            "==" => eq(a, b)?,
            "!=" => !eq(a, b)?,
            "<" | ">" | "<=" | ">=" => {
                let ord = match (a, b) {
                    (Value::Int(x), Value::Int(y))
                    | (Value::Duration(x), Value::Duration(y))
                    | (Value::Time(x), Value::Time(y)) => x.cmp(y),
                    _ => bail!("can't compare {} {op} {}", a.type_name(), b.type_name()),
                };
                match op {
                    "<" => ord.is_lt(),
                    ">" => ord.is_gt(),
                    "<=" => ord.is_le(),
                    _ => ord.is_ge(),
                }
            }
            "in" => {
                let Value::List(ys) = b else {
                    bail!("'in' needs a list on the right, got {}", b.type_name());
                };
                match a {
                    Value::List(xs) => any_eq(xs, ys)?,
                    _ => any_eq(std::slice::from_ref(a), ys)?,
                }
            }
            "contains" => match (a, b) {
                (Value::List(xs), _) => any_eq(xs, std::slice::from_ref(b))?,
                (Value::Str(x), Value::Str(y)) => x.contains(y.as_str()),
                _ => bail!("can't do {} contains {}", a.type_name(), b.type_name()),
            },
            "matches" => {
                let Value::Str(g) = b else {
                    bail!("'matches' needs a glob string, got {}", b.type_name());
                };
                let re = self.glob(g)?;
                match a {
                    Value::Str(s) => re.is_match(s),
                    Value::List(xs) => xs
                        .iter()
                        .any(|x| matches!(x, Value::Str(s) if re.is_match(s))),
                    _ => bail!("can't match a {}", a.type_name()),
                }
            }
            _ => bail!("unknown operator {op}"),
        })
    }

    fn call(
        &self,
        name: &str,
        args: &[Value],
        named: &[(&str, Value)],
        item: &Item,
        now: i64,
    ) -> Result<Value> {
        let named = |k: &str| named.iter().find(|(n, _)| *n == k).map(|(_, v)| v);
        Ok(match name {
            "any" | "all" => {
                let (list, Value::Str(g)) = (&args[0], &args[1]) else {
                    bail!("{name}() needs a glob string as its second argument");
                };
                let xs = match list {
                    Value::Null => return Ok(Value::Bool(false)),
                    Value::List(xs) => xs,
                    v => bail!("{name}() needs a list, got {}", v.type_name()),
                };
                let re = self.glob(g)?;
                let m = |x: &Value| matches!(x, Value::Str(s) if re.is_match(s));
                Value::Bool(if name == "any" {
                    xs.iter().any(m)
                } else {
                    !xs.is_empty() && xs.iter().all(m)
                })
            }
            "age" => match &args[0] {
                Value::Time(t) => Value::Duration(now - t),
                Value::Null => Value::Null,
                v => bail!("age() needs a time, got {}", v.type_name()),
            },
            "len" => match &args[0] {
                Value::List(xs) => Value::Int(xs.len() as i64),
                Value::Sets(xs) => Value::Int(xs.len() as i64),
                Value::Null => Value::Null,
                v => bail!("len() needs a list, got {}", v.type_name()),
            },
            "lower" => match &args[0] {
                Value::Str(s) => Value::Str(s.to_lowercase()),
                Value::Null => Value::Null,
                v => bail!("lower() needs a string, got {}", v.type_name()),
            },
            "owned_by" => {
                let Value::Str(team) = &args[0] else {
                    bail!("owned_by() needs a team string");
                };
                let team = normalize_owner(team);
                let max = match named("max_owners") {
                    None => usize::MAX,
                    Some(Value::Int(n)) if *n >= 1 => *n as usize,
                    Some(v) => bail!("max_owners must be a positive int, got {v:?}"),
                };
                match item.get("codeowners") {
                    Value::Sets(sets) => Value::Bool(
                        sets.iter()
                            .any(|owners| owners.len() <= max && owners.contains(&team)),
                    ),
                    _ => Value::Bool(false),
                }
            }
            "requested" => {
                let Value::Str(who) = &args[0] else {
                    bail!("requested() needs a string");
                };
                let who = normalize_owner(who);
                let has = |field: &str| match item.get(field) {
                    Value::List(xs) => xs
                        .iter()
                        .any(|x| matches!(x, Value::Str(s) if s.eq_ignore_ascii_case(&who))),
                    _ => false,
                };
                Value::Bool(has("requested_users") || has("requested_teams"))
            }
            _ => return Err(anyhow!("unknown function {name}")),
        })
    }
}

fn truthy(v: Value) -> Result<bool> {
    match v {
        Value::Bool(b) => Ok(b),
        Value::Null => Ok(false),
        v => bail!("expected a bool, got {}", v.type_name()),
    }
}

fn eq(a: &Value, b: &Value) -> Result<bool> {
    match (a, b) {
        (Value::Str(_), Value::Str(_))
        | (Value::Int(_), Value::Int(_))
        | (Value::Bool(_), Value::Bool(_))
        | (Value::Duration(_), Value::Duration(_))
        | (Value::Time(_), Value::Time(_))
        | (Value::List(_), Value::List(_)) => Ok(a == b),
        _ => bail!("can't compare {} with {}", a.type_name(), b.type_name()),
    }
}

fn any_eq(xs: &[Value], ys: &[Value]) -> Result<bool> {
    for x in xs {
        for y in ys {
            if eq(x, y)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
