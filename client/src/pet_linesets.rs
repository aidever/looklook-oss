//! 宠物台词的“套”（docs/PET_PACK.md §3.6.1）。
//!
//! - **包里带的**（只读）：pet.json 的 `lines`（默认那套，ID `pack`）和 `lineSets`（ID `pack:<id>`）。
//!   跟着宠物包走：从其他电脑导入宠物、重新装都在。
//! - **自己的**（ID `user:<id>`）：按人设生成的、从文件导入的、从其他电脑带过来的，存在 kv `pet.linesets.<source:id>`，
//!   能改名、删。生成、导入都只会新加一套，不会盖掉包里的。
//!
//! 选一套用（`active`，没选时用包里默认那套）；选中的那套缺的键，用包里默认那套、再用看看内置的通用台词补（前端 lines.js）。
//! 导出成一个 JSON 文件（一套或几套，`format: looklook-pet-lines`），导入到哪只宠物都行。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::LocalError;
use crate::pets::{Pack, Text};
use crate::store::Store;

pub type Lines = BTreeMap<String, BTreeMap<String, Vec<String>>>;

pub const FORMAT: &str = "looklook-pet-lines";
/// 自己的最多几套（每套几十 KB）
const MAX_USER: usize = 30;
const NAME_MAX: usize = 40;
pub const PACK: &str = "pack";

