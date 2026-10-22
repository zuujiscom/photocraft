//! Portable Actions JSON files share the preset store's versioned representation.

use std::collections::{HashMap, HashSet};

use super::{Action, ActionsFile, bad};
use crate::{Result, Session};
use serde_json::{Value, json};

const MAX_ACTIONS: usize = 1024;
const MAX_STEPS: usize = 10_000;
const MAX_NAME_BYTES: usize = 256;

fn idle(s: &Session, cmd: &str) -> Result<()> {
    if s.actions.recording.is_some() || s.actions.playing > 0 {
        return Err(bad(cmd, "stop recording or playing before transferring Actions"));
    }
    Ok(())
}

fn validate(file: &ActionsFile, cmd: &str) -> Result<()> {
    if file.version != 1 {
        return Err(bad(cmd, format!("unsupported Actions file version {}", file.version)));
    }
    if file.actions.len() > MAX_ACTIONS {
        return Err(bad(cmd, "an Actions JSON file may contain at most 1024 actions"));
    }
    let mut names = HashSet::new();
    for action in &file.actions {
        if action.name.trim().is_empty() || action.name.len() > MAX_NAME_BYTES || !names.insert(&action.name) {
            return Err(bad(cmd, "action names must be unique, non-empty and at most 256 bytes"));
        }
        if action.steps.len() > MAX_STEPS {
            return Err(bad(cmd, "an action may contain at most 10,000 steps"));
        }
        if action.disabled.iter().chain(&action.dialogs).any(|&i| i >= action.steps.len()) {
            return Err(bad(cmd, "a disabled or dialog step index is out of range"));
        }
        if action.set.as_ref().is_some_and(|name| name.len() > MAX_NAME_BYTES) {
            return Err(bad(cmd, "action set names may contain at most 256 bytes"));
        }
    }
    Ok(())
}

fn same_contents(a: &Action, b: &Action) -> bool {
    a.steps == b.steps && a.disabled == b.disabled && a.dialogs == b.dialogs && a.set == b.set && a.color == b.color
}

fn same_name_family(source: &str, candidate: &str) -> bool {
    if source == candidate {
        return true;
    }
    let Some((stem, tail)) = candidate.rsplit_once(" (Imported ") else { return false };
    if tail.strip_suffix(')').and_then(|n| n.parse::<usize>().ok()).is_none_or(|n| n < 2) {
        return false;
    }
    let suffix_len = candidate.len().saturating_sub(stem.len());
    let mut end = source.len().min(MAX_NAME_BYTES.saturating_sub(suffix_len));
    while !source.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    source.get(..end) == Some(stem)
}

fn retarget(action: &mut Action, original: &[String], mapping: &HashMap<String, String>, cmd: &str) -> Result<()> {
    for (id, params) in &mut action.steps {
        if id != "actions.play" {
            continue;
        }
        let name = match params.get("action") {
            Some(Value::String(name)) => name.clone(),
            Some(Value::Number(n)) => {
                let index = n.as_u64().and_then(|i| usize::try_from(i).ok()).ok_or_else(|| bad(cmd, "nested action index must be a non-negative integer"))?;
                original.get(index).cloned().ok_or_else(|| bad(cmd, format!("invalid nested action index {index}")))?
            }
            _ => return Err(bad(cmd, "nested action calls need a name or index")),
        };
        if let Some(target) = mapping.get(&name) {
            let object = params.as_object_mut().ok_or_else(|| bad(cmd, "nested action parameters must be an object"))?;
            object.insert("action".into(), json!(target));
        }
    }
    Ok(())
}

fn imported_name(name: &str, reserved: &mut HashSet<String>, cmd: &str) -> Result<String> {
    if reserved.insert(name.to_string()) {
        return Ok(name.to_string());
    }
    // There can be no more occupied candidates than reserved names. The bound also avoids
    // an unbounded counter on a hostile library.
    for n in 2..=reserved.len().saturating_add(2) {
        let suffix = format!(" (Imported {n})");
        let limit = MAX_NAME_BYTES.saturating_sub(suffix.len());
        let mut end = name.len().min(limit);
        while !name.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        let base = name.get(..end).ok_or_else(|| bad(cmd, "invalid action name boundary"))?;
        let candidate = format!("{base}{suffix}");
        if reserved.insert(candidate.clone()) {
            return Ok(candidate);
        }
    }
    Err(bad(cmd, "cannot allocate a unique imported action name"))
}

