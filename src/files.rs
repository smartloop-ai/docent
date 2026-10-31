//! Files a reply hands over, written into the working folder.
//!
//! The agent answers in text: asked to make a project, it shows the files
//! rather than writing them. A prompt that asks for something to be made
//! carries `HINT`, so the reply gives each file whole, in a fenced block
//! with its path on the line above; `extract` finds those blocks, and the
//! TUI writes them, asking before it replaces a file that's already there.

use std::fs;
use std::path::{Component, Path, PathBuf};

/// Told to the agent with a prompt that asks for something to be made.
pub const HINT: &str = "If your answer creates or changes files, start with a short summary of what you \
changed and why, then give each file's complete contents in its own \
fenced code block, with the file's path relative to the working folder alone on the line just above the block, \
like `src/main.rs`. Include every file the change needs, such as the manifest when the code uses a new \
dependency. The files are written for me from those blocks, so don't tell me to create them by hand or \
with a command.";

/// Files at a folder's top that say it's a code project, and its kind.
const MARKERS: [(&str, &str); 14] = [
    ("Cargo.toml", "Rust"),
    ("package.json", "JavaScript/TypeScript"),
    ("pyproject.toml", "Python"),
    ("requirements.txt", "Python"),
    ("setup.py", "Python"),
    ("go.mod", "Go"),
    ("pom.xml", "Java"),
    ("build.gradle", "Java/Kotlin"),
    ("build.gradle.kts", "Kotlin"),
    ("Gemfile", "Ruby"),
    ("composer.json", "PHP"),
    ("Package.swift", "Swift"),
    ("CMakeLists.txt", "C/C++"),
    ("mix.exs", "Elixir"),
];

/// Folders of build output and dependencies, never the project's own code.
const SKIPPED: [&str; 10] =
    ["target", "node_modules", "dist", "build", ".venv", "venv", "__pycache__", "vendor", ".out", ".docent"];

/// Generated files the agent needn't read.
const LOCKS: [&str; 7] =
    ["Cargo.lock", "package-lock.json", "yarn.lock", "pnpm-lock.yaml", "poetry.lock", "go.sum", "Gemfile.lock"];

/// Most of the project's files listed for the agent.
const LISTED: usize = 200;
/// Most characters of file contents sent with a prompt, and of one file.
const SHOWN: usize = 60_000;
const SHOWN_EACH: usize = 30_000;

/// The kind of code project `root` is, from the files at its top.
pub fn project_kind(root: &Path) -> Option<&'static str> {
    MARKERS.iter().find(|(marker, _)| root.join(marker).is_file()).map(|(_, kind)| *kind)
}

/// Source files, by extension: where a change most often goes, so shown
/// before configuration and docs.
const SOURCE: [&str; 26] = [
    "rs", "js", "jsx", "ts", "tsx", "mjs", "cjs", "py", "go", "java", "kt", "kts", "rb", "php", "swift", "c", "cc",
    "cpp", "h", "hpp", "ex", "exs", "vue", "svelte", "cs", "scala",
];

/// Words of a prompt too common to pick out a file's lines.
const COMMON: [&str; 28] = [
    "this", "that", "with", "from", "what", "when", "then", "into", "file", "files", "code", "project", "make",
    "create", "update", "change", "please", "should", "would", "also", "have", "there", "where", "which", "their",
    "about", "like", "just",
];
/// Least room left for one more file's contents to be worth showing.
const EXCERPT_MIN: usize = 1_500;
/// Lines shown around one a prompt's word is on, in a file too big to show whole.
const AROUND: usize = 6;

/// What the agent is told of a code project with a prompt that makes files.
pub struct Context {
    pub text: String,
    /// The files shown whole, which a reply can give back whole.
    pub whole: Vec<String>,
}

/// What the agent is told of the code project at `root` with `prompt`, a
/// request that makes files: the kind of project, its files, and the
/// contents of those that matter most to the request, as far as they fit,
/// so it answers about this project with code that fits what's there,
/// rather than in general. Files the prompt names come first, then the
/// source, then the rest; one too big to show whole shows the parts the
/// prompt's words are in, and says so. `kind` is what the project is,
/// as in "Rust".
pub fn project_context(root: &Path, kind: &str, prompt: &str) -> Context {
    let mut paths: Vec<String> = ignore::WalkBuilder::new(root)
        .require_git(false)
        .filter_entry(|e| e.depth() == 0 || !SKIPPED.contains(&e.file_name().to_string_lossy().as_ref()))
        .build()
        .flatten()
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .filter_map(|e| e.path().strip_prefix(root).ok().map(|p| p.to_string_lossy().replace('\\', "/")))
        .collect();
    let is_manifest = |p: &str| MARKERS.iter().any(|(m, _)| *m == p);
    // The manifests first, then by path.
    paths.sort_by_key(|p| (!is_manifest(p), p.clone()));
    let mut text = format!(
        "I'm working in the {} project at {}. This is about that project, not a general question: answer from \
         its files below and what's in the folder, not from the web or general knowledge. When I ask for code \
         or a change, answer with the code itself, not a description of it: change the project's files where \
         the request fits them, keeping what they already do unless I ask otherwise.\n\n\
         Its files:\n",
        kind,
        root.display()
    );
    for path in paths.iter().take(LISTED) {
        text.push_str(&format!("- {}\n", path));
    }
    if paths.len() > LISTED {
        text.push_str(&format!("- …and {} more\n", paths.len() - LISTED));
    }
    // Contents, the most relevant first: the manifests, the files the
    // prompt names, the source, then the rest, each group by path.
    let lower = prompt.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| w.len() >= 4 && !COMMON.contains(w))
        .collect();
    let mentioned = |p: &str| {
        let name = p.rsplit('/').next().unwrap_or(p).to_lowercase();
        let stem = name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s);
        lower.contains(&name) || words.iter().any(|w| *w == stem)
    };
    let is_source = |p: &str| p.rsplit_once('.').is_some_and(|(_, ext)| SOURCE.contains(&ext));
    let mut order: Vec<&String> =
        paths.iter().filter(|p| !LOCKS.contains(&p.rsplit('/').next().unwrap_or(p))).collect();
    order.sort_by_key(|p| (!is_manifest(p), !mentioned(p), !is_source(p), (*p).clone()));
    let mut room = SHOWN;
    let mut shown = String::new();
    let mut whole = Vec::new();
    for path in order {
        if room < EXCERPT_MIN {
            break;
        }
        let Ok(content) = fs::read_to_string(root.join(path)) else { continue };
        if content.contains('\0') {
            continue;
        }
        let limit = SHOWN_EACH.min(room);
        let (body, note) = if content.len() <= limit {
            whole.push(path.clone());
            (content.trim_end().to_string(), String::new())
        } else {
            let (part, lines) = excerpt(&content, &words, limit);
            let what = if lines == 0 { "its start".to_string() } else { format!("{} of its {} lines", lines, content.lines().count()) };
            (part, format!(" ({}, for reference: don't give this file back whole)", what))
        };
        room = room.saturating_sub(body.len());
        let fence = if body.contains("```") { "````" } else { "```" };
        let lang = path.rsplit_once('.').map_or("", |(_, ext)| ext);
        shown.push_str(&format!("\n`{}`{}\n{}{}\n{}\n{}\n", path, note, fence, lang, body, fence));
    }
    if !shown.is_empty() {
        text.push_str("\nTheir contents now:\n");
        text.push_str(&shown);
    }
    Context { text, whole }
}

