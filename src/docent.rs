//! The project's `.docent/` folder and the indexer that keeps Smartloop in
//! step with it.
//!
//! `.docent/manifest.json` maps every document and skill pushed to a Smartloop
//! project: what it was, its hash, and the id or name it has there. Once
//! `/sync` links a folder to a project, the chat scans the folder every few
//! seconds, diffs the scan against that map and pushes only what was added,
//! changed or removed, so the Smartloop project follows the folder. Nothing
//! is linked, and no project created, on its own.
//!
//! The project root is the nearest folder up from the working directory that
//! holds a `.docent/`, or the working directory when none does. Each machine
//! also records the folder every project is linked to, in
//! `~/.smartloop/docent-folders.json`, so a project keeps syncing from its
//! own folder whichever one the chat was opened in. A project has one folder:
//! linking it to another pauses the old one.
//!
//! Documents are the files under the project root with a supported extension
//! (hidden folders, `.gitignore` and `.docentignore` are honoured). Skills are
//! `.docent/skills/<name>.md` or `.docent/skills/<name>/SKILL.md`; they are
//! the part of `.docent/` meant to be committed, the manifest is local.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::blocking::{Client, multipart};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use md5::Md5;
use sha2::{Digest, Sha256};

use crate::{error_message, projects_url, request_json, try_fetch_projects};

pub const DIR: &str = ".docent";
const MANIFEST: &str = "manifest.json";
const SKILLS: &str = "skills";
const IGNORE_FILE: &str = ".docentignore";
/// The folder each project is linked to on this machine, in the install dir.
const FOLDERS: &str = "docent-folders.json";
/// What the agent can turn into searchable documents.
const EXTENSIONS: [&str; 7] = ["pdf", "docx", "pptx", "xlsx", "csv", "md", "txt"];
/// Those the agent can't read in place: they need indexing to be searched.
const DOCUMENTS: [&str; 4] = ["pdf", "docx", "pptx", "xlsx"];
/// How many changes a pass pushes at once.
const PARALLEL: usize = 4;
/// A first sync pushing more documents than this, or more bytes, asks first.
const LARGE_FILES: usize = 200;
const LARGE_BYTES: u64 = 500 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Entry {
    hash: String,
    size: u64,
    mtime: u64,
    /// The document's name in the project's inbox, or the skill's id.
    remote: String,
    synced_at: u64,
    /// The project has listed the document since its upload, so its absence
    /// there now means it was deleted, not that it's still being registered.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    registered: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
    #[serde(default)]
    project: Option<String>,
    /// Set by `/sync stop`: the chat leaves the folder alone until `/sync`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    paused: bool,
    /// Keyed by path relative to the project root, with `/` separators.
    #[serde(default)]
    documents: BTreeMap<String, Entry>,
    /// Keyed by path relative to `.docent/`.
    #[serde(default)]
    skills: BTreeMap<String, Entry>,
}

impl Manifest {
    fn load(root: &Path) -> Result<Self, String> {
        let path = root.join(DIR).join(MANIFEST);
        match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| format!("{} is not valid ({}); delete it to start over", path.display(), e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("Failed to read {}: {}", path.display(), e)),
        }
    }

    /// Written through a temporary file so a crash never leaves it half done.
    fn save(&self, root: &Path) -> Result<(), String> {
        let dir = root.join(DIR);
        fs::create_dir_all(&dir).map_err(|e| format!("Failed to create {}: {}", dir.display(), e))?;
        // The manifest holds this machine's project id: keep it out of git.
        let ignore = dir.join(".gitignore");
        if !ignore.exists() {
            let _ = fs::write(&ignore, format!("{}\n", MANIFEST));
        }
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        let tmp = dir.join(format!("{}.tmp", MANIFEST));
        fs::write(&tmp, text + "\n").map_err(|e| format!("Failed to write {}: {}", tmp.display(), e))?;
        fs::rename(&tmp, dir.join(MANIFEST)).map_err(|e| format!("Failed to save the manifest: {}", e))
    }
}

