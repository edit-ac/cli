//! Bounded retained-source replay for the qualified structure v3 profile.
//! Replay and resource rules are ported from the ENG-1.17 mapper; no live writes.
use std::collections::{BTreeMap, HashMap};
use yrs::updates::decoder::Decode;
use yrs::{Any, Assoc, Doc, Map, Out, ReadTxn, StickyIndex, Text, Transact, XmlTextRef};
mod bounded_decoder;
pub(crate) use bounded_decoder::decode;
pub const ROOTS: [&str; 7] = [
    "registry-parents",
    "registry-texts",
    "registry-placements",
    "delete-probe-tombstones",
    "registry-block-attributes",
    "registry-ownership-ops",
    "registry-owner-origins",
];
pub type Result<T> = std::result::Result<T, &'static str>;
pub fn arr(v: Vec<Any>) -> Any {
    Any::Array(v.into())
}
pub fn obj(v: Vec<(&str, Any)>) -> Any {
    Any::Map(
        v.into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect::<HashMap<_, _>>()
            .into(),
    )
}
pub fn text(d: &Doc, id: &str) -> XmlTextRef {
    let t = d.transact();
    let Some(Out::YXmlText(x)) = t.get_map(ROOTS[1]).unwrap().get(&t, id) else {
        panic!("text")
    };
    x
}
fn string(v: &Any) -> Result<String> {
    if let Any::String(s) = v {
        Ok(s.to_string())
    } else {
        Err("invalid-shape")
    }
}
fn integer(v: &Any) -> Result<u32> {
    if let Any::Number(n) = v {
        if let Some(n) = n.as_f64() {
            if n.is_finite() && n >= 0.0 && n <= u32::MAX as f64 && n.fract() == 0.0 {
                return Ok(n as u32);
            }
        }
    }
    Err("invalid-shape")
}
pub fn id(v: &str, prefix: &str) -> Result<(u32, u32)> {
    let parts: Vec<_> = v.split(':').collect();
    if v.len() > 96 || parts.len() != 3 || parts[0] != prefix {
        return Err("invalid-id");
    };
    let parse = |s: &str| -> Result<u32> {
        let n = s.parse::<u32>().map_err(|_| "invalid-id")?;
        if n == 0 || n.to_string() != s {
            return Err("invalid-id");
        }
        Ok(n)
    };
    Ok((parse(parts[1])?, parse(parts[2])?))
}
fn owner_id(v: &str) -> Result<()> {
    id(v, if v.starts_with("n:") { "n" } else { "o" }).map(|_| ())
}
fn valid_json(v: &Any, depth: usize) -> bool {
    if depth > 32 {
        return false;
    }
    match v {
        Any::Null | Any::Bool(_) | Any::String(_) => true,
        Any::Number(n) => n.as_f64().is_some_and(|n| n.is_finite()),
        Any::Array(a) => a.iter().all(|v| valid_json(v, depth + 1)),
        Any::Map(m) => m.values().all(|v| valid_json(v, depth + 1)),
        _ => false,
    }
}
fn tuple(v: &Any) -> Result<()> {
    let Any::Array(a) = v else {
        return Err("invalid-tuple");
    };
    let Some(Any::String(kind)) = a.first() else {
        return Err("invalid-tuple");
    };
    if !valid_json(v, 0) {
        return Err("invalid-tuple");
    };
    match kind.as_ref() {
        "paragraph" if a.len() == 1 => Ok(()),
        "heading" if a.len() == 2 && integer(&a[1]).is_ok_and(|n| (1..=6).contains(&n)) => Ok(()),
        "paragraph" | "heading" => Err("invalid-tuple"),
        _ => Ok(()),
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub source: String,
    pub start: u32,
    pub end: u32,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Owner {
    pub id: String,
    pub version: String,
    pub spans: Vec<Span>,
    pub deleted: bool,
    pub attrs: Any,
}
#[derive(Clone, Debug, PartialEq)]
pub struct View {
    pub owners: Vec<Owner>,
    pub outcomes: BTreeMap<String, String>,
    pub retained: BTreeMap<String, Any>,
    pub sources: BTreeMap<String, Any>,
    pub inherited: BTreeMap<String, String>,
}
#[derive(Clone)]
struct Op {
    id: String,
    left: String,
    lv: String,
    right: Option<(String, String)>,
    cut: Option<(usize, String, Vec<u8>)>,
}
impl Op {
    fn deps(&self) -> Vec<String> {
        let mut v = vec![self.lv.clone()];
        if let Some((_, r)) = &self.right {
            v.push(r.clone())
        }
        v.into_iter().filter(|v| v != "base").collect()
    }
}
fn operation(key: &str, v: &Any) -> Result<Op> {
    id(key, "o")?;
    let Any::Array(a) = v else {
        return Err("invalid-operation");
    };
    let mut op = Op {
        id: key.into(),
        left: String::new(),
        lv: String::new(),
        right: None,
        cut: None,
    };
    match a.as_ref() {
        [kind, l, lv, index, source, Any::Buffer(cut)] if string(kind)? == "split" => {
            if cut.len() > 256 {
                return Err("resource-limit");
            };
            op.left = string(l)?;
            op.lv = string(lv)?;
            let s = string(source)?;
            id(&s, "n")?;
            StickyIndex::decode_v1(cut).map_err(|_| "invalid-cut")?;
            op.cut = Some((integer(index)? as usize, s, cut.to_vec()));
        }
        [kind, l, lv, r, rv] if string(kind)? == "join" => {
            op.left = string(l)?;
            op.lv = string(lv)?;
            op.right = Some((string(r)?, string(rv)?));
        }
        _ => return Err("invalid-operation"),
    };
    owner_id(&op.left)?;
    if let Some((r, _)) = &op.right {
        owner_id(r)?;
        if r == &op.left {
            return Err("invalid-operation");
        }
    }
    for dep in op.deps() {
        id(&dep, "o")?;
        if dep == key {
            return Err("cycle");
        }
    }
    Ok(op)
}
fn values<T: ReadTxn>(t: &T, root: usize) -> Result<BTreeMap<String, Any>> {
    t.get_map(ROOTS[root])
        .ok_or("invalid-root")?
        .iter(t)
        .map(|(k, v)| match v {
            Out::Any(v) => Ok((k.into(), v)),
            _ => Err("invalid-shape"),
        })
        .collect()
}
fn plain_text<T: ReadTxn>(t: &T, x: &XmlTextRef) -> Result<String> {
    x.diff(t, |_| ())
        .into_iter()
        .map(|r| match r.insert {
            Out::Any(Any::String(s)) => Ok(s.to_string()),
            _ => Err("invalid-text"),
        })
        .collect()
}
fn boundary(s: &str, index: u32) -> bool {
    let mut p = 0;
    for c in s.chars() {
        if p == index {
            return true;
        }
        p += c.len_utf16() as u32;
    }
    p == index
}
fn slice_runs<T: ReadTxn>(t: &T, x: &XmlTextRef, start: u32, end: u32) -> Result<Any> {
    let mut pos = 0;
    let mut runs = vec![];
    for run in x.diff(t, |_| ()) {
        let Out::Any(Any::String(s)) = run.insert else {
            return Err("invalid-text");
        };
        let mut out = String::new();
        for c in s.chars() {
            let next = pos + c.len_utf16() as u32;
            if next > start && pos < end {
                if pos < start || next > end {
                    return Err("invalid-cut");
                };
                out.push(c)
            }
            pos = next;
        }
        if !out.is_empty() {
            runs.push(obj(vec![
                ("insert", Any::from(out)),
                (
                    "attributes",
                    Any::Map(
                        run.attributes
                            .map(|a| {
                                a.into_iter()
                                    .map(|(k, v)| (k.to_string(), v))
                                    .collect::<HashMap<_, _>>()
                            })
                            .unwrap_or_default()
                            .into(),
                    ),
                ),
            ]));
        }
    }
    Ok(arr(runs))
}
pub fn project(d: &Doc) -> Result<View> {
    let t = d.transact();
    if t.store().pending_update().is_some() || t.store().pending_ds().is_some() {
        return Err("missing-binary-dependency");
    };
    for (name, v) in t.root_refs() {
        if !ROOTS.contains(&name) || !matches!(v,Out::YMap(ref m) if m.as_ref().len()==0) {
            return Err("invalid-root");
        }
    }
    let kinds = values(&t, 0)?;
    let origins = values(&t, 6)?;
    let placements = values(&t, 2)?;
    let tombstones = values(&t, 3)?;
    let attrs = values(&t, 4)?;
    let records = values(&t, 5)?;
    if kinds.len() > 4096 || records.len() > 4096 {
        return Err("resource-limit");
    };
    if kinds.keys().ne(origins.keys()) || kinds.keys().ne(placements.keys()) {
        return Err("invalid-source");
    };
    let texts = t.get_map(ROOTS[1]).ok_or("invalid-root")?;
    if texts.len(&t) as usize != kinds.len() {
        return Err("invalid-source");
    };
    let mut source_texts = BTreeMap::new();
    let mut sources = BTreeMap::new();
    let mut owners = vec![];
    let mut total = 0;
    for (key, kind) in &kinds {
        id(key, "n")?;
        if kind != &Any::from("block") || origins[key] != Any::Bool(true) {
            return Err("invalid-source");
        };
        let Some(Out::YXmlText(x)) = texts.get(&t, key) else {
            return Err("invalid-source");
        };
        let plain = plain_text(&t, &x)?;
        total += plain.len();
        if total > 1048576 || x.len(&t) > 262144 {
            return Err("resource-limit");
        };
        let Any::Array(p) = &placements[key] else {
            return Err("invalid-placement");
        };
        if p.len() != 2 || p[0] != Any::from("@root") {
            return Err("invalid-placement");
        };
        let order = integer(&p[1])?;
        let runs = slice_runs(&t, &x, 0, x.len(&t))?;
        let identity = match AsRef::<yrs::branch::Branch>::as_ref(&x).id() {
            yrs::branch::BranchID::Nested(i) => format!("{}:{}", i.client, i.clock),
            _ => return Err("invalid-source"),
        };
        sources.insert(
            key.clone(),
            obj(vec![("identity", Any::from(identity)), ("runs", runs)]),
        );
        owners.push((
            order,
            id(key, "n")?,
            Owner {
                id: key.clone(),
                version: "base".into(),
                spans: vec![Span {
                    source: key.clone(),
                    start: 0,
                    end: x.len(&t),
                }],
                deleted: false,
                attrs: Any::Null,
            },
        ));
        source_texts.insert(key.clone(), (x, plain));
    }
    owners.sort_by_key(|(order, key, _)| (*order, *key));
    let mut owners: Vec<_> = owners.into_iter().map(|(_, _, o)| o).collect();
    let ops: BTreeMap<_, _> = records
        .iter()
        .map(|(k, v)| Ok((k.clone(), operation(k, v)?)))
        .collect::<Result<_>>()?;
    for (k, v) in &tombstones {
        owner_id(k)?;
        if v != &Any::Bool(true)
            || (!kinds.contains_key(k) && !ops.get(k).is_some_and(|op| op.cut.is_some()))
        {
            return Err("invalid-tombstone");
        }
    }
    for (k, v) in &attrs {
        owner_id(k)?;
        if !kinds.contains_key(k) && !ops.get(k).is_some_and(|op| op.cut.is_some()) {
            return Err("invalid-target");
        };
        tuple(v)?;
    }
    // Validate resolved identity/affinity even on a losing branch.
    for op in ops.values() {
        if let Some((_, source, cut)) = &op.cut {
            let sticky = StickyIndex::decode_v1(cut).map_err(|_| "invalid-cut")?;
            if sticky.assoc != Assoc::Before {
                return Err("invalid-cut");
            }
            if let (Some((x, plain)), Some(offset)) =
                (source_texts.get(source), sticky.get_offset(&t))
            {
                if offset.branch.id() != AsRef::<yrs::branch::Branch>::as_ref(x).id()
                    || !boundary(plain, offset.index)
                {
                    return Err("invalid-cut");
                }
            }
        }
    }
    let mut depths: BTreeMap<String, Option<usize>> = BTreeMap::new();
    let mut heights: BTreeMap<String, usize> = BTreeMap::new();
    for _ in 0..=ops.len() {
        let mut changed = false;
        for (key, op) in &ops {
            if depths.contains_key(key) {
                continue;
            }
            let deps = op.deps();
            let known: Vec<_> = deps.iter().filter(|d| ops.contains_key(*d)).collect();
            if known.iter().all(|d| depths.contains_key(*d)) {
                let depth = known.iter().map(|d| heights[*d]).max().unwrap_or(0) + 1;
                if depth > 128 {
                    return Err("resource-limit");
                }
                heights.insert(key.clone(), depth);
                let dormant = deps
                    .iter()
                    .any(|d| !ops.contains_key(d) || depths.get(d) == Some(&None));
                depths.insert(key.clone(), if dormant { None } else { Some(depth) });
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    if depths.len() != ops.len() {
        return Err("cycle");
    };
    let mut ranked: Vec<_> = ops.values().filter(|o| depths[&o.id].is_some()).collect();
    ranked.sort_by_key(|o| (depths[&o.id].unwrap(), id(&o.id, "o").unwrap()));
    let mut outcomes: BTreeMap<_, _> = depths
        .iter()
        .filter(|(_, d)| d.is_none())
        .map(|(k, _)| (k.clone(), "dormant".into()))
        .collect();
    let mut inherited: BTreeMap<String, String> = BTreeMap::new();
    for op in ranked {
        let mut status = "losing";
        if op
            .deps()
            .iter()
            .all(|d| outcomes.get(d).is_some_and(|v| v == "applied"))
        {
            if let Some(i) = owners.iter().position(|o| {
                o.id == op.left && o.version == op.lv && !tombstones.contains_key(&o.id)
            }) {
                if let Some((right, rv)) = &op.right {
                    // Hidden owners retain recovery spans but cannot obstruct
                    // adjacency between the visible command targets.
                    if let Some(j) =
                        (i + 1..owners.len()).find(|j| !tombstones.contains_key(&owners[*j].id))
                    {
                        if owners[j].id == *right && owners[j].version == *rv {
                            let r = owners.remove(j);
                            owners[i].spans.extend(r.spans);
                            owners[i].version = op.id.clone();
                            status = "applied";
                        }
                    }
                } else if let Some((index, source, cut)) = &op.cut {
                    let Some((x, plain)) = source_texts.get(source) else {
                        outcomes.insert(op.id.clone(), "dormant".into());
                        continue;
                    };
                    let sticky = StickyIndex::decode_v1(cut).map_err(|_| "invalid-cut")?;
                    if sticky.assoc != Assoc::Before {
                        return Err("invalid-cut");
                    };
                    if let Some(offset) = sticky.get_offset(&t) {
                        if offset.branch.id() != AsRef::<yrs::branch::Branch>::as_ref(x).id()
                            || !boundary(plain, offset.index)
                        {
                            return Err("invalid-cut");
                        };
                        let span = owners[i].spans.get(*index).ok_or("invalid-cut")?;
                        if span.source != *source
                            || offset.index < span.start
                            || offset.index > span.end
                        {
                            return Err("invalid-cut");
                        };
                        let mut left = owners[i].spans[..*index].to_vec();
                        left.push(Span {
                            source: source.clone(),
                            start: span.start,
                            end: offset.index,
                        });
                        let mut right = vec![Span {
                            source: source.clone(),
                            start: offset.index,
                            end: span.end,
                        }];
                        right.extend_from_slice(&owners[i].spans[index + 1..]);
                        owners[i].spans = left;
                        owners[i].version = op.id.clone();
                        owners.insert(
                            i + 1,
                            Owner {
                                id: op.id.clone(),
                                version: op.id.clone(),
                                spans: right,
                                deleted: false,
                                attrs: Any::Null,
                            },
                        );
                        inherited.insert(op.id.clone(), op.left.clone());
                        status = "applied";
                    } else {
                        status = "dormant"
                    }
                }
            }
        }
        outcomes.insert(op.id.clone(), status.into());
    }
    for o in &mut owners {
        o.deleted = tombstones.contains_key(&o.id);
        let mut k = o.id.clone();
        let mut count = 0;
        loop {
            if let Some(a) = attrs.get(&k) {
                o.attrs = a.clone();
                break;
            }
            if let Some(parent) = inherited.get(&k) {
                k = parent.clone();
                count += 1;
                if count > 128 {
                    return Err("resource-limit");
                }
            } else {
                o.attrs = arr(vec![Any::from("paragraph")]);
                break;
            }
        }
    }
    if owners.iter().map(|o| o.spans.len()).sum::<usize>() > 8192 {
        return Err("resource-limit");
    };
    for (source, (x, _)) in &source_texts {
        let mut spans: Vec<_> = owners
            .iter()
            .flat_map(|o| &o.spans)
            .filter(|s| &s.source == source)
            .collect();
        spans.sort_by_key(|s| (s.start, s.end));
        let mut end = 0;
        for s in spans {
            if s.start != end || s.end < s.start {
                return Err("coverage");
            };
            end = s.end;
        }
        if end != x.len(&t) {
            return Err("coverage");
        }
    }
    Ok(View {
        owners,
        outcomes,
        retained: attrs,
        sources,
        inherited,
    })
}
fn presentation(value: &Any) -> Any {
    if let Any::Array(v) = value {
        if v.first()
            .is_some_and(|v| v == &Any::from("paragraph") || v == &Any::from("heading"))
        {
            return value.clone();
        }
    }
    arr(vec![Any::from("paragraph")])
}
pub fn snapshot(d: &Doc) -> Result<Any> {
    let v = project(d)?;
    let t = d.transact();
    let mut visible = vec![];
    let mut deleted = vec![];
    for o in &v.owners {
        let mut runs = vec![];
        let spans = o
            .spans
            .iter()
            .map(|s| {
                let x = text(d, &s.source);
                let Any::Array(r) = slice_runs(&t, &x, s.start, s.end).unwrap() else {
                    unreachable!()
                };
                runs.extend(r.iter().cloned());
                arr(vec![
                    Any::from(s.source.clone()),
                    Any::from(s.start),
                    Any::from(s.end),
                ])
            })
            .collect();
        let node = obj(vec![
            ("id", Any::from(o.id.clone())),
            ("version", Any::from(o.version.clone())),
            ("spans", arr(spans)),
            ("runs", arr(runs)),
            ("tuple", o.attrs.clone()),
            ("presentation", presentation(&o.attrs)),
        ]);
        if o.deleted {
            deleted.push(node)
        } else {
            visible.push(node)
        }
    }
    let intentions = values(&t, 5)?
        .into_iter()
        .map(|(k, v)| {
            let Any::Array(a) = v else { unreachable!() };
            (
                k,
                arr(a
                    .iter()
                    .map(|v| match v {
                        Any::Buffer(b) => arr(b.iter().map(|n| Any::from(*n as u32)).collect()),
                        _ => v.clone(),
                    })
                    .collect()),
            )
        })
        .collect::<HashMap<_, _>>();
    Ok(obj(vec![
        ("intentions", Any::Map(intentions.into())),
        ("visible", arr(visible)),
        ("deleted", arr(deleted)),
        (
            "outcomes",
            Any::Map(
                v.outcomes
                    .into_iter()
                    .map(|(k, v)| (k, Any::from(v)))
                    .collect::<HashMap<_, _>>()
                    .into(),
            ),
        ),
        (
            "retained",
            Any::Map(v.retained.into_iter().collect::<HashMap<_, _>>().into()),
        ),
        (
            "sources",
            Any::Map(v.sources.into_iter().collect::<HashMap<_, _>>().into()),
        ),
    ]))
}
