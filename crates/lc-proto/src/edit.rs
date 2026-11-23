//! Edits to the show document, for several Producers working at once.
//!
//! A Producer does not send the whole show after each change (which would
//! erase whatever another Producer changed meanwhile). It sends the change
//! as a list of [`EditOp`]s. The Director applies them, in the order they
//! arrive, to the show it holds, and distributes the result to everyone.
//!
//! Objects in lists — tracks, cues, displays, virtual displays, tasks, control cues, assets —
//! are addressed by their `id`, never by position, so two people inserting,
//! deleting or reordering at the same time cannot make each other's edits
//! land on the wrong object. Conflicts resolve simply:
//!
//! * two people set the same field: the edit that reaches the Director last
//!   wins;
//! * someone edits (or moves, or deletes) an object another person has just
//!   deleted: that part of the edit is skipped ([`Applied::skipped`]).
//!
//! The Producer applies the same operations to its own copy (in JavaScript,
//! `app.js`), so both implementations follow the rules below exactly.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Most operations one edit may carry, and the deepest path: generous for a
/// show, but bounded (they come from the network).
pub const MAX_OPS: usize = 2_000;
pub const MAX_DEPTH: usize = 16;
/// Deepest a show document may be nested. A show needs about 8 levels; JSON
/// parsers stop at 128, and a document deeper than that could be stored but
/// never read back (state file, Runners).
pub const MAX_DOCUMENT_DEPTH: usize = 64;

/// How deeply `v` is nested (a scalar is 0).
pub fn depth(v: &Value) -> usize {
    match v {
        Value::Object(map) => 1 + map.values().map(depth).max().unwrap_or(0),
        Value::Array(list) => 1 + list.iter().map(depth).max().unwrap_or(0),
        _ => 0,
    }
}

/// One step of a path into the document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Step {
    /// A field of an object.
    Key(String),
    /// The element of a list whose `id` is this.
    Id { id: String },
}

/// One change to the document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum EditOp {
    /// Set the value at `path` (a field, or a whole list element). Missing
    /// fields on the way are created.
    Set { path: Vec<Step>, value: Value },
    /// Remove a field.
    Unset { path: Vec<Step> },
    /// Insert `value` (an object with an `id`) into the list at `path`, after
    /// the element with id `after` (`None`: at the start; an `after` that no
    /// longer exists: at the end).
    Insert {
        path: Vec<Step>,
        #[serde(default)]
        after: Option<String>,
        value: Value,
    },
    /// Remove the list element at `path` (whose last step is an id).
    Remove { path: Vec<Step> },
    /// Move the list element at `path` after the element with id `after`.
    Move {
        path: Vec<Step>,
        #[serde(default)]
        after: Option<String>,
    },
}

/// What happened when applying an edit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Applied {
    pub applied: usize,
    /// Operations whose target no longer exists (deleted by someone else).
    pub skipped: usize,
}

/// Apply `ops` to `doc`, in order. Operations whose target has gone are
/// skipped; a malformed edit (too many operations, too deep, an insert
/// without id) is refused as a whole, leaving `doc` unchanged.
pub fn apply(doc: &mut Value, ops: &[EditOp]) -> Result<Applied, String> {
    if ops.len() > MAX_OPS {
        return Err(format!("edit_show: more than {MAX_OPS} operations"));
    }
    for op in ops {
        let path = match op {
            EditOp::Set { path, .. }
            | EditOp::Unset { path }
            | EditOp::Insert { path, .. }
            | EditOp::Remove { path }
            | EditOp::Move { path, .. } => path,
        };
        if path.len() > MAX_DEPTH {
            return Err(format!("edit_show: a path deeper than {MAX_DEPTH}"));
        }
        if let EditOp::Insert { value, .. } = op {
            if id_of(value).is_none() {
                return Err("edit_show: an inserted list element needs an id".into());
            }
        }
    }
    let mut result = Applied::default();
    for op in ops {
        if apply_one(doc, op) {
            result.applied += 1;
        } else {
            result.skipped += 1;
        }
    }
    Ok(result)
}