/// A file as the scan found it.
#[derive(Debug, Clone, PartialEq)]
struct Found {
    path: PathBuf,
    hash: String,
    size: u64,
    mtime: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Change {
    Add,
    Update,
    Remove,
}

impl Change {
    fn symbol(self) -> char {
        match self {
            Change::Add => '+',
            Change::Update => '~',
            Change::Remove => '-',
        }
    }
}

#[derive(Debug, Default)]
struct Plan {
    documents: Vec<(String, Change)>,
    skills: Vec<(String, Change)>,
}

impl Plan {
    fn is_empty(&self) -> bool {
        self.documents.is_empty() && self.skills.is_empty()
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default()
}

fn relative_key(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn hash_file(path: &Path) -> std::io::Result<String> {
    hex_digest::<Sha256>(path)
}

fn hex_digest<D: Digest + std::io::Write>(path: &Path) -> std::io::Result<String> {
    let mut hasher = D::new();
    std::io::copy(&mut fs::File::open(path)?, &mut hasher)?;
    Ok(hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect())
}

/// The id the project gives a document: the MD5 of its content.
fn document_id(path: &Path) -> std::io::Result<String> {
    hex_digest::<Md5>(path)
}

fn found(path: &Path, known: Option<&Entry>) -> std::io::Result<Found> {
    let meta = fs::metadata(path)?;
    let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    // A file whose size and time are unchanged keeps its recorded hash, so a
    // pass over a large unchanged project reads nothing.
    let hash = match known {
        Some(entry) if entry.size == meta.len() && entry.mtime == mtime => entry.hash.clone(),
        _ => hash_file(path)?,
    };
    Ok(Found { path: path.to_path_buf(), hash, size: meta.len(), mtime })
}

/// The documents under `root`: files of a kind the agent reads, past the
/// ignore files and hidden folders.
fn walk(root: &Path) -> impl Iterator<Item = ignore::DirEntry> {
    ignore::WalkBuilder::new(root)
        .add_custom_ignore_filename(IGNORE_FILE)
        .require_git(false)
        .build()
        .flatten()
        .filter(|entry| {
            let supported = entry
                .path()
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
            supported && entry.file_type().is_some_and(|t| t.is_file())
        })
}

/// The project's documents, by relative path.
fn scan_documents(root: &Path, known: &BTreeMap<String, Entry>) -> BTreeMap<String, Found> {
    let mut files = BTreeMap::new();
    for entry in walk(root) {
        let key = relative_key(root, entry.path());
        if let Ok(file) = found(entry.path(), known.get(&key)) {
            files.insert(key, file);
        }
    }
    files
}

/// The project's skills, by path relative to `.docent/`.
fn scan_skills(root: &Path) -> BTreeMap<String, Found> {
    let base = root.join(DIR);
    let dir = base.join(SKILLS);
    let mut files = BTreeMap::new();
    let Ok(entries) = fs::read_dir(&dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let skill = if path.is_dir() {
            path.join("SKILL.md")
        } else if path.extension().is_some_and(|e| e == "md") {
            path
        } else {
            continue;
        };
        if let Ok(file) = found(&skill, None) {
            files.insert(relative_key(&base, &skill), file);
        }
    }
    files
}

/// What it takes to bring `known` to `found`.
fn diff(found: &BTreeMap<String, Found>, known: &BTreeMap<String, Entry>) -> Vec<(String, Change)> {
    let mut changes = Vec::new();
    for (key, file) in found {
        match known.get(key) {
            None => changes.push((key.clone(), Change::Add)),
            Some(entry) if entry.hash != file.hash => changes.push((key.clone(), Change::Update)),
            Some(_) => {}
        }
    }
    changes.extend(known.keys().filter(|key| !found.contains_key(*key)).map(|key| (key.clone(), Change::Remove)));
    changes
}

/// A frontmatter value of a skill (`name:`, `description:`).
fn frontmatter(content: &str, key: &str) -> Option<String> {
    let block = content.strip_prefix("---")?.split("\n---").next()?;
    block
        .lines()
        .find_map(|line| {
            let value = line.trim().strip_prefix(key)?.strip_prefix(':')?;
            Some(value.trim().trim_matches(['"', '\'']).to_string())
        })
        .filter(|value| !value.is_empty())
}

/// The skill's name: its frontmatter's, else its file's or folder's.
fn skill_name(key: &str, content: &str) -> String {
    frontmatter(content, "name").unwrap_or_else(|| {
        let path = Path::new(key);
        let named = if path.file_name().is_some_and(|n| n == "SKILL.md") { path.parent() } else { Some(path) };
        named
            .and_then(|p| p.file_stem())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| key.to_string())
    })
}

/// The name a document goes by in the project's inbox, which is flat.
fn remote_name(key: &str) -> String {
    key.replace('/', "__")
}

/// The project at Smartloop that `.docent/` syncs to.
struct Remote<'a> {
    client: &'a Client,
    project: String,
}

impl Remote<'_> {
    fn url(&self, tail: &str) -> String {
        format!("{}/{}/{}", projects_url(), self.project, tail)
    }

    fn documents(&self) -> Result<Vec<Value>, String> {
        let reply = request_json(self.client, self.url("documents"), "list documents")?;
        Ok(reply["documents"].as_array().cloned().unwrap_or_default())
    }

    fn skills(&self) -> Result<Vec<Value>, String> {
        let reply = request_json(self.client, self.url("skills"), "list skills")?;
        Ok(reply["skills"].as_array().cloned().unwrap_or_default())
    }

    /// Upload `file` to the project's inbox as `name`; returns the name it
    /// was stored under, which differs when one by that name is there still.
    fn upload(&self, root: &Path, file: &Path, name: &str) -> Result<String, String> {
        let action = format!("upload {}", name);
        let part = multipart::Part::file(file)
            .map_err(|e| format!("Failed to read {}: {}", file.display(), e))?
            .file_name(name.to_string());
        let form = multipart::Form::new().part("file", part).text("cwd", root.to_string_lossy().into_owned());
        let response = self
            .client
            .post(self.url("documents/upload"))
            .multipart(form)
            .send()
            .map_err(|e| format!("Failed to {}: {}", action, e))?;
        if !response.status().is_success() {
            return Err(error_message(&action, response));
        }
        let reply: Value = response.json().map_err(|e| format!("Failed to {}: {}", action, e))?;
        Ok(reply["filename"].as_str().unwrap_or(name).to_string())
    }

    /// Delete the document stored under `name`; one the project no longer
    /// has counts as deleted. False when it's there but not yet registered,
    /// so it has no id to delete it by.
    fn delete_document(&self, name: &str) -> Result<bool, String> {
        let documents = self.documents()?;
        let Some(document) = documents.iter().find(|d| stored_name(d).as_deref() == Some(name)) else {
            return Ok(true);
        };
        let Some(id) = document["id"].as_str() else {
            return Ok(false);
        };
        let action = format!("delete {}", name);
        let response = self
            .client
            .delete(self.url(&format!("documents/{}", id)))
            .send()
            .map_err(|e| format!("Failed to {}: {}", action, e))?;
        if response.status().is_success() || response.status() == 404 {
            Ok(true)
        } else {
            Err(error_message(&action, response))
        }
    }

