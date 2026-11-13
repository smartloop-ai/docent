//! Files a reply hands over, written into the working folder.
//!
//! The agent answers in text: asked to make a project, it shows the files
//! rather than writing them. A prompt that asks for something to be made
//! carries `HINT`, so the reply gives each new file whole, as a `cat > path`
//! heredoc in a shell block (or, as models also write it, a fenced block
//! with its path on the line above), and each change to a file that's there
//! as an `edit` block of SEARCH/REPLACE pairs; `extract` finds those, and the TUI
//! shows each as a diff and writes it, asking before it replaces a file
//! that's already there. A skill is a file like any other, under
//! `.docent/skills/`, which docent hands on to the framework.
//! Work that isn't a file, such as a commit, comes as a shell block with
//! no path; `commands` finds those, and the TUI runs them in the folder.

use std::fs;
use std::path::{Component, Path, PathBuf};

/// Told to the agent with a prompt that asks for something to be made.
pub const HINT: &str = "If your answer creates or changes files, start with a line or two on what the \
files below do and why, then give every one of them: a file you only describe isn't written. Give each \
file whole with a shell heredoc in a fenced block marked `sh`: a line `cat > path/to/file <<'EOF'`, with the path \
relative to the working folder, then the file's complete contents, then `EOF` alone on its line. Do so to change \
a file that's there too: give it again whole, every line of it, changed or not, never a comment such as \
`// ... rest unchanged` in place of lines, or it isn't written. When a file's contents hold a line of three \
backticks, open and close its block with four. Include every file the change \
needs, such as the manifest when the code uses a new dependency. I'm shown each change as a diff and the files \
are written for me from those blocks, so don't tell me to create or edit them by hand. Asked to make \
or save a skill, write it the same way, to `.docent/skills/<name>/SKILL.md`: frontmatter with its `name` and a \
one-line `description` of when to use it, then its instructions. Something to do in the folder that isn't a \
file, such as committing or pushing to git, installing a dependency or running the tests, you do by giving the \
shell commands for it in a fenced block marked `sh`: they're run for me in the working folder, in order, after \
the files are written, and their output comes back to you with my next message. So never say you can't run \
commands or reach git: give the commands. Use the folder's real values in them, such as its git branch and \
remote as given below, never a placeholder like `<your-username>`; something you'd need and don't have, such as \
a remote that isn't set, ask me for instead. A command that keeps running until it's stopped, such as a dev \
server or a watcher, you give with `&` at its end, as `npm run dev &`, in a block of its own: it's started in \
the background, and what comes after doesn't wait for it.";

/// A change to a file as it's to be written, the file given again whole,
/// shown to the agent with `HINT` and again when a change is asked for over.
pub const EDIT_EXAMPLE: &str = "For example, to change one line of `src/app.js`, give all of it again:\n\n```sh\ncat > src/app.js <<'EOF'\nconst speed = 3;\n\nfunction tick(x) {\n  return x + speed;\n}\nEOF\n```";

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
/// Characters of file contents sent with a prompt when nothing better is
/// known of the model's window: what a 32k-token local model takes.
pub const DEFAULT_BUDGET: usize = 60_000;
/// Most characters of file contents sent with one prompt, however wide the
/// model's window: a turn's worth of reading, not the whole disk.
pub const MOST_BUDGET: usize = 400_000;

/// Characters of file contents a prompt may carry for a model with a
/// `window` of tokens: two characters a token, so that the files take
/// about half of it, between `DEFAULT_BUDGET` and `MOST_BUDGET`.
pub fn budget_for(window: Option<u64>) -> usize {
    match window {
        Some(tokens) => (usize::try_from(tokens).unwrap_or(usize::MAX).saturating_mul(2)).clamp(DEFAULT_BUDGET, MOST_BUDGET),
        None => DEFAULT_BUDGET,
    }
}

/// The kind of code project `root` is, from the files at its top.
pub fn project_kind(root: &Path) -> Option<&'static str> {
    MARKERS.iter().find(|(marker, _)| root.join(marker).is_file()).map(|(_, kind)| *kind)
}

/// Languages whose source doesn't build without a manifest beside it:
/// `(extension, manifest, language)`.
const NEEDS_MANIFEST: [(&str, &str, &str); 2] = [("rs", "Cargo.toml", "Rust"), ("go", "go.mod", "Go")];

/// The manifest `written` needs and `root` hasn't, with its language: Rust
/// or Go source written into a folder that isn't a project yet, as a small
/// model leaves it.
pub fn missing_manifest(root: &Path, written: &[String]) -> Option<(&'static str, &'static str)> {
    if project_kind(root).is_some() {
        return None;
    }
    written.iter().find_map(|path| {
        let ext = Path::new(path).extension()?.to_str()?;
        NEEDS_MANIFEST.iter().find(|(e, _, _)| *e == ext).map(|(_, manifest, lang)| (*manifest, *lang))
    })
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
/// as in "Rust"; `budget` the characters of contents to send, for the
/// model's window (`budget_for`), half of it at most for one file. The
/// framework fits the prompt to the model its orchestrator picks, keeping
/// its head and tail; so what matters most comes first (the file list,
/// the git state, the manifests and the files the prompt names), the bulk
/// after, and the request with `HINT` closes the message.
pub fn project_context(root: &Path, kind: &str, prompt: &str, budget: usize) -> Context {
    let shown_each = budget / 2;
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
         its files below and what's in the folder; the web is for what they don't tell, such as a library's \
         current API, not for a general answer. When I ask for code or a change, answer with the code itself, \
         not a description of it: change the project's files where the request fits them, keeping what they \
         already do unless I ask otherwise.\n\n\
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
    let mut room = budget;
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
        let limit = shown_each.min(room);
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
    // Ahead of the contents: a model whose window the prompt outruns gets
    // its head and tail, and loses the middle.
    if let Some(git) = git_state(root) {
        text.push_str("\nIts git state:\n```\n");
        text.push_str(&git);
        text.push_str("\n```\n");
    }
    if !shown.is_empty() {
        text.push_str("\nTheir contents now:\n");
        text.push_str(&shown);
    }
    Context { text, whole }
}

/// The folder's git branch, remotes and changed files, as `git status
/// --short --branch` and `git remote -v` print them, for commands that
/// commit or push; `None` where it isn't a repository, or git is missing.
fn git_state(root: &Path) -> Option<String> {
    if !root.join(".git").exists() {
        return None;
    }
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim_end().to_string())
    };
    let status = run(&["status", "--short", "--branch"])?;
    let mut lines: Vec<&str> = status.lines().collect();
    let more = lines.len().saturating_sub(GIT_STATUS_LINES + 1);
    lines.truncate(GIT_STATUS_LINES + 1);
    let mut text = lines.join("\n");
    if more > 0 {
        text.push_str(&format!("\n… and {} more changed files", more));
    }
    match run(&["remote", "-v"]) {
        Some(remotes) if !remotes.is_empty() => {
            text.push('\n');
            text.push_str(&remotes);
        }
        _ => text.push_str("\n(no remote is set)"),
    }
    Some(text)
}

/// Changed files the git state lists, at most.
const GIT_STATUS_LINES: usize = 40;

/// Tools looked for on `PATH`, so commands use the ones the machine has:
/// its package manager above all (`apt-get` or `brew`, `dnf` or `pacman`).
const TOOLS: [&str; 34] = [
    "git", "gh", "apt-get", "dnf", "yum", "pacman", "zypper", "apk", "brew", "port", "nix", "winget", "choco",
    "scoop", "cargo", "rustup", "node", "npm", "pnpm", "yarn", "bun", "deno", "python3", "python", "pip3", "uv",
    "go", "java", "mvn", "gradle", "make", "cmake", "docker", "podman",
];

/// Environment variables worth knowing for commands: the locale, the
/// active toolchains and virtual environments, and whether it's CI or WSL.
/// A fixed list, never the whole environment, which holds tokens and keys.
const ENV_VARS: [&str; 18] = [
    "LANG", "LC_ALL", "TERM", "VIRTUAL_ENV", "CONDA_DEFAULT_ENV", "PYENV_VERSION", "NVM_BIN", "NODE_ENV",
    "GOPATH", "GOROOT", "CARGO_HOME", "RUSTUP_TOOLCHAIN", "JAVA_HOME", "ANDROID_HOME", "CI", "WSL_DISTRO_NAME",
    "container", "XDG_SESSION_TYPE",
];

/// The machine the reply's commands run on, told to the agent ahead of a
/// prompt that works in a folder, so its commands fit it: the OS and its
/// version, the architecture, the shell they run with, the user, the tools
/// on `PATH` and a few environment variables. Worked out once.
pub fn system_context() -> &'static str {
    static TEXT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TEXT.get_or_init(|| {
        let os = sysinfo::System::long_os_version().unwrap_or_else(|| std::env::consts::OS.to_string());
        let kernel = sysinfo::System::kernel_version().map(|k| format!(", kernel {}", k)).unwrap_or_default();
        let shell = if cfg!(windows) {
            "cmd /C".to_string()
        } else {
            format!("{} -c", std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string()))
        };
        let mut text = format!(
            "The machine my commands run on:\n- OS: {} ({}{})\n- Shell: commands run with `{}`\n",
            os,
            std::env::consts::ARCH,
            kernel,
            shell
        );
        let user = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).ok();
        let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).ok();
        match (user, home) {
            (Some(u), Some(h)) => text.push_str(&format!("- User: {}, home {}\n", u, h)),
            (Some(u), None) => text.push_str(&format!("- User: {}\n", u)),
            (None, Some(h)) => text.push_str(&format!("- Home: {}\n", h)),
            (None, None) => {}
        }
        let found: Vec<&str> = TOOLS.iter().copied().filter(|t| on_path(t)).collect();
        if !found.is_empty() {
            text.push_str(&format!("- On PATH: {}\n", found.join(", ")));
        }
        let vars: Vec<String> = ENV_VARS
            .iter()
            .filter_map(|v| std::env::var(v).ok().filter(|x| !x.is_empty()).map(|x| format!("{}={}", v, x)))
            .collect();
        if !vars.is_empty() {
            text.push_str(&format!("- Environment: {}\n", vars.join(", ")));
        }
        text.push_str(
            "Give commands for this machine: its shell, and its package manager rather than another OS's.\n",
        );
        text
    })
}