/// Up to `limit` bytes of `content`, a file too big to show whole: the
/// lines around those that have one of `words`, runs apart marked `…`,
/// and the number of lines that is; with no such line, its start.
fn excerpt(content: &str, words: &[&str], limit: usize) -> (String, usize) {
    let lines: Vec<&str> = content.lines().collect();
    let mut keep = vec![false; lines.len()];
    for (i, line) in lines.iter().enumerate() {
        let low = line.to_lowercase();
        if words.iter().any(|w| low.contains(w)) {
            for k in i.saturating_sub(AROUND)..(i + AROUND + 1).min(lines.len()) {
                keep[k] = true;
            }
        }
    }
    if !keep.iter().any(|k| *k) {
        return (cut(content, limit).trim_end().to_string(), 0);
    }
    let (mut text, mut count, mut last) = (String::new(), 0, None);
    for (i, line) in lines.iter().enumerate().filter(|(i, _)| keep[*i]) {
        if text.len() + line.len() + 2 > limit {
            break;
        }
        if last.is_some_and(|l| l + 1 != i) {
            text.push_str("…\n");
        }
        text.push_str(line);
        text.push('\n');
        count += 1;
        last = Some(i);
    }
    (text.trim_end().to_string(), count)
}

/// The first `limit` bytes of `text`, on a character.
fn cut(text: &str, limit: usize) -> &str {
    let mut end = limit.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Words that ask for something to be made, as the prompt's first concern.
const MAKING: [&str; 13] = [
    "create", "generate", "scaffold", "build", "make", "write", "add", "implement", "bootstrap", "init",
    "setup", "update", "fix",
];

/// Names without an extension that are still files a project keeps.
const BARE_NAMES: [&str; 8] =
    ["Makefile", "Dockerfile", "Justfile", "Procfile", "Gemfile", "Rakefile", "LICENSE", "Containerfile"];

/// Whether `message` asks for something to be made, so the reply should
/// come as files.
pub fn wants_files(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("set up")
        || lower.split(|c: char| !c.is_alphanumeric()).any(|word| MAKING.contains(&word))
}

/// Whether `message` names a file that's under `root`, as a pasted build
/// error or stack trace does (`src/app.js:2:7`, or the path in full): a
/// prompt about the project's files, to be answered with files.
pub fn mentions_file(root: &Path, message: &str) -> bool {
    let root_text = root.to_string_lossy().replace('\\', "/");
    message
        .split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '<' | '>' | ','))
        .filter_map(|token| {
            // `path:12:7`, and the folder itself ahead of the path.
            let mut token = token.replace('\\', "/");
            while let Some((head, tail)) = token.rsplit_once(':')
                && !tail.is_empty()
                && tail.chars().all(|c| c.is_ascii_digit())
            {
                token = head.to_string();
            }
            let token = token.strip_prefix(&root_text).map_or(token.as_str(), |rest| rest.trim_start_matches('/'));
            as_path(token)
        })
        .any(|path| path.contains('/') && root.join(path).is_file())
}

/// One file from a reply: its path as given, relative, and its contents.
#[derive(Debug, Clone, PartialEq)]
pub struct File {
    pub path: String,
    pub content: String,
}

/// A file in a reply, and the line of the reply its code starts on: the
/// opening fence, or the first indented line.
pub struct Block {
    pub file: File,
    pub fence: usize,
}

/// The files in `reply`: each fenced block whose path is on the line above
/// it, in its info string (```` ```rust src/main.rs ````), or in a comment
/// on its first line. A path given twice keeps its last contents.
pub fn extract(reply: &str) -> Vec<File> {
    let mut files: Vec<File> = Vec::new();
    for block in blocks(reply) {
        files.retain(|f| f.path != block.file.path);
        files.push(block.file);
    }
    files
}