    /// Send a skill request and return the skill's id.
    fn send_skill(&self, request: reqwest::blocking::RequestBuilder, action: &str) -> Result<String, String> {
        let response = request.send().map_err(|e| format!("Failed to {}: {}", action, e))?;
        if !response.status().is_success() {
            return Err(error_message(action, response));
        }
        let reply: Value = response.json().map_err(|e| format!("Failed to {}: {}", action, e))?;
        Ok(reply["id"].as_str().unwrap_or_default().to_string())
    }

    fn add_skill(&self, name: &str, content: &str) -> Result<String, String> {
        let mut body = json!({ "content": content, "name": name });
        if let Some(description) = frontmatter(content, "description") {
            body["description"] = Value::String(description);
        }
        self.send_skill(self.client.post(self.url("skills")).json(&body), &format!("add skill {}", name))
    }

    fn update_skill(&self, id: &str, name: &str, content: &str) -> Result<String, String> {
        let request = self.client.patch(self.url(&format!("skills/{}", id))).json(&json!({ "content": content }));
        self.send_skill(request, &format!("update skill {}", name))?;
        Ok(id.to_string())
    }

    fn delete_skill(&self, id: &str, name: &str) -> Result<(), String> {
        let action = format!("delete skill {}", name);
        let response = self
            .client
            .delete(self.url(&format!("skills/{}", id)))
            .send()
            .map_err(|e| format!("Failed to {}: {}", action, e))?;
        if response.status().is_success() || response.status() == 404 {
            Ok(())
        } else {
            Err(error_message(&action, response))
        }
    }
}

/// The name a listed document is stored under in the project's inbox.
fn stored_name(document: &Value) -> Option<String> {
    let file = document["path"].as_str().and_then(|p| Path::new(p).file_name()?.to_str().map(str::to_string));
    file.or_else(|| document["filename"].as_str().map(str::to_string))
}

/// Entries the project no longer has are dropped from the manifest, so the
/// next diff adds them again. A document counts as gone only once the project
/// has listed it: the agent registers an upload when it next scans its inbox,
/// which takes as long as the indexing ahead of it, and pushing one again
/// before then would only index it twice.
fn forget_missing(manifest: &mut Manifest, listed: &[Value], remote: &Remote) -> Result<(), String> {
    // Registered is listed with an id: an upload still waiting in the inbox
    // is listed without one, and can't be deleted yet.
    let stored: HashSet<String> = listed.iter().filter(|d| d["id"].is_string()).filter_map(stored_name).collect();
    reconcile(&mut manifest.documents, &stored);

    let ids: HashSet<String> =
        remote.skills()?.iter().filter_map(|s| s["id"].as_str().map(str::to_string)).collect();
    manifest.skills.retain(|_, e| ids.contains(&e.remote));
    Ok(())
}

/// Take up the documents to add that the project has already, by content,
/// as a map lost or made afresh leaves them: recorded, not pushed again,
/// where an upload would only sit beside them never registered. Returns how
/// many it took up.
fn adopt(
    manifest: &mut Manifest,
    listed: &[Value],
    changes: &mut Vec<(String, Change)>,
    found: &BTreeMap<String, Found>,
) -> usize {
    let ids: BTreeMap<&str, String> =
        listed.iter().filter_map(|d| Some((d["id"].as_str()?, stored_name(d)?))).collect();
    if ids.is_empty() {
        return 0;
    }
    // One document is tracked once: a second copy here is pushed as usual.
    let mut taken: HashSet<String> = manifest.documents.values().map(|e| e.remote.clone()).collect();
    let before = changes.len();
    changes.retain(|(key, change)| {
        let Some(file) = found.get(key).filter(|_| *change == Change::Add) else { return true };
        let Some(name) = document_id(&file.path).ok().and_then(|id| ids.get(id.as_str())) else { return true };
        if !taken.insert(name.clone()) {
            return true;
        }
        let entry = Entry {
            hash: file.hash.clone(),
            size: file.size,
            mtime: file.mtime,
            remote: name.clone(),
            synced_at: now(),
            registered: true,
        };
        manifest.documents.insert(key.clone(), entry);
        false
    });
    before - changes.len()
}

/// `2 documents in the project aren't in this folder (a.pdf, b.md)`: those
/// it has that nothing here tracks or is about to replace, said on a first
/// sync and left alone, as they may have come from elsewhere.
fn strays(manifest: &Manifest, listed: &[Value], changes: &[(String, Change)]) -> Option<String> {
    let tracked: HashSet<String> = manifest
        .documents
        .values()
        .map(|e| e.remote.clone())
        .chain(changes.iter().map(|(key, _)| remote_name(key)))
        .collect();
    let names: Vec<String> = listed
        .iter()
        .filter(|d| d["id"].is_string())
        .filter_map(stored_name)
        .filter(|name| !tracked.contains(name))
        .collect();
    let n = names.len();
    if n == 0 {
        return None;
    }
    let shown = names.iter().take(2).cloned().collect::<Vec<_>>().join(", ");
    let more = if n > 2 { format!(", and {} more", n - 2) } else { String::new() };
    let (noun, verb, them) = if n == 1 { ("document", "isn't", "it") } else { ("documents", "aren't", "them") };
    Some(format!("{n} {noun} in the project {verb} in this folder ({shown}{more}); sync leaves {them} be"))
}

/// Mark the documents the project lists as registered, and drop those it
/// listed once and no longer does.
fn reconcile(documents: &mut BTreeMap<String, Entry>, stored: &HashSet<String>) {
    documents.retain(|_, e| {
        let listed = stored.contains(&e.remote);
        e.registered |= listed;
        listed || !e.registered
    });
}

