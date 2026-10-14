//! Disposable ENG-1.20 whole-batch policy. Production commands are ENG-1.21/1.9.
use super::*;
#[derive(Clone)]
pub enum Command {
    Block {
        owner: String,
        tuple: Any,
    },
    Insert {
        owner: String,
        span: usize,
        offset: u32,
        value: String,
    },
    Mark {
        owner: String,
        span: usize,
        offset: u32,
        len: u32,
    },
    Delete {
        owner: String,
    },
    Separator {
        id: String,
        parent: String,
        before: Option<String>,
        place: String,
    },
}
#[derive(Clone)]
pub struct Neighbor {
    pub id: String,
    pub place: String,
}
pub struct Prepared {
    baseline: Vec<u8>,
    update: Vec<u8>,
}
fn target(d: &Doc, key: &str) -> Result<Owner> {
    project(d)?
        .owners
        .into_iter()
        .find(|o| o.id == key && !o.deleted)
        .filter(|o| {
            o.kind == "block"
                && !o.spans.is_empty()
                && matches!(
                    tuple_kind(&o.attrs),
                    "paragraph" | "heading" | "code" | "separator"
                )
        })
        .ok_or("unsupported-target")
}
fn register(d: &Doc, key: &str, parent: &str, tuple: Any) -> Result<()> {
    id(key, "n")?;
    if values(&d.transact(), 0)?.contains_key(key) {
        return Err("reused-id");
    }
    let mut t = d.transact_mut_with("local:policy-stage");
    t.get_map(ROOTS[0]).unwrap().insert(&mut t, key, "block");
    t.get_map(ROOTS[1])
        .unwrap()
        .insert(&mut t, key, XmlTextPrelim::new(""));
    t.get_map(ROOTS[2]).unwrap().insert(
        &mut t,
        key,
        arr(vec![Any::from(parent), Any::from(u32::MAX)]),
    );
    t.get_map(ROOTS[4]).unwrap().insert(&mut t, key, tuple);
    t.get_map(ROOTS[6]).unwrap().insert(&mut t, key, true);
    Ok(())
}
fn execute(d: &Doc, c: &Command) -> Result<String> {
    match c {
        Command::Block {
            owner,
            tuple: value,
        } => {
            tuple(value)?;
            if !matches!(
                tuple_kind(value),
                "paragraph" | "heading" | "code" | "separator"
            ) {
                return Err("unsupported-target");
            }
            let o = target(d, owner)?;
            if tuple_kind(value) == "separator" && o.spans.iter().any(|s| s.end != s.start) {
                return Err("nonempty-separator");
            }
            set(d, 4, owner, value.clone());
            Ok(owner.clone())
        }
        Command::Insert {
            owner,
            span,
            offset,
            value,
        } => {
            let o = target(d, owner)?;
            if tuple_kind(&o.attrs) == "separator" {
                return Err("unsupported-target");
            }
            let s = o.spans.get(*span).ok_or("invalid-cut")?;
            let x = text(d, &s.source);
            if *offset < s.start
                || *offset > s.end
                || !boundary(&plain_text(&d.transact(), &x)?, *offset)
            {
                return Err("invalid-cut");
            }
            if value.len() > 1048576 {
                return Err("resource-limit");
            }
            let mut t = d.transact_mut_with("local:policy-stage");
            if tuple_kind(&o.attrs) == "code" {
                x.insert_with_attributes(&mut t, *offset, value, HashMap::new());
            } else {
                x.insert(&mut t, *offset, value);
            }
            Ok(owner.clone())
        }
        Command::Mark {
            owner,
            span,
            offset,
            len,
        } => {
            let o = target(d, owner)?;
            if matches!(tuple_kind(&o.attrs), "code" | "separator") {
                return Err("unsupported-target");
            }
            let s = o.spans.get(*span).ok_or("invalid-cut")?;
            let end = offset.checked_add(*len).ok_or("invalid-cut")?;
            let x = text(d, &s.source);
            let plain = plain_text(&d.transact(), &x)?;
            if *offset < s.start
                || end > s.end
                || !boundary(&plain, *offset)
                || !boundary(&plain, end)
            {
                return Err("invalid-cut");
            }
            x.format(
                &mut d.transact_mut_with("local:policy-stage"),
                *offset,
                *len,
                HashMap::from([("italic".into(), Any::Bool(true))]),
            );
            Ok(owner.clone())
        }
        Command::Delete { owner } => {
            target(d, owner)?;
            set(d, 3, owner, Any::Bool(true));
            Ok(owner.clone())
        }
        Command::Separator {
            id,
            parent,
            before,
            place: op,
        } => {
            register(d, id, "@root", arr(vec![Any::from("separator")]))?;
            place(d, op, id, parent, before.as_deref())?;
            Ok(id.clone())
        }
    }
}
/// All writes, including neighbor identity/placement, are prepared away from live state.
pub fn prepare(d: &Doc, commands: &[Command], neighbor: Option<&Neighbor>) -> Result<Prepared> {
    if commands.is_empty() {
        return Err("empty-batch");
    }
    let baseline = bytes(d);
    let stage = fork(d, d.client_id().get());
    let mut last = String::new();
    for c in commands {
        last = execute(&stage, c)?;
        project(&stage)?;
    }
    let view = project(&stage)?;
    if !view.owners.iter().any(|o| {
        !o.deleted
            && o.kind == "block"
            && !o.spans.is_empty()
            && matches!(tuple_kind(&o.attrs), "paragraph" | "heading" | "code")
    }) {
        let n = neighbor.ok_or("missing-editable-neighbor")?;
        let at = view
            .owners
            .iter()
            .position(|o| o.id == last)
            .ok_or("unsupported-target")?;
        let parent = view.owners[at].parent.clone();
        let before = view
            .owners
            .iter()
            .skip(at + 1)
            .find(|o| !o.deleted && o.parent == parent)
            .map(|o| o.id.clone());
        register(&stage, &n.id, "@root", arr(vec![Any::from("paragraph")]))?;
        place(&stage, &n.place, &n.id, &parent, before.as_deref())?;
    }
    project(&stage)?;
    let update = bytes(&stage);
    // Exercise the exact envelope, immutable-row and bounded transport admission.
    let check = fork(d, 4000000000);
    admit(&check, &envelope(), &update)?;
    Ok(Prepared { baseline, update })
}
pub fn commit(d: &Doc, p: Prepared) -> Result<()> {
    if bytes(d) != p.baseline {
        return Err("stale-preparation");
    }
    let update = bounded_decoder::decode(&p.update)?;
    d.transact_mut_with("local:policy")
        .apply_update(update)
        .map_err(|_| "invalid-bytes")
}
pub fn batch(d: &Doc, commands: &[Command], neighbor: Option<&Neighbor>) -> Result<()> {
    commit(d, prepare(d, commands, neighbor)?)
}
/// Rendering view only: original attributes remain on the authoritative XmlText.
pub fn rendered_runs(d: &Doc, key: &str) -> Result<Any> {
    let o = target(d, key)?;
    let mut runs = vec![];
    let t = d.transact();
    for s in &o.spans {
        let Any::Array(r) = slice_runs(&t, &text(d, &s.source), s.start, s.end)? else {
            unreachable!()
        };
        for v in r.iter() {
            let Any::Map(m) = v else { unreachable!() };
            runs.push(if tuple_kind(&o.attrs) == "code" {
                obj(vec![
                    ("insert", m["insert"].clone()),
                    ("attributes", obj(vec![])),
                ])
            } else {
                v.clone()
            });
        }
    }
    Ok(arr(runs))
}
