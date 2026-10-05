//! Request rewrite rules. The rules are data (config); the code only knows the kinds of operation.
//! Every operation reports whether it changed the body, so the ledger names only the rules that
//! did something, and an untouched body is forwarded byte for byte.

use crate::config::{OpKind, RuleCfg};
use serde_json::{Map, Value};

#[derive(Debug, Clone)]
pub enum Op {
    /// Delete the value at `pointer` (object key or array index).
    Remove { pointer: Vec<String> },
    /// Keep only the array elements where every `subpointer` equals its value.
    FilterArray { pointer: Vec<String>, keep_where: Vec<(String, Value)> },
    /// Deep-merge an object at `pointer` (default: the root). Keys the value does not mention stay.
    Merge { pointer: Vec<String>, value: Value },
    /// Set `pointer` to `value` only when it is absent.
    SetDefault { pointer: Vec<String>, value: Value },
    /// Rename `/model` (optionally only from the listed names).
    RenameModel { from: Option<Vec<String>>, to: String },
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub name: String,
    pub paths: Vec<String>,
    pub op: Op,
}

impl Rule {
    pub fn from_cfg(c: &RuleCfg) -> Result<Rule, String> {
        let err = |m: &str| format!("config: rule {}: {m}", c.name);
        if c.paths.is_empty() {
            return Err(err("paths is empty"));
        }
        let ptr = |required: bool| -> Result<Vec<String>, String> {
            match &c.pointer {
                Some(p) => parse_pointer(p).ok_or_else(|| err(&format!("bad JSON pointer {p:?}"))),
                None if required => Err(err("pointer is required")),
                None => Ok(vec![]),
            }
        };
        let op = match c.op {
            OpKind::Remove => {
                let p = ptr(true)?;
                if p.is_empty() {
                    return Err(err("remove needs a non-root pointer"));
                }
                Op::Remove { pointer: p }
            }
            OpKind::FilterArray => {
                let kw = c.keep_where.as_ref().ok_or_else(|| err("filter_array needs keep_where"))?;
                if kw.is_empty() {
                    return Err(err("keep_where is empty"));
                }
                for k in kw.keys() {
                    if parse_pointer(k).is_none() {
                        return Err(err(&format!("bad keep_where pointer {k:?}")));
                    }
                }
                Op::FilterArray { pointer: ptr(true)?, keep_where: kw.iter().map(|(k, v)| (k.clone(), v.clone())).collect() }
            }
            OpKind::Merge => {
                let v = c.value.clone().ok_or_else(|| err("merge needs value"))?;
                if !v.is_object() {
                    return Err(err("merge value must be a table/object"));
                }
                Op::Merge { pointer: ptr(false)?, value: v }
            }
            OpKind::SetDefault => {
                let p = ptr(true)?;
                if p.is_empty() {
                    return Err(err("set_default needs a non-root pointer"));
                }
                Op::SetDefault { pointer: p, value: c.value.clone().ok_or_else(|| err("set_default needs value"))? }
            }
            OpKind::RenameModel => Op::RenameModel { from: c.from.clone(), to: c.to.clone().ok_or_else(|| err("rename_model needs to"))? },
        };
        Ok(Rule { name: c.name.clone(), paths: c.paths.clone(), op })
    }

    pub fn applies_to(&self, path: &str) -> bool {
        self.paths.iter().any(|p| p == path)
    }

    /// Apply to a parsed body. Returns true when the body changed.
    pub fn apply(&self, body: &mut Value) -> bool {
        match &self.op {
            Op::Remove { pointer } => {
                let (last, parent) = pointer.split_last().expect("non-root");
                match get_mut(body, parent) {
                    Some(Value::Object(m)) => m.shift_remove(last).is_some(),
                    Some(Value::Array(a)) => match last.parse::<usize>() {
                        Ok(i) if i < a.len() => {
                            a.remove(i);
                            true
                        }
                        _ => false,
                    },
                    _ => false,
                }
            }
            Op::FilterArray { pointer, keep_where } => match get_mut(body, pointer) {
                Some(Value::Array(a)) => {
                    let before = a.len();
                    a.retain(|el| keep_where.iter().all(|(sp, want)| el.pointer(sp) == Some(want)));
                    a.len() != before
                }
                _ => false,
            },
            Op::Merge { pointer, value } => match ensure_mut(body, pointer) {
                Some(target) => deep_merge(target, value),
                None => false,
            },
            Op::SetDefault { pointer, value } => {
                let (last, parent) = pointer.split_last().expect("non-root");
                match ensure_mut(body, parent) {
                    Some(Value::Object(m)) if !m.contains_key(last) => {
                        m.insert(last.clone(), value.clone());
                        true
                    }
                    _ => false,
                }
            }
            Op::RenameModel { from, to } => rename_model(body, from.as_deref(), to),
        }
    }
}