/// One change, pushed; returns what the project now stores it as, `None`
/// once it's gone. `old` is what the manifest recorded for it.
fn push(
    remote: &Remote,
    root: &Path,
    skill: bool,
    key: &str,
    change: Change,
    file: Option<&Found>,
    old: Option<&str>,
) -> Result<Option<String>, String> {
    let content = || {
        let path = &file.ok_or("File vanished")?.path;
        fs::read_to_string(path).map_err(|e| format!("Failed to read {}: {}", path.display(), e))
    };

    Ok(match (skill, change) {
        (false, Change::Remove) => {
            let name = old.unwrap_or_default();
            if !remote.delete_document(name)? {
                return Err(format!("{} isn't registered yet; it's deleted once it is", name));
            }
            None
        }
        (true, Change::Remove) => {
            remote.delete_skill(old.unwrap_or_default(), key)?;
            None
        }
        (false, _) => {
            // A changed document goes in again under the same name. One the
            // project has by name only isn't this folder's to replace: it
            // may have come from elsewhere, and the upload goes in beside it.
            if let Some(name) = old {
                remote.delete_document(name)?;
            }
            Some(remote.upload(root, &file.ok_or("File vanished")?.path, &remote_name(key))?)
        }
        (true, _) => {
            let content = content()?;
            let name = skill_name(key, &content);
            Some(match old {
                Some(id) => remote.update_skill(id, &name, &content)?,
                None => remote.add_skill(&name, &content)?,
            })
        }
    })
}

/// The manifest catches up with a change `push` made.
fn record(
    root: &Path,
    manifest: &mut Manifest,
    skill: bool,
    key: &str,
    stored: Option<String>,
    file: Option<&Found>,
) -> Result<(), String> {
    let entries = if skill { &mut manifest.skills } else { &mut manifest.documents };
    match (stored, file) {
        (Some(stored), Some(file)) => {
            let entry = Entry { hash: file.hash.clone(), size: file.size, mtime: file.mtime, remote: stored, synced_at: now(), registered: false };
            entries.insert(key.to_string(), entry);
        }
        _ => {
            entries.remove(key);
        }
    }
    manifest.save(root)
}

/// A document's upload not yet registered can't be found to delete: one
/// changed or removed meanwhile waits until the project lists it, or the old
/// upload would stay in the project with nothing tracking it.
fn settled(documents: Vec<(String, Change)>, known: &BTreeMap<String, Entry>) -> Vec<(String, Change)> {
    documents
        .into_iter()
        .filter(|(key, change)| *change == Change::Add || known.get(key).is_none_or(|e| e.registered))
        .collect()
}

fn plan(root: &Path, manifest: &Manifest) -> (Plan, BTreeMap<String, Found>, BTreeMap<String, Found>) {
    let documents = scan_documents(root, &manifest.documents);
    let skills = scan_skills(root);
    let plan = Plan {
        documents: settled(diff(&documents, &manifest.documents), &manifest.documents),
        skills: diff(&skills, &manifest.skills),
    };
    (plan, documents, skills)
}

#[derive(Default)]
struct Summary {
    /// Documents the project had already, now tracked without a push.
    adopted: usize,
    added: usize,
    updated: usize,
    removed: usize,
    failed: usize,
}

impl Summary {
    /// What changed, leaving out what didn't: `2 added, 1 removed`.
    fn line(&self) -> String {
        [
            (self.adopted, "already in the project"),
            (self.added, "added"),
            (self.updated, "updated"),
            (self.removed, "removed"),
            (self.failed, "failed"),
        ]
            .iter()
            .filter(|(n, _)| *n > 0)
            .map(|(n, what)| format!("{} {}", n, what))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// What a pass reports as it goes.
pub enum Event {
    /// A pass starts pushing this many changes.
    Working(usize),
    /// How a pass ended: `2 added, 1 removed`.
    Summary(String),
    /// A change that didn't go through, or a pass that couldn't run.
    Failure(String),
    /// Worth knowing, not a change: what a first sync left alone.
    Note(String),
}

/// Hold the sync back until `idle` says so, checking once a second; false
/// when it was stopped meanwhile.
fn wait_for(idle: &dyn Fn() -> bool, stop: &AtomicBool) -> bool {
    while !idle() && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(1));
    }
    !stop.load(Ordering::Relaxed)
}