/// Whether `tool` is a program in a `PATH` directory.
pub(crate) fn on_path(tool: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    let names: Vec<String> = if cfg!(windows) {
        ["exe", "cmd", "bat"].iter().map(|ext| format!("{}.{}", tool, ext)).collect()
    } else {
        vec![tool.to_string()]
    };
    std::env::split_paths(&path).any(|dir| names.iter().any(|n| dir.join(n).is_file()))
}

/// The placeholders in `script`, a command the agent gave with a value
/// to fill in, as `<your-username>`, `<API KEY>` or `YOUR_TOKEN`: it
/// mustn't run as it is. A redirect, `< file`, a heredoc, `<<EOF`, or an
/// HTML tag the command writes, `<h1>`, isn't one: a placeholder names a
/// thing, in more than one word or in capitals, where a tag is one short
/// lowercase word.
pub fn placeholders(script: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut add = |text: String| {
        if !found.contains(&text) {
            found.push(text);
        }
    };
    for (i, _) in script.match_indices('<') {
        let rest = &script[i + 1..];
        let Some(end) = rest.find('>') else { continue };
        let inner = &rest[..end];
        let named = !script[..i].ends_with('<')
            && inner.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && inner.chars().last().is_some_and(|c| c.is_ascii_alphanumeric())
            && inner.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ' ' | '.'))
            && !inner.contains("  ");
        let lower = inner.to_lowercase();
        let names_a_thing = inner.contains(['-', '_', ' '])
            || lower.starts_with("your")
            || (inner.len() > 1 && inner.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()));
        if named && names_a_thing {
            add(format!("<{}>", inner));
        }
    }
    for token in script.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
        if token.starts_with("YOUR_") && token.len() > 5 {
            add(token.to_string());
        }
    }
    found
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

/// Words that ask for something to be made or done in the folder, as the
/// prompt's first concern. A turn of one works in the folder, where the
/// reply's commands run: without them, "install terraform" outside a code
/// project was a chat turn, and the agent wrote out the steps for the user
/// to run instead of giving them to be run.
const MAKING: [&str; 23] = [
    "create", "generate", "scaffold", "build", "make", "write", "add", "implement", "bootstrap", "init",
    "setup", "update", "fix", "save", "install", "uninstall", "reinstall", "run", "deploy", "configure",
    "upgrade", "commit", "push",
];

/// Names without an extension that are still files a project keeps.
const BARE_NAMES: [&str; 8] =
    ["Makefile", "Dockerfile", "Justfile", "Procfile", "Gemfile", "Rakefile", "LICENSE", "Containerfile"];

/// The working folder `root`, said to be the project's root: a small model
/// otherwise makes a project folder inside it (`cargo new name`) and `cd`s
/// about, expecting that to move where files go.
pub fn folder_note(root: &Path) -> String {
    format!(
        "The working folder is `{}`, and it is the project's root: every path is relative to it, as `README.md` \
         or `src/app` rather than `<name>/src/app`. Don't make a project folder inside it (no scaffolding command \
         that takes a project name), and don't `cd`: commands always run in this folder, and a `cd` doesn't \
         change where files are written.",
        root.display()
    )
}

/// Told to the agent when the folder it works in has no files yet, so it
/// doesn't take an edit block's example path for a file that's there.
pub const EMPTY_FOLDER: &str = "The working folder is empty: there are no files to change, so write every file \
whole as a heredoc, with no edit blocks. Write it in TypeScript on Node, with its `package.json` and \
`tsconfig.json`, unless the request names another language or one is otherwise preferred, as Python or Rust.";

/// Whether `root` has no files of its own: nothing but hidden entries,
/// such as docent's `.docent/`.
pub fn is_empty_folder(root: &Path) -> bool {
    fs::read_dir(root).map_or(false, |entries| {
        entries.flatten().all(|e| e.file_name().to_string_lossy().starts_with('.'))
    })
}

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

/// A file in a reply, and the lines of the reply that give it: from the
/// opening fence, the first indented line, or the heredoc's `cat` line, to
/// the line past its end.
pub struct Block {
    pub file: File,
    pub fence: usize,
    pub end: usize,
    /// A heredoc written with `>>`: its contents go at the end of the file.
    pub append: bool,
    /// Its end has come: its closing fence, or its heredoc's end word.
    pub closed: bool,
    /// An edit block: search/replace pairs for the file there, in place of
    /// its whole contents (`file.content` is then empty).
    pub edit: Option<Vec<Edit>>,
}

/// One change of an edit block: `search`, text of the file as it is,
/// becomes `replace`.
#[derive(Debug, Clone, PartialEq)]
pub struct Edit {
    pub search: String,
    pub replace: String,
    /// Other ways to split the pair, as (search, replace), when it has more
    /// than one `=======` line, as a markdown heading underlined with one
    /// gives it: the first whose search is in the file is made.
    pub splits: Vec<(String, String)>,
}

impl Edit {
    /// The edit of the lines `pair`, split at each of `dividers` in turn:
    /// the first is the edit, the rest are tried after it.
    fn split(pair: &[&str], dividers: &[usize]) -> Edit {
        let at = |d: usize| (pair[..d].join("\n"), pair[d + 1..].join("\n"));
        let (search, replace) = at(dividers[0]);
        Edit { search, replace, splits: dividers[1..].iter().map(|&d| at(d)).collect() }
    }
}

/// What a reply gives: its files, whole, with its edits made; the paths
/// it edited; and each edit that couldn't be made, by path, with why.
#[derive(Debug, Default)]
pub struct Extracted {
    pub files: Vec<File>,
    pub edited: Vec<String>,
    pub failed: Vec<(String, String)>,
}

/// The files in `reply`: each fenced block whose path is on the line above
/// it, in its info string (```` ```rust src/main.rs ````), or in a comment
/// on its first line, and each `cat > path <<'EOF'` heredoc of a shell
/// block. A path given twice keeps its last contents. A heredoc written
/// with `>>` adds to what's before it in the reply, or else to nothing: see
/// `extract_in` for the file it adds to.
#[cfg(test)]
pub fn extract(reply: &str) -> Vec<File> {
    gather(reply, |_| None).files
}

/// `extract` for a reply about the folder `root`: a heredoc written with
/// `>>` adds to the file there when the reply hasn't given it before, and
/// an edit block changes it, or what the reply gave of it before.
#[cfg(test)]
pub fn extract_in(root: &Path, reply: &str) -> Extracted {
    gather(reply, |path| fs::read_to_string(root.join(path)).ok())
}

/// `extract` for a reply about files as `before` gives them, by path, as
/// they were before any of its files were written.
pub fn extract_with(reply: &str, before: impl Fn(&str) -> Option<String>) -> Extracted {
    gather(reply, before)
}

fn gather(reply: &str, on_disk: impl Fn(&str) -> Option<String>) -> Extracted {
    let mut files: Vec<File> = Vec::new();
    let (mut edited, mut failed) = (Vec::new(), Vec::new());
    for block in blocks(reply) {
        let mut file = block.file;
        if let Some(edits) = &block.edit {
            let before = files.iter().find(|f| f.path == file.path).map(|f| f.content.clone()).or_else(|| on_disk(&file.path));
            match apply_edits(before.as_deref(), edits) {
                Ok(after) => file.content = after,
                Err(why) => {
                    failed.push((file.path, why));
                    continue;
                }
            }
            if !edited.contains(&file.path) {
                edited.push(file.path.clone());
            }
        } else if block.append {
            let before = files.iter().find(|f| f.path == file.path).map(|f| f.content.clone()).or_else(|| on_disk(&file.path));
            if let Some(before) = before {
                file.content = before + &file.content;
            }
        }
        files.retain(|f| f.path != file.path);
        files.push(file);
    }
    Extracted { files, edited, failed }
}

/// A line opening an edit's search text: `<<<<<<< SEARCH`.
pub(crate) fn is_search(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("<<<<<<<") && t.trim_start_matches('<').trim() == "SEARCH"
}

/// A line ending an edit's replacement: `>>>>>>> REPLACE`.
fn is_replace(line: &str) -> bool {
    let t = line.trim();
    t.starts_with(">>>>>>>") && t.trim_start_matches('>').trim() == "REPLACE"
}

/// A line between an edit's search and its replacement: `=======`, seven
/// and no more, so a markdown heading's longer underline isn't taken for it.
fn is_divider(line: &str) -> bool {
    line.trim() == "======="
}

/// An edit given bare, its SEARCH/REPLACE pairs outside any fence, as
/// models also write it, when `lines[k]` opens it: the index past its
/// last pair (past the reply while one is still coming in), and whether
/// it has ended, its last pair done and something else after it.
pub(crate) fn bare_edit(lines: &[&str], k: usize) -> Option<(usize, bool)> {
    if !is_search(lines.get(k)?) {
        return None;
    }
    let mut at = k;
    loop {
        let Some(divider) = (at + 1..lines.len()).find(|&j| is_divider(lines[j])) else { return Some((lines.len(), false)) };
        let Some(replace) = (divider + 1..lines.len()).find(|&j| is_replace(lines[j])) else { return Some((lines.len(), false)) };
        let next = (replace + 1..lines.len()).find(|&j| !lines[j].trim().is_empty());
        match next {
            Some(j) if is_search(lines[j]) => at = j,
            Some(_) => return Some((replace + 1, true)),
            None => return Some((replace + 1, false)),
        }
    }
}

