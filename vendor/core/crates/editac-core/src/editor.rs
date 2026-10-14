//! Profile dispatch to the exact qualified replay, without migration or repair.
use crate::{editor_mapping as v4, ownership as v3, stable_editor_mapping as v5};
use yrs::{Any, Doc, ReadTxn, Transact};

pub(crate) fn is_editor(d: &Doc) -> bool {
    d.transact().get_map(v4::ROOTS[9]).is_some()
}
pub(crate) fn project(d: &Doc, stable: bool) -> v3::Result<v3::View> {
    if !is_editor(d) {
        return v3::project(d);
    }
    let view = view(d, stable)?;
    let inherited = view
        .retained
        .iter()
        .filter_map(|(id, value)| {
            let Any::Array(a) = value else { return None };
            if a.first() == Some(&Any::from("split"))
                && view.outcomes.get(id).is_some_and(|v| v == "applied")
            {
                if let Some(Any::String(left)) = a.get(1) {
                    return Some((id.clone(), left.to_string()));
                }
            }
            None
        })
        .collect();
    Ok(v3::View {
        owners: view
            .owners
            .into_iter()
            .map(|o| v3::Owner {
                id: o.id,
                version: o.version,
                spans: o
                    .spans
                    .into_iter()
                    .map(|s| v3::Span {
                        source: s.source,
                        start: s.start,
                        end: s.end,
                    })
                    .collect(),
                deleted: o.deleted,
                attrs: o.attrs,
            })
            .collect(),
        outcomes: view.outcomes,
        retained: view.retained,
        sources: view.sources,
        inherited,
    })
}
pub(crate) fn snapshot(d: &Doc, stable: bool) -> v3::Result<Any> {
    if is_editor(d) {
        if stable {
            v5::snapshot(d)
        } else {
            v4::snapshot(d)
        }
    } else {
        v3::snapshot(d)
    }
}

/// Dispatch only from the caller's admitted envelope. Roots do not encode v4/v5.
pub(crate) fn view(d: &Doc, stable: bool) -> v4::Result<v4::View> {
    if !stable {
        return v4::project(d);
    }
    let v = v5::project(d)?;
    Ok(v4::View {
        owners: v
            .owners
            .into_iter()
            .map(|o| v4::Owner {
                id: o.id,
                version: o.version,
                parent: o.parent,
                kind: o.kind,
                spans: o
                    .spans
                    .into_iter()
                    .map(|s| v4::Span {
                        source: s.source,
                        start: s.start,
                        end: s.end,
                    })
                    .collect(),
                deleted: o.deleted,
                attrs: o.attrs,
            })
            .collect(),
        outcomes: v.outcomes,
        retained: v.retained,
        sources: v.sources,
    })
}
pub(crate) fn split(
    d: &Doc,
    stable: bool,
    op: &str,
    left: &str,
    index: usize,
    offset: u32,
) -> v4::Result<()> {
    if stable {
        v5::split(d, op, left, index, offset)
    } else {
        v4::split(d, op, left, index, offset)
    }
}
pub(crate) fn join(d: &Doc, stable: bool, op: &str, left: &str, right: &str) -> v4::Result<()> {
    if stable {
        v5::join(d, op, left, right)
    } else {
        v4::join(d, op, left, right)
    }
}
pub(crate) fn place(
    d: &Doc,
    stable: bool,
    op: &str,
    target: &str,
    parent: &str,
    before: Option<&str>,
) -> v4::Result<()> {
    if stable {
        v5::place(d, op, target, parent, before)
    } else {
        v4::place(d, op, target, parent, before)
    }
}