/// One pass: bring the project at Smartloop in line with the folder. Each
/// batch waits for `idle`, so what it sets the agent indexing doesn't
/// compete with a reply being written, and none starts once stopped.
fn sync_once(
    remote: &Remote,
    root: &Path,
    manifest: &mut Manifest,
    idle: &dyn Fn() -> bool,
    stop: &AtomicBool,
    first: bool,
    emit: &mut dyn FnMut(Event),
) -> Result<Summary, String> {
    let listed = remote.documents()?;
    forget_missing(manifest, &listed, remote)?;
    let (mut plan, documents, skills) = plan(root, manifest);
    let mut summary = Summary { adopted: adopt(manifest, &listed, &mut plan.documents, &documents), ..Summary::default() };
    if first && let Some(note) = strays(manifest, &listed, &plan.documents) {
        emit(Event::Note(note));
    }
    if plan.is_empty() {
        manifest.save(root)?;
        if summary.adopted > 0 {
            emit(Event::Summary(summary.line()));
        }
        return Ok(summary);
    }

    emit(Event::Working(plan.skills.len() + plan.documents.len()));
    let work: Vec<(bool, &String, Change)> = plan
        .skills
        .iter()
        .map(|(key, change)| (true, key, *change))
        .chain(plan.documents.iter().map(|(key, change)| (false, key, *change)))
        .collect();
    let file = |skill: bool, key: &str| if skill { skills.get(key) } else { documents.get(key) };
    for batch in work.chunks(PARALLEL) {
        if !wait_for(idle, stop) {
            break;
        }
        // The batch goes out together; the manifest catches up one by one.
        let pushed: Vec<Result<Option<String>, String>> = std::thread::scope(|scope| {
            let handles: Vec<_> = batch
                .iter()
                .map(|&(skill, key, change)| {
                    let entries = if skill { &manifest.skills } else { &manifest.documents };
                    let old = entries.get(key).map(|e| e.remote.clone());
                    let file = file(skill, key);
                    scope.spawn(move || push(remote, root, skill, key, change, file, old.as_deref()))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err("Push panicked".to_string()))).collect()
        });
        for (&(skill, key, change), stored) in batch.iter().zip(pushed) {
            let label = if skill { format!("skill {}", key) } else { key.to_string() };
            match stored.and_then(|stored| record(root, manifest, skill, key, stored, file(skill, key))) {
                Ok(()) => {
                    match change {
                        Change::Add => summary.added += 1,
                        Change::Update => summary.updated += 1,
                        Change::Remove => summary.removed += 1,
                    }
                }
                Err(e) => {
                    emit(Event::Failure(format!("{}: {}", label, e)));
                    summary.failed += 1;
                }
            }
        }
    }
    // One stopped before doing anything has nothing to say.
    let line = summary.line();
    if !line.is_empty() {
        emit(Event::Summary(line));
    }
    Ok(summary)
}

/// Pass after pass, `interval` seconds apart, until `stop`. A failure is
/// reported when it first happens, not on every pass after. A pass starts
/// only once `idle`.
fn watch(
    remote: &Remote,
    root: &Path,
    manifest: &mut Manifest,
    interval: u64,
    idle: &dyn Fn() -> bool,
    stop: &AtomicBool,
    emit: &mut dyn FnMut(Event),
) {
    let mut reported: HashSet<String> = HashSet::new();
    // A map made afresh says what the project has beyond the folder, once.
    let mut first = manifest.documents.is_empty() && manifest.skills.is_empty();
    while wait_for(idle, stop) {
        let mut failures = HashSet::new();
        let mut relay = |event: Event| match event {
            Event::Failure(text) => {
                if reported.insert(text.clone()) {
                    emit(Event::Failure(text.clone()));
                }
                failures.insert(text);
            }
            other => emit(other),
        };
        match sync_once(remote, root, manifest, idle, stop, first, &mut relay) {
            Ok(_) => first = false,
            Err(e) => relay(Event::Failure(e)),
        }
        reported = failures;
        for _ in 0..interval.max(1) {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

/// Held by whatever writes a manifest from outside a pass, and by a sync for
/// as long as it runs, so a new one starts only once the old one is done
/// saving.
static SYNC: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    SYNC.lock().unwrap_or_else(|e| e.into_inner())
}

/// Why `root` may not be synced: the home folder, or one holding it, where
/// it would push everything a person owns.
pub fn refusal(root: &Path) -> Option<String> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .and_then(|h| fs::canonicalize(h).ok());
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let holds_home = root.parent().is_none() || home.is_some_and(|home| home.starts_with(&root));
    holds_home.then(|| format!("{} holds your whole home folder; open docent in a project folder to sync it", root.display()))
}

/// The folder `start` is part of: the nearest one holding a `.docent/`, as
/// git finds its repository, else `start` itself. The home folder and those
/// above it are never taken, since they're never synced.
pub fn root(start: &Path) -> PathBuf {
    start
        .ancestors()
        .take_while(|dir| refusal(dir).is_none())
        .find(|dir| dir.join(DIR).is_dir())
        .map_or_else(|| start.to_path_buf(), Path::to_path_buf)
}

/// The documents a first sync of `root` would push, without reading them.
#[derive(Debug, Default, PartialEq)]
pub struct Size {
    pub files: usize,
    pub bytes: u64,
}

impl Size {
    /// Too much to push without asking first.
    pub fn large(&self) -> bool {
        self.files > LARGE_FILES || self.bytes > LARGE_BYTES
    }
}

pub fn measure(root: &Path) -> Size {
    let mut size = Size::default();
    for entry in walk(root) {
        size.files += 1;
        size.bytes += entry.metadata().map_or(0, |m| m.len());
    }
    size
}

/// The documents under `root` only a sync makes searchable, by relative
/// path: the text ones the agent reads in place are left out.
pub fn unsynced(root: &Path) -> Vec<String> {
    walk(root)
        .filter(|entry| {
            let ext = entry.path().extension().and_then(|e| e.to_str()).unwrap_or_default();
            DOCUMENTS.contains(&ext.to_ascii_lowercase().as_str())
        })
        .map(|entry| relative_key(root, entry.path()))
        .collect()
}

/// The Smartloop project `root` syncs to, and whether its sync is paused.
pub struct Link {
    pub project: String,
    pub paused: bool,
}

pub fn linked(root: &Path) -> Option<Link> {
    let manifest = Manifest::load(root).ok()?;
    Some(Link { project: manifest.project?, paused: manifest.paused })
}

/// The folder `project` is linked to on this machine, paused or not; none
/// when it was never linked, or its folder has since been linked elsewhere,
/// unlinked or removed.
pub fn folder(project: &str) -> Option<PathBuf> {
    let root = Folders::load().0.remove(project)?;
    linked(&root).is_some_and(|l| l.project == project).then_some(root)
}

/// The folder a turn of `project` works in, from `here`: the project's own,
/// else the one `here` is part of unless that's linked to another project,
/// whose documents aren't this one's to index.
pub fn workspace(project: &str, here: &Path) -> Option<PathBuf> {
    folder(project).or_else(|| {
        let root = root(here);
        linked(&root).is_none_or(|l| l.project == project).then(|| here.to_path_buf())
    })
}

/// Each project's folder, by project id.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Folders(BTreeMap<String, PathBuf>);

impl Folders {
    fn path() -> PathBuf {
        // Each test runs on a thread of its own: keep theirs apart, and out
        // of the real install dir.
        if cfg!(test) {
            let name = format!("docent-folders-{}-{:?}.json", std::process::id(), std::thread::current().id());
            return std::env::temp_dir().join(name.replace(['(', ')'], ""));
        }
        crate::framework::install_dir().join(FOLDERS)
    }

    /// One unreadable starts afresh: the manifests still say what's linked.
    fn load() -> Self {
        fs::read_to_string(Self::path()).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
    }

    fn save(&self) -> Result<(), String> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("Failed to create {}: {}", dir.display(), e))?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, text + "\n").map_err(|e| format!("Failed to write {}: {}", tmp.display(), e))?;
        fs::rename(&tmp, &path).map_err(|e| format!("Failed to save {}: {}", path.display(), e))
    }
}