/// Every block of `reply` that gives a file, in order, with its lines.
pub fn blocks(reply: &str) -> Vec<Block> {
    let lines: Vec<&str> = reply.lines().collect();
    let mut blocks: Vec<Block> = Vec::new();
    let mut above: Option<(usize, &str)> = None;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let at = i;
        i += 1;
        let Some((fence, info)) = opening_fence(line) else {
            // A path on a line of its own, then a block indented by four
            // spaces rather than fenced, as markdown also has it, is that
            // file too.
            if let Some(path) = label_path(line).or_else(|| file_label(line))
                && let Some((content, next)) = indented_block(&lines, i)
            {
                let fence = (i..next).find(|k| !lines[*k].trim().is_empty()).unwrap_or(i);
                blocks.push(Block { file: File { path, content }, fence });
                i = next;
                above = None;
                continue;
            }
            if !line.trim().is_empty() {
                above = Some((at, line));
            }
            continue;
        };
        let start = i;
        while i < lines.len() && !closes(lines[i], &fence) {
            i += 1;
        }
        let mut body: Vec<&str> = lines[start..i].to_vec();
        // Past the closing fence, when there is one.
        i = (i + 1).min(lines.len());
        let mut path = info_path(info);
        if path.is_none()
            && let Some((_, label)) = above
            && let Some(labelled) = label_path(label)
        {
            path = Some(labelled);
        }
        if path.is_none() {
            path = body.first().and_then(|first| comment_path(first));
        }
        // A "File: src/main.rs" line leading the block names it, and goes;
        // so does a fence right under it, the block's real start, as small
        // models write it inside a bare one.
        if path.is_none()
            && let Some(named) = body.first().and_then(|first| file_label(first))
        {
            path = Some(named);
            body.remove(0);
            if body.first().is_some_and(|l| opening_fence(l).is_some()) {
                body.remove(0);
            }
        }
        above = None;
        let Some(path) = path else { continue };
        let mut content = body.join("\n");
        content.push('\n');
        blocks.push(Block { file: File { path, content }, fence: at });
    }
    blocks
}

/// Whether `block`, a fenced block's text from its opening fence, has its
/// closing fence: not while it streams.
pub fn block_closed(block: &str) -> bool {
    let mut lines = block.lines();
    let Some((fence, _)) = lines.next().and_then(opening_fence) else { return false };
    lines.any(|l| closes(l, &fence))
}

/// Lines of context shown around a change in a diff.
const CONTEXT: usize = 3;
/// Lines of a new file shown before the rest is folded.
const NEW_SHOWN: usize = 40;

/// The change from `old`, the file as it is (none when it's new), to
/// `new`, the file as a reply gives it, by lines: a header, then `+` and
/// `-` lines with `CONTEXT` lines around them and `…` between hunks. A new
/// file is all `+`, folded past `NEW_SHOWN` lines. While the reply is
/// `streaming` the block, what's left of the old file past the new one's
/// end isn't taken out yet, so it doesn't show as removed.
pub fn diff(path: &str, old: Option<&str>, new: &str, streaming: bool) -> String {
    let b: Vec<&str> = new.lines().collect();
    let mut out = String::new();
    let Some(old) = old else {
        out.push_str(&format!("@@ {} · created, +{} @@\n", path, b.len()));
        for line in b.iter().take(NEW_SHOWN) {
            out.push('+');
            out.push_str(line);
            out.push('\n');
        }
        if b.len() > NEW_SHOWN {
            out.push_str(&format!("… {} more lines\n", b.len() - NEW_SHOWN));
        }
        return out;
    };
    let a: Vec<&str> = old.lines().collect();
    let mut ops: Vec<(char, &str)> = if a.len() * b.len() > 4_000_000 {
        a.iter().map(|l| ('-', *l)).chain(b.iter().map(|l| ('+', *l))).collect()
    } else {
        lcs_ops(&a, &b)
    };
    // Streaming, the last run of changes is still arriving: of the old
    // lines in it, only as many as new lines have come are taken out, as
    // replaced; the rest may yet be kept, so they don't show as removed.
    if streaming {
        let run = ops.iter().rposition(|(c, _)| *c == ' ').map_or(0, |i| i + 1);
        let arrived = ops[run..].iter().filter(|(c, _)| *c == '+').count();
        let mut kept = 0;
        let tail: Vec<(char, &str)> = ops[run..]
            .iter()
            .filter(|(c, _)| {
                if *c != '-' {
                    return true;
                }
                kept += 1;
                kept <= arrived
            })
            .copied()
            .collect();
        ops.truncate(run);
        ops.extend(tail);
    }
    let added = ops.iter().filter(|(c, _)| *c == '+').count();
    let removed = ops.iter().filter(|(c, _)| *c == '-').count();
    out.push_str(&format!("@@ {} · updated, +{} −{} @@\n", path, added, removed));
    let changed: Vec<bool> = ops.iter().map(|(c, _)| *c != ' ').collect();
    let keep = |i: usize| (i.saturating_sub(CONTEXT)..(i + CONTEXT + 1).min(ops.len())).any(|k| changed[k]);
    let mut gap = false;
    for (i, (c, line)) in ops.iter().enumerate() {
        if !keep(i) {
            gap = true;
            continue;
        }
        if gap {
            out.push_str("…\n");
            gap = false;
        }
        out.push(*c);
        out.push_str(line);
        out.push('\n');
    }
    if gap {
        out.push_str("…\n");
    }
    out
}