pub(super) fn import(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "actions.import";
    idle(s, CMD)?;
    let data = p.get("data").and_then(Value::as_str).ok_or_else(|| bad(CMD, "data must be an Actions JSON string"))?;
    if data.len() as u64 > crate::preset_store::MAX_ACTIONS_BYTES {
        return Err(bad(CMD, "Actions JSON exceeds the 16 MiB import limit"));
    }
    let file: ActionsFile = serde_json::from_str(data).map_err(|e| bad(CMD, format!("invalid Actions JSON: {e}")))?;
    validate(&file, CMD)?;
    let original: Vec<String> = file.actions.iter().map(|a| a.name.clone()).collect();
    let existing = &s.actions.list;
    let mut reserved: HashSet<String> = existing.iter().map(|a| a.name.clone()).chain(original.iter().cloned()).collect();
    let mut mapping = HashMap::new();
    for action in &file.actions {
        let target = if let Some(found) = existing.iter().find(|a| a.name == action.name && same_contents(a, action)) {
            found.name.clone()
        } else if !existing.iter().any(|a| a.name == action.name) {
            action.name.clone()
        } else {
            imported_name(&action.name, &mut reserved, CMD)?
        };
        mapping.insert(action.name.clone(), target);
    }
    // A parent can precede its child. Reusing an already imported child can make the
    // parent reusable too, so resolve those dependencies before mutating the library.
    let mut planned = Vec::new();
    for _ in 0..=file.actions.len() {
        planned.clear();
        let mut corrections = Vec::new();
        for source in &file.actions {
            let mut action = source.clone();
            retarget(&mut action, &original, &mapping, CMD)?;
            let target = mapping.get(&source.name).ok_or_else(|| bad(CMD, "missing imported action name"))?;
            action.name = target.clone();
            if let Some(found) = existing.iter().find(|a| same_contents(a, &action) && (a.name == *target || same_name_family(&source.name, &a.name))) {
                if action.name != found.name {
                    corrections.push((source.name.clone(), found.name.clone()));
                }
                action.name = found.name.clone();
            }
            planned.push(action);
        }
        if corrections.is_empty() {
            break;
        }
        mapping.extend(corrections);
    }
    // Apply the final mapping once more so every retained nested call uses the destination.
    for (source, action) in file.actions.iter().zip(&mut planned) {
        *action = source.clone();
        retarget(action, &original, &mapping, CMD)?;
        action.name = mapping.get(&source.name).cloned().ok_or_else(|| bad(CMD, "missing imported action name"))?;
    }
    let insert: Vec<Action> = planned.into_iter().filter(|a| !existing.iter().any(|b| a == b)).collect();
    let names: Vec<String> = insert.iter().map(|a| a.name.clone()).collect();
    let imported = insert.len();
    if imported > 0 {
        s.actions.list.extend(insert);
        s.actions.touch();
    }
    Ok(json!({"imported": imported, "skipped": original.len().saturating_sub(imported), "actions": names}))
}