fn apply_one(doc: &mut Value, op: &EditOp) -> bool {
    // `_applied` (and the former `_edit`) are the Director's record of
    // which edits it has applied: nobody may write them.
    let path = match op {
        EditOp::Set { path, .. }
        | EditOp::Unset { path }
        | EditOp::Insert { path, .. }
        | EditOp::Remove { path }
        | EditOp::Move { path, .. } => path,
    };
    if matches!(path.first(), Some(Step::Key(k)) if k == "_edit" || k == "_applied") {
        return false;
    }
    match op {
        EditOp::Set { path, value } => {
            let Some((last, parent)) = split(path) else {
                // An empty path replaces the whole document.
                *doc = value.clone();
                return true;
            };
            let Some(parent) = walk(doc, parent, Some(last)) else {
                return false;
            };
            match (last, parent) {
                (Step::Key(k), Value::Object(map)) => {
                    map.insert(k.clone(), value.clone());
                    true
                }
                // Replacing a list element keeps its id (a different one
                // would make two elements share an id).
                (Step::Id { id }, Value::Array(list)) => match position(list, id) {
                    Some(i) if id_of(value) == Some(id.as_str()) => {
                        list[i] = value.clone();
                        true
                    }
                    _ => false,
                },
                _ => false,
            }
        }
        EditOp::Unset { path } => {
            let Some((Step::Key(k), parent)) = split(path) else {
                return false;
            };
            match walk(doc, parent, None) {
                Some(Value::Object(map)) => {
                    map.remove(k);
                    true
                }
                _ => false,
            }
        }
        EditOp::Insert { path, after, value } => {
            // Ids are unique among all lists of a kind (all `cues`, all
            // `tracks`, …): an element that exists anywhere is not inserted
            // again (two people moving one cue to two different tracks would
            // otherwise duplicate it).
            let kind = path.iter().rev().find_map(|s| match s {
                Step::Key(k) => Some(k.as_str()),
                Step::Id { .. } => None,
            });
            if let Some(kind) = kind {
                if id_exists(doc, kind, id_of(value).unwrap_or_default()) {
                    return false;
                }
            }
            // A list that does not exist yet (an older document) is created.
            if walk(doc, path, None).is_none() {
                let Some((Step::Key(k), parent)) = split(path) else {
                    return false;
                };
                let Some(Value::Object(map)) = walk(doc, parent, path.last()) else {
                    return false;
                };
                map.insert(k.clone(), Value::Array(Vec::new()));
            }
            let Some(Value::Array(list)) = walk(doc, path, None) else {
                return false;
            };
            let id = id_of(value).unwrap_or_default().to_string();
            if position(list, &id).is_some() {
                return false; // already there (inserted twice)
            }
            let at = insertion_point(list, after.as_deref());
            list.insert(at, value.clone());
            true
        }
        EditOp::Remove { path } => {
            let Some((Step::Id { id }, parent)) = split(path) else {
                return false;
            };
            match walk(doc, parent, None) {
                Some(Value::Array(list)) => match position(list, id) {
                    Some(i) => {
                        list.remove(i);
                        true
                    }
                    None => false,
                },
                _ => false,
            }
        }
        EditOp::Move { path, after } => {
            let Some((Step::Id { id }, parent)) = split(path) else {
                return false;
            };
            let Some(Value::Array(list)) = walk(doc, parent, None) else {
                return false;
            };
            let Some(i) = position(list, id) else {
                return false;
            };
            let element = list.remove(i);
            let at = insertion_point(list, after.as_deref());
            list.insert(at, element);
            true
        }
    }
}

/// Whether some element of a list called `kind` anywhere in `doc` has
/// this id.
fn id_exists(doc: &Value, kind: &str, id: &str) -> bool {
    match doc {
        Value::Object(map) => map.iter().any(|(k, v)| {
            (k == kind
                && v.as_array()
                    .is_some_and(|l| l.iter().any(|e| id_of(e) == Some(id))))
                || id_exists(v, kind, id)
        }),
        Value::Array(list) => list.iter().any(|v| id_exists(v, kind, id)),
        _ => false,
    }
}

/// How many list elements share their id with an earlier element of the
/// same kind (all `cues` lists together, all `tracks`, …). The Director
/// refuses an edit that makes this grow: two cues with one id would be
/// indistinguishable to every edit, selection and Runner.
pub fn duplicate_ids(doc: &Value) -> usize {
    let mut seen = std::collections::HashSet::new();
    let mut duplicates = 0;
    count_duplicates(doc, &mut seen, &mut duplicates);
    duplicates
}

fn count_duplicates<'a>(
    v: &'a Value,
    seen: &mut std::collections::HashSet<(&'a str, &'a str)>,
    duplicates: &mut usize,
) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                if KEYED_LISTS.contains(&k.as_str()) {
                    for e in child.as_array().into_iter().flatten() {
                        if let Some(id) = id_of(e) {
                            if !seen.insert((k.as_str(), id)) {
                                *duplicates += 1;
                            }
                        }
                    }
                }
                count_duplicates(child, seen, duplicates);
            }
        }
        Value::Array(list) => list
            .iter()
            .for_each(|e| count_duplicates(e, seen, duplicates)),
        _ => {}
    }
}