/// The pairs of an edit block's `body` given with no SEARCH lines: each
/// runs to a REPLACE line or the body's end and has an `=======`. None
/// when one hasn't.
fn markerless_edits(body: &[&str]) -> Vec<Edit> {
    let mut out = Vec::new();
    let mut from = 0;
    while from < body.len() {
        let to = (from..body.len()).find(|&j| is_replace(body[j])).unwrap_or(body.len());
        let chunk = &body[from..to];
        from = to + 1;
        if chunk.iter().all(|l| l.trim().is_empty()) {
            continue;
        }
        let lead = chunk.iter().take_while(|l| l.trim().is_empty()).count();
        let chunk = &chunk[lead..];
        let dividers: Vec<usize> = (0..chunk.len()).filter(|&k| is_divider(chunk[k])).collect();
        if dividers.is_empty() {
            return Vec::new();
        }
        out.push(Edit::split(chunk, &dividers));
    }
    out
}

/// The search/replace pairs of an edit block's `body` that have ended;
/// one still coming in isn't among them.
pub fn edits(body: &[&str]) -> Vec<Edit> {
    let mut out = Vec::new();
    let mut k = 0;
    while k < body.len() {
        if !is_search(body[k]) {
            k += 1;
            continue;
        }
        let Some(end) = (k + 1..body.len()).find(|&j| is_replace(body[j])) else { break };
        let pair = &body[k + 1..end];
        let dividers: Vec<usize> = (0..pair.len()).filter(|&j| is_divider(pair[j])).collect();
        if !dividers.is_empty() {
            out.push(Edit::split(pair, &dividers));
        }
        k = end + 1;
    }
    out
}

/// `old`, a file's contents (none when it isn't there), with `edits`
/// made in turn; or why one couldn't be. Each edit's search text must be
/// found once: as it is, else line by line ignoring the whitespace around
/// each, else ignoring all whitespace, else by its first and last lines,
/// as opencode finds an edit. An empty search makes a file that isn't
/// there.
pub fn apply_edits(old: Option<&str>, edits: &[Edit]) -> Result<String, String> {
    if edits.is_empty() {
        return Err("the edit block has no complete SEARCH/REPLACE pair".to_string());
    }
    let (mut text, rest) = match old {
        Some(old) => (old.to_string(), edits),
        None if edits[0].search.trim().is_empty() => (format!("{}\n", edits[0].replace), &edits[1..]),
        None => return Err("the file isn't there; give a new file whole as a heredoc".to_string()),
    };
    for (n, edit) in rest.iter().enumerate() {
        let which = if edits.len() > 1 { format!("edit {} of {}", n + 1 + edits.len() - rest.len(), edits.len()) } else { "the edit".to_string() };
        if edit.search.trim().is_empty() {
            return Err(format!("{which} has an empty SEARCH, and the file is there"));
        }
        // Split more than one way, the split made is the one with the
        // most search text found in the file; if none is, why the first
        // couldn't be made.
        let first = replace_once(&text, edit);
        let best = edit
            .splits
            .iter()
            .filter(|(search, _)| !search.trim().is_empty())
            .filter_map(|(search, replace)| {
                let other = Edit { search: search.clone(), replace: replace.clone(), splits: Vec::new() };
                replace_once(&text, &other).ok().map(|t| (search.len(), t))
            })
            .chain(first.clone().ok().map(|t| (edit.search.len(), t)))
            .max_by_key(|(len, _)| *len);
        // The first split made, it's among them: none found, it failed.
        text = match best {
            Some((_, t)) => t,
            None => return Err(format!("{which}: {}", first.unwrap_err())),
        };
    }
    Ok(text)
}

/// Why an edit couldn't be made, as `apply_edits` tells the agent, put
/// for the person reading along: what's wrong with the change, not the
/// format it was to be written in.
pub fn plainly(why: &str) -> String {
    let (which, rest) = match why.split_once(": ") {
        Some((which, rest)) if which == "the edit" || which.starts_with("edit ") => (which.replacen("edit", "change", 1), rest),
        _ => (String::new(), why),
    };
    let what = if rest.starts_with("the edit block has no complete") {
        "the change came in a form that can't be read".to_string()
    } else if rest.starts_with("the file isn't there") {
        "the file isn't there".to_string()
    } else if rest.contains("has an empty SEARCH") {
        let which = rest.split(" has an empty").next().unwrap_or("").replacen("edit", "change", 1);
        return format!("{which} doesn't say which lines it replaces");
    } else if let Some(n) = rest.strip_prefix("its SEARCH is found ").and_then(|r| r.split(' ').next()) {
        format!("the lines it replaces are in the file {n} times, so it's unclear which to change")
    } else if rest.starts_with("its SEARCH isn't in the file") {
        "the lines it replaces aren't in the file as it is now".to_string()
    } else {
        rest.to_string()
    };
    if which.is_empty() || which == "the change" { what } else { format!("{which}: {what}") }
}

/// `text` with `edit`'s search text, found once, replaced.
fn replace_once(text: &str, edit: &Edit) -> Result<String, String> {
    let (search, replace) = (edit.search.as_str(), edit.replace.as_str());
    match text.matches(search).count() {
        1 => return Ok(text.replacen(search, replace, 1)),
        0 => {}
        n => return Err(format!("its SEARCH is found {n} times; give more lines around it so it's found once")),
    }
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let wanted: Vec<&str> = search.lines().collect();
    let squeeze = |s: &str| s.split_whitespace().collect::<String>();
    let trimmed = |a: &str, b: &str| a.trim() == b.trim();
    let spaced = |a: &str, b: &str| squeeze(a) == squeeze(b);
    for same in [&trimmed as &dyn Fn(&str, &str) -> bool, &spaced] {
        let found: Vec<usize> = (0..lines.len().saturating_sub(wanted.len() - 1))
            .filter(|&i| wanted.iter().enumerate().all(|(j, w)| same(lines[i + j], w)))
            .collect();
        match found.as_slice() {
            [i] => return Ok(splice(&lines, *i, *i + wanted.len(), replace)),
            [] => {}
            many => return Err(format!("its SEARCH is found {} times; give more lines around it so it's found once", many.len())),
        }
    }
    // By its first and last lines, with as many lines between, give or
    // take a quarter: the middle may differ a little from the file's.
    if wanted.len() >= 3 {
        let (first, last) = (wanted[0].trim(), wanted[wanted.len() - 1].trim());
        let slack = wanted.len() / 4;
        let found: Vec<(usize, usize)> = (0..lines.len())
            .filter(|&i| !first.is_empty() && lines[i].trim() == first)
            .filter_map(|i| {
                let lo = i + wanted.len() - 1 - slack.min(wanted.len() - 2);
                (lo..=(i + wanted.len() - 1 + slack).min(lines.len().saturating_sub(1))).find(|&j| lines[j].trim() == last).map(|j| (i, j))
            })
            .collect();
        if let [(i, j)] = found.as_slice() {
            return Ok(splice(&lines, *i, j + 1, replace));
        }
    }
    Err("its SEARCH isn't in the file".to_string())
}

/// `lines` with those from `from` to `to` put in place by `replace`.
fn splice(lines: &[&str], from: usize, to: usize, replace: &str) -> String {
    let mut out: String = lines[..from].concat();
    if !replace.is_empty() {
        out.push_str(replace);
        // The lines taken out ended a line; so does what goes in.
        if lines[to - 1].ends_with('\n') || to < lines.len() {
            out.push('\n');
        }
    }
    out.push_str(&lines[to..].concat());
    out
}