/// Record `root` as the folder `project` syncs from, pausing the one it
/// synced from before; held under the sync lock.
fn bind(root: &Path, project: &str) -> Result<(), String> {
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut folders = Folders::load();
    if folders.0.get(project) == Some(&root) {
        return Ok(());
    }
    // A folder syncs to one project: one linked to another before leaves it.
    folders.0.retain(|_, folder| *folder != root);
    if let Some(old) = folders.0.insert(project.to_string(), root)
        && let Ok(mut manifest) = Manifest::load(&old)
        && manifest.project.as_deref() == Some(project)
        && !manifest.paused
    {
        manifest.paused = true;
        manifest.save(&old)?;
    }
    folders.save()
}

/// Sync `root` to `project` from now on; one it synced elsewhere before
/// starts its map afresh, and the folder `project` synced from before stops.
pub fn link(root: &Path, project: &str) -> Result<(), String> {
    let _held = lock();
    let mut manifest = Manifest::load(root)?;
    if manifest.project.as_deref() != Some(project) {
        manifest.documents.clear();
        manifest.skills.clear();
        manifest.project = Some(project.to_string());
    }
    manifest.paused = false;
    manifest.save(root)?;
    bind(root, project)
}

/// Stop syncing `root` until it's linked again, waiting for a sync still
/// running to finish its save.
pub fn pause(root: &Path) -> Result<(), String> {
    let _held = lock();
    let mut manifest = Manifest::load(root)?;
    if manifest.project.is_none() || manifest.paused {
        return Ok(());
    }
    manifest.paused = true;
    manifest.save(root)
}

/// Where `root` stands against its last sync, from the folder alone.
pub struct Status {
    pub documents: usize,
    pub skills: usize,
    /// What the next pass would push: `+ notes.md`.
    pub changes: Vec<String>,
}

pub fn status(root: &Path) -> Result<Status, String> {
    let manifest = Manifest::load(root)?;
    let (plan, _, _) = plan(root, &manifest);
    let changes = plan
        .skills
        .iter()
        .map(|(key, change)| format!("{} skill {}", change.symbol(), key))
        .chain(plan.documents.iter().map(|(key, change)| format!("{} {}", change.symbol(), key)))
        .collect();
    Ok(Status { documents: manifest.documents.len(), skills: manifest.skills.len(), changes })
}