/// `a` turned into `b` line by line, by their longest common run: each
/// line kept (` `), taken out (`-`) or put in (`+`).
fn lcs_ops<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<(char, &'a str)> {
    let (n, m) = (a.len(), b.len());
    let w = m + 1;
    let mut table = vec![0u32; (n + 1) * w];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i * w + j] =
                if a[i] == b[j] { table[(i + 1) * w + j + 1] + 1 } else { table[(i + 1) * w + j].max(table[i * w + j + 1]) };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut ops = Vec::new();
    while i < n && j < m {
        if a[i] == b[j] {
            ops.push((' ', a[i]));
            i += 1;
            j += 1;
        } else if table[(i + 1) * w + j] >= table[i * w + j + 1] {
            ops.push(('-', a[i]));
            i += 1;
        } else {
            ops.push(('+', b[j]));
            j += 1;
        }
    }
    ops.extend(a[i..].iter().map(|l| ('-', *l)));
    ops.extend(b[j..].iter().map(|l| ('+', *l)));
    ops
}

/// The indented code block starting at `lines[from]`, past any blank
/// lines: its contents with the indent taken off, and the index of the
/// line after it. None when what follows isn't indented by four spaces or
/// a tab.
fn indented_block(lines: &[&str], from: usize) -> Option<(String, usize)> {
    let indented = |l: &str| l.starts_with("    ") || l.starts_with('\t');
    let mut i = from;
    while i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }
    if i >= lines.len() || !indented(lines[i]) {
        return None;
    }
    let mut body: Vec<&str> = Vec::new();
    while i < lines.len() && (lines[i].trim().is_empty() || indented(lines[i])) {
        body.push(lines[i].strip_prefix("    ").or_else(|| lines[i].strip_prefix('\t')).unwrap_or(""));
        i += 1;
    }
    while body.last().is_some_and(|l| l.trim().is_empty()) {
        body.pop();
    }
    let mut content = body.join("\n");
    content.push('\n');
    Some((content, i))
}

/// The fence and info string of a line that opens a code block.
pub(crate) fn opening_fence(line: &str) -> Option<(String, &str)> {
    let trimmed = line.trim_start();
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let mark = trimmed.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let fence: String = trimmed.chars().take_while(|c| *c == mark).collect();
    let info = trimmed[fence.len()..].trim();
    (fence.len() >= 3 && !(mark == '`' && info.contains('`'))).then_some((fence, info))
}

/// Whether `line` closes the block `fence` opened.
pub(crate) fn closes(line: &str, fence: &str) -> bool {
    let trimmed = line.trim();
    let mark = fence.chars().next().unwrap_or('`');
    trimmed.len() >= fence.len() && trimmed.chars().all(|c| c == mark)
}

/// A path in a fence's info string: the word after the language, a
/// `title=`/`file=` attribute, or the info string alone.
fn info_path(info: &str) -> Option<String> {
    info.split_whitespace()
        .map(|word| {
            let word = word.split_once('=').map_or(word, |(key, value)| {
                if matches!(key, "title" | "file" | "filename" | "path") { value } else { "" }
            });
            word.trim_matches(|c| c == '"' || c == '\'')
        })
        .find_map(as_path)
}

/// A path that's all a line above a block says, past markdown and a
/// "File:" label: `**src/main.rs**`, `### Cargo.toml`, `` `a.rs`: ``.
fn label_path(line: &str) -> Option<String> {
    let mut text = line.trim().trim_start_matches(|c: char| c == '#' || c == '>' || c.is_whitespace());
    text = text.strip_prefix("- ").or_else(|| text.strip_prefix("* ")).unwrap_or(text);
    if let Some((number, rest)) = text.split_once(". ")
        && number.chars().all(|c| c.is_ascii_digit())
    {
        text = rest;
    }
    let text = text.trim_matches(|c: char| matches!(c, '*' | '_' | '`' | ':') || c.is_whitespace());
    let text = ["File:", "Filename:", "Path:"]
        .iter()
        .find_map(|label| text.strip_prefix(label))
        .unwrap_or(text);
    as_path(text.trim_matches(|c: char| matches!(c, '*' | '_' | '`' | ':' | '(' | ')') || c.is_whitespace()))
}

/// The path in a "File: src/main.rs" line.
fn file_label(line: &str) -> Option<String> {
    let text = line.trim().trim_matches(|c| c == '*' || c == '`');
    let rest = ["File:", "file:", "Filename:", "Path:"].iter().find_map(|label| text.strip_prefix(label))?;
    as_path(rest.trim().trim_matches(|c: char| matches!(c, '*' | '`')))
}

/// A path named by a comment alone on a block's first line.
fn comment_path(line: &str) -> Option<String> {
    let text = line.trim();
    let inner = ["//", "#", "--", ";"]
        .iter()
        .find_map(|open| text.strip_prefix(open))
        .or_else(|| text.strip_prefix("<!--").and_then(|t| t.strip_suffix("-->")))
        .or_else(|| text.strip_prefix("/*").and_then(|t| t.strip_suffix("*/")))?;
    let inner = inner.trim();
    let inner = ["File:", "file:", "Filename:"].iter().find_map(|l| inner.strip_prefix(l)).unwrap_or(inner);
    as_path(inner.trim())
}

/// `text` when it reads as a relative file path: one word of path
/// characters, with no way out of the folder, whose name has an extension
/// or is one of `BARE_NAMES`.
fn as_path(text: &str) -> Option<String> {
    let text = text.strip_prefix("./").unwrap_or(text);
    let allowed = |c: char| c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | '+' | '@');
    if text.is_empty() || !text.chars().all(allowed) || text.starts_with('/') || text.ends_with('/') {
        return None;
    }
    if Path::new(text).components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    let name = text.rsplit('/').next()?;
    let named = match name.rsplit_once('.') {
        // `.gitignore`, `main.rs`; not `1.0` or `e.g.`.
        Some((_, ext)) => !ext.is_empty() && !ext.chars().all(|c| c.is_ascii_digit()) && name.chars().any(char::is_alphabetic),
        None => BARE_NAMES.contains(&name),
    };
    named.then(|| text.to_string())
}