/// Every block of `reply` that gives a file, in order, with its lines.
pub fn blocks(reply: &str) -> Vec<Block> {
    let lines: Vec<&str> = reply.lines().collect();
    let mut blocks: Vec<Block> = Vec::new();
    let mut above: Option<(usize, &str)> = None;
    // A heading naming a path since the last block: it names the next code
    // block even with a paragraph about the file between them.
    let mut heading: Option<String> = None;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let at = i;
        i += 1;
        let Some((fence, info)) = opening_fence(line) else {
            // SEARCH/REPLACE pairs with no fence around them: an edit of
            // the file named on the line above.
            if let Some((end, closed)) = bare_edit(&lines, at) {
                if let Some((_, label)) = above
                    && let Some(path) = label_path(label)
                {
                    let file = File { path, content: String::new() };
                    blocks.push(Block { file, fence: at, end, append: false, closed, edit: Some(edits(&lines[at..end])) });
                }
                i = end;
                above = None;
                continue;
            }
            // A path on a line of its own, then a block indented by four
            // spaces rather than fenced, as markdown also has it, is that
            // file too.
            if let Some(path) = label_path(line).or_else(|| file_label(line))
                && let Some((content, next)) = indented_block(&lines, i)
            {
                let fence = (i..next).find(|k| !lines[*k].trim().is_empty()).unwrap_or(i);
                blocks.push(Block { file: File { path, content }, fence, end: next, append: false, closed: true, edit: None });
                i = next;
                above = None;
                continue;
            }
            if !line.trim().is_empty() {
                above = Some((at, line));
                if line.trim_start().starts_with('#')
                    && let Some(path) = label_path(line)
                {
                    heading = Some(path);
                }
            }
            continue;
        };
        let heading = heading.take();
        let start = i;
        let close = close_of(&lines, at, &fence, info);
        let mut body: Vec<&str> = lines[start..close.unwrap_or(lines.len())].to_vec();
        // Past the closing fence, when there is one.
        i = close.map_or(lines.len(), |c| c + 1);
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
        // An edit block's path on a line of its own ahead of its first
        // SEARCH, as models also write it: ```` ```edit ````, then the path.
        if path.is_none()
            && let Some(first) = body.iter().position(|l| !l.trim().is_empty())
            && !is_search(body[first])
            && body[first..].iter().any(|l| is_search(l))
            && let Some(named) = label_path(body[first])
        {
            path = Some(named);
            body.drain(..=first);
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
        // Last, a heading a paragraph or so up; never for a shell block,
        // whose heredocs name their own files.
        if path.is_none() && !heredoc_lang(info) {
            path = heading;
        }
        above = None;
        // A block that opens with a heredoc writing a file is that heredoc,
        // however the block is marked or named: `**build.mjs**` over a
        // ```` ```sh ```` block of `cat > build.mjs <<'EOF'`, or a ```` ```js ````
        // block wrapped the same way, writes the heredoc's body, never the
        // `cat` line and end word around it.
        let wrapped = body
            .iter()
            .find(|l| !l.trim().is_empty())
            .is_some_and(|l| heredoc_word(l).is_some() && heredoc_target(l).is_some());
        let path = path.filter(|_| !wrapped);
        let Some(path) = path else {
            // A shell block names no file of its own; its heredocs do.
            if wrapped || heredoc_lang(info) {
                for doc in heredocs(&body) {
                    blocks.push(Block {
                        file: File { path: doc.path, content: doc.content },
                        fence: start + doc.from,
                        end: start + doc.to,
                        append: doc.append,
                        closed: doc.ended || close.is_some(),
                        edit: None,
                    });
                }
            }
            continue;
        };
        // Search/replace pairs, not the file whole: an edit block.
        if body.iter().any(|l| is_search(l)) {
            let file = File { path, content: String::new() };
            blocks.push(Block { file, fence: at, end: i, append: false, closed: close.is_some(), edit: Some(edits(&body)) });
            continue;
        }
        // A block marked `edit` is never the file whole, markers or not.
        // Without its SEARCH lines, as models also write it, each pair
        // starts where the block or the last REPLACE line leaves off: what
        // to find above its `=======`, what goes in its place below it, to
        // its REPLACE line or the block's end. Anything else is no edit,
        // and asked for again.
        if info.split_whitespace().next() == Some("edit") {
            let edit = if close.is_some() { markerless_edits(&body) } else { Vec::new() };
            let file = File { path, content: String::new() };
            blocks.push(Block { file, fence: at, end: i, append: false, closed: close.is_some(), edit: Some(edit) });
            continue;
        }
        let mut content = body.join("\n");
        content.push('\n');
        blocks.push(Block { file: File { path, content }, fence: at, end: i, append: false, closed: close.is_some(), edit: None });
    }
    blocks
}

/// A heredoc of a shell block that writes a file, by its lines in the
/// block: `from` its `cat` line to `to`, past its end word.
struct Heredoc {
    path: String,
    content: String,
    append: bool,
    from: usize,
    to: usize,
    /// Its end word has come.
    ended: bool,
}

/// The heredocs in a shell block's `body` that write a file.
fn heredocs(body: &[&str]) -> Vec<Heredoc> {
    let mut docs = Vec::new();
    let mut k = 0;
    while k < body.len() {
        let line = body[k];
        k += 1;
        let Some((word, tabs)) = heredoc_word(line) else { continue };
        let from = k - 1;
        let mut end = k;
        while end < body.len() && !ends_heredoc(body[end], &word) {
            end += 1;
        }
        let ended = end < body.len();
        let to = if ended { end + 1 } else { end };
        if let Some((path, append)) = heredoc_target(line) {
            let content: String = body[k..end]
                .iter()
                .map(|l| if tabs { l.trim_start_matches('\t') } else { l })
                .flat_map(|l| [l, "\n"])
                .collect();
            docs.push(Heredoc { path, content, append, from, to, ended });
        }
        k = to;
    }
    docs
}

/// Info strings of a block whose heredocs can write files: a shell block,
/// or one that names no language.
fn heredoc_lang(info: &str) -> bool {
    let lang = info.split_whitespace().next().unwrap_or("").to_lowercase();
    lang.is_empty() || SHELL_LANGS.contains(&lang.as_str())
}

/// The end word of a heredoc `line` starts (`<<EOF`, `<<'EOF'`,
/// `<<-"EOF"`), and whether its body's leading tabs go (`<<-`).
fn heredoc_word(line: &str) -> Option<(String, bool)> {
    if line.contains("$((") {
        return None;
    }
    let at = line.find("<<")?;
    let rest = &line[at + 2..];
    if rest.starts_with('<') || line[..at].ends_with('<') {
        return None;
    }
    let (tabs, rest) = match rest.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, rest),
    };
    let word: String =
        rest.trim_start().chars().take_while(|c| !c.is_whitespace() && !matches!(c, ';' | '|' | '&' | '>' | '<' | ')')).collect();
    let word = word.trim_start_matches('\\').trim_matches(|c| c == '\'' || c == '"');
    let named = word.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && word.chars().all(|c| c.is_alphanumeric() || c == '_');
    named.then(|| (word.to_string(), tabs))
}

/// Whether `line` ends the heredoc whose end word is `word`.
fn ends_heredoc(line: &str, word: &str) -> bool {
    line.trim() == word
}

/// The file a heredoc `line` writes, and whether it adds to it (`>>`):
/// `cat > path <<'EOF'`, `cat <<EOF >> path`, `tee path <<EOF`, after any
/// `mkdir -p dir &&`. None for a heredoc that does anything else.
fn heredoc_target(line: &str) -> Option<(String, bool)> {
    let mut parts: Vec<&str> = line.split("&&").map(str::trim).collect();
    let last = parts.pop()?;
    if parts.iter().any(|p| !p.starts_with("mkdir ")) {
        return None;
    }
    if last.contains(['|', ';', '`', '$']) {
        return None;
    }
    // The heredoc's own part goes: `<<`, `-`, and the word, quoted or not.
    let at = last.find("<<")?;
    let rest = last[at + 2..].trim_start_matches('-').trim_start();
    let word_len = rest.find(|c: char| c.is_whitespace() || c == '>').unwrap_or(rest.len());
    let command = format!("{} {}", &last[..at], &rest[word_len..]);
    let mut words: Vec<String> = Vec::new();
    for word in command.split_whitespace() {
        // `>path` and `>>path` as two words.
        let (op, rest) = if let Some(rest) = word.strip_prefix(">>") {
            (">>", rest)
        } else if let Some(rest) = word.strip_prefix('>') {
            (">", rest)
        } else {
            ("", word)
        };
        words.extend([op, rest].into_iter().filter(|w| !w.is_empty()).map(|w| w.trim_matches(['\'', '"']).to_string()));
    }
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    let (path, append) = match words.as_slice() {
        ["cat", ">", path] => (*path, false),
        ["cat", ">>", path] => (*path, true),
        ["tee", rest @ ..] => {
            let append = rest.iter().any(|w| matches!(*w, "-a" | "--append"));
            let rest: Vec<&str> = rest.iter().copied().filter(|w| !matches!(*w, "-a" | "--append")).collect();
            match rest.as_slice() {
                [path] | [path, ">", "/dev/null"] => (*path, append),
                _ => return None,
            }
        }
        _ => return None,
    };
    as_path(path).map(|path| (path, append))
}

/// The line that closes the block opened at `lines[open]`, by `fence`;
/// none while it's open. In a shell block, a fence line in a heredoc is
/// the heredoc's, so long as the heredoc's end word comes after it, as a
/// README written with `cat` has its own code blocks.
pub(crate) fn close_of(lines: &[&str], open: usize, fence: &str, info: &str) -> Option<usize> {
    let shell = heredoc_lang(info);
    let mut doc: Option<String> = None;
    // Inside a SEARCH/REPLACE pair: a fence there is the edited file's,
    // as a README's code block is, so long as a REPLACE line is to come.
    let mut pair = false;
    for i in open + 1..lines.len() {
        let line = lines[i];
        if pair {
            if is_replace(line) {
                pair = false;
            } else if closes(line, fence) && !lines[i + 1..].iter().any(|l| is_replace(l)) {
                return Some(i);
            }
            continue;
        }
        if is_search(line) {
            pair = true;
            continue;
        }
        if let Some(word) = &doc {
            if ends_heredoc(line, word) {
                doc = None;
            } else if closes(line, fence) && !lines[i + 1..].iter().any(|l| ends_heredoc(l, word)) {
                return Some(i);
            }
            continue;
        }
        if closes(line, fence) {
            return Some(i);
        }
        if shell {
            doc = heredoc_word(line).map(|(word, _)| word);
        }
    }
    None
}

/// The shell commands `reply` gives to run, one script per fenced block
/// marked as shell (`sh`, `bash`, `zsh`, `shell`, `console`, `terminal`)
/// that names no file, in order. A block written as a transcript, with
/// `$ ` ahead of each command, keeps the commands and not what they
/// printed. A block with a path above or in its fence is a file to write,
/// not a script to run; so is a heredoc that writes a file, which is taken
/// out of its script.
pub fn commands(reply: &str) -> Vec<String> {
    let files: Vec<usize> = blocks(reply).iter().map(|b| b.fence).collect();
    let lines: Vec<&str> = reply.lines().collect();
    let mut scripts = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let at = i;
        i += 1;
        let Some((fence, info)) = opening_fence(lines[at]) else { continue };
        let start = i;
        let close = close_of(&lines, at, &fence, info);
        let body = &lines[start..close.unwrap_or(lines.len())];
        i = close.map_or(lines.len(), |c| c + 1);
        let lang = info.split_whitespace().next().unwrap_or("").to_lowercase();
        if files.contains(&at) || !SHELL_LANGS.contains(&lang.as_str()) {
            continue;
        }
        let written: Vec<(usize, usize)> = heredocs(body).iter().map(|d| (d.from, d.to)).collect();
        let body: Vec<&str> = body
            .iter()
            .enumerate()
            .filter(|(k, _)| !written.iter().any(|(from, to)| (*from..*to).contains(k)))
            .map(|(_, l)| *l)
            .collect();
        // A transcript, with prompts, shows what each command printed too:
        // only the commands are run.
        let prompted = |l: &str| l.starts_with("$ ") || l == "$";
        let transcript = body.iter().any(|l| prompted(l));
        let script: Vec<&str> = body
            .iter()
            .map(|l| l.trim_end())
            .filter(|l| !transcript || prompted(l))
            .map(|l| l.strip_prefix('$').map_or(l, str::trim_start))
            .collect();
        let script = script.join("\n");
        if script.lines().any(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#')) {
            scripts.push(script.trim().to_string());
        }
    }
    scripts
}