/// 自己的一套
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct UserSet {
    pub id: String,
    pub name: String,
    /// `gen` 生成的 / `import` 导入的（从文件或其他电脑）
    pub kind: String,
    /// 生成时的风格要求
    #[serde(skip_serializing_if = "String::is_empty")]
    pub style: String,
    /// 生成用的：claude / codex / api，和模型
    #[serde(skip_serializing_if = "String::is_empty")]
    pub via: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub model: String,
    pub at: i64,
    pub lines: Lines,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Stored {
    active: Option<String>,
    sets: Vec<UserSet>,
}

fn kv_key(pet: &str) -> String {
    format!("pet.linesets.{pet}")
}

/// 以前只有一套生成的（`pet.lines.<宠物>`，生成一次替换一次）
fn old_key(pet: &str) -> String {
    format!("pet.lines.{pet}")
}

static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// 读出来；还有旧的那套生成的就搬进来（以前包里写了 `lines` 时包里的优先，搬过来后也照这样选）
fn load(store: &Store, pack: &Pack) -> Stored {
    let pet = pack.key();
    let mut st: Stored = store.get(&kv_key(&pet)).ok().flatten().unwrap_or_default();
    if let Ok(Some(old)) = store.get::<Value>(&old_key(&pet)) {
        let lines = crate::pet_lines::clean_set(&old, 1);
        if !lines.is_empty() {
            let set = UserSet {
                id: new_id(&st),
                name: String::new(),
                kind: "gen".into(),
                via: old["via"].as_str().unwrap_or_default().into(),
                at: old["at"].as_i64().unwrap_or(0),
                lines,
                ..Default::default()
            };
            if st.active.is_none() && pack.manifest.lines.is_empty() {
                st.active = Some(format!("user:{}", set.id));
            }
            st.sets.push(set);
        }
        if store.set(&kv_key(&pet), &st).is_ok() {
            let _ = store.remove(&old_key(&pet));
        }
    }
    st
}

fn save(store: &Store, pack: &Pack, st: &Stored) -> Result<(), LocalError> {
    store.set(&kv_key(&pack.key()), st)?;
    crate::pets::changed();
    Ok(())
}

fn new_id(st: &Stored) -> String {
    let mut n = crate::util::system_ms().max(0) as u64;
    loop {
        let id = format!("{n:x}");
        if !st.sets.iter().any(|s| s.id == id) {
            return id;
        }
        n += 1;
    }
}

/// 包里带的几套：`(ID, 名字, 台词)`
fn pack_sets(pack: &Pack) -> Vec<(String, Option<Text>, &Lines)> {
    let m = &pack.manifest;
    let mut out = vec![];
    if !m.lines.is_empty() {
        out.push((PACK.to_string(), None, &m.lines));
    }
    for s in &m.line_sets {
        out.push((format!("pack:{}", s.id), s.name.clone(), &s.lines));
    }
    out
}

fn exists(pack: &Pack, st: &Stored, id: &str) -> bool {
    match id.strip_prefix("user:") {
        Some(u) => st.sets.iter().any(|s| s.id == u),
        None => pack_sets(pack).iter().any(|(i, _, _)| i == id),
    }
}

/// 现在用哪套（选的那套不在了就回到包里默认那套 / 没有）
fn active_id(pack: &Pack, st: &Stored) -> Option<String> {
    st.active.clone().filter(|a| exists(pack, st, a)).or_else(|| (!pack.manifest.lines.is_empty()).then(|| PACK.to_string()))
}

/// 选中的那套台词（宠物说话用）。是包里默认那套或没选时为 Null：前端本来就先用包里的 `lines`
pub fn active_lines(store: &Store, pack: &Pack) -> Value {
    let st = load(store, pack);
    let Some(id) = active_id(pack, &st).filter(|a| a != PACK) else { return Value::Null };
    match id.strip_prefix("user:") {
        Some(u) => st.sets.iter().find(|s| s.id == u).map(|s| json!(s.lines)).unwrap_or(Value::Null),
        None => pack_sets(pack).into_iter().find(|(i, _, _)| *i == id).map(|(_, _, l)| json!(l)).unwrap_or(Value::Null),
    }
}

fn counts(l: &Lines) -> Value {
    json!(l.iter().map(|(lang, keys)| (lang.clone(), keys.values().map(Vec::len).sum::<usize>())).collect::<BTreeMap<_, _>>())
}

/// 设置页要的：全部几套（包里的在前、只读）、现在用哪套
pub fn view(store: &Store, pack: &Pack) -> Value {
    let st = load(store, pack);
    let mut sets: Vec<Value> = pack_sets(pack)
        .into_iter()
        .map(|(id, name, lines)| json!({ "id": id, "kind": "pack", "name": name, "readonly": true, "lines": lines, "count": counts(lines) }))
        .collect();
    for s in &st.sets {
        sets.push(json!({ "id": format!("user:{}", s.id), "kind": s.kind, "name": s.name, "style": s.style, "via": s.via, "model": s.model, "at": s.at, "readonly": false, "lines": s.lines, "count": counts(&s.lines) }));
    }
    json!({ "active": active_id(pack, &st), "sets": sets, "max": MAX_USER })
}

/// 新加一套自己的（生成完、导入），返回 ID（`user:…`）；`activate` = 马上用它
pub fn add(store: &Store, pack: &Pack, mut set: UserSet, activate: bool) -> Result<String, LocalError> {
    let _g = lock();
    let mut st = load(store, pack);
    if st.sets.len() >= MAX_USER {
        return Err(LocalError::new("PET_LINES_FULL").with("max", MAX_USER.to_string()));
    }
    set.id = new_id(&st);
    set.name = clip(&set.name);
    if set.at == 0 {
        set.at = crate::util::system_ms();
    }
    let id = format!("user:{}", set.id);
    st.sets.push(set);
    if activate {
        st.active = Some(id.clone());
    }
    save(store, pack, &st)?;
    Ok(id)
}

fn clip(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(NAME_MAX).collect()
}

fn user_id(id: &str) -> Result<&str, LocalError> {
    // 包里带的不能改、不能删
    id.strip_prefix("user:").ok_or_else(|| LocalError::new("PET_LINES_READONLY"))
}

pub fn remove(store: &Store, pack: &Pack, id: &str) -> Result<(), LocalError> {
    let u = user_id(id)?;
    let _g = lock();
    let mut st = load(store, pack);
    let n = st.sets.len();
    st.sets.retain(|s| s.id != u);
    if st.sets.len() == n {
        return Err(LocalError::not_found());
    }
    if st.active.as_deref() == Some(id) {
        st.active = None;
    }
    save(store, pack, &st)
}

pub fn rename(store: &Store, pack: &Pack, id: &str, name: &str) -> Result<(), LocalError> {
    let u = user_id(id)?;
    let _g = lock();
    let mut st = load(store, pack);
    let s = st.sets.iter_mut().find(|s| s.id == u).ok_or_else(LocalError::not_found)?;
    s.name = clip(name);
    save(store, pack, &st)
}

/// 用哪套：`None` = 回到包里默认那套（没有就是看看内置的）
pub fn set_active(store: &Store, pack: &Pack, id: Option<&str>) -> Result<(), LocalError> {
    let _g = lock();
    let mut st = load(store, pack);
    if let Some(id) = id {
        if !exists(pack, &st, id) {
            return Err(LocalError::not_found());
        }
    }
    st.active = id.map(String::from);
    save(store, pack, &st)
}

/// 名字（包里的按界面语言取不到就取第一个）
fn text(t: &Option<Text>) -> String {
    match t {
        Some(Text::One(s)) => s.clone(),
        Some(Text::Many(m)) => m.get("zh-CN").or_else(|| m.values().next()).cloned().unwrap_or_default(),
        None => String::new(),
    }
}

/// 导出成一个文件：`ids` 为空 = 自己的全部（从其他电脑导入宠物时带过去的就是这些）
pub fn export(store: &Store, pack: &Pack, ids: &[String]) -> Result<Value, LocalError> {
    let st = load(store, pack);
    let mut sets = vec![];
    if ids.is_empty() {
        for s in &st.sets {
            sets.push(json!({ "name": s.name, "kind": s.kind, "style": s.style, "via": s.via, "model": s.model, "at": s.at, "lines": s.lines }));
        }
    }
    for id in ids {
        if let Some(u) = id.strip_prefix("user:") {
            let s = st.sets.iter().find(|s| s.id == u).ok_or_else(LocalError::not_found)?;
            sets.push(json!({ "name": s.name, "kind": s.kind, "style": s.style, "via": s.via, "model": s.model, "at": s.at, "lines": s.lines }));
        } else {
            let (_, name, lines) = pack_sets(pack).into_iter().find(|(i, _, _)| i == id).ok_or_else(LocalError::not_found)?;
            let name = if id == PACK { text(&pack.manifest.name) } else { text(&name) };
            sets.push(json!({ "name": name, "kind": "pack", "lines": lines }));
        }
    }
    Ok(json!({ "format": FORMAT, "version": 1, "pet": pack.id, "petName": text(&pack.manifest.name), "sets": sets }))
}

/// 导入一个导出文件里的几套（都当成自己的）。和已有的一模一样的跳过。返回新加的 ID；一套都没有时报错
pub fn import(store: &Store, pack: &Pack, file: &Value, activate: bool) -> Result<Vec<String>, LocalError> {
    if file["format"] != FORMAT {
        return Err(LocalError::new("PET_LINES_FORMAT"));
    }
    let list = file["sets"].as_array().ok_or_else(|| LocalError::new("PET_LINES_FORMAT"))?;
    let mut added = vec![];
    let mut any = false;
    for v in list.iter().take(MAX_USER) {
        let lines = crate::pet_lines::clean_set(&v["lines"], 1);
        if lines.is_empty() {
            continue;
        }
        any = true;
        let st = load(store, pack);
        if st.sets.iter().any(|s| s.lines == lines) || pack_sets(pack).iter().any(|(_, _, l)| **l == lines) {
            continue;
        }
        let s = |k: &str| v[k].as_str().unwrap_or_default().chars().take(500).collect::<String>();
        let set = UserSet { name: s("name"), kind: "import".into(), style: s("style"), via: s("via"), model: s("model"), at: v["at"].as_i64().unwrap_or(0), lines, ..Default::default() };
        added.push(add(store, pack, set, false)?);
    }
    if !any {
        return Err(LocalError::new("PET_LINES_EMPTY"));
    }
    if activate {
        if let Some(id) = added.first() {
            set_active(store, pack, Some(id))?;
        }
    }
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(lines: bool) -> Pack {
        let mut m = crate::pets::Manifest { id: "neko".into(), ..Default::default() };
        if lines {
            m.lines.insert("zh-CN".into(), BTreeMap::from([("done".to_string(), vec!["好啦".to_string()])]));
        }
        m.line_sets.push(crate::pets::LineSet { id: "cat".into(), name: Some(Text::One("猫".into())), lines: BTreeMap::from([("zh-CN".to_string(), BTreeMap::from([("done".to_string(), vec!["喵".to_string()])]))]), ..Default::default() });
        Pack { source: "local", id: "neko".into(), dir: Default::default(), manifest: m, files: Default::default(), error: None }
    }

    fn lines(s: &str) -> Lines {
        BTreeMap::from([("zh-CN".to_string(), BTreeMap::from([("done".to_string(), vec![s.to_string()])]))])
    }

    #[test]
    fn sets_pick_import_export() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("s.db")).unwrap();
        let p = pack(true);
        // 旧的那套生成的搬进来；包里写了 lines 时还是先用包里的
        store.set("pet.lines.local:neko", &json!({ "zh-CN": { "done": ["旧的"] }, "via": "claude", "at": 5 })).unwrap();
        let v = view(&store, &p);
        assert_eq!(v["active"], PACK);
        assert_eq!(v["sets"].as_array().unwrap().len(), 3);
        assert_eq!(v["sets"][2]["kind"], "gen");
        assert!(store.get::<Value>("pet.lines.local:neko").unwrap().is_none());
        assert_eq!(active_lines(&store, &p), Value::Null);
        // 生成一套：新加，不盖包里的
        let id = add(&store, &p, UserSet { kind: "gen".into(), name: "傲娇".into(), lines: lines("哼"), ..Default::default() }, true).unwrap();
        assert_eq!(active_lines(&store, &p)["zh-CN"]["done"][0], "哼");
        assert!(remove(&store, &p, PACK).is_err() && rename(&store, &p, "pack:cat", "x").is_err(), "包里的只读");
        set_active(&store, &p, Some("pack:cat")).unwrap();
        assert_eq!(active_lines(&store, &p)["zh-CN"]["done"][0], "喵");
        // 导出两套 → 导到另一只：包里那套和自己的都成了自己的；再导一次全跳过
        let file = export(&store, &p, &["pack:cat".into(), id.clone()]).unwrap();
        let q = Pack { id: "dog".into(), ..pack(false) };
        let got = import(&store, &q, &file, true).unwrap();
        assert_eq!(got.len(), 1, "和包里那套一样的跳过");
        assert_eq!(active_lines(&store, &q)["zh-CN"]["done"][0], "哼");
        assert!(import(&store, &q, &file, false).unwrap().is_empty());
        assert!(import(&store, &q, &json!({ "format": FORMAT, "sets": [{ "lines": {} }] }), false).is_err());
        // 不指定 = 自己的全部（从其他电脑带过去的）
        assert_eq!(export(&store, &p, &[]).unwrap()["sets"].as_array().unwrap().len(), 2);
        remove(&store, &p, &id).unwrap();
        set_active(&store, &p, Some("pack:cat")).unwrap();
        assert_eq!(view(&store, &p)["active"], "pack:cat");
    }
}