/// Where `file` goes under `root`, unless that leads outside it, e.g.
/// through a linked directory.
pub fn target(root: &Path, file: &File) -> Option<PathBuf> {
    let path = root.join(&file.path);
    let root = root.canonicalize().ok()?;
    let parent = path.ancestors().skip(1).find(|dir| dir.exists())?.canonicalize().ok()?;
    if !parent.starts_with(&root) {
        return None;
    }
    // A link at the file's own place would be followed by the write.
    if path.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
        return None;
    }
    Some(path)
}

/// The files of a reply sorted by what writing them would do.
#[derive(Debug, Default)]
pub struct Plan {
    /// Not there yet: safe to write.
    pub new: Vec<File>,
    /// There, with other contents: to ask about.
    pub changed: Vec<File>,
    /// Paths that lead outside the folder, left alone.
    pub refused: Vec<String>,
    /// There, and not shown to the agent whole: a reply shorter than the
    /// file can't be all of it, so it's left as it is, unless `/write`
    /// says otherwise.
    pub kept: Vec<File>,
}

/// Sort `files` against what's under `root`, `whole` being the files the
/// agent was shown whole; a file already holding the same contents is left
/// out.
pub fn plan(root: &Path, files: Vec<File>, whole: &[String]) -> Plan {
    let mut plan = Plan::default();
    for file in files {
        match target(root, &file) {
            None => plan.refused.push(file.path),
            Some(path) => match fs::read_to_string(&path) {
                Ok(text) if text == file.content => {}
                Ok(text) if !whole.contains(&file.path) && file.content.len() < text.len() / 10 * 9 => {
                    plan.kept.push(file)
                }
                Ok(_) => plan.changed.push(file),
                Err(_) if path.exists() => plan.changed.push(file),
                Err(_) => plan.new.push(file),
            },
        }
    }
    plan
}

/// Lines added and removed going from `old` to `new`, by their longest
/// common run of lines; past a size where that costs too much, every line
/// counts as changed.
pub fn line_changes(old: &str, new: &str) -> (usize, usize) {
    let (a, b): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    if a.len() * b.len() > 4_000_000 {
        return (b.len(), a.len());
    }
    let mut row = vec![0usize; b.len() + 1];
    for x in &a {
        let mut diagonal = 0;
        for (j, y) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = if x == y { diagonal + 1 } else { above.max(row[j]) };
            diagonal = above;
        }
    }
    let common = row[b.len()];
    (b.len() - common, a.len() - common)
}

/// Write `files` under `root`, making their folders: the paths written,
/// then a message for each that failed.
pub fn write(root: &Path, files: &[File]) -> (Vec<String>, Vec<String>) {
    let (mut written, mut failed) = (Vec::new(), Vec::new());
    for file in files {
        let Some(path) = target(root, file) else {
            failed.push(format!("{}: outside {}", file.path, root.display()));
            continue;
        };
        let result = path
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| fs::write(&path, &file.content));
        match result {
            Ok(()) => written.push(file.path.clone()),
            Err(e) => failed.push(format!("{}: {}", file.path, e)),
        }
    }
    (written, failed)
}

/// The command that checks the project at `root` still builds after
/// `written` changed, by its `kind`, when it has one: a quick one that
/// needs only the toolchain, run in `root`. A project with no build of its
/// own, or no kind at all, as a folder of scripts, gets each file written
/// checked on its own by what it is: a shell script parsed by `bash -n`, a
/// Python file compiled, and so on. None when nothing fitting can be run.
pub fn check_command(root: &Path, kind: Option<&str>, written: &[String]) -> Option<String> {
    let project = match kind {
        Some("Rust") => Some("cargo check --quiet".to_string()),
        Some("Go") => Some("go build ./...".to_string()),
        Some("Elixir") => Some("mix compile".to_string()),
        Some("Swift") => Some("swift build".to_string()),
        // The project's own build, which resolves imports as a bundler
        // does; else the type checker.
        Some("JavaScript/TypeScript") if build_script(root) => Some("npm run --silent build".to_string()),
        Some("JavaScript/TypeScript") if root.join("tsconfig.json").is_file() => {
            Some("npx --no-install tsc --noEmit -p .".to_string())
        }
        Some("Java") => Some("mvn -q -DskipTests compile".to_string()),
        Some("Java/Kotlin" | "Kotlin") => root.join("gradlew").is_file().then(|| "./gradlew -q build -x test".to_string()),
        _ => None,
    };
    project.or_else(|| each_file(written))
}

/// What checks each file of `written` on its own, by its extension, run
/// one after the other and stopping at the first that fails.
fn each_file(written: &[String]) -> Option<String> {
    let checks: Vec<String> = written
        .iter()
        .filter_map(|path| {
            let ext = path.rsplit_once('.').map(|(_, e)| e)?;
            let cmd = match ext {
                "sh" | "bash" => "bash -n",
                "zsh" => "zsh -n",
                "py" => "python3 -m py_compile",
                "js" | "mjs" | "cjs" | "jsx" => "node --check",
                "rb" => "ruby -c",
                "php" => "php -l",
                "pl" => "perl -c",
                "lua" => "luac -p",
                _ => return None,
            };
            Some(format!("{} '{}'", cmd, path.replace('\'', "'\\''")))
        })
        .collect();
    (!checks.is_empty()).then(|| checks.join(" && "))
}