/// Keep `root` in step with the project it's linked to until `stop`; for
/// the chat, which reports through `emit` and holds changes back while
/// `idle` says it's busy. A project that's gone is never made again: the
/// folder has to be linked to another.
pub fn follow(
    client: &Client,
    root: &Path,
    interval: u64,
    idle: &dyn Fn() -> bool,
    stop: &AtomicBool,
    emit: &mut dyn FnMut(Event),
) {
    let _held = lock();
    if stop.load(Ordering::Relaxed) {
        return;
    }
    let mut manifest = match Manifest::load(root) {
        Ok(manifest) => manifest,
        Err(e) => return emit(Event::Failure(e)),
    };
    let Some(project) = manifest.project.clone() else {
        return emit(Event::Failure("This folder isn't linked; /sync links it".to_string()));
    };
    match try_fetch_projects(client) {
        Ok(projects) if projects.iter().any(|p| p["id"].as_str() == Some(project.as_str())) => {}
        Ok(_) => {
            return emit(Event::Failure(
                "The project this folder synced to is gone; /sync links it to the current one".to_string(),
            ));
        }
        Err(e) => return emit(Event::Failure(e)),
    }
    // Linked before folders were recorded: record it now.
    if folder(&project).is_none()
        && let Err(e) = bind(root, &project)
    {
        emit(Event::Failure(e));
    }
    watch(&Remote { client, project }, root, &mut manifest, interval, idle, stop, emit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(hash: &str) -> Entry {
        Entry { hash: hash.into(), size: 1, mtime: 1, remote: "r".into(), synced_at: 0, registered: false }
    }

    fn file(hash: &str) -> Found {
        Found { path: PathBuf::from("x"), hash: hash.into(), size: 1, mtime: 1 }
    }

    #[test]
    fn diff_finds_added_changed_and_removed() {
        let known = BTreeMap::from([("same".to_string(), entry("a")), ("edit".to_string(), entry("a")), ("gone".to_string(), entry("a"))]);
        let found = BTreeMap::from([
            ("same".to_string(), file("a")),
            ("edit".to_string(), file("b")),
            ("new".to_string(), file("a")),
        ]);
        let mut changes = diff(&found, &known);
        changes.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            changes,
            vec![
                ("edit".to_string(), Change::Update),
                ("gone".to_string(), Change::Remove),
                ("new".to_string(), Change::Add),
            ]
        );
    }

    #[test]
    fn documents_not_yet_registered_are_never_pushed_again() {
        let mut documents = BTreeMap::from([("a.md".to_string(), entry("h"))]);
        // However long the agent takes to list it, the upload stands.
        reconcile(&mut documents, &HashSet::new());
        assert!(documents.get("a.md").is_some_and(|e| !e.registered));
        reconcile(&mut documents, &HashSet::from(["r".to_string()]));
        assert!(documents.get("a.md").is_some_and(|e| e.registered));
        // Listed once and then gone: deleted at Smartloop, so it goes again.
        reconcile(&mut documents, &HashSet::new());
        assert!(documents.is_empty());
    }

    #[test]
    fn documents_not_yet_registered_wait_to_be_removed_or_replaced() {
        let registered = Entry { registered: true, ..entry("a") };
        let known = BTreeMap::from([
            ("gone".to_string(), entry("a")),
            ("edit".to_string(), entry("a")),
            ("listed".to_string(), registered.clone()),
            ("relisted".to_string(), registered),
        ]);
        let found = BTreeMap::from([("edit".to_string(), file("b")), ("relisted".to_string(), file("b")), ("new".to_string(), file("a"))]);
        let mut changes = settled(diff(&found, &known), &known);
        changes.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            changes,
            vec![
                ("listed".to_string(), Change::Remove),
                ("new".to_string(), Change::Add),
                ("relisted".to_string(), Change::Update),
            ]
        );
    }

    #[test]
    fn documents_the_project_has_by_content_are_taken_up_not_pushed() {
        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| {
            let path = dir.path().join(name);
            fs::write(&path, format!("content of {}", if name.starts_with("copy") { "same" } else { name })).unwrap();
            path
        };
        let found = BTreeMap::from([
            ("same.md".to_string(), Found { path: path("copy-a.md"), ..file("a") }),
            ("again.md".to_string(), Found { path: path("copy-b.md"), ..file("a") }),
            ("new.md".to_string(), Found { path: path("new.md"), ..file("b") }),
        ]);
        let id = document_id(&found["same.md"].path).unwrap();
        assert_eq!(id, format!("{:x}", Md5::digest(b"content of same")));
        let listed = vec![
            json!({"id": id, "path": "documents/README.md"}),
            json!({"id": "other", "path": "documents/elsewhere.pdf"}),
            json!({"id": null, "path": "/inbox/documents/README_1.md"}),
        ];
        let mut manifest = Manifest::default();
        let mut changes: Vec<(String, Change)> = found.keys().map(|k| (k.clone(), Change::Add)).collect();
        assert_eq!(adopt(&mut manifest, &listed, &mut changes, &found), 1);
        // Two copies here, one there: the second goes in as usual.
        let taken = manifest.documents.values().next().unwrap();
        assert!(taken.registered && taken.remote == "README.md" && manifest.documents.len() == 1);
        assert_eq!(changes.len(), 2);
        assert_eq!(
            strays(&manifest, &listed, &changes).as_deref(),
            Some("1 document in the project isn't in this folder (elsewhere.pdf); sync leaves it be")
        );
    }

    #[test]
    fn reads_skill_frontmatter() {
        let content = "---\nname: \"Summarize\"\ndescription: Short summaries\n---\nBody: text";
        assert_eq!(frontmatter(content, "name").as_deref(), Some("Summarize"));
        assert_eq!(frontmatter(content, "description").as_deref(), Some("Short summaries"));
        assert_eq!(frontmatter(content, "body"), None);
        assert_eq!(frontmatter("no frontmatter\nname: x", "name"), None);
    }

    #[test]
    fn names_skills_from_frontmatter_or_path() {
        assert_eq!(skill_name("skills/recap.md", "text"), "recap");
        assert_eq!(skill_name("skills/recap/SKILL.md", "text"), "recap");
        assert_eq!(skill_name("skills/recap.md", "---\nname: weekly\n---\n"), "weekly");
    }

    #[test]
    fn home_and_root_are_never_synced() {
        let dir = tempfile::tempdir().unwrap();
        assert!(refusal(dir.path()).is_none());
        assert!(refusal(Path::new("/")).is_some());
        if let Some(home) = std::env::var_os("HOME") {
            assert!(refusal(Path::new(&home)).is_some());
        }
    }

    #[test]
    fn finds_the_root_above() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b");
        fs::create_dir_all(&nested).unwrap();
        // Nothing linked yet: the folder itself.
        assert_eq!(root(&nested), nested);
        fs::create_dir(dir.path().join(DIR)).unwrap();
        assert_eq!(root(&nested), dir.path());
        assert_eq!(root(dir.path()), dir.path());
        // The nearest one wins.
        fs::create_dir(nested.join(DIR)).unwrap();
        assert_eq!(root(&nested), nested);
    }

    #[test]
    fn measures_only_what_would_be_pushed() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.md"), "12345").unwrap();
        fs::write(dir.path().join("b.rs"), "skipped").unwrap();
        assert_eq!(measure(dir.path()), Size { files: 1, bytes: 5 });
        assert!(!measure(dir.path()).large());
        assert!(Size { files: LARGE_FILES + 1, bytes: 0 }.large());
    }

    #[test]
    fn unsynced_lists_only_documents_that_need_indexing() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("docs")).unwrap();
        fs::write(dir.path().join("README.md"), "read in place").unwrap();
        fs::write(dir.path().join("docs/Report.PDF"), "pdf").unwrap();
        fs::write(dir.path().join("plan.docx"), "docx").unwrap();
        let mut found = unsynced(dir.path());
        found.sort();
        assert_eq!(found, ["docs/Report.PDF", "plan.docx"]);
    }

    #[test]
    fn linking_and_pausing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(linked(root).is_none());
        // Pausing a folder never linked leaves nothing behind.
        pause(root).unwrap();
        assert!(!root.join(DIR).exists());

        link(root, "p1").unwrap();
        let mut manifest = Manifest::load(root).unwrap();
        manifest.documents.insert("a.md".into(), entry("h"));
        manifest.save(root).unwrap();
        pause(root).unwrap();
        assert!(linked(root).is_some_and(|l| l.project == "p1" && l.paused));
        // Linked again to the same project, the map is kept.
        link(root, "p1").unwrap();
        assert!(linked(root).is_some_and(|l| !l.paused));
        assert_eq!(status(root).unwrap().documents, 1);
        // To another, it starts afresh.
        link(root, "p2").unwrap();
        assert_eq!(status(root).unwrap().documents, 0);
    }

    #[test]
    fn a_project_syncs_from_the_one_folder_last_linked() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, b) = (fs::canonicalize(a.path()).unwrap(), fs::canonicalize(b.path()).unwrap());
        assert!(folder("p1").is_none());
        link(&a, "p1").unwrap();
        assert_eq!(folder("p1"), Some(a.clone()));
        // Paused, it's still the project's folder.
        pause(&a).unwrap();
        assert_eq!(folder("p1"), Some(a.clone()));

        // Linked to another folder, the first one stops.
        link(&a, "p1").unwrap();
        link(&b, "p1").unwrap();
        assert_eq!(folder("p1"), Some(b.clone()));
        assert!(linked(&a).is_some_and(|l| l.project == "p1" && l.paused));

        // A folder linked to another project leaves this one without.
        link(&b, "p2").unwrap();
        assert!(folder("p1").is_none());
        assert_eq!(folder("p2"), Some(b.clone()));
        // As does one removed.
        drop(fs::remove_dir_all(&b));
        assert!(folder("p2").is_none());
        let _ = fs::remove_file(Folders::path());
    }

    #[test]
    fn a_turn_never_works_in_a_folder_linked_to_another_project() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, b) = (fs::canonicalize(a.path()).unwrap(), fs::canonicalize(b.path()).unwrap());
        let inside = a.join("docs");
        fs::create_dir_all(&inside).unwrap();
        // Unlinked, the working directory is the turn's.
        assert_eq!(workspace("p1", &inside), Some(inside.clone()));
        link(&a, "p1").unwrap();
        assert_eq!(workspace("p1", &inside), Some(a.clone()));
        // Another project, without a folder of its own, gets none here.
        assert!(workspace("p2", &inside).is_none());
        // With one, it works there.
        link(&b, "p2").unwrap();
        assert_eq!(workspace("p2", &inside), Some(b.clone()));
        let _ = fs::remove_file(Folders::path());
    }

    #[test]
    fn summary_names_only_what_changed() {
        assert_eq!(Summary { removed: 1, ..Summary::default() }.line(), "1 removed");
        assert_eq!(Summary { added: 2, updated: 1, failed: 1, ..Summary::default() }.line(), "2 added, 1 updated, 1 failed");
    }

    #[test]
    fn flattens_document_paths() {
        assert_eq!(remote_name("docs/guide.pdf"), "docs__guide.pdf");
        assert_eq!(remote_name("notes.md"), "notes.md");
    }

    #[test]
    fn scans_documents_and_skills() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for (path, body) in [
            ("README.md", "readme"),
            ("docs/guide.TXT", "guide"),
            ("src/main.rs", "fn main() {}"),
            ("skipped/notes.md", "ignored"),
            ("ignored.md", "ignored"),
            (".hidden/secret.md", "hidden"),
            (".docent/skills/recap.md", "---\nname: recap\n---\n"),
            (".docent/skills/weekly/SKILL.md", "weekly"),
            (".docent/skills/readme.txt", "not a skill"),
        ] {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }
        fs::write(root.join(IGNORE_FILE), "skipped/\nignored.md\n").unwrap();

        let documents = scan_documents(root, &BTreeMap::new());
        assert_eq!(documents.keys().collect::<Vec<_>>(), ["README.md", "docs/guide.TXT"]);
        let skills = scan_skills(root);
        assert_eq!(skills.keys().collect::<Vec<_>>(), ["skills/recap.md", "skills/weekly/SKILL.md"]);
    }

    #[test]
    fn unchanged_files_keep_their_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        fs::write(&path, "one").unwrap();
        let first = found(&path, None).unwrap();
        let mut known = entry("recorded");
        known.size = first.size;
        known.mtime = first.mtime;
        assert_eq!(found(&path, Some(&known)).unwrap().hash, "recorded");
        known.size += 1;
        assert_eq!(found(&path, Some(&known)).unwrap().hash, first.hash);
    }

    #[test]
    fn manifest_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::default();
        manifest.project = Some("p1".into());
        manifest.documents.insert("a.md".into(), entry("h"));
        manifest.save(dir.path()).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join(DIR).join(".gitignore")).unwrap(), "manifest.json\n");
        let loaded = Manifest::load(dir.path()).unwrap();
        assert_eq!(loaded.project.as_deref(), Some("p1"));
        assert_eq!(loaded.documents["a.md"], entry("h"));
    }
}