/// Info-string languages that mark a block as shell commands to run.
const SHELL_LANGS: [&str; 6] = ["sh", "bash", "zsh", "shell", "console", "terminal"];

/// Whether `reply` holds tool-call markup a model wrote as text (see
/// [`tool_calls_as_blocks`]).
pub fn has_tool_call_markup(reply: &str) -> bool {
    reply.contains(TOOL_CALL_OPEN) || control_token_at(reply).is_some()
}

/// `reply` with each tool call a model wrote as text turned into what docent
/// runs: a fenced `bash` block for a shell call, nothing for any other.
///
/// Agent-trained models (MiniMax, say) answer "let me look around" with
/// their own native call, `<tool_call>{"name": "bash", "arguments":
/// {"command": "ls"}}</tool_call>`, wrapped in control tokens such as
/// `]<]minimax[>[`, when no tools were offered. Docent runs the commands in
/// a reply's shell blocks, so the call becomes one; the control tokens go.
/// A call still streaming (no closing tag yet) is left for the next pass.
pub fn tool_calls_as_blocks(reply: &str) -> String {
    let text = strip_control_tokens(reply);
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(open) = rest.find(TOOL_CALL_OPEN) {
        let Some(len) = rest[open..].find(TOOL_CALL_CLOSE) else { break };
        let close = open + len;
        out.push_str(&rest[..open]);
        if let Some(command) = shell_call(&rest[open + TOOL_CALL_OPEN.len()..close]) {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("```bash\n");
            out.push_str(command.trim_end());
            out.push_str("\n```\n");
        }
        rest = rest[close + TOOL_CALL_CLOSE.len()..].trim_start_matches([' ', '\t', '\n']);
    }
    out.push_str(rest);
    out
}

const TOOL_CALL_OPEN: &str = "<tool_call>";
const TOOL_CALL_CLOSE: &str = "</tool_call>";

/// Tool names that mean "run this in a shell".
const SHELL_TOOLS: [&str; 9] = ["bash", "sh", "shell", "zsh", "terminal", "run", "run_command", "execute", "exec"];

/// The command of a tool call's JSON `body` when it is a shell call:
/// `{"name": "bash", "arguments": {"command": "..."}}`, with `arguments`
/// as an object or a JSON string, and `cmd` or `script` for `command`.
fn shell_call(body: &str) -> Option<String> {
    let call: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    let name = call.get("name")?.as_str()?.to_lowercase();
    if !SHELL_TOOLS.contains(&name.as_str()) {
        return None;
    }
    let args = match call.get("arguments").or_else(|| call.get("parameters"))? {
        serde_json::Value::String(s) => serde_json::from_str(s).ok()?,
        args => args.clone(),
    };
    ["command", "cmd", "script"]
        .iter()
        .find_map(|k| args.get(k).and_then(serde_json::Value::as_str))
        .map(str::to_string)
}

/// `text` without a model's control tokens: `]<]word[>[`.
fn strip_control_tokens(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((at, len)) = control_token_at(rest) {
        out.push_str(&rest[..at]);
        rest = &rest[at + len..];
    }
    out.push_str(rest);
    out
}

/// Where the first `]<]word[>[` control token in `text` starts, and its length.
fn control_token_at(text: &str) -> Option<(usize, usize)> {
    let mut from = 0;
    while let Some(i) = text[from..].find("]<]") {
        let at = from + i;
        let word = &text[at + 3..];
        let n = word.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(word.len());
        if word[n..].starts_with("[>[") {
            return Some((at, 3 + n + 3));
        }
        from = at + 3;
    }
    None
}

/// Whether `block`, a fenced block's text from its opening fence, has its
/// closing fence: not while it streams.
pub fn block_closed(block: &str) -> bool {
    let lines: Vec<&str> = block.lines().collect();
    let Some((fence, info)) = lines.first().and_then(|l| opening_fence(l)) else { return false };
    close_of(&lines, 0, &fence, info).is_some()
}

/// Lines of context shown around a change in a diff.
const CONTEXT: usize = 3;
/// Lines of a new file shown before the rest is folded.
const NEW_SHOWN: usize = 40;

/// The change a reply makes to a file, by lines, as opencode shows an
/// edit: hunks with `@@` headers, each line numbered in the old and the
/// new file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    pub path: String,
    /// New: there was no old file.
    pub created: bool,
    pub added: usize,
    pub removed: usize,
    pub lines: Vec<DiffLine>,
}

/// A line of a `Diff`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    /// `@@ -old,count +new,count @@`: where the next lines are in each file.
    Hunk { old: (usize, usize), new: (usize, usize) },
    /// Kept, with its number in the old and the new file.
    Keep(usize, usize, String),
    /// Put in, with its number in the new file.
    Add(usize, String),
    /// Taken out, with its number in the old file.
    Remove(usize, String),
    /// Lines of a new file past `NEW_SHOWN`, folded.
    Fold(usize),
}

impl std::fmt::Display for Diff {
    /// As a unified diff, under a line naming the file and its counts.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.created {
            writeln!(f, "{} · created, +{}", self.path, self.added)?;
        } else {
            writeln!(f, "{} · updated, +{} −{}", self.path, self.added, self.removed)?;
        }
        for line in &self.lines {
            match line {
                DiffLine::Hunk { old, new } => writeln!(f, "@@ -{},{} +{},{} @@", old.0, old.1, new.0, new.1)?,
                DiffLine::Keep(_, _, text) => writeln!(f, " {}", text)?,
                DiffLine::Add(_, text) => writeln!(f, "+{}", text)?,
                DiffLine::Remove(_, text) => writeln!(f, "-{}", text)?,
                DiffLine::Fold(n) => writeln!(f, "… {} more lines", n)?,
            }
        }
        Ok(())
    }
}

/// Words that, in a comment of a file a reply gives with `...` in it, say
/// lines were left out: `// ... rest of the file unchanged`.
const LEFT_OUT: &[&str] =
    &["rest", "unchanged", "existing", "same as", "remain", "previous", "omitted", "as before", "other", "more", "etc"];

/// Whether a reply's `new` contents of a file leave lines out, a comment
/// such as `// ... rest unchanged` standing in for them, where the `old`
/// file there has no such line: so not a file to write over it.
pub fn elides(old: &str, new: &str) -> bool {
    let had: std::collections::HashSet<&str> = old.lines().map(str::trim).collect();
    new.lines().map(str::trim).filter(|l| !had.contains(l)).any(|line| {
        let Some(said) = ["//", "#", "/*", "<!--", "--", ";", "*", "{/*"].iter().find_map(|c| line.strip_prefix(c)) else {
            return false;
        };
        let said = said.to_lowercase();
        (said.contains("...") || said.contains('…')) && LEFT_OUT.iter().any(|w| said.contains(w))
    })
}

/// Why a file `elides` lines, as told to the agent when it's asked for
/// the file again.
pub const ELIDED: &str = "it leaves lines out, a comment standing in for them, so it wasn't written over the file";

/// Whether a reply's `new` contents of a file hold less than the `old`
/// file there: not all of it, so not a file to write over it.
pub fn partial(old: &str, new: &str) -> bool {
    new.len() < old.len() / 10 * 9
}

