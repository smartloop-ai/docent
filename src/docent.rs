//! The project's `.docent/` folder and the indexer that keeps Smartloop in
//! step with it.
//!
//! `.docent/manifest.json` maps every document and skill pushed to a Smartloop
//! project: what it was, its hash, and the id or name it has there. `docent
//! sync` scans the project, diffs the scan against that map and pushes only
//! what was added, changed or removed, so the Smartloop project follows the
//! repo. `--watch` repeats the pass until stopped.
//!
//! Documents are the files under the project root with a supported extension
//! (hidden folders, `.gitignore` and `.docentignore` are honoured). Skills are
//! `.docent/skills/<name>.md` or `.docent/skills/<name>/SKILL.md`; they are
//! the part of `.docent/` meant to be committed, the manifest is local.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::blocking::{Client, multipart};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{error_message, projects_url, request_json, try_create_project, try_fetch_projects};

pub const DIR: &str = ".docent";
const MANIFEST: &str = "manifest.json";
const SKILLS: &str = "skills";
const IGNORE_FILE: &str = ".docentignore";
/// What the agent can turn into searchable documents.
const EXTENSIONS: [&str; 7] = ["pdf", "docx", "pptx", "xlsx", "csv", "md", "txt"];
/// A document the agent hasn't listed this long after its upload is gone.
const REGISTER_GRACE: u64 = 120;
/// How many changes a pass pushes at once.
const PARALLEL: usize = 4;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Entry {
    hash: String,
    size: u64,
    mtime: u64,
    /// The document's name in the project's inbox, or the skill's id.
    remote: String,
    synced_at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
    #[serde(default)]
    project: Option<String>,
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
    let mut hasher = Sha256::new();
    std::io::copy(&mut fs::File::open(path)?, &mut hasher)?;
    Ok(hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect())
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

/// The project's documents, by relative path.
fn scan_documents(root: &Path, known: &BTreeMap<String, Entry>) -> BTreeMap<String, Found> {
    let walk = ignore::WalkBuilder::new(root)
        .add_custom_ignore_filename(IGNORE_FILE)
        .require_git(false)
        .build();
    let mut files = BTreeMap::new();
    for entry in walk.flatten() {
        let path = entry.path();
        let supported = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
        if !supported || !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let key = relative_key(root, path);
        if let Ok(file) = found(path, known.get(&key)) {
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
    /// has counts as deleted.
    fn delete_document(&self, name: &str) -> Result<(), String> {
        let stored = |doc: &Value| {
            let file = doc["path"].as_str().and_then(|p| Path::new(p).file_name()?.to_str().map(str::to_string));
            file.or_else(|| doc["filename"].as_str().map(str::to_string))
        };
        let documents = self.documents()?;
        let Some(id) = documents.iter().find(|d| stored(d).as_deref() == Some(name)).and_then(|d| d["id"].as_str())
        else {
            return Ok(());
        };
        let action = format!("delete {}", name);
        let response = self
            .client
            .delete(self.url(&format!("documents/{}", id)))
            .send()
            .map_err(|e| format!("Failed to {}: {}", action, e))?;
        if response.status().is_success() || response.status() == 404 {
            Ok(())
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

/// Entries the project no longer has are dropped from the manifest, so the
/// next diff adds them again. A document gets `REGISTER_GRACE` seconds after
/// its upload to show up, as the agent registers it only when it next scans.
fn forget_missing(manifest: &mut Manifest, remote: &Remote, now: u64) -> Result<(), String> {
    let stored: HashSet<String> = remote
        .documents()?
        .iter()
        .filter_map(|d| d["path"].as_str().and_then(|p| Path::new(p).file_name()?.to_str().map(str::to_string)))
        .collect();
    manifest
        .documents
        .retain(|_, e| stored.contains(&e.remote) || now.saturating_sub(e.synced_at) < REGISTER_GRACE);

    let ids: HashSet<String> =
        remote.skills()?.iter().filter_map(|s| s["id"].as_str().map(str::to_string)).collect();
    manifest.skills.retain(|_, e| ids.contains(&e.remote));
    Ok(())
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
            remote.delete_document(old.unwrap_or_default())?;
            None
        }
        (true, Change::Remove) => {
            remote.delete_skill(old.unwrap_or_default(), key)?;
            None
        }
        (false, _) => {
            if let Some(name) = old {
                // A changed document goes in again under the same name.
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
            let entry = Entry { hash: file.hash.clone(), size: file.size, mtime: file.mtime, remote: stored, synced_at: now() };
            entries.insert(key.to_string(), entry);
        }
        _ => {
            entries.remove(key);
        }
    }
    manifest.save(root)
}

fn plan(root: &Path, manifest: &Manifest) -> (Plan, BTreeMap<String, Found>, BTreeMap<String, Found>) {
    let documents = scan_documents(root, &manifest.documents);
    let skills = scan_skills(root);
    let plan = Plan {
        documents: diff(&documents, &manifest.documents),
        skills: diff(&skills, &manifest.skills),
    };
    (plan, documents, skills)
}

/// Pick the Smartloop project to sync to: the one asked for, else the one
/// the manifest names, else one named after the folder, created when needed.
/// Pointing at a different project than before starts the map afresh.
fn resolve_project(
    client: &Client,
    root: &Path,
    manifest: &mut Manifest,
    requested: Option<&str>,
) -> Result<String, String> {
    let projects = try_fetch_projects(client)?;
    let field = |p: &Value, key: &str| p[key].as_str().unwrap_or_default().to_string();
    let find = |wanted: &str| {
        projects
            .iter()
            .find(|p| field(p, "id") == wanted || field(p, "name").eq_ignore_ascii_case(wanted))
            .map(|p| field(p, "id"))
    };

    let folder = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "docent".into());
    let id = match requested {
        Some(wanted) => find(wanted).ok_or_else(|| format!("No project named {}", wanted))?,
        None => match manifest.project.as_deref().and_then(find).or_else(|| find(&folder)) {
            Some(id) => id,
            None => field(&try_create_project(client, &folder, None)?, "id"),
        },
    };

    if manifest.project.as_deref() != Some(id.as_str()) {
        manifest.documents.clear();
        manifest.skills.clear();
        manifest.project = Some(id.clone());
        manifest.save(root)?;
    }
    Ok(id)
}

#[derive(Default)]
struct Summary {
    added: usize,
    updated: usize,
    removed: usize,
    failed: usize,
}

impl Summary {
    fn line(&self) -> String {
        let mut parts = vec![
            format!("{} added", self.added),
            format!("{} updated", self.updated),
            format!("{} removed", self.removed),
        ];
        if self.failed > 0 {
            parts.push(format!("{} failed", self.failed));
        }
        parts.join(", ")
    }
}

/// What a pass reports as it goes.
pub enum Event {
    /// A pass starts pushing this many changes.
    Working(usize),
    /// One change pushed: `+ README.md`.
    Change(String),
    /// How a pass ended: `2 added, 0 updated, 0 removed`.
    Summary(String),
    /// A change that didn't go through, or a pass that couldn't run.
    Failure(String),
}

/// Hold the sync back until `idle` says so, checking once a second.
fn wait_for(idle: &dyn Fn() -> bool) {
    while !idle() {
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// One pass: bring the project at Smartloop in line with the folder. A pass
/// with nothing to do reports nothing when `quiet_when_idle`. Each change
/// waits for `idle`, so what it sets the agent indexing doesn't compete with
/// a reply being written.
fn sync_once(
    remote: &Remote,
    root: &Path,
    manifest: &mut Manifest,
    quiet_when_idle: bool,
    idle: &dyn Fn() -> bool,
    emit: &mut dyn FnMut(Event),
) -> Result<Summary, String> {
    forget_missing(manifest, remote, now())?;
    let (plan, documents, skills) = plan(root, manifest);
    let mut summary = Summary::default();
    if plan.is_empty() {
        if !quiet_when_idle {
            emit(Event::Summary("Up to date".to_string()));
        }
        manifest.save(root)?;
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
        wait_for(idle);
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
                    emit(Event::Change(format!("{} {}", change.symbol(), label)));
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
    emit(Event::Summary(summary.line()));
    Ok(summary)
}

/// Pass after pass, `interval` seconds apart, until the process ends. A
/// failure is reported when it first happens, not on every pass after. A
/// pass starts only once `idle`.
fn watch(
    remote: &Remote,
    root: &Path,
    manifest: &mut Manifest,
    interval: u64,
    quiet_first: bool,
    idle: &dyn Fn() -> bool,
    emit: &mut dyn FnMut(Event),
) -> ! {
    let mut reported: HashSet<String> = HashSet::new();
    let mut quiet = quiet_first;
    loop {
        wait_for(idle);
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
        if let Err(e) = sync_once(remote, root, manifest, quiet, idle, &mut relay) {
            relay(Event::Failure(e));
        }
        reported = failures;
        quiet = true;
        std::thread::sleep(Duration::from_secs(interval.max(1)));
    }
}

/// True when the chat may sync `root` on its own: any folder opened as a
/// project, but never the home folder or one holding it, where it would
/// push everything a person owns.
pub fn enabled(root: &Path) -> bool {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .and_then(|h| fs::canonicalize(h).ok());
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    root.parent().is_some() && !home.is_some_and(|home| home.starts_with(&root))
}

/// Make sure `root` has its project at Smartloop, creating it when missing,
/// and record it in the manifest, so the chat can open in it.
pub fn link(client: &Client, root: &Path) -> Result<(), String> {
    let mut manifest = Manifest::load(root)?;
    resolve_project(client, root, &mut manifest, None).map(|_| ())
}

/// The Smartloop project `root` last synced to.
pub fn linked_project(root: &Path) -> Option<String> {
    Manifest::load(root).ok()?.project
}

/// Keep `root` in step with its Smartloop project for as long as the process
/// lives; for the chat, which reports through `emit` rather than printing
/// and holds changes back while `idle` says it's busy.
pub fn follow(client: &Client, root: &Path, interval: u64, idle: &dyn Fn() -> bool, emit: &mut dyn FnMut(Event)) {
    let mut manifest = match Manifest::load(root) {
        Ok(manifest) => manifest,
        Err(e) => return emit(Event::Failure(e)),
    };
    let project = match resolve_project(client, root, &mut manifest, None) {
        Ok(project) => project,
        Err(e) => return emit(Event::Failure(e)),
    };
    watch(&Remote { client, project }, root, &mut manifest, interval, true, idle, emit)
}

/// What `docent sync` was asked to do.
pub struct Options {
    pub path: PathBuf,
    pub project: Option<String>,
    pub dry_run: bool,
    pub watch: bool,
    pub interval: u64,
}

fn print(event: Event) {
    match event {
        Event::Working(_) => {}
        Event::Change(text) | Event::Summary(text) => println!("{}", text),
        Event::Failure(text) => eprintln!("! {}", text),
    }
    let _ = std::io::stdout().flush();
}

/// `docent sync`. Returns whether every change went through.
pub fn run(client: &Client, options: Options) -> Result<bool, String> {
    let root = fs::canonicalize(&options.path)
        .map_err(|e| format!("Cannot open {}: {}", options.path.display(), e))?;
    let mut manifest = Manifest::load(&root)?;

    if options.dry_run {
        let (plan, _, _) = plan(&root, &manifest);
        if plan.is_empty() {
            println!("Up to date with the last sync (Smartloop was not asked)");
        }
        for (key, change) in &plan.skills {
            println!("{} skill {}", change.symbol(), key);
        }
        for (key, change) in &plan.documents {
            println!("{} {}", change.symbol(), key);
        }
        return Ok(true);
    }

    let project = resolve_project(client, &root, &mut manifest, options.project.as_deref())?;
    let remote = Remote { client, project };
    if options.watch {
        println!("Watching {} (every {}s, Ctrl-C to stop)", root.display(), options.interval);
        watch(&remote, &root, &mut manifest, options.interval, false, &|| true, &mut print);
    }
    Ok(sync_once(&remote, &root, &mut manifest, false, &|| true, &mut print)?.failed == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(hash: &str) -> Entry {
        Entry { hash: hash.into(), size: 1, mtime: 1, remote: "r".into(), synced_at: 0 }
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
    fn home_and_root_are_not_synced_on_their_own() {
        let dir = tempfile::tempdir().unwrap();
        assert!(enabled(dir.path()));
        assert!(!enabled(Path::new("/")));
        if let Some(home) = std::env::var_os("HOME") {
            assert!(!enabled(Path::new(&home)));
        }
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
