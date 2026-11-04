use anyhow::{Result, anyhow, bail};

use super::lex::{Spanned, Tok, lex};
use super::{Action, Expr, FUNCTIONS, Program, Rule, Schema};
use crate::item::Value;

pub fn parse(text: &str, schema: &Schema) -> Result<Program, Vec<(usize, String)>> {
    let mut rules = Vec::new();
    let mut default = None;
    let mut errors = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let lineno = n + 1;
        match parse_line(line, lineno, schema) {
            Ok(None) => {}
            Ok(Some(Line::Rule(r))) => {
                if default.is_some() {
                    errors.push((lineno, "rule after default is never reached".into()));
                }
                rules.push(r);
            }
            Ok(Some(Line::Default(a))) => {
                if default.is_some() {
                    errors.push((lineno, "default given twice".into()));
                }
                default = Some(a);
            }
            Err(e) => errors.push((lineno, e.to_string())),
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(Program {
        rules,
        default: default.unwrap_or(Action::Show),
    })
}

enum Line {
    Rule(Rule),
    Default(Action),
}

fn parse_line(line: &str, lineno: usize, schema: &Schema) -> Result<Option<Line>> {
    let (toks, code_end) = lex(line)?;
    if toks.is_empty() {
        return Ok(None);
    }
    let mut p = Parser {
        toks: &toks,
        pos: 0,
        schema,
    };
    let action = match p.ident()?.as_str() {
        "show" => Action::Show,
        "hide" => Action::Hide,
        "tag" => Action::Tag,
        "default" => {
            let a = match p.ident()?.as_str() {
                "show" => Action::Show,
                "hide" => Action::Hide,
                other => bail!("default must be show or hide, not {other}"),
            };
            p.end()?;
            return Ok(Some(Line::Default(a)));
        }
        other => bail!("expected show, hide, tag, or default, not {other}"),
    };
    let name = match p.peek() {
        Some(Tok::Str(s)) => {
            let s = s.clone();
            p.pos += 1;
            Some(s)
        }
        _ => None,
    };
    if action == Action::Tag && name.is_none() {
        bail!("tag needs a name: tag \"name\" if ...");
    }
    if p.ident()? != "if" {
        bail!("expected 'if'");
    }
    let cond_start = p.offset();
    let cond = p.expr()?;
    p.end()?;
    let cond_src = line[cond_start..code_end].trim().to_string();
    Ok(Some(Line::Rule(Rule {
        action,
        name: name.unwrap_or_else(|| format!("line {lineno}")),
        cond,
        cond_src,
    })))
}

struct Parser<'a> {
    toks: &'a [Spanned],
    pos: usize,
    schema: &'a Schema,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|t| &t.0)
    }

    fn offset(&self) -> usize {
        self.toks.get(self.pos).map_or(0, |t| t.1)
    }

    fn next(&mut self) -> Result<Tok> {
        let t = self
            .toks
            .get(self.pos)
            .ok_or_else(|| anyhow!("unexpected end of rule"))?;
        self.pos += 1;
        Ok(t.0.clone())
    }

    fn ident(&mut self) -> Result<String> {
        match self.next()? {
            Tok::Ident(s) => Ok(s),
            t => bail!("expected a word, found {t:?}"),
        }
    }

    fn keyword(&mut self, kw: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Ident(s)) if s == kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, t: Tok) -> Result<()> {
        let got = self.next()?;
        if got != t {
            bail!("expected {t:?}, found {got:?}");
        }
        Ok(())
    }

    fn end(&self) -> Result<()> {
        match self.toks.get(self.pos) {
            None => Ok(()),
            Some((t, off)) => bail!("unexpected {t:?} at column {}", off + 1),
        }
    }

    fn expr(&mut self) -> Result<Expr> {
        let mut e = self.and()?;
        while self.keyword("or") {
            e = Expr::Or(Box::new(e), Box::new(self.and()?));
        }
        Ok(e)
    }

    fn and(&mut self) -> Result<Expr> {
        let mut e = self.not()?;
        while self.keyword("and") {
            e = Expr::And(Box::new(e), Box::new(self.not()?));
        }
        Ok(e)
    }

    fn not(&mut self) -> Result<Expr> {
        if self.keyword("not") {
            return Ok(Expr::Not(Box::new(self.not()?)));
        }
        self.cmp()
    }

    fn cmp(&mut self) -> Result<Expr> {
        let lhs = self.primary()?;
        let op = match self.peek() {
            Some(Tok::Op(op)) => *op,
            Some(Tok::Ident(s)) if matches!(s.as_str(), "in" | "contains" | "matches") => {
                match s.as_str() {
                    "in" => "in",
                    "contains" => "contains",
                    _ => "matches",
                }
            }
            _ => return Ok(lhs),
        };
        self.pos += 1;
        let rhs = self.primary()?;
        if op == "matches"
            && let Expr::Lit(Value::Str(g)) = &rhs
        {
            crate::glob::compile(g)?;
        }
        Ok(Expr::Cmp(op, Box::new(lhs), Box::new(rhs)))
    }

    fn primary(&mut self) -> Result<Expr> {
        match self.next()? {
            Tok::Str(s) => Ok(Expr::Lit(Value::Str(s))),
            Tok::Int(n) => Ok(Expr::Lit(Value::Int(n))),
            Tok::Duration(n) => Ok(Expr::Lit(Value::Duration(n))),
            Tok::LParen => {
                let e = self.expr()?;
                self.expect(Tok::RParen)?;
                Ok(e)
            }
            Tok::LBrack => {
                let mut items = Vec::new();
                if self.peek() != Some(&Tok::RBrack) {
                    loop {
                        items.push(self.primary()?);
                        if self.peek() == Some(&Tok::Comma) {
                            self.pos += 1;
                            continue;
                        }
                        break;
                    }
                }
                self.expect(Tok::RBrack)?;
                Ok(Expr::List(items))
            }
            Tok::Ident(name) => match name.as_str() {
                "true" => Ok(Expr::Lit(Value::Bool(true))),
                "false" => Ok(Expr::Lit(Value::Bool(false))),
                _ if self.peek() == Some(&Tok::LParen) => self.call(name),
                _ => {
                    if !self.schema.fields.iter().any(|f| f.name == name) {
                        bail!("unknown field '{name}'");
                    }
                    Ok(Expr::Field(name))
                }
            },
            t => bail!("unexpected {t:?}"),
        }
    }

    fn call(&mut self, name: String) -> Result<Expr> {
        let f = FUNCTIONS
            .iter()
            .find(|f| f.name == name)
            .ok_or_else(|| anyhow!("unknown function '{name}'"))?;
        self.expect(Tok::LParen)?;
        let mut args = Vec::new();
        let mut named = Vec::new();
        if self.peek() != Some(&Tok::RParen) {
            loop {
                let is_named = matches!(
                    (self.toks.get(self.pos), self.toks.get(self.pos + 1)),
                    (Some((Tok::Ident(_), _)), Some((Tok::Colon, _)))
                );
                if is_named {
                    let k = self.ident()?;
                    self.pos += 1;
                    if !f.named.contains(&k.as_str()) {
                        bail!("{name}() has no argument '{k}'");
                    }
                    named.push((k, self.expr()?));
                } else {
                    args.push(self.expr()?);
                }
                if self.peek() == Some(&Tok::Comma) {
                    self.pos += 1;
                    continue;
                }
                break;
            }
        }
        self.expect(Tok::RParen)?;
        if args.len() != f.args {
            bail!("{name}() takes {} argument(s), got {}", f.args, args.len());
        }
        Ok(Expr::Call { name, args, named })
    }
}