/// The change from `old`, the file as it is (none when it's new), to
/// `new`, the file as a reply gives it, by lines: `+` and `-` lines with
/// `CONTEXT` lines around them, in hunks. A new file is all `+`, folded
/// past `NEW_SHOWN` lines. While the reply is `streaming` the block, what's
/// left of the old file past the new one's end isn't taken out yet, so it
/// doesn't show as removed.
pub fn diff(path: &str, old: Option<&str>, new: &str, streaming: bool) -> Diff {
    let b: Vec<&str> = new.lines().collect();
    let mut diff = Diff { path: path.to_string(), created: old.is_none(), added: 0, removed: 0, lines: Vec::new() };
    let Some(old) = old else {
        diff.added = b.len();
        if !b.is_empty() {
            diff.lines.push(DiffLine::Hunk { old: (0, 0), new: (1, b.len()) });
        }
        for (i, line) in b.iter().enumerate().take(NEW_SHOWN) {
            diff.lines.push(DiffLine::Add(i + 1, line.to_string()));
        }
        if b.len() > NEW_SHOWN {
            diff.lines.push(DiffLine::Fold(b.len() - NEW_SHOWN));
        }
        return diff;
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
    diff.added = ops.iter().filter(|(c, _)| *c == '+').count();
    diff.removed = ops.iter().filter(|(c, _)| *c == '-').count();
    let changed: Vec<bool> = ops.iter().map(|(c, _)| *c != ' ').collect();
    let keep = |i: usize| (i.saturating_sub(CONTEXT)..(i + CONTEXT + 1).min(ops.len())).any(|k| changed[k]);
    // Each run of kept lines is a hunk, headed by where it starts in each
    // file and how many lines of each it holds.
    let (mut old_no, mut new_no) = (1, 1);
    let mut hunk: Option<usize> = None;
    for (i, (c, line)) in ops.iter().enumerate() {
        if keep(i) {
            if hunk.is_none() {
                hunk = Some(diff.lines.len());
                diff.lines.push(DiffLine::Hunk { old: (old_no, 0), new: (new_no, 0) });
            }
            let at = hunk.unwrap();
            let DiffLine::Hunk { old, new } = &mut diff.lines[at] else { unreachable!() };
            match c {
                '+' => {
                    new.1 += 1;
                    diff.lines.push(DiffLine::Add(new_no, line.to_string()));
                }
                '-' => {
                    old.1 += 1;
                    diff.lines.push(DiffLine::Remove(old_no, line.to_string()));
                }
                _ => {
                    old.1 += 1;
                    new.1 += 1;
                    diff.lines.push(DiffLine::Keep(old_no, new_no, line.to_string()));
                }
            }
        } else {
            hunk = None;
        }
        match c {
            '+' => new_no += 1,
            '-' => old_no += 1,
            _ => {
                old_no += 1;
                new_no += 1;
            }
        }
    }
    diff
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
    let text = text.trim_matches(|c: char| matches!(c, '*' | '_' | '`' | ':' | '(' | ')') || c.is_whitespace());
    as_path(text).or_else(|| described_path(text))
}

/// A path ahead of a note on it, as small models write one: `Cargo.toml -
/// Edit block`, `src/main.rs (updated)`.
fn described_path(text: &str) -> Option<String> {
    let (path, _) = [" - ", " — ", " – ", " ("].iter().filter_map(|sep| text.split_once(sep)).min_by_key(|(p, _)| p.len())?;
    as_path(path.trim().trim_matches(|c: char| matches!(c, '*' | '_' | '`' | ':')))
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
    let inner = ["File:", "file:", "Filename:"].iter().find_map(|l| inner.strip_prefix(l)).unwrap_or(inner).trim();
    as_path(inner).or_else(|| described_path(inner))
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
    /// file can't be all of it, so it's left as it is.
    pub kept: Vec<File>,
    /// There, and a comment in the reply stands in for lines of it: see
    /// `elides`. Left as it is.
    pub elided: Vec<File>,
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
                Ok(text) if elides(&text, &file.content) => plan.elided.push(file),
                Ok(text) if !whole.contains(&file.path) && partial(&text, &file.content) => plan.kept.push(file),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_heading_names_the_block_after_a_paragraph_about_it() {
        let reply = "Here is the app.\n\n### File: `main.rs`\nThis file reads a line and prints it.\n\n```rust\nfn main() {}\n```\n";
        assert_eq!(paths(reply), ["main.rs"]);
        // Named once: a second block under no heading of its own is no file.
        let twice = format!("{reply}\nRun it:\n\n```rust\nfn other() {{}}\n```\n");
        assert_eq!(paths(&twice), ["main.rs"]);
        // A shell block under it is commands; its heredoc names the file.
        let shell = "### File: `main.rs`\nWrite it with:\n\n```sh\ncat > src/main.rs <<'EOF'\nfn main() {}\nEOF\n```\n";
        assert_eq!(paths(shell), ["src/main.rs"]);
    }

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
    fn shell_blocks_without_a_path_are_commands_to_run() {
        let reply = "Committing it:\n\n```bash\ngit add -A\ngit commit -m \"feat: tags\"\n```\n\nthen\n\n```sh\n$ git push\n```\n\n\
            `scripts/run.sh`\n```sh\necho a file\n```\n\n```\nplain\n```\n\n```sh\n# only a comment\n```\n";
        assert_eq!(commands(reply), ["git add -A\ngit commit -m \"feat: tags\"", "git push"]);
        assert_eq!(extract(reply).len(), 1);
        assert!(commands("No commands here\n").is_empty());
        let transcript = "```console\n$ git status\nOn branch main\nnothing to commit\n$ git push\n```\n";
        assert_eq!(commands(transcript), ["git status\ngit push"]);
        assert_eq!(
            placeholders("git remote add origin https://github.com/<your-username>/todo-app.git\ngit push -u origin main"),
            ["<your-username>"]
        );
        assert_eq!(placeholders("export TOKEN=YOUR_TOKEN_HERE"), ["YOUR_TOKEN_HERE"]);
        assert_eq!(placeholders("sort < names.txt > out.txt && a <branch-name> c"), ["<branch-name>"]);
        assert_eq!(placeholders("export KEY=<API KEY> && cd <PATH>"), ["<API KEY>", "<PATH>"]);
        assert_eq!(placeholders("ssh <yourhost>"), ["<yourhost>"]);
        // HTML a command writes isn't a placeholder.
        assert!(placeholders("echo '<h1>Hello</h1><p>Hi</p><br/>' > index.html").is_empty());
        assert!(placeholders("cat > index.html <<EOF\n<div class=\"app\"><b>x</b></div>\nEOF").is_empty());
        assert!(placeholders("git push -u origin main").is_empty());
        assert!(placeholders("cat <<EOF > x\nhi\nEOF").is_empty());
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
    fn the_system_context_names_the_os_shell_and_tools_but_no_secrets() {
        let text = system_context();
        assert!(text.starts_with("The machine my commands run on:\n- OS: "), "{}", text);
        assert!(text.contains(std::env::consts::ARCH) && text.contains("- Shell: commands run with `"), "{}", text);
        // Only the listed variables: never a token from the environment.
        for (key, _) in std::env::vars() {
            if !ENV_VARS.contains(&key.as_str()) {
                let leaked = [": ", ", "].iter().any(|at| text.contains(&format!("{}{}=", at, key)));
                assert!(!leaked, "{} leaked: {}", key, text);
            }
        }
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
    fn rust_or_go_source_outside_a_project_needs_its_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let w = |p: &[&str]| p.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(missing_manifest(dir.path(), &w(&["src/main.rs"])), Some(("Cargo.toml", "Rust")));
        assert_eq!(missing_manifest(dir.path(), &w(&["README.md", "main.go"])), Some(("go.mod", "Go")));
        assert_eq!(missing_manifest(dir.path(), &w(&["app.py", "index.html"])), None);
        fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        assert_eq!(missing_manifest(dir.path(), &w(&["src/main.rs"])), None);
    }

    #[test]
    fn the_folder_note_names_the_folder_as_the_projects_root() {
        let note = folder_note(Path::new("/home/me/small-app"));
        assert!(note.starts_with("The working folder is `/home/me/small-app`, and it is the project's root"), "{note}");
        assert!(note.contains("don't `cd`"), "{note}");
        // No one language as the example, which a model then writes in.
        assert!(!note.contains("Rust") && !note.contains("cargo") && !note.contains(".rs"), "{note}");
    }

    #[test]
    fn a_folder_with_only_hidden_entries_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(is_empty_folder(dir.path()));
        fs::create_dir_all(dir.path().join(".docent")).unwrap();
        fs::write(dir.path().join(".docentignore"), "").unwrap();
        assert!(is_empty_folder(dir.path()));
        fs::write(dir.path().join("notes.md"), "hi").unwrap();
        assert!(!is_empty_folder(dir.path()));
        assert!(!is_empty_folder(&dir.path().join("missing")));
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
        let context = project_context(dir.path(), "Rust", "", DEFAULT_BUDGET);
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
        let text = project_context(dir.path(), "Rust", "Fix the status line in the news feed", DEFAULT_BUDGET);
        // A wider window takes the big file whole.
        let wide = project_context(dir.path(), "Rust", "Fix the status line in the news feed", MOST_BUDGET);
        assert!(wide.whole.contains(&"src/ui.rs".to_string()), "{:?}", wide.whole);
        assert_eq!(budget_for(None), DEFAULT_BUDGET);
        assert_eq!(budget_for(Some(32_768)), 65_536);
        assert_eq!(budget_for(Some(4_096)), DEFAULT_BUDGET);
        assert_eq!(budget_for(Some(1_048_576)), MOST_BUDGET);
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
            diff("x.rs", Some(old), new, false).to_string(),
            "x.rs · updated, +2 −1\n@@ -2,7 +2,7 @@\n b\n c\n d\n-e\n+E\n f\n g\n h\n@@ -10,3 +10,4 @@\n j\n k\n l\n+m\n"
        );
        let numbered = diff("x.rs", Some(old), new, false);
        assert_eq!(numbered.lines[4..6], [DiffLine::Remove(5, "e".into()), DiffLine::Add(5, "E".into())]);
        assert_eq!(numbered.lines[13], DiffLine::Add(13, "m".into()));
        assert_eq!(diff("x.rs", None, "x\ny\n", false).to_string(), "x.rs · created, +2\n@@ -0,0 +1,2 @@\n+x\n+y\n");
        let long: String = (1..=50).map(|i| format!("l{}\n", i)).collect();
        assert!(diff("x.rs", None, &long, true).to_string().ends_with("+l40\n… 10 more lines\n"));
        assert_eq!(diff("x.rs", Some("same\n"), "same\n", false).to_string(), "x.rs · updated, +0 −0\n");
        // Streaming, the old file's tail isn't removed yet; closed, it is.
        assert_eq!(diff("x.rs", Some("a\nb\nc\n"), "a\nB\n", true).to_string(), "x.rs · updated, +1 −1\n@@ -1,2 +1,2 @@\n a\n-b\n+B\n");
        assert_eq!(
            diff("x.rs", Some("a\nb\nc\n"), "a\nB\n", false).to_string(),
            "x.rs · updated, +1 −2\n@@ -1,3 +1,2 @@\n a\n-b\n-c\n+B\n"
        );
        // A reply with much less than the file isn't all of it.
        assert!(partial(old, "a\nb\n") && !partial(old, new));

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
        // Work in the folder that isn't a file is done by its commands.
        assert!(wants_files("install terraform"));
        assert!(wants_files("Run ./box.sh up"));
        assert!(wants_files("deploy it"));
    }

    #[test]
    fn heredocs_in_a_shell_block_are_files_and_not_commands() {
        let reply = "Done.\n\n```sh\nmkdir -p src && cat > src/main.rs <<'EOF'\nfn main() {}\nEOF\ncat >Cargo.toml << \"EOF\"\n[package]\nEOF\ngit add -A\n```\n";
        let files = extract(reply);
        assert_eq!(paths(reply), ["src/main.rs", "Cargo.toml"]);
        assert_eq!(files[0].content, "fn main() {}\n");
        assert_eq!(files[1].content, "[package]\n");
        assert_eq!(commands(reply), ["git add -A"]);
        let blocks = blocks(reply);
        assert_eq!((blocks[0].fence, blocks[0].end, blocks[0].closed), (3, 6, true));
    }

    #[test]
    fn a_heredoc_in_a_block_named_otherwise_writes_its_body_not_the_command() {
        let body = "cat > build.mjs <<'EOF'\nconsole.log(1);\nEOF\n";
        for reply in [
            format!("**build.mjs**\n\n```sh\n{body}```\n"),
            format!("### build.mjs\n\n```js\n{body}```\n"),
            format!("```js build.mjs\n{body}```\n"),
        ] {
            let files = extract(&reply);
            assert_eq!(files.len(), 1, "{reply}");
            assert_eq!((files[0].path.as_str(), files[0].content.as_str()), ("build.mjs", "console.log(1);\n"), "{reply}");
            assert!(commands(&reply).is_empty(), "{reply}");
        }
    }

    #[test]
    fn a_fence_inside_a_heredoc_is_the_files() {
        let reply = "```sh\ncat > README.md <<'EOF'\n# Hi\n```\ncargo run\n```\nEOF\n```\nafter";
        assert_eq!(extract(reply)[0].content, "# Hi\n```\ncargo run\n```\n");
        assert!(commands(reply).is_empty());
        // Its end word not come yet, the fence still closes the block.
        let cut = "```sh\ncat > a.txt <<EOF\nhi\n```\nmore prose";
        assert_eq!(extract(cut)[0].content, "hi\n");
        assert!(blocks(cut)[0].closed);
    }

    #[test]
    fn a_heredoc_streaming_in_is_open() {
        let blocks = blocks("```sh\ncat > a.txt <<'EOF'\none\ntw");
        assert_eq!(blocks[0].file.content, "one\ntw\n");
        assert!(!blocks[0].closed);
    }

    #[test]
    fn a_fence_inside_an_edit_is_the_files() {
        let readme = "# App\n\n```sh\nnpm start\n```\n";
        let reply = "```edit README.md\n<<<<<<< SEARCH\n```sh\nnpm start\n```\n=======\n```sh\nnpm run dev\n```\n>>>>>>> REPLACE\n```\n\nDone.\n";
        let got = extract_with(reply, |_| Some(readme.to_string()));
        assert!(got.failed.is_empty(), "{:?}", got.failed);
        assert_eq!(got.files[0].content, "# App\n\n```sh\nnpm run dev\n```\n");
        assert!(blocks(reply)[0].closed);
        assert!(commands(reply).is_empty());
        // Its REPLACE line not come, the fence still closes the block.
        let cut = "```edit README.md\n<<<<<<< SEARCH\n```sh\nnpm start\n```\nmore prose";
        assert!(blocks(cut)[0].closed);
    }

    #[test]
    fn an_edit_block_opened_with_four_backticks_holds_three() {
        let readme = "# App\n\n```sh\nnpm start\n```\n";
        let reply = "````edit README.md\n<<<<<<< SEARCH\n```sh\nnpm start\n```\n=======\n```sh\nnpm run dev\n```\n>>>>>>> REPLACE\n````\n";
        let got = extract_with(reply, |_| Some(readme.to_string()));
        assert!(got.failed.is_empty(), "{:?}", got.failed);
        assert_eq!(got.files[0].content, "# App\n\n```sh\nnpm run dev\n```\n");
    }

    #[test]
    fn a_heading_underline_in_an_edit_isnt_its_divider() {
        // Underlined with seven: the split whose search is in the file.
        let readme = "Docent\n=======\n\nOld intro.\n";
        let reply = "```edit README.md\n<<<<<<< SEARCH\nDocent\n=======\n\nOld intro.\n=======\nDocent\n=======\n\nNew intro.\n>>>>>>> REPLACE\n```\n";
        let got = extract_with(reply, |_| Some(readme.to_string()));
        assert!(got.failed.is_empty(), "{:?}", got.failed);
        assert_eq!(got.files[0].content, "Docent\n=======\n\nNew intro.\n");
        // Underlined with more than seven: never a divider.
        let readme = "Smartloop\n=========\n\nOld.\n";
        let reply = "```edit README.md\n<<<<<<< SEARCH\nSmartloop\n=========\n\nOld.\n=======\nSmartloop\n=========\n\nNew.\n>>>>>>> REPLACE\n```\n";
        let got = extract_with(reply, |_| Some(readme.to_string()));
        assert!(got.failed.is_empty(), "{:?}", got.failed);
        assert_eq!(got.files[0].content, "Smartloop\n=========\n\nNew.\n");
    }

    fn edit(search: &str, replace: &str) -> Edit {
        Edit { search: search.into(), replace: replace.into(), splits: Vec::new() }
    }

    #[test]
    fn an_edit_block_gives_its_search_and_replace_pairs() {
        let reply = "Fix:\n\n```edit src/main.rs\n<<<<<<< SEARCH\n    old();\n=======\n    new();\n>>>>>>> REPLACE\n<<<<<<< SEARCH\nfn a() {}\n=======\n>>>>>>> REPLACE\n```\n";
        let blocks = blocks(reply);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].file.path, "src/main.rs");
        assert_eq!(blocks[0].edit.as_deref(), Some(&[edit("    old();", "    new();"), edit("fn a() {}", "")][..]));
        // Not a command to run, and not the file whole.
        assert!(commands(reply).is_empty());
        // Still coming in, only the pairs that have ended count.
        let open = "```edit a.rs\n<<<<<<< SEARCH\nx\n=======\ny\n>>>>>>> REPLACE\n<<<<<<< SEARCH\nz\n=======\n";
        let blocks = super::blocks(open);
        assert!(!blocks[0].closed);
        assert_eq!(blocks[0].edit.as_deref(), Some(&[edit("x", "y")][..]));
    }

    #[test]
    fn an_edit_block_is_never_the_file_whole() {
        // Its markers left out: above the divider is found, below goes in.
        let reply = "```edit src/main.js\nold();\n=======\nnew();\n```\n";
        let blocks = blocks(reply);
        assert_eq!(blocks[0].edit.as_deref(), Some(&[edit("old();", "new();")][..]));
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.js"), "a();\nold();\nb();\n").unwrap();
        assert_eq!(extract_in(dir.path(), reply).files[0].content, "a();\nnew();\nb();\n");
        // No divider, or none splitting it into lines that are in the file:
        // no edit, and nothing written.
        for body in ["new();", "p\n=======\nq\n=======\nr"] {
            let found = extract_in(dir.path(), &format!("```edit src/main.js\n{body}\n```\n"));
            assert!(found.files.is_empty(), "{body}");
            assert_eq!(found.failed.len(), 1, "{body}");
        }
    }

    #[test]
    fn a_comment_standing_in_for_lines_is_found() {
        let old = "fn a() {}\nfn b() {}\nfn c() {}\n";
        for new in [
            "fn a() { 1 }\n// ... rest unchanged\n",
            "fn a() { 1 }\n    // ... existing code ...\n",
            "fn a() { 1 }\n/* … other functions as before */\n",
            "def a(): pass\n# ... rest of the file\n",
            "<p>hi</p>\n<!-- ... same as before -->\n",
        ] {
            assert!(elides(old, new), "{new}");
        }
        // Lines that aren't such a comment, or that the file had already.
        for new in [
            "fn a() { 1 }\nfn b() {}\nfn c() {}\n",
            "def stub(): ...\n",
            "// Loading...\nfn a() {}\n",
            "// see the rest of the docs\nfn a() {}\n",
        ] {
            assert!(!elides(old, new), "{new}");
        }
        let had = "x\n// ... rest unchanged\n";
        assert!(!elides(had, "y\n// ... rest unchanged\n"));
    }

    #[test]
    fn a_file_leaving_lines_out_isnt_planned_to_be_written() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        let file = File { path: "a.rs".into(), content: "fn a() { 1 }\n// ... rest unchanged\nfn b() {}\n".into() };
        let sorted = plan(dir.path(), vec![file.clone()], &["a.rs".to_string()]);
        assert_eq!(sorted.elided, [file]);
        assert!(sorted.changed.is_empty());
    }

    #[test]
    fn why_an_edit_failed_is_put_plainly() {
        let none = apply_edits(Some("a\n"), &[]).unwrap_err();
        assert_eq!(plainly(&none), "the change came in a form that can't be read");
        let missing = apply_edits(Some("a\n"), &[edit("zzz", "y")]).unwrap_err();
        assert_eq!(plainly(&missing), "the lines it replaces aren't in the file as it is now");
        let twice = apply_edits(Some("a\nb\na\n"), &[edit("b", "c"), edit("a", "y")]).unwrap_err();
        assert_eq!(plainly(&twice), "change 2 of 2: the lines it replaces are in the file 2 times, so it's unclear which to change");
        let empty = apply_edits(Some("a\n"), &[edit("", "y")]).unwrap_err();
        assert_eq!(plainly(&empty), "the change doesn't say which lines it replaces");
        assert!([none, missing, twice, empty].iter().all(|w| !plainly(w).contains("SEARCH")));
    }

    #[test]
    fn an_edit_block_may_leave_out_its_search_lines() {
        let reply = "```edit a.js\nold();\n=======\nnew();\n>>>>>>> REPLACE\n\nx();\n=======\ny();\n>>>>>>> REPLACE\n```\n";
        assert_eq!(blocks(reply)[0].edit.as_deref(), Some(&[edit("old();", "new();"), edit("x();", "y();")][..]));
    }

    #[test]
    fn an_edit_given_bare_is_an_edit_of_the_file_named_above() {
        let reply = "Now the canvas.\n\n**src/game.js**\n<<<<<<< SEARCH\nconst w = 1;\n=======\nconst w = 2;\n>>>>>>> REPLACE\n\n<<<<<<< SEARCH\ncanvas.\n=======\nboard.\n>>>>>>> REPLACE\n\nDone.\n";
        let blocks = blocks(reply);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].file.path, "src/game.js");
        assert!(blocks[0].closed);
        assert_eq!(blocks[0].edit.as_deref(), Some(&[edit("const w = 1;", "const w = 2;"), edit("canvas.", "board.")][..]));
        // Still coming in, it hasn't ended.
        let open = "src/game.js\n<<<<<<< SEARCH\nconst w = 1;\n=======\nconst w = 2;\n>>>>>>> REPLACE\n";
        assert!(!super::blocks(open)[0].closed);
        // With no file named, it isn't one.
        assert!(super::blocks("<<<<<<< SEARCH\na\n=======\nb\n>>>>>>> REPLACE\n").is_empty());
    }

    #[test]
    fn a_path_may_come_with_a_note_on_it() {
        let reply = "Here are the changes:\n\n```\n# Cargo.toml - Edit block\n<<<<<<< SEARCH\na\n=======\nb\n>>>>>>> REPLACE\n```\n\n\
                     ### src/main.rs (updated)\n\n```rust\n<<<<<<< SEARCH\nc\n=======\nd\n>>>>>>> REPLACE\n```\n";
        let blocks = blocks(reply);
        let paths: Vec<&str> = blocks.iter().map(|b| b.file.path.as_str()).collect();
        assert_eq!(paths, ["Cargo.toml", "src/main.rs"]);
        assert_eq!(blocks[0].edit.as_deref(), Some(&[edit("a", "b")][..]));
        assert_eq!(super::label_path("see the docs - e.g. here"), None);
    }

    #[test]
    fn an_edit_blocks_path_may_be_its_first_line() {
        let reply = "```edit\nsrc/sound.js\n<<<<<<< SEARCH\na\n=======\nb\n>>>>>>> REPLACE\n```\n\n```edit\n**index.html**\n<<<<<<< SEARCH\nc\n=======\nd\n>>>>>>> REPLACE\n```\n";
        let blocks = blocks(reply);
        let paths: Vec<&str> = blocks.iter().map(|b| b.file.path.as_str()).collect();
        assert_eq!(paths, ["src/sound.js", "index.html"]);
        assert_eq!(blocks[0].edit.as_deref(), Some(&[edit("a", "b")][..]));
    }

    #[test]
    fn edits_are_made_in_turn_found_once() {
        let file = "fn main() {\n    old();\n}\n\nfn a() {}\n";
        let made = apply_edits(Some(file), &[edit("    old();", "    new();\n    more();"), edit("fn a() {}", "fn b() {}")]);
        assert_eq!(made.unwrap(), "fn main() {\n    new();\n    more();\n}\n\nfn b() {}\n");
        // Taking lines out.
        assert_eq!(apply_edits(Some("a\nb\nc\n"), &[edit("b", "")]).unwrap(), "a\n\nc\n");
        assert_eq!(apply_edits(Some("a\nb\nc\n"), &[edit("b\n", "")]).unwrap(), "a\nc\n");
        // Found twice, or not at all, it isn't made.
        let twice = apply_edits(Some("x\nx\n"), &[edit("x", "y")]).unwrap_err();
        assert!(twice.contains("found 2 times"), "{twice}");
        let missing = apply_edits(Some("a\n"), &[edit("a\n", "b"), edit("zz", "y")]).unwrap_err();
        assert!(missing.starts_with("edit 2 of 2") && missing.contains("isn't in the file"), "{missing}");
        // A new file from an empty search; an existing one isn't replaced so.
        assert_eq!(apply_edits(None, &[edit("", "hi")]).unwrap(), "hi\n");
        assert!(apply_edits(None, &[edit("a", "b")]).is_err());
        assert!(apply_edits(Some("a\n"), &[edit("", "b")]).is_err());
    }

    #[test]
    fn an_edit_is_found_past_whitespace_and_by_its_ends() {
        let file = "fn main() {\n\tlet x = 1;\n    let y  = 2;\n}\n";
        // Indented otherwise.
        let made = apply_edits(Some(file), &[edit("  let x = 1;\n  let y  = 2;", "\tlet z = 3;")]).unwrap();
        assert_eq!(made, "fn main() {\n\tlet z = 3;\n}\n");
        // Spaced otherwise inside the line.
        let made = apply_edits(Some(file), &[edit("let y = 2;", "let y = 4;")]).unwrap();
        assert_eq!(made, "fn main() {\n\tlet x = 1;\nlet y = 4;\n}\n");
        // Its middle a little off, found by its first and last lines.
        let file = "a\nb\nc\nd\ne\nf\n";
        let made = apply_edits(Some(file), &[edit("b\nC!\nd\ne", "x")]).unwrap();
        assert_eq!(made, "a\nx\nf\n");
    }

    #[test]
    fn a_reply_s_edits_change_the_file_there_or_say_why_not() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn a() {\n    old();\n}\n").unwrap();
        fs::write(dir.path().join("b.rs"), "fn b() {}\n").unwrap();
        let reply = "```edit a.rs\n<<<<<<< SEARCH\n    old();\n=======\n    new();\n>>>>>>> REPLACE\n```\n\n\
                     ```edit b.rs\n<<<<<<< SEARCH\nnope\n=======\nx\n>>>>>>> REPLACE\n```\n\n\
                     ```edit a.rs\n<<<<<<< SEARCH\n    new();\n=======\n    newer();\n>>>>>>> REPLACE\n```\n";
        let found = extract_in(dir.path(), reply);
        assert_eq!(found.files, [File { path: "a.rs".into(), content: "fn a() {\n    newer();\n}\n".into() }]);
        assert_eq!(found.edited, ["a.rs"]);
        assert_eq!(found.failed.len(), 1);
        assert_eq!(found.failed[0].0, "b.rs");
    }

    #[test]
    fn appending_adds_to_the_file_there() {
        let dir = std::env::temp_dir().join(format!("docent-append-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("notes.txt"), "a\n").unwrap();
        let reply = "```sh\ncat >> notes.txt <<'EOF'\nb\nEOF\ntee -a notes.txt <<EOF\nc\nEOF\n```\n";
        let files = extract_in(&dir, reply).files;
        assert_eq!(files, [File { path: "notes.txt".into(), content: "a\nb\nc\n".into() }]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn heredocs_that_dont_write_a_file_stay_commands() {
        for line in ["cat <<EOF | sh", "cat > /etc/hosts <<EOF", "cat > ../x.txt <<EOF", "python3 - <<'PY'"] {
            let reply = format!("```sh\n{line}\nbody\nEOF\nPY\n```\n");
            assert!(extract(&reply).is_empty(), "{line}");
            assert!(!commands(&reply).is_empty(), "{line}");
        }
    }

    #[test]
    fn a_skill_is_a_file_under_docent() {
        let reply = "```sh\nmkdir -p .docent/skills/review && cat > .docent/skills/review/SKILL.md <<'EOF'\n---\nname: review\n---\nRead it.\nEOF\n```";
        assert_eq!(paths(reply), [".docent/skills/review/SKILL.md"]);
    }

    #[test]
    fn a_shell_tool_call_written_as_text_runs_as_a_block() {
        let reply = "Let me first explore the codebase.]<]minimax[>[<tool_call> {\"name\": \"bash\", \"arguments\": {\"command\": \"ls -la /tmp/boxy-bird/\", \"description\": \"List the project root\"}}\n]<]minimax[>[</tool_call>\n";
        assert!(has_tool_call_markup(reply));
        let tidy = tool_calls_as_blocks(reply);
        assert_eq!(tidy, "Let me first explore the codebase.\n```bash\nls -la /tmp/boxy-bird/\n```\n");
        assert!(!has_tool_call_markup(&tidy));
        assert_eq!(commands(&tidy), ["ls -la /tmp/boxy-bird/"]);
    }

    #[test]
    fn a_tool_call_with_string_arguments_runs_too() {
        let reply = "<tool_call>{\"name\": \"shell\", \"arguments\": \"{\\\"cmd\\\": \\\"cargo build\\\"}\"}</tool_call>";
        assert_eq!(commands(&tool_calls_as_blocks(reply)), ["cargo build"]);
    }

    #[test]
    fn a_tool_call_docent_cannot_run_is_dropped() {
        let reply = "Reading.\n<tool_call>{\"name\": \"read_file\", \"arguments\": {\"path\": \"a.rs\"}}</tool_call>\nDone.";
        assert_eq!(tool_calls_as_blocks(reply), "Reading.\nDone.");
    }

    #[test]
    fn a_streaming_tool_call_waits_for_its_closing_tag() {
        let reply = "Looking.]<]minimax[>[<tool_call> {\"name\": \"bash\", \"argu";
        assert_eq!(tool_calls_as_blocks(reply), "Looking.<tool_call> {\"name\": \"bash\", \"argu");
    }

    #[test]
    fn text_that_only_looks_like_a_control_token_is_kept() {
        let reply = "a[0]<]b c [>[ d";
        assert!(!has_tool_call_markup(reply));
        assert_eq!(tool_calls_as_blocks(reply), reply);
    }
}