/// `(last step, the steps before it)`.
fn split(path: &[Step]) -> Option<(&Step, &[Step])> {
    let (last, parent) = path.split_last()?;
    Some((last, parent))
}

/// Follow `path` from `doc`. With `create` (the step that will follow), a
/// missing object field is created as the container that step needs.
fn walk<'a>(doc: &'a mut Value, path: &[Step], create: Option<&Step>) -> Option<&'a mut Value> {
    let mut here = doc;
    for (n, step) in path.iter().enumerate() {
        here = match step {
            Step::Key(k) => {
                let map = here.as_object_mut()?;
                if create.is_some() && !map.contains_key(k) {
                    let next = path.get(n + 1).or(create)?;
                    let empty = match next {
                        Step::Key(_) => Value::Object(Map::new()),
                        Step::Id { .. } => Value::Array(Vec::new()),
                    };
                    map.insert(k.clone(), empty);
                }
                map.get_mut(k)?
            }
            Step::Id { id } => {
                let list = here.as_array_mut()?;
                let i = position(list, id)?;
                &mut list[i]
            }
        };
    }
    Some(here)
}

fn id_of(value: &Value) -> Option<&str> {
    value.get("id")?.as_str().filter(|s| !s.is_empty())
}

fn position(list: &[Value], id: &str) -> Option<usize> {
    list.iter().position(|v| id_of(v) == Some(id))
}

fn insertion_point(list: &[Value], after: Option<&str>) -> usize {
    match after {
        None => 0,
        Some(a) => position(list, a).map_or(list.len(), |i| i + 1),
    }
}

/// The lists whose elements are addressed by id. (`asset_folders`: the
/// Producer's folders of the Assets window, which only it reads.)
pub const KEYED_LISTS: [&str; 8] = [
    "tracks",
    "cues",
    "displays",
    "virtuals",
    "tasks",
    "control_cues",
    "assets",
    "asset_folders",
];

/// Give every element of the keyed lists an id, where one is missing (a
/// hand-written show): edits can only address elements that have one.
/// Returns how many were assigned.
pub fn ensure_ids(doc: &mut Value) -> usize {
    let mut taken = std::collections::HashSet::new();
    collect_ids(doc, &mut taken);
    let mut assigned = 0;
    assign_ids(doc, &mut taken, &mut assigned, false);
    assigned
}

fn collect_ids(v: &Value, taken: &mut std::collections::HashSet<String>) {
    match v {
        Value::Object(map) => {
            if let Some(id) = map.get("id").and_then(Value::as_str) {
                taken.insert(id.to_string());
            }
            map.values().for_each(|v| collect_ids(v, taken));
        }
        Value::Array(list) => list.iter().for_each(|v| collect_ids(v, taken)),
        _ => {}
    }
}