/// Whether the package.json at `root` has a `build` script.
fn build_script(root: &Path) -> bool {
    fs::read_to_string(root.join("package.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .is_some_and(|pkg| pkg["scripts"]["build"].is_string())
}

/// Remove the agent's own copy of a reply, at `copy`, when it's in `.out/`
/// right under `root`, and `.out/` with it once empty; whether it went.
pub fn remove_agent_copy(root: &Path, copy: &str) -> bool {
    let out = root.join(".out");
    let copy = Path::new(copy);
    if copy.parent() != Some(out.as_path()) || copy.extension().is_none_or(|e| e != "md") {
        return false;
    }
    let removed = fs::remove_file(copy).is_ok();
    if removed {
        // Fails, as it should, while anything else is in there.
        let _ = fs::remove_dir(&out);
    }
    removed
}

/// `reply` without the link to `copy` the agent puts at its end.
pub fn without_link(reply: &str, copy: &str) -> String {
    let name = Path::new(copy).file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    let posix = copy.replace('\\', "/");
    let link = format!("[{}]({})", name, posix);
    match reply.trim_end().strip_suffix(&link) {
        Some(rest) => rest.trim_end().to_string(),
        None => reply.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(reply: &str) -> Vec<String> {
        extract(reply).into_iter().map(|f| f.path).collect()
    }

    #[test]
    fn finds_paths_above_in_the_info_string_and_in_a_first_comment() {
        let reply = "Here's the project.\n\n**`Cargo.toml`**\n```toml\n[package]\nname = \"hi\"\n```\n\n\
            ### src/main.rs\n\n```rust\nfn main() {}\n```\n\n```rust src/lib.rs\npub fn f() {}\n```\n\n\
            ```python\n# tools/run.py\nprint(1)\n```\n";
        let files = extract(reply);
        assert_eq!(paths(reply), ["Cargo.toml", "src/main.rs", "src/lib.rs", "tools/run.py"]);
        assert_eq!(files[0].content, "[package]\nname = \"hi\"\n");
        assert_eq!(files[3].content, "# tools/run.py\nprint(1)\n");
    }

    #[test]
    fn leaves_commands_and_unnamed_blocks_alone() {
        let reply = "Run this:\n\n```bash\ncargo new hello\n```\n\nThen edit your `Cargo.toml`:\n\n```toml\n\
            [dependencies]\n```\n\nVersion 1.0\n```\nplain\n```\n";
        assert!(extract(reply).is_empty(), "{:?}", extract(reply));
    }

    #[test]
    fn refuses_paths_out_of_the_folder() {
        for bad in ["/etc/passwd", "../x.rs", "src/../../x.rs", "~/.zshrc", "a b.rs", "http://x.io/a.js"] {
            assert_eq!(as_path(bad), None, "{}", bad);
        }
        assert_eq!(as_path("./src/main.rs").as_deref(), Some("src/main.rs"));
        assert_eq!(as_path(".gitignore").as_deref(), Some(".gitignore"));
        assert_eq!(as_path("Makefile").as_deref(), Some("Makefile"));
    }

    #[test]
    fn a_file_line_over_an_indented_block_is_that_file() {
        let reply = "Here's the missing file:\n\nFile: src/components/ThemeToggle.css\n\n    .theme-toggle {\n      \
            background: transparent;\n\n      border: none;\n    }\n\nThen import it.\n\n`src/x.js`\n\n\tconst a = 1;\n";
        let files = extract(reply);
        assert_eq!(paths(reply), ["src/components/ThemeToggle.css", "src/x.js"]);
        assert_eq!(files[0].content, ".theme-toggle {\n  background: transparent;\n\n  border: none;\n}\n");
        assert_eq!(files[1].content, "const a = 1;\n");
        // A path line with prose after it names nothing.
        assert!(extract("File: a.rs\nnot code\n    nor this\n").is_empty());
        // A fence left open runs to the end.
        assert_eq!(extract("`a.rs`\n```rust\nfn f() {}\n")[0].content, "fn f() {}\n");
    }

    #[test]
    fn a_file_label_inside_a_bare_fence_names_the_block_within() {
        let reply = "```\nFile: src/main.rs\n```rust\nfn main() {}\n```\n";
        let files = extract(reply);
        assert_eq!(files, [File { path: "src/main.rs".into(), content: "fn main() {}\n".into() }]);
        let reply = "```toml\nFile: Cargo.toml\n[package]\n```\n";
        assert_eq!(extract(reply)[0].content, "[package]\n");
    }

    #[test]
    fn a_block_inside_a_longer_fence_is_its_content() {
        let reply = "`README.md`\n````markdown\n# Hi\n```sh\ncargo run\n```\n````\n";
        let files = extract(reply);
        assert_eq!(paths(reply), ["README.md"]);
        assert_eq!(files[0].content, "# Hi\n```sh\ncargo run\n```\n");
    }

    #[test]
    fn plans_new_changed_and_unchanged_files_then_writes() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("same.txt"), "same\n").unwrap();
        fs::write(dir.path().join("old.txt"), "old\n").unwrap();
        let file = |path: &str, content: &str| File { path: path.into(), content: content.into() };
        let plan = plan(dir.path(), vec![file("same.txt", "same\n"), file("old.txt", "new\n"), file("src/a.rs", "a\n")], &[]);
        assert_eq!(plan.new, [file("src/a.rs", "a\n")]);
        assert_eq!(plan.changed, [file("old.txt", "new\n")]);
        let (written, failed) = write(dir.path(), &plan.new);
        assert_eq!((written, failed), (vec!["src/a.rs".to_string()], vec![]));
        assert_eq!(fs::read_to_string(dir.path().join("src/a.rs")).unwrap(), "a\n");
    }

    #[cfg(unix)]
    #[test]
    fn will_not_write_through_a_link_out_of_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("out")).unwrap();
        let file = File { path: "out/x.rs".into(), content: "x\n".into() };
        assert_eq!(target(dir.path(), &file), None);
        assert_eq!(plan(dir.path(), vec![file], &[]).refused, ["out/x.rs"]);
    }

    #[test]
    fn a_file_the_agent_never_saw_whole_is_not_cut_down() {
        let dir = tempfile::tempdir().unwrap();
        let long: String = (1..=100).map(|i| format!("line {}\n", i)).collect();
        fs::write(dir.path().join("big.rs"), &long).unwrap();
        let file = |content: &str| File { path: "big.rs".into(), content: content.into() };
        // A snippet for a file shown in part, or not at all, is kept out.
        let sorted = plan(dir.path(), vec![file("fn snippet() {}\n")], &[]);
        assert_eq!(sorted.kept, [file("fn snippet() {}\n")]);
        assert!(sorted.changed.is_empty());
        // Shown whole, the reply is the edit, however short.
        let sorted = plan(dir.path(), vec![file("fn snippet() {}\n")], &["big.rs".to_string()]);
        assert_eq!(sorted.changed.len(), 1);
        // Near its size, it's taken as the whole file.
        let most = long.lines().skip(5).map(|l| format!("{}\n", l)).collect::<String>();
        assert_eq!(plan(dir.path(), vec![file(&most)], &[]).changed.len(), 1);
    }

    #[test]
    fn the_agents_own_copy_goes_only_from_out() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join(".out");
        fs::create_dir(&out).unwrap();
        let copy = out.join("project_summary.md");
        fs::write(&copy, "x").unwrap();
        let elsewhere = dir.path().join("notes.md");
        fs::write(&elsewhere, "x").unwrap();
        assert!(!remove_agent_copy(dir.path(), &elsewhere.to_string_lossy()));
        assert!(elsewhere.exists());
        let copy = copy.to_string_lossy().into_owned();
        assert!(remove_agent_copy(dir.path(), &copy));
        assert!(!out.exists());
        let reply = format!("Done.\n\n[project_summary.md]({})\n", copy);
        assert_eq!(without_link(&reply, &copy), "Done.");
        assert_eq!(without_link("Done.", &copy), "Done.");
    }

    #[test]
    fn a_code_project_is_described_with_its_files() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(project_kind(dir.path()), None);
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"t\"\n").unwrap();
        assert_eq!(project_kind(dir.path()), Some("Rust"));
        fs::write(dir.path().join("Cargo.lock"), "lock").unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::create_dir_all(dir.path().join("target/debug")).unwrap();
        fs::write(dir.path().join("target/debug/x.rs"), "built").unwrap();
        fs::create_dir_all(dir.path().join(".hidden")).unwrap();
        fs::write(dir.path().join(".hidden/x.rs"), "hidden").unwrap();
        let context = project_context(dir.path(), "Rust", "");
        let text = context.text;
        assert_eq!(context.whole, ["Cargo.toml", "src/main.rs"]);
        assert!(text.starts_with("I'm working in the Rust project at"), "{}", text);
        assert!(text.contains("not a general question"), "{}", text);
        assert!(text.contains("- Cargo.toml\n- Cargo.lock\n- src/main.rs\n"), "{}", text);
        assert!(text.contains("`src/main.rs`\n```rs\nfn main() {}\n```"), "{}", text);
        assert!(!text.contains("target") && !text.contains("\nlock\n"), "{}", text);
        assert!(!text.contains(".hidden"), "{}", text);
    }

    #[test]
    fn the_files_a_prompt_concerns_are_shown_first_and_big_ones_in_part() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        fs::write(dir.path().join("README.md"), "# Docs\n").unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(dir.path().join("src/news.rs"), "pub fn news() {}\n").unwrap();
        let big: String = (1..=1400).map(|i| format!("    let value_{} = {};\n", i, i)).collect();
        let big = format!("{}fn status_line() {{}}\n{}", big, big);
        fs::write(dir.path().join("src/ui.rs"), &big).unwrap();
        let text = project_context(dir.path(), "Rust", "Fix the status line in the news feed");
        let at = |needle: &str| text.text.find(needle).unwrap_or(usize::MAX);
        // The manifest, the file the prompt names, the source, then the docs.
        assert!(at("`Cargo.toml`") < at("`src/news.rs`"), "{}", text.text);
        assert!(at("`src/news.rs`") < at("`src/main.rs`"), "{}", text.text);
        assert!(at("`src/main.rs`") < at("`README.md`"), "{}", text.text);
        // The big file shows the lines around the prompt's words, and says so.
        assert!(text.text.contains("`src/ui.rs` (13 of its 2801 lines, for reference: don't give this file back whole)"), "{}", text.text);
        assert!(text.text.contains("fn status_line() {}"), "{}", text.text);
        assert!(!text.text.contains("let value_300 = 300;"), "{}", text.text);
        assert!(!text.whole.contains(&"src/ui.rs".to_string()));
        assert!(text.whole.contains(&"src/news.rs".to_string()));
    }

    #[test]
    fn an_excerpt_takes_the_lines_around_the_words_or_the_start() {
        // Hits on lines 5 and 35 of 41: six lines around each, a break between.
        let content: String = (0..41).map(|i| if i == 5 || i == 35 { "hit\n".to_string() } else { format!("l{}\n", i) }).collect();
        let (part, lines) = excerpt(&content, &["hit"], 1000);
        assert_eq!(lines, 24);
        assert!(part.starts_with("l0\nl1\n"), "{}", part);
        assert!(part.contains("l11\n…\nl29\n"), "{}", part);
        assert!(part.ends_with("hit\nl36\nl37\nl38\nl39\nl40"), "{}", part);
        assert_eq!(excerpt("one\ntwo\n", &["zzz"], 5), ("one\nt".to_string(), 0));
        assert_eq!(cut("héllo", 2), "h");
    }

    #[test]
    fn a_check_command_fits_the_project_and_the_files_written() {
        let dir = tempfile::tempdir().unwrap();
        let w = |paths: &[&str]| paths.iter().map(|p| p.to_string()).collect::<Vec<_>>();
        assert_eq!(check_command(dir.path(), Some("Rust"), &w(&["src/main.rs"])).as_deref(), Some("cargo check --quiet"));
        assert_eq!(check_command(dir.path(), Some("Go"), &[]).as_deref(), Some("go build ./..."));
        assert_eq!(
            check_command(dir.path(), Some("Python"), &w(&["app.py", "README.md", "it's.py"])).as_deref(),
            Some("python3 -m py_compile 'app.py' && python3 -m py_compile 'it'\\''s.py'")
        );
        assert_eq!(check_command(dir.path(), Some("Python"), &w(&["README.md"])), None);
        assert_eq!(
            check_command(dir.path(), Some("JavaScript/TypeScript"), &w(&["index.js", "App.jsx"])).as_deref(),
            Some("node --check 'index.js' && node --check 'App.jsx'")
        );
        fs::write(dir.path().join("tsconfig.json"), "{}").unwrap();
        assert_eq!(
            check_command(dir.path(), Some("JavaScript/TypeScript"), &w(&["index.ts"])).as_deref(),
            Some("npx --no-install tsc --noEmit -p .")
        );
        fs::write(dir.path().join("package.json"), r#"{"scripts": {"build": "vite build"}}"#).unwrap();
        assert_eq!(
            check_command(dir.path(), Some("JavaScript/TypeScript"), &w(&["index.ts"])).as_deref(),
            Some("npm run --silent build")
        );
        // No build of its own, or no kind at all: each file by what it is.
        assert_eq!(check_command(dir.path(), Some("C/C++"), &w(&["main.c"])), None);
        assert_eq!(check_command(dir.path(), Some("Kotlin"), &w(&["App.kt", "run.sh"])).as_deref(), Some("bash -n 'run.sh'"));
        assert_eq!(
            check_command(dir.path(), None, &w(&["deploy.sh", "tool.py", "notes.md"])).as_deref(),
            Some("bash -n 'deploy.sh' && python3 -m py_compile 'tool.py'")
        );
        assert_eq!(check_command(dir.path(), None, &w(&["notes.md"])), None);
    }

    #[test]
    fn a_file_in_a_reply_shows_as_its_change_as_it_streams() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\n";
        let new = "a\nb\nc\nd\nE\nf\ng\nh\ni\nj\nk\nl\nm\n";
        assert_eq!(
            diff("x.rs", Some(old), new, false),
            "@@ x.rs · updated, +2 −1 @@\n…\n b\n c\n d\n-e\n+E\n f\n g\n h\n…\n j\n k\n l\n+m\n"
        );
        assert_eq!(diff("x.rs", None, "x\ny\n", false), "@@ x.rs · created, +2 @@\n+x\n+y\n");
        let long: String = (1..=50).map(|i| format!("l{}\n", i)).collect();
        assert!(diff("x.rs", None, &long, true).ends_with("+l40\n… 10 more lines\n"));
        assert_eq!(diff("x.rs", Some("same\n"), "same\n", false), "@@ x.rs · updated, +0 −0 @@\n…\n");
        // Streaming, the old file's tail isn't removed yet; closed, it is.
        assert_eq!(diff("x.rs", Some("a\nb\nc\n"), "a\nB\n", true), "@@ x.rs · updated, +1 −1 @@\n a\n-b\n+B\n");
        assert_eq!(diff("x.rs", Some("a\nb\nc\n"), "a\nB\n", false), "@@ x.rs · updated, +1 −2 @@\n a\n-b\n-c\n+B\n");

        let reply = "Done.\n\n**`src/a.rs`**\n\n```rust\nfn a() {}\n```\n\n`b.txt`\n\n    hello\n\n```sh\nls\n";
        let blocks = blocks(reply);
        let seen: Vec<(&str, usize)> = blocks.iter().map(|b| (b.file.path.as_str(), b.fence)).collect();
        assert_eq!(seen, [("src/a.rs", 4), ("b.txt", 10)]);
        assert!(block_closed("```rust\nfn a() {}\n```"));
        assert!(!block_closed("```rust\nfn a() {}"));
    }

    #[test]
    fn counts_lines_added_and_removed() {
        assert_eq!(line_changes("", "a\nb\n"), (2, 0));
        assert_eq!(line_changes("a\nb\nc\n", "a\nx\nc\nd\n"), (2, 1));
        assert_eq!(line_changes("a\n", "a\n"), (0, 0));
    }

    #[test]
    fn a_prompt_naming_a_file_of_the_project_is_about_its_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/components")).unwrap();
        fs::write(dir.path().join("src/components/TodoList.jsx"), "x").unwrap();
        let error = format!(
            "[plugin:vite:import-analysis] Failed to resolve import \"./TodoList.css\" from \"src/components/TodoList.jsx\". \
             Does the file exist?\n{}/src/components/TodoList.jsx:2:7",
            dir.path().display()
        );
        assert!(mentions_file(dir.path(), &error));
        assert!(mentions_file(dir.path(), &format!("see {}/src/components/TodoList.jsx:2:7", dir.path().display())));
        assert!(!mentions_file(dir.path(), "import ./TodoList.css fails"));
        assert!(!mentions_file(dir.path(), "what does the contract say about src/other.jsx?"));
    }

    #[test]
    fn asks_for_files_when_the_prompt_makes_something() {
        assert!(wants_files("Generate a rust project"));
        assert!(wants_files("set up a vite app"));
        assert!(!wants_files("what does the contract say about renewals?"));
    }
}