/// Set `/model` to `to` (when the current name is in `from`, if given). Returns true on change.
pub fn rename_model(body: &mut Value, from: Option<&[String]>, to: &str) -> bool {
    let Some(m) = body.as_object_mut() else { return false };
    let cur = m.get("model").and_then(Value::as_str);
    let Some(cur) = cur else { return false };
    if cur == to || from.is_some_and(|f| !f.iter().any(|x| x == cur)) {
        return false;
    }
    m.insert("model".into(), Value::String(to.to_string()));
    true
}

/// RFC 6901: "" is the root; otherwise "/a/b~1c" -> ["a", "b/c"].
pub fn parse_pointer(p: &str) -> Option<Vec<String>> {
    if p.is_empty() {
        return Some(vec![]);
    }
    let rest = p.strip_prefix('/')?;
    Some(rest.split('/').map(|t| t.replace("~1", "/").replace("~0", "~")).collect())
}

fn get_mut<'a>(mut v: &'a mut Value, toks: &[String]) -> Option<&'a mut Value> {
    for t in toks {
        v = match v {
            Value::Object(m) => m.get_mut(t)?,
            Value::Array(a) => a.get_mut(t.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(v)
}

/// Like get_mut, but creates missing object members (as empty objects) on the way.
fn ensure_mut<'a>(mut v: &'a mut Value, toks: &[String]) -> Option<&'a mut Value> {
    for t in toks {
        v = match v {
            Value::Object(m) => m.entry(t.clone()).or_insert_with(|| Value::Object(Map::new())),
            Value::Array(a) => a.get_mut(t.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(v)
}

/// Merge `src` into `dst`, keeping keys of `dst` that `src` does not mention (dgx-lb's inject).
pub fn deep_merge(dst: &mut Value, src: &Value) -> bool {
    match (dst, src) {
        (Value::Object(d), Value::Object(s)) => {
            let mut changed = false;
            for (k, sv) in s {
                match d.get_mut(k) {
                    Some(dv) if dv.is_object() && sv.is_object() => changed |= deep_merge(dv, sv),
                    Some(dv) if dv == sv => {}
                    _ => {
                        d.insert(k.clone(), sv.clone());
                        changed = true;
                    }
                }
            }
            changed
        }
        (d, s) => {
            if d == s {
                false
            } else {
                *d = s.clone();
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(toml_text: &str) -> Rule {
        let c: RuleCfg = toml::from_str(toml_text).unwrap();
        Rule::from_cfg(&c).unwrap()
    }

    #[test]
    fn pointer_parsing() {
        assert_eq!(parse_pointer(""), Some(vec![]));
        assert_eq!(parse_pointer("/a/b~1c/~0d"), Some(vec!["a".into(), "b/c".into(), "~d".into()]));
        assert_eq!(parse_pointer("a"), None);
    }

    #[test]
    fn remove_and_set_default() {
        let r = rule("name='x'\npaths=['/v1/responses']\nop='remove'\npointer='/include'");
        let mut v = json!({"a": 1, "include": [1], "b": 2});
        assert!(r.apply(&mut v));
        assert_eq!(serde_json::to_string(&v).unwrap(), r#"{"a":1,"b":2}"#);
        assert!(!r.apply(&mut v));
        let r = rule("name='x'\npaths=['/v1/responses']\nop='set_default'\npointer='/reasoning/effort'\nvalue='high'");
        assert!(r.apply(&mut v));
        assert_eq!(v["reasoning"]["effort"], "high");
        v["reasoning"]["effort"] = json!("low");
        assert!(!r.apply(&mut v));
        assert_eq!(v["reasoning"]["effort"], "low");
    }

    #[test]
    fn merge_reports_change_only_when_different() {
        let r = rule("name='x'\npaths=['/v1/chat/completions']\nop='merge'\nvalue={chat_template_kwargs={enable_thinking=false}}");
        let mut v = json!({"chat_template_kwargs": {"foo": 1, "enable_thinking": true}});
        assert!(r.apply(&mut v));
        assert_eq!(v, json!({"chat_template_kwargs": {"foo": 1, "enable_thinking": false}}));
        assert!(!r.apply(&mut v));
    }

    #[test]
    fn rename_model_from_list() {
        let r = rule("name='x'\npaths=['/v1/chat/completions']\nop='rename_model'\nfrom=['a']\nto='b'");
        let mut v = json!({"model": "c"});
        assert!(!r.apply(&mut v));
        v["model"] = json!("a");
        assert!(r.apply(&mut v));
        assert_eq!(v["model"], "b");
    }
}
