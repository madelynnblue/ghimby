use super::*;
use crate::item::{Item, Value};

fn schema() -> Schema {
    let mut s = Schema::default();
    s.extend(&[
        FieldDoc { name: "state", help: "" },
        FieldDoc { name: "author", help: "" },
        FieldDoc { name: "draft", help: "" },
        FieldDoc { name: "paths", help: "" },
        FieldDoc { name: "labels", help: "" },
        FieldDoc { name: "updated", help: "" },
        FieldDoc { name: "codeowners", help: "" },
        FieldDoc { name: "requested_teams", help: "" },
        FieldDoc { name: "requested_users", help: "" },
        FieldDoc { name: "reviewed_by_me", help: "" },
        FieldDoc { name: "new_since_my_review", help: "" },
    ]);
    s
}

fn item(fields: &[(&str, Value)]) -> Item {
    let mut it = Item {
        id: "1".into(),
        url: String::new(),
        title: String::new(),
        label: String::new(),
        fields: Default::default(),
    };
    for (k, v) in fields {
        it.set(k, v.clone());
    }
    it
}

const RULES: &str = r#"
# comment
hide "finished" if state != "open"   # trailing comment
hide "bots" if author in ["dependabot", "acme-ci"]
hide "already reviewed" if reviewed_by_me and not new_since_my_review
tag "draft" if draft
tag "stale" if age(updated) > 14d
show "storage" if owned_by("@acme/Storage", max_owners: 2) or any(paths, "storage/**")
default hide
"#;

fn decide(fields: &[(&str, Value)]) -> Decision {
    compile(RULES, &schema()).unwrap().decide(&item(fields), 100 * 86400)
}

fn open() -> (&'static str, Value) {
    ("state", Value::Str("open".into()))
}

#[test]
fn cond_src_excludes_comment() {
    let c = compile(RULES, &schema()).unwrap();
    assert_eq!(c.program.rules[0].cond_src, r#"state != "open""#);
    assert_eq!(c.program.default, Action::Hide);
}

#[test]
fn first_match_wins() {
    let d = decide(&[("state", Value::Str("merged".into()))]);
    assert_eq!((d.show, d.rule.as_str()), (false, "finished"));
    let d = decide(&[open(), ("author", Value::Str("dependabot".into()))]);
    assert_eq!(d.rule, "bots");
}

#[test]
fn missing_fields_dont_match() {
    // No state at all: "state != open" is false, so it falls through to default.
    let d = decide(&[]);
    assert_eq!(d.rule, "default");
    assert!(d.errors.is_empty(), "{:?}", d.errors);
}

#[test]
fn owned_by_respects_max_owners() {
    let sets = |v: Vec<Vec<&str>>| {
        Value::Sets(
            v.into_iter()
                .map(|s| s.into_iter().map(String::from).collect())
                .collect(),
        )
    };
    let d = decide(&[open(), ("codeowners", sets(vec![vec!["acme/storage", "acme/infra"]]))]);
    assert_eq!(d.rule, "storage");
    let d = decide(&[
        open(),
        ("codeowners", sets(vec![vec!["acme/storage", "acme/infra", "acme/platform"]])),
    ]);
    assert_eq!(d.rule, "default");
}

#[test]
fn tags_accumulate() {
    let d = decide(&[
        open(),
        ("draft", Value::Bool(true)),
        ("updated", Value::Time(0)),
        ("paths", Value::strs(["storage/x.go"])),
    ]);
    assert_eq!(d.rule, "storage");
    assert_eq!(d.tags, ["draft", "stale"]);
}

#[test]
fn reviewed() {
    let d = decide(&[open(), ("reviewed_by_me", Value::Bool(true)), ("new_since_my_review", Value::Bool(false))]);
    assert_eq!(d.rule, "already reviewed");
    let d = decide(&[open(), ("reviewed_by_me", Value::Bool(true)), ("new_since_my_review", Value::Bool(true))]);
    assert_eq!(d.rule, "default");
}

#[test]
fn parse_errors() {
    let errs = |s: &str| compile(s, &schema()).err().unwrap();
    assert!(errs("show if sate == \"open\"")[0].1.contains("unknown field"));
    assert!(errs("show if nope(1)")[0].1.contains("unknown function"));
    assert!(errs("tag if draft")[0].1.contains("tag needs a name"));
    assert!(errs("show if draft and")[0].1.contains("end of rule"));
    assert!(errs("show if owned_by(\"x\", max: 2)")[0].1.contains("no argument"));
    assert_eq!(errs("default hide\nshow if draft")[0].0, 2);
}

#[test]
fn type_errors_are_reported() {
    let c = compile("show \"x\" if state > 3\ndefault hide", &schema()).unwrap();
    let d = c.decide(&item(&[open()]), 0);
    assert!(!d.show);
    assert_eq!(d.errors.len(), 1);
}

#[test]
fn requested() {
    let c = compile("show \"r\" if requested(\"@acme/storage\")\ndefault hide", &schema()).unwrap();
    let d = c.decide(&item(&[("requested_teams", Value::strs(["acme/storage"]))]), 0);
    assert!(d.show);
}