fn assign_ids(
    v: &mut Value,
    taken: &mut std::collections::HashSet<String>,
    assigned: &mut usize,
    keyed: bool,
) {
    match v {
        Value::Object(map) => {
            for (k, child) in map.iter_mut() {
                assign_ids(child, taken, assigned, KEYED_LISTS.contains(&k.as_str()));
            }
        }
        Value::Array(list) => {
            for element in list.iter_mut() {
                if keyed {
                    if let Some(map) = element.as_object_mut() {
                        let missing = map
                            .get("id")
                            .and_then(Value::as_str)
                            .is_none_or(|s| s.is_empty());
                        if missing {
                            let mut n = taken.len();
                            let id = loop {
                                let id = format!("auto-{n}");
                                if !taken.contains(&id) {
                                    break id;
                                }
                                n += 1;
                            };
                            taken.insert(id.clone());
                            map.insert("id".into(), Value::String(id));
                            *assigned += 1;
                        }
                    }
                }
                assign_ids(element, taken, assigned, false);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ops(v: Value) -> Vec<EditOp> {
        serde_json::from_value(v).unwrap()
    }

    fn doc() -> Value {
        json!({
            "name": "show",
            "tracks": [
                { "id": "t1", "cues": [ { "id": "a", "x": 0 }, { "id": "b", "x": 10 } ] },
                { "id": "t2", "cues": [] }
            ],
            "stage": { "displays": [ { "id": "d1", "warp_corners": null } ] }
        })
    }

    #[test]
    fn two_people_editing_different_things_both_keep_their_change() {
        let mut d = doc();
        // Alice moves a warp corner, Bob moves a cue: in either order the
        // result holds both.
        let alice = ops(json!([{ "op": "set",
            "path": ["stage", "displays", { "id": "d1" }, "warp_corners"],
            "value": [[0,0],[1,0],[1,1],[0,1]] }]));
        let bob = ops(json!([{ "op": "set",
            "path": ["tracks", { "id": "t1" }, "cues", { "id": "b" }, "x"], "value": 99 }]));
        apply(&mut d, &alice).unwrap();
        apply(&mut d, &bob).unwrap();
        assert_eq!(d["tracks"][0]["cues"][1]["x"], 99);
        assert_eq!(d["stage"]["displays"][0]["warp_corners"][2], json!([1, 1]));
    }

    #[test]
    fn lists_are_addressed_by_id_not_position() {
        let mut d = doc();
        // Someone inserts a cue at the start; an edit to "b" written before
        // that still reaches "b".
        apply(
            &mut d,
            &ops(json!([{ "op": "insert",
            "path": ["tracks", { "id": "t1" }, "cues"], "after": null,
            "value": { "id": "new", "x": 5 } }])),
        )
        .unwrap();
        apply(
            &mut d,
            &ops(json!([{ "op": "set",
            "path": ["tracks", { "id": "t1" }, "cues", { "id": "b" }, "x"], "value": 7 }])),
        )
        .unwrap();
        let cues = &d["tracks"][0]["cues"];
        assert_eq!(cues[0]["id"], "new");
        assert_eq!(cues[2]["x"], 7);
    }

    #[test]
    fn editing_something_deleted_meanwhile_is_skipped() {
        let mut d = doc();
        apply(&mut d, &ops(json!([{ "op": "remove", "path": ["tracks", { "id": "t1" }, "cues", { "id": "a" }] }]))).unwrap();
        let r = apply(&mut d, &ops(json!([
            { "op": "set", "path": ["tracks", { "id": "t1" }, "cues", { "id": "a" }, "x"], "value": 1 },
            { "op": "set", "path": ["name"], "value": "renamed" }
        ]))).unwrap();
        assert_eq!(
            r,
            Applied {
                applied: 1,
                skipped: 1
            }
        );
        assert_eq!(d["name"], "renamed");
    }

    #[test]
    fn move_and_insert_after() {
        let mut d = doc();
        apply(
            &mut d,
            &ops(json!([{ "op": "move", "path": ["tracks", { "id": "t1" }], "after": "t2" }])),
        )
        .unwrap();
        assert_eq!(d["tracks"][0]["id"], "t2");
        // An `after` that no longer exists appends.
        apply(
            &mut d,
            &ops(
                json!([{ "op": "insert", "path": ["tracks"], "after": "gone",
            "value": { "id": "t3", "cues": [] } }]),
            ),
        )
        .unwrap();
        assert_eq!(d["tracks"][2]["id"], "t3");
        // Inserting the same id twice does nothing the second time.
        let r = apply(
            &mut d,
            &ops(json!([{ "op": "insert", "path": ["tracks"], "after": null,
            "value": { "id": "t3" } }])),
        )
        .unwrap();
        assert_eq!(r.skipped, 1);
    }

    #[test]
    fn missing_containers_are_created() {
        let mut d = json!({});
        apply(
            &mut d,
            &ops(json!([
                { "op": "set", "path": ["stage", "background"], "value": [0, 0, 0, 1] },
                { "op": "insert", "path": ["tasks"], "value": { "id": "sting" } }
            ])),
        )
        .unwrap();
        assert_eq!(d["stage"]["background"][3], 1);
        assert_eq!(d["tasks"][0]["id"], "sting");
    }

    #[test]
    fn malformed_edits_are_refused_whole() {
        let mut d = doc();
        let before = d.clone();
        let bad = ops(json!([
            { "op": "set", "path": ["name"], "value": "x" },
            { "op": "insert", "path": ["tracks"], "value": { "no_id": true } }
        ]));
        assert!(apply(&mut d, &bad).is_err());
        assert_eq!(d, before);
        let deep: Vec<Step> = (0..20).map(|_| Step::Key("a".into())).collect();
        assert!(apply(&mut d, &[EditOp::Unset { path: deep }]).is_err());
    }

    #[test]
    fn a_cue_moved_to_two_tracks_at_once_is_not_duplicated() {
        let mut d = doc();
        let mv = |to: &str| {
            ops(json!([
                { "op": "remove", "path": ["tracks", { "id": "t1" }, "cues", { "id": "a" }] },
                { "op": "insert", "path": ["tracks", { "id": to }, "cues"], "value": { "id": "a" } }
            ]))
        };
        apply(
            &mut d,
            &ops(json!([{ "op": "insert", "path": ["tracks"], "after": "t2",
            "value": { "id": "t3", "cues": [] } }])),
        )
        .unwrap();
        apply(&mut d, &mv("t2")).unwrap();
        let r = apply(&mut d, &mv("t3")).unwrap();
        assert_eq!(
            r.applied, 0,
            "the second move finds 'a' gone and already elsewhere"
        );
        let count = d["tracks"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|t| t["cues"].as_array().unwrap())
            .filter(|c| c["id"] == "a")
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn the_confirmation_tag_and_ids_cannot_be_overwritten() {
        let mut d = doc();
        let r = apply(
            &mut d,
            &ops(json!([
                { "op": "set", "path": ["_edit"], "value": { "by": "someone else" } },
                { "op": "set", "path": ["tracks", { "id": "t2" }], "value": { "id": "t1" } }
            ])),
        )
        .unwrap();
        assert_eq!(r.skipped, 2);
        assert!(d.get("_edit").is_none());
        assert_eq!(d["tracks"][1]["id"], "t2");
    }

    #[test]
    fn ids_are_unique_per_kind_not_across_kinds() {
        // A cue may share its id with an asset; moving it still works.
        let mut d = json!({ "assets": [ { "id": "intro" } ],
            "tracks": [ { "id": "t1", "cues": [ { "id": "intro" } ] }, { "id": "t2", "cues": [] } ] });
        let r = apply(&mut d, &ops(json!([
            { "op": "remove", "path": ["tracks", { "id": "t1" }, "cues", { "id": "intro" }] },
            { "op": "insert", "path": ["tracks", { "id": "t2" }, "cues"], "value": { "id": "intro" } }
        ]))).unwrap();
        assert_eq!(r.skipped, 0);
        assert_eq!(d["tracks"][1]["cues"][0]["id"], "intro");
        assert_eq!(duplicate_ids(&d), 0);
    }

    #[test]
    fn nested_duplicates_are_counted() {
        // Re-inserting a deleted track whose cue now lives elsewhere.
        let d = json!({ "tracks": [ { "id": "t1", "cues": [ { "id": "c1" } ] },
                                    { "id": "t2", "cues": [ { "id": "c1" } ] } ] });
        assert_eq!(duplicate_ids(&d), 1);
    }

    #[test]
    fn depth_is_measured() {
        assert_eq!(depth(&json!(1)), 0);
        assert_eq!(depth(&json!({ "a": [ { "b": 1 } ] })), 3);
    }

    #[test]
    fn asset_folders_are_kept_and_edited_like_any_list() {
        // The Producer's folders (a keyed list) and where each asset is
        // filed (a map): two people filing different assets both keep their
        // change, and the show still reads and validates as a show. (An
        // older show has neither field: both are created.)
        let mut d = json!({ "name": "show", "tracks": [] });
        apply(
            &mut d,
            &ops(
                json!([{ "op": "insert", "path": ["asset_folders"], "after": null,
                "value": { "id": "f1", "name": "Show A", "parent": "" } }]),
            ),
        )
        .unwrap();
        let alice =
            ops(json!([{ "op": "set", "path": ["asset_placement", "636c6970"], "value": "f1" }]));
        let bob =
            ops(json!([{ "op": "set", "path": ["asset_placement", "6c6f676f"], "value": "f1" }]));
        apply(&mut d, &alice).unwrap();
        apply(&mut d, &bob).unwrap();
        assert_eq!(
            d["asset_placement"],
            json!({ "636c6970": "f1", "6c6f676f": "f1" })
        );
        // Moving a folder is one field.
        apply(
            &mut d,
            &ops(json!([{ "op": "set", "path": ["asset_folders", { "id": "f1" }, "parent"], "value": "f0" }])),
        )
        .unwrap();
        assert_eq!(d["asset_folders"][0]["parent"], "f0");
        let show: crate::show::ShowDocument = serde_json::from_value(d).unwrap();
        show.validate().unwrap();
    }

    #[test]
    fn hand_written_shows_get_ids() {
        let mut d = json!({ "tracks": [ { "cues": [ { "x": 1 }, { "id": "auto-1" } ] } ] });
        assert_eq!(ensure_ids(&mut d), 2);
        let track = d["tracks"][0]["id"].as_str().unwrap();
        let cue = d["tracks"][0]["cues"][0]["id"].as_str().unwrap();
        assert!(track.starts_with("auto-") && cue.starts_with("auto-"));
        assert_ne!(cue, "auto-1", "existing ids are not reused");
        assert_ne!(track, cue);
        assert_eq!(ensure_ids(&mut d), 0);
    }
}
