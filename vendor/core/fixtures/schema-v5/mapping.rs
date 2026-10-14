//! Disposable ENG-1.22 witnessed mapper. Not production v5 support.
use std::collections::{BTreeMap, HashMap};
use yrs::updates::{decoder::Decode, encoder::Encode};
use yrs::{
    Any, Assoc, ClientID, Doc, IndexedSequence, Map, OffsetKind, Options, Out, ReadTxn,
    StateVector, StickyIndex, Text, Transact, XmlTextPrelim, XmlTextRef,
};
#[path = "bounded_decoder.rs"]
mod bounded_decoder;
pub const ROOTS: [&str; 10] = [
    "registry-parents",
    "registry-texts",
    "registry-placements",
    "delete-probe-tombstones",
    "registry-block-attributes",
    "registry-ownership-ops",
    "registry-owner-origins",
    "registry-container-attributes",
    "registry-list-item-attributes",
    "registry-entity-controls",
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
pub fn json(v: &Any) -> String {
    let mut s = String::new();
    v.to_json(&mut s);
    s
}
pub fn envelope() -> Any {
    let Any::Map(v) = Any::from_json(include_str!("seed.json")).unwrap() else {
        panic!()
    };
    v["envelope"].clone()
}
pub fn replica(client: u64) -> Doc {
    let d = Doc::with_options(Options {
        client_id: ClientID::new(client),
        offset_kind: OffsetKind::Utf16,
        ..Options::default()
    });
    for r in ROOTS {
        d.get_or_insert_map(r);
    }
    d
}
pub fn bytes(d: &Doc) -> Vec<u8> {
    d.transact()
        .encode_state_as_update_v1(&StateVector::default())
}
pub fn apply(d: &Doc, b: &[u8]) -> Result<()> {
    d.transact_mut_with("remote")
        .apply_update(bounded_decoder::decode(b)?)
        .map_err(|_| "invalid-bytes")
}
pub fn fork(d: &Doc, client: u64) -> Doc {
    let r = replica(client);
    apply(&r, &bytes(d)).unwrap();
    r
}
pub fn text(d: &Doc, id: &str) -> XmlTextRef {
    let t = d.transact();
    let Some(Out::YXmlText(x)) = t.get_map(ROOTS[1]).unwrap().get(&t, id) else {
        panic!("text")
    };
    x
}
pub fn set(d: &Doc, root: usize, id: &str, v: Any) {
    let mut t = d.transact_mut_with("local:fixture");
    t.get_map(ROOTS[root]).unwrap().insert(&mut t, id, v);
}
pub fn seed() -> Doc {
    let d = replica(11);
    let Any::Map(s) = Any::from_json(include_str!("seed.json")).unwrap() else {
        panic!()
    };
    let Any::Array(nodes) = &s["nodes"] else {
        panic!()
    };
    let mut t = d.transact_mut_with("initialize");
    for n in nodes.iter() {
        let Any::Map(n) = n else { panic!() };
        let Any::String(id) = &n["id"] else { panic!() };
        let Any::String(value) = &n["text"] else {
            panic!()
        };
        t.get_map(ROOTS[0])
            .unwrap()
            .insert(&mut t, id.as_ref(), "block");
        let x = t.get_map(ROOTS[1]).unwrap().insert(
            &mut t,
            id.as_ref(),
            XmlTextPrelim::new(value.as_ref()),
        );
        if !value.is_empty() {
            x.format(
                &mut t,
                0,
                value.encode_utf16().count() as u32,
                HashMap::from([
                    ("bold".into(), Any::Bool(true)),
                    (
                        "ext:future".into(),
                        obj(vec![("retained", Any::Bool(true))]),
                    ),
                ]),
            );
        }
        t.get_map(ROOTS[2]).unwrap().insert(
            &mut t,
            id.as_ref(),
            arr(vec![n["parent"].clone(), n["order"].clone()]),
        );
        t.get_map(ROOTS[4])
            .unwrap()
            .insert(&mut t, id.as_ref(), n["tuple"].clone());
        t.get_map(ROOTS[6])
            .unwrap()
            .insert(&mut t, id.as_ref(), true);
    }
    for n in nodes.iter() {
        let Any::Map(n) = n else { panic!() };
        if let Some(check) = n.get("check") {
            let Any::String(key) = &n["id"] else { panic!() };
            t.get_map(ROOTS[8]).unwrap().insert(
                &mut t,
                key.as_ref(),
                arr(vec![Any::from("check"), check.clone()]),
            );
        }
    }
    let Any::Array(containers) = &s["containers"] else {
        panic!()
    };
    for c in containers.iter() {
        let Any::Map(c) = c else { panic!() };
        let Any::String(key) = &c["id"] else { panic!() };
        t.get_map(ROOTS[0])
            .unwrap()
            .insert(&mut t, key.as_ref(), c["kind"].clone());
        t.get_map(ROOTS[2]).unwrap().insert(
            &mut t,
            key.as_ref(),
            arr(vec![c["parent"].clone(), c["order"].clone()]),
        );
        t.get_map(ROOTS[7])
            .unwrap()
            .insert(&mut t, key.as_ref(), c["tuple"].clone());
    }
    drop(t);
    d
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
        "code"
            if a.len() == 2
                && matches!(&a[1], Any::String(v) if v.len() <= 64 && v.bytes().all(|c| c.is_ascii_alphanumeric() || b"+._-".contains(&c))) =>
        {
            Ok(())
        }
        "separator" if a.len() == 1 => Ok(()),
        "paragraph" | "heading" | "code" | "separator" | "unordered" | "ordered" | "quote"
        | "checklist" | "check" | "enabled" => Err("invalid-tuple"),
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
    pub parent: String,
    pub kind: String,
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
}
#[derive(Clone)]
struct Op {
    id: String,
    left: String,
    lv: String,
    right: Option<(String, String)>,
    cut: Option<(usize, String, Vec<u8>)>,
    place: Option<(String, String, Option<(String, String)>)>,
    witnesses: Vec<Any>,
}
impl Op {
    fn deps(&self) -> Vec<String> {
        let mut v = vec![self.lv.clone()];
        if let Some((_, r)) = &self.right {
            v.push(r.clone())
        }
        if let Some((_, pv, before)) = &self.place {
            v.push(pv.clone());
            if let Some((_, bv)) = before {
                v.push(bv.clone());
            }
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
        place: None,
        witnesses: vec![],
    };
    match a.as_ref() {
        [kind, l, lv, index, source, Any::Buffer(cut), witness] if string(kind)? == "split" => {
            witness_tuple(witness)?;
            op.witnesses.push(witness.clone());
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
        [kind, l, lv, r, rv, a, b] if string(kind)? == "join" => {
            witness_pair(a, b)?;
            op.witnesses.extend([a.clone(), b.clone()]);
            op.left = string(l)?;
            op.lv = string(lv)?;
            op.right = Some((string(r)?, string(rv)?));
        }
        [kind, target, tv, parent, pv, before, bv] if string(kind)? == "place" => {
            op.left = string(target)?;
            op.lv = string(tv)?;
            entity_id(&op.left)?;
            let parent = string(parent)?;
            let pv = string(pv)?;
            if parent == "@root" {
                if pv != "base" {
                    return Err("invalid-operation");
                }
            } else {
                entity_id(&parent)?;
            }
            let before = match (before, bv) {
                (Any::Null, Any::Null) => None,
                (Any::String(b), Any::String(v)) => {
                    entity_id(b)?;
                    Some((b.to_string(), v.to_string()))
                }
                _ => return Err("invalid-operation"),
            };
            op.place = Some((parent, pv, before));
        }
        _ => return Err("invalid-operation"),
    };
    if op.place.is_none() {
        owner_id(&op.left)?;
    }
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
    let container_attrs = values(&t, 7)?;
    let checks = values(&t, 8)?;
    let controls = values(&t, 9)?;
    if kinds.len() > 4096 || records.len() > 4096 || controls.len() > 8192 {
        return Err("resource-limit");
    }
    if kinds.keys().ne(placements.keys()) {
        return Err("invalid-source");
    }
    for (key, value) in &controls {
        entity_id(key)?;
        bool_tuple(value, "enabled")?;
    }
    let originals: Vec<_> = kinds.keys().filter(|key| key.starts_with("n:")).collect();
    if originals.iter().copied().ne(origins.keys()) {
        return Err("invalid-source");
    }
    let texts = t.get_map(ROOTS[1]).ok_or("invalid-root")?;
    if texts.len(&t) as usize != originals.len() {
        return Err("invalid-source");
    }
    let mut source_texts = BTreeMap::new();
    let mut sources = BTreeMap::new();
    let mut owners = vec![];
    let mut total = 0;
    for (key, value) in &kinds {
        let kind = string(value)?;
        let is_source = key.starts_with("n:");
        entity_id(key)?;
        if key.starts_with("o:")
            || (is_source && matches!(kind.as_str(), "list" | "quote"))
            || (!is_source && kind == "block")
        {
            return Err("invalid-source");
        }
        let Any::Array(p) = &placements[key] else {
            return Err("invalid-placement");
        };
        if p.len() != 2 {
            return Err("invalid-placement");
        }
        let parent = string(&p[0])?;
        if parent != "@root" {
            entity_id(&parent)?;
        }
        let order = integer(&p[1])?;
        let mut spans = vec![];
        if is_source {
            if origins[key] != Any::Bool(true) {
                return Err("invalid-source");
            }
            let Some(Out::YXmlText(x)) = texts.get(&t, key) else {
                return Err("invalid-source");
            };
            let plain = plain_text(&t, &x)?;
            total += plain.len();
            if total > 1048576 || x.len(&t) > 262144 {
                return Err("resource-limit");
            }
            let identity = match AsRef::<yrs::branch::Branch>::as_ref(&x).id() {
                yrs::branch::BranchID::Nested(i) => format!("{}:{}", i.client, i.clock),
                _ => return Err("invalid-source"),
            };
            sources.insert(
                key.clone(),
                obj(vec![
                    ("identity", Any::from(identity)),
                    ("runs", slice_runs(&t, &x, 0, x.len(&t))?),
                ]),
            );
            spans.push(Span {
                source: key.clone(),
                start: 0,
                end: x.len(&t),
            });
            source_texts.insert(key.clone(), (x, plain));
        } else {
            let value = container_attrs.get(key).ok_or("invalid-container")?;
            container_tuple(value)?;
            if kind == "list" && tuple_kind(value) == "quote"
                || kind == "quote"
                    && matches!(tuple_kind(value), "ordered" | "unordered" | "checklist")
            {
                return Err("invalid-container");
            }
        }
        owners.push((
            order,
            canonical(key)?,
            Owner {
                id: key.clone(),
                version: "base".into(),
                parent,
                kind,
                spans,
                deleted: tombstones.contains_key(key) || !enabled(&controls, key),
                attrs: Any::Null,
            },
        ));
    }
    for key in container_attrs.keys() {
        if !kinds.contains_key(key) || !key.starts_with("c:") {
            return Err("invalid-container");
        }
    }
    owners.sort_by_key(|(order, key, _)| (*order, *key));
    let mut owners: Vec<_> = owners.into_iter().map(|(_, _, o)| o).collect();
    let mut visits = 0usize;
    normalize(&mut owners, true, &mut visits)?;
    let ops: BTreeMap<_, _> = records
        .iter()
        .map(|(k, v)| Ok((k.clone(), operation(k, v)?)))
        .collect::<Result<_>>()?;
    for (k, v) in &tombstones {
        entity_id(k)?;
        if v != &Any::Bool(true)
            || (!kinds.contains_key(k) && !ops.get(k).is_some_and(|op| op.cut.is_some()))
        {
            return Err("invalid-tombstone");
        }
    }
    for (k, v) in &attrs {
        owner_id(k)?;
        if !origins.contains_key(k) && !ops.get(k).is_some_and(|op| op.cut.is_some()) {
            return Err("invalid-target");
        };
        tuple(v)?;
    }
    for (key, value) in &checks {
        owner_id(key)?;
        if !origins.contains_key(key) && !ops.get(key).is_some_and(|op| op.cut.is_some()) {
            return Err("invalid-target");
        }
        bool_tuple(value, "check")?;
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
            visits += deps.len();
            if visits > 16777216 {
                return Err("resource-limit");
            }
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
        if !enabled(&controls, &op.id) {
            outcomes.insert(op.id.clone(), "disabled".into());
            continue;
        }
        let mut status = "losing";
        if op
            .deps()
            .iter()
            .all(|d| outcomes.get(d).is_some_and(|v| v == "applied"))
        {
            if let Some(i) = owners
                .iter()
                .position(|o| o.id == op.left && o.version == op.lv && !o.deleted)
            {
                if let Some((parent, pv, before)) = &op.place {
                    let parent_ok = parent == "@root"
                        || owners.iter().any(|o| {
                            o.id == *parent
                                && o.version == *pv
                                && !o.deleted
                                && supported_container(o, &container_attrs)
                        });
                    let before_index = before.as_ref().and_then(|(b, bv)| {
                        owners.iter().position(|o| {
                            o.id == *b
                                && o.version == *bv
                                && !o.deleted
                                && o.parent == *parent
                                && o.id != op.left
                        })
                    });
                    if parent_ok
                        && (before.is_none() || before_index.is_some())
                        && (supported_container(&owners[i], &container_attrs)
                            || (owners[i].kind == "block" && !owners[i].spans.is_empty()))
                        && !descendant(&owners, parent, &op.left)
                    {
                        let mut moved = owners.remove(i);
                        moved.parent = parent.clone();
                        moved.version = op.id.clone();
                        let at = before
                            .as_ref()
                            .and_then(|(b, _)| owners.iter().position(|o| o.id == *b))
                            .unwrap_or(owners.len());
                        owners.insert(at, moved);
                        normalize(&mut owners, false, &mut visits)?;
                        status = "applied";
                    }
                } else if let Some((right, rv)) = &op.right {
                    // Hidden owners retain recovery spans but cannot obstruct
                    // adjacency between the visible command targets.
                    if let Some(j) = (i + 1..owners.len()).find(|j| {
                        !owners[*j].deleted
                            && owners[*j].parent == owners[i].parent
                            && !owners[*j].spans.is_empty()
                    }) {
                        if owners[j].id == *right
                            && owners[j].version == *rv
                            && owners[i].kind == "block"
                            && owners[j].kind == "block"
                            && !owners[i].spans.is_empty()
                        {
                            let r = owners.remove(j);
                            owners[i].spans.extend(r.spans);
                            owners[i].version = op.id.clone();
                            status = "applied";
                        }
                    }
                } else if let Some((index, source, cut)) = &op.cut {
                    if owners[i].kind != "block" || owners[i].spans.is_empty() {
                        outcomes.insert(op.id.clone(), "losing".into());
                        continue;
                    }
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
                                parent: owners[i].parent.clone(),
                                kind: "block".into(),
                                spans: right,
                                deleted: tombstones.contains_key(&op.id),
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
        o.deleted = tombstones.contains_key(&o.id)
            || (o.id.starts_with("n:") || o.id.starts_with("c:")) && !enabled(&controls, &o.id);
        if o.id.starts_with("c:") {
            o.attrs = container_attrs[&o.id].clone();
            continue;
        }
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
    normalize(&mut owners, false, &mut visits)?;
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
    })
}
fn presentation(value: &Any) -> Any {
    if let Any::Array(v) = value {
        if v.first()
            .is_some_and(|v| matches!(v, Any::String(s) if matches!(s.as_ref(), "paragraph" | "heading" | "code" | "separator")))
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
            ("parent", Any::from(o.parent.clone())),
            ("kind", Any::from(o.kind.clone())),
            ("spans", arr(spans)),
            ("runs", arr(runs)),
            ("tuple", o.attrs.clone()),
            ("presentation", presentation(&o.attrs)),
            (
                "check",
                if v.owners
                    .iter()
                    .any(|p| p.id == o.parent && tuple_kind(&p.attrs) == "checklist")
                    && is_numbered(o)
                {
                    Any::Bool(values(&t, 8)?.get(&o.id).is_some_and(
                        |v| matches!(v, Any::Array(a) if a.get(1) == Some(&Any::Bool(true))),
                    ))
                } else {
                    Any::Null
                },
            ),
            (
                "number",
                if let Some(p) = v
                    .owners
                    .iter()
                    .find(|p| p.id == o.parent && tuple_kind(&p.attrs) == "ordered")
                {
                    let Any::Array(a) = &p.attrs else {
                        unreachable!()
                    };
                    if is_numbered(o) && !o.deleted {
                        Any::from(
                            integer(&a[1])?
                                + v.owners
                                    .iter()
                                    .take_while(|p| p.id != o.id)
                                    .filter(|p| {
                                        p.parent == o.parent && !p.deleted && is_numbered(p)
                                    })
                                    .count() as u32,
                        )
                    } else {
                        Any::Null
                    }
                } else {
                    Any::Null
                },
            ),
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
        (
            "containers",
            Any::Map(values(&t, 7)?.into_iter().collect::<HashMap<_, _>>().into()),
        ),
        (
            "checks",
            Any::Map(values(&t, 8)?.into_iter().collect::<HashMap<_, _>>().into()),
        ),
        (
            "controls",
            Any::Map(values(&t, 9)?.into_iter().collect::<HashMap<_, _>>().into()),
        ),
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
pub fn split(d: &Doc, op: &str, left: &str, index: usize, offset: u32) -> Result<()> {
    id(op, "o")?;
    let view = project(d)?;
    if view.outcomes.contains_key(op) {
        return Err("reused-id");
    };
    let owner = view
        .owners
        .iter()
        .find(|o| o.id == left && !o.deleted)
        .ok_or("deleted-target")?;
    if tuple_kind(&owner.attrs) == "separator" {
        return Err("unsupported-target");
    }
    witness_tuple(&owner.attrs)?;
    let span = owner.spans.get(index).ok_or("invalid-cut")?;
    let x = text(d, &span.source);
    if offset < span.start
        || offset > span.end
        || !boundary(&plain_text(&d.transact(), &x)?, offset)
    {
        return Err("invalid-cut");
    };
    let cut = x
        .sticky_index(&d.transact(), offset, Assoc::Before)
        .ok_or("invalid-cut")?;
    let record = arr(vec![
        Any::from("split"),
        Any::from(left),
        Any::from(owner.version.clone()),
        Any::from(index as u32),
        Any::from(span.source.clone()),
        Any::Buffer(cut.encode_v1().into()),
        owner.attrs.clone(),
    ]);
    commit_record(d, op, record)?;
    Ok(())
}
pub fn join(d: &Doc, op: &str, left: &str, right: &str) -> Result<()> {
    id(op, "o")?;
    let v = project(d)?;
    if v.outcomes.contains_key(op) {
        return Err("reused-id");
    };
    let i = v
        .owners
        .iter()
        .position(|o| o.id == left && !o.deleted)
        .ok_or("deleted-target")?;
    let a = &v.owners[i];
    if tuple_kind(&a.attrs) == "separator"
        || v.owners
            .iter()
            .any(|o| o.id == right && !o.deleted && tuple_kind(&o.attrs) == "separator")
    {
        return Err("unsupported-target");
    }
    let b = v
        .owners
        .iter()
        .skip(i + 1)
        .find(|o| !o.deleted && o.parent == a.parent && !o.spans.is_empty())
        .filter(|o| o.id == right)
        .ok_or("nonadjacent-target")?;
    if tuple_kind(&a.attrs) == "separator" || tuple_kind(&b.attrs) == "separator" {
        return Err("unsupported-target");
    }
    witness_pair(&a.attrs, &b.attrs)?;
    commit_record(
        d,
        op,
        arr(vec![
            Any::from("join"),
            Any::from(left),
            Any::from(a.version.clone()),
            Any::from(right),
            Any::from(b.version.clone()),
            a.attrs.clone(),
            b.attrs.clone(),
        ]),
    )?;
    Ok(())
}
pub fn resolve(d: &Doc, source: &str, encoded: &[u8]) -> Result<(String, u32, bool)> {
    let v = project(d)?;
    let t = d.transact();
    let anchor = StickyIndex::decode_v1(encoded).map_err(|_| "unresolvable")?;
    let pos = anchor.get_offset(&t).ok_or("unresolvable")?;
    let Some(Out::YXmlText(x)) = t.get_map(ROOTS[1]).and_then(|m| m.get(&t, source)) else {
        return Err("unresolvable");
    };
    if pos.branch.id() != AsRef::<yrs::branch::Branch>::as_ref(&x).id() {
        return Err("unresolvable");
    };
    let mut interior = vec![];
    let mut edge = vec![];
    for o in &v.owners {
        let mut owner_offset = 0;
        for s in &o.spans {
            if s.source == source {
                if pos.index > s.start && pos.index < s.end {
                    interior.push((o.id.clone(), owner_offset + pos.index - s.start, o.deleted))
                } else if (anchor.assoc == Assoc::Before && pos.index == s.end)
                    || (anchor.assoc == Assoc::After && pos.index == s.start)
                {
                    edge.push((o.id.clone(), owner_offset + pos.index - s.start, o.deleted))
                }
            }
            owner_offset += s.end - s.start;
        }
    }
    if let Some(v) = interior.first() {
        return Ok(v.clone());
    }
    if anchor.assoc == Assoc::Before {
        edge.first()
    } else {
        edge.last()
    }
    .cloned()
    .ok_or("unresolvable")
}
pub fn admit(d: &Doc, offered: &Any, b: &[u8]) -> Result<()> {
    let expected = envelope();
    let Any::Map(e) = &expected else {
        unreachable!()
    };
    let Any::Map(o) = offered else {
        return Err("invalid-metadata");
    };
    if o.len() != e.len() || e.keys().any(|k| !o.contains_key(k)) {
        return Err("invalid-metadata");
    };
    for (k, v) in e.iter() {
        if o[k] != *v {
            return Err(match k.as_str() {
                "epoch" => "wrong-epoch",
                "documentId" => "wrong-document",
                _ => "unsupported-profile",
            });
        }
    }
    if b.len() > 16777216 {
        return Err("resource-limit");
    };
    let stage = fork(d, 4000000000);
    apply(&stage, b)?;
    project(&stage)?;
    let before = d.transact();
    let after = stage.transact();
    for root in [0, 2, 3, 5, 6] {
        for (k, v) in values(&before, root)? {
            if values(&after, root)?.get(&k) != Some(&v) {
                return Err("immutable-replacement");
            }
        }
    }
    let old = project(d)?;
    let new = project(&stage)?;
    for (key, value) in old.sources {
        let Any::Map(a) = value else { unreachable!() };
        let Some(Any::Map(b)) = new.sources.get(&key) else {
            return Err("immutable-replacement");
        };
        if a["identity"] != b["identity"] {
            return Err("immutable-replacement");
        }
    }
    drop(after);
    drop(before);
    apply(d, b)
}
/// External initialized state is required; restoring never initializes source data.
pub fn restore_checkpoint(
    offered: &Any,
    initialized: bool,
    checkpoint: &[u8],
    client: u64,
) -> Result<Doc> {
    if !initialized {
        return Err("missing-initializer");
    };
    if client == 0 || client > u32::MAX as u64 {
        return Err("invalid-writer");
    };
    let d = replica(client);
    admit(&d, offered, checkpoint)?;
    Ok(d)
}

fn entity_id(key: &str) -> Result<()> {
    canonical(key).map(|_| ())
}
fn canonical(key: &str) -> Result<(u8, u32, u32)> {
    let namespace = key.split(':').next().ok_or("invalid-id")?;
    if !matches!(namespace, "n" | "c" | "o") {
        return Err("invalid-id");
    }
    let (writer, counter) = id(key, namespace)?;
    Ok((namespace.as_bytes()[0], writer, counter))
}
fn tuple_kind(value: &Any) -> &str {
    if let Any::Array(a) = value {
        if let Some(Any::String(s)) = a.first() {
            return s;
        }
    }
    ""
}
fn bool_tuple(value: &Any, kind: &str) -> Result<()> {
    if matches!(value, Any::Array(a) if a.len() == 2 && a[0] == Any::from(kind) && matches!(a[1], Any::Bool(_)))
    {
        Ok(())
    } else {
        Err("invalid-tuple")
    }
}
fn container_tuple(value: &Any) -> Result<()> {
    let Any::Array(a) = value else {
        return Err("invalid-tuple");
    };
    if !valid_json(value, 0) || !matches!(a.first(), Some(Any::String(_))) {
        return Err("invalid-tuple");
    }
    match tuple_kind(value) {
        "unordered" | "checklist" | "quote" if a.len() == 1 => Ok(()),
        "ordered"
            if a.len() == 2 && integer(&a[1]).is_ok_and(|n| (1..=2147483647).contains(&n)) =>
        {
            Ok(())
        }
        "unordered" | "checklist" | "quote" | "ordered" | "paragraph" | "heading" | "code"
        | "separator" | "check" | "enabled" => Err("invalid-tuple"),
        _ => Ok(()),
    }
}
fn enabled(controls: &BTreeMap<String, Any>, key: &str) -> bool {
    !controls
        .get(key)
        .is_some_and(|v| matches!(v, Any::Array(a) if a[1] == Any::Bool(false)))
}
fn container(o: &Owner) -> bool {
    matches!(o.kind.as_str(), "list" | "quote") && o.id.starts_with("c:")
}
fn descendant(owners: &[Owner], start: &str, target: &str) -> bool {
    let mut cursor = start;
    for _ in 0..=owners.len() {
        if cursor == target {
            return true;
        }
        if cursor == "@root" {
            return false;
        }
        let Some(o) = owners.iter().find(|o| o.id == cursor) else {
            return false;
        };
        cursor = &o.parent;
    }
    true
}
/// Normalize only the derived tree; no registry repairs. Initial cycles detach
/// the smallest canonical ID. Existing order is the ordered sibling sequence.
fn normalize(owners: &mut Vec<Owner>, repair_cycles: bool, visits: &mut usize) -> Result<()> {
    let valid: std::collections::BTreeSet<_> = owners
        .iter()
        .filter(|o| !o.deleted && container(o))
        .map(|o| o.id.clone())
        .collect();
    for o in owners.iter_mut() {
        if o.parent != "@root" && !valid.contains(&o.parent) {
            o.parent = "@root".into();
        }
    }
    let mut parents: BTreeMap<_, _> = owners
        .iter()
        .map(|o| (o.id.clone(), o.parent.clone()))
        .collect();
    for start in parents.keys().cloned().collect::<Vec<_>>() {
        let mut path: Vec<String> = vec![];
        let mut cursor = start;
        while cursor != "@root" {
            *visits += 1;
            if *visits > 16777216 {
                return Err("resource-limit");
            }
            if let Some(at) = path.iter().position(|id| id == &cursor) {
                if !repair_cycles {
                    return Err("cycle");
                }
                let detached = path[at..]
                    .iter()
                    .min_by_key(|id| canonical(id).unwrap())
                    .unwrap()
                    .clone();
                parents.insert(detached.clone(), "@root".into());
                owners.iter_mut().find(|o| o.id == detached).unwrap().parent = "@root".into();
                break;
            }
            path.push(cursor.clone());
            if path.len() > 4096 {
                return Err("resource-limit");
            }
            cursor = parents[&cursor].clone();
        }
    }
    // Root children have depth zero, with at most 128 parent edges.
    for start in parents.keys() {
        let mut cursor = parents[start].as_str();
        let mut depth = 0;
        while cursor != "@root" {
            *visits += 1;
            if *visits > 16777216 {
                return Err("resource-limit");
            }
            depth += 1;
            if depth > 128 {
                return Err("resource-limit");
            }
            cursor = &parents[cursor];
        }
    }
    let mut children: BTreeMap<String, Vec<Owner>> = BTreeMap::new();
    for o in owners.drain(..) {
        children.entry(o.parent.clone()).or_default().push(o);
    }
    let mut stack: Vec<_> = children
        .remove("@root")
        .unwrap_or_default()
        .into_iter()
        .rev()
        .collect();
    while let Some(o) = stack.pop() {
        let id = o.id.clone();
        owners.push(o);
        stack.extend(children.remove(&id).unwrap_or_default().into_iter().rev());
    }
    if !children.is_empty() {
        return Err("cycle");
    }
    Ok(())
}
/// A witness declares authored preparation, independent of current mutable tuples.
pub fn witness_tuple(v: &Any) -> Result<()> {
    tuple(v)?;
    if !matches!(tuple_kind(v), "paragraph" | "heading" | "code") {
        return Err("invalid-witness");
    }
    Ok(())
}
pub fn witness_pair(a: &Any, b: &Any) -> Result<()> {
    witness_tuple(a)?;
    witness_tuple(b)?;
    if (tuple_kind(a) == "code" || tuple_kind(b) == "code") && a != b {
        return Err("incompatible-witness");
    }
    Ok(())
}
fn commit_record(d: &Doc, op: &str, record: Any) -> Result<()> {
    let stage = fork(d, 4000000001);
    set(&stage, 5, op, record.clone());
    if project(&stage)?.outcomes.get(op).map(String::as_str) != Some("applied") {
        return Err("ineligible-operation");
    }
    set(d, 5, op, record);
    Ok(())
}
pub fn place(d: &Doc, op: &str, target: &str, parent: &str, before: Option<&str>) -> Result<()> {
    id(op, "o")?;
    let view = project(d)?;
    if view.outcomes.contains_key(op) {
        return Err("reused-id");
    }
    let a = view
        .owners
        .iter()
        .find(|o| o.id == target && !o.deleted)
        .ok_or("deleted-target")?;
    if !(container(a)
        && matches!(
            tuple_kind(&a.attrs),
            "ordered" | "unordered" | "checklist" | "quote"
        )
        || a.kind == "block"
            && !a.spans.is_empty()
            && matches!(
                tuple_kind(&a.attrs),
                "paragraph" | "heading" | "code" | "separator"
            ))
    {
        return Err("unsupported-target");
    }
    let pv = if parent == "@root" {
        "base"
    } else {
        &view
            .owners
            .iter()
            .find(|o| {
                o.id == parent
                    && !o.deleted
                    && container(o)
                    && matches!(
                        tuple_kind(&o.attrs),
                        "ordered" | "unordered" | "checklist" | "quote"
                    )
            })
            .ok_or("illegal-parent")?
            .version
    };
    if descendant(&view.owners, parent, target) {
        return Err("cycle");
    }
    let (b, bv) = if let Some(before) = before {
        let b = view
            .owners
            .iter()
            .find(|o| o.id == before && !o.deleted && o.parent == parent && o.id != target)
            .ok_or("invalid-before")?;
        (Any::from(b.id.clone()), Any::from(b.version.clone()))
    } else {
        (Any::Null, Any::Null)
    };
    commit_record(
        d,
        op,
        arr(vec![
            Any::from("place"),
            Any::from(target),
            Any::from(a.version.clone()),
            Any::from(parent),
            Any::from(pv),
            b,
            bv,
        ]),
    )
}

fn supported_container(o: &Owner, attrs: &BTreeMap<String, Any>) -> bool {
    container(o)
        && attrs.get(&o.id).is_some_and(|a| match o.kind.as_str() {
            "list" => matches!(tuple_kind(a), "ordered" | "unordered" | "checklist"),
            "quote" => tuple_kind(a) == "quote",
            _ => false,
        })
}
fn is_numbered(o: &Owner) -> bool {
    o.kind == "block"
        && !o.spans.is_empty()
        && matches!(tuple_kind(&o.attrs), "paragraph" | "heading" | "code")
}

#[path = "policy.rs"]
pub mod policy;