pub(super) fn export(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "actions.export";
    idle(s, CMD)?;
    let path = match p.get("path") {
        None => None,
        Some(Value::String(path)) if !path.trim().is_empty() => Some(path.as_str()),
        Some(_) => return Err(bad(CMD, "path must be a non-empty string")),
    };
    let file = ActionsFile { version: 1, actions: s.actions.list.clone() };
    validate(&file, CMD)?;
    let data = serde_json::to_string_pretty(&file).map_err(|e| bad(CMD, e.to_string()))?;
    if data.len() as u64 > crate::preset_store::MAX_ACTIONS_BYTES {
        return Err(bad(CMD, "Actions JSON exceeds the 16 MiB export limit"));
    }
    let mut out = json!({"count": file.actions.len(), "bytes": data.len()});
    if let Some(path) = path {
        crate::file_cmds::write_file(path, data.as_bytes())?;
        out["path"] = json!(path);
    } else {
        out["data"] = json!(data);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_actions_round_trip_preserves_metadata() {
        let mut s = Session::new();
        let mut action = Action::new("Draw", vec![("layer.new.layer".into(), json!({}))]);
        action.disabled.insert(0);
        action.dialogs.insert(0);
        action.set = Some("Painting".into());
        s.actions.list.push(action.clone());
        let exported = s.execute("actions.export", json!({})).unwrap();
        s.actions.list.clear();
        let result = s.execute("actions.import", json!({"data": exported["data"]})).unwrap();
        assert_eq!(result["imported"], 1);
        assert_eq!(s.actions.list, [action]);
        let rev = s.actions.rev;
        assert_eq!(s.execute("actions.import", json!({"data": exported["data"]})).unwrap()["imported"], 0);
        assert_eq!(s.actions.rev, rev);
    }

    #[test]
    fn reimport_after_changing_button_color_keeps_names_unique() {
        let mut s = Session::new();
        s.actions.list.push(Action::new("Draw", vec![]));
        let data = s.execute("actions.export", json!({})).unwrap()["data"].clone();
        s.execute("actions.setColor", json!({"action": "Draw", "color": "red"})).unwrap();
        let params = json!({"data": data});
        assert_eq!(s.execute("actions.import", params.clone()).unwrap()["imported"], 1);
        assert_eq!(s.actions.list[0].name, "Draw");
        assert_eq!(s.actions.list[0].color, "red");
        assert_eq!(s.actions.list[1].name, "Draw (Imported 2)");
        assert!(s.actions.list[1].color.is_empty());
        let before = s.actions.clone();
        assert_eq!(s.execute("actions.import", params).unwrap()["imported"], 0);
        assert_eq!(s.actions, before);
    }

    #[test]
    fn parents_before_children_are_retargeted_and_reimport_is_idempotent() {
        let mut s = Session::new();
        s.actions.list.push(Action::new("Child", vec![]));
        let file = ActionsFile {
            version: 1,
            actions: vec![
                Action::new("Parent", vec![("actions.play".into(), json!({"action": 1}))]),
                Action::new("Child", vec![("layer.new.layer".into(), json!({}))]),
                Action::new("Named", vec![("actions.play".into(), json!({"action": "Child"}))]),
            ],
        };
        let params = json!({"data": serde_json::to_string(&file).unwrap()});
        assert_eq!(s.execute("actions.import", params.clone()).unwrap()["imported"], 3);
        assert_eq!(s.actions.list[1].steps[0].1["action"], "Child (Imported 2)");
        assert_eq!(s.actions.list[2].name, "Child (Imported 2)");
        assert_eq!(s.actions.list[3].steps[0].1["action"], "Child (Imported 2)");
        let before = s.actions.clone();
        assert_eq!(s.execute("actions.import", params).unwrap()["imported"], 0);
        assert_eq!(s.actions, before);
    }

    #[test]
    fn invalid_json_params_and_nested_indices_fail_atomically() {
        let mut s = Session::new();
        s.actions.list.push(Action::new("Keep", vec![]));
        let before = s.actions.clone();
        for p in [Value::Null, json!([]), json!({"data": true}), json!({"data": "invalid"}), json!({"data": "{\"version\":2,\"actions\":[]}"})] {
            assert!(s.execute("actions.import", p).is_err());
            assert_eq!(s.actions, before);
        }
        for target in [json!(-1), json!(0.5), json!(1), Value::Null] {
            let file = ActionsFile { version: 1, actions: vec![Action::new("A", vec![("actions.play".into(), json!({"action": target}))])] };
            assert!(s.execute("actions.import", json!({"data": serde_json::to_string(&file).unwrap()})).is_err());
            assert_eq!(s.actions, before);
        }
        assert!(s.execute("actions.export", json!({"path": 1})).is_err());
        s.actions.recording = Some((0, 0));
        assert!(s.execute("actions.import", json!({"data": "{}"})).is_err());
        assert!(s.execute("actions.export", json!({})).is_err());
        assert_eq!(s.actions.list, before.list);
    }

    #[test]
    fn long_unicode_collision_names_remain_portable_and_idempotent() {
        let mut s = Session::new();
        let name = "界".repeat(85);
        s.actions.list.push(Action::new(&name, vec![]));
        let file = ActionsFile { version: 1, actions: vec![Action::new(&name, vec![("select.all".into(), json!({}))])] };
        let params = json!({"data": serde_json::to_string(&file).unwrap()});
        assert_eq!(s.execute("actions.import", params.clone()).unwrap()["imported"], 1);
        assert!(s.actions.list[1].name.len() <= MAX_NAME_BYTES);
        assert_eq!(s.execute("actions.import", params).unwrap()["imported"], 0);
        assert!(s.execute("actions.export", json!({})).is_ok());
    }
}
