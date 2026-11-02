//! `remuda memory import --from <dir> [--dry-run]`: the one-time Phase 1
//! import (main-agent.md §12.4, cb-memory-guard).
//!
//! In Phase 1 a seat kept its memory as files in its persistent workspace,
//! in the layout Claude Code's own memory directory uses, which is where
//! D-060's four entry types come from:
//!
//! - `MEMORY.md`: the index, one `- [Title](file.md) — hook` line per entry;
//! - one `<name>.md` per entry: YAML front matter with `name`, `description`
//!   and `type` (`user`, `feedback`, `project` or `reference`), either at the
//!   top level or under `metadata:`, then the body. `origin: human|agent`
//!   records who the entry came from; a file that records none was written
//!   by the seat on its own and imports as `agent`.
//!
//! Only regular files directly inside the directory are read. A symbolic
//! link, a directory, a FIFO or a device is skipped and reported, never
//! opened for reading: each name is checked with `lstat`, opened with
//! `O_NOFOLLOW | O_NONBLOCK`, checked again on the open handle, and its
//! canonical path must sit directly in the canonical source directory. A
//! source directory that is itself a link is refused. Each file is read up to
//! [`MAX_FILE_BYTES`].
//!
//! Every file is read before anything is sent. "Not found" is empty — a
//! missing directory imports nothing, a file that vanished between listing
//! and reading is skipped — but any other read failure (permission, I/O) is
//! an error and nothing is sent, so the Hub's memory is neither deleted nor
//! overwritten. The Hub then applies the whole import in one transaction:
//! new entries are created with the recorded origin, an entry whose slug or
//! content is already there is skipped (so a re-run changes nothing), and
//! `--dry-run` decides the same and commits nothing. Owner (Human) only.

use anyhow::bail;
use remuda_protocol::{MemoryImportEntry, MemoryOrigin, MemoryType, validate_memory_slug};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use super::hub_client::{HubClient, hub_http_error, print_json};

/// The index file of a Phase 1 memory directory; not an entry itself.
const INDEX_FILE: &str = "MEMORY.md";

/// The most bytes read from one file. An entry holds at most 8 KiB of
/// title, description and body; the rest is front matter and slack.
pub(crate) const MAX_FILE_BYTES: u64 = 64 * 1024;

/// What opening one name in the source directory gave.
#[derive(Debug)]
pub(crate) enum Opened {
    /// A regular file directly in the directory, and its text.
    Text(String),
    /// Not read, and why: a link, a directory, a FIFO, a device, a file
    /// outside the directory, an oversized file.
    Skipped(String),
}

/// Filesystem access for the import, so tests can inject read failures.
pub(crate) trait MemoryFiles {
    /// The source directory as it will be read: refused when it is a link
    /// or not a directory; its canonical path otherwise.
    fn source(&self, dir: &Path) -> io::Result<Result<PathBuf, String>>;
    /// Paths of the directory's entries (files and directories alike).
    fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>>;
    /// A file directly in `root` (canonical), read only if it is a regular
    /// file there, up to [`MAX_FILE_BYTES`].
    fn read(&self, root: &Path, path: &Path) -> io::Result<Opened>;
}

/// The real filesystem.
pub(crate) struct DiskFiles;

/// What an `lstat` or `fstat` type names, for a skip reason.
fn kind_of(file_type: std::fs::FileType) -> &'static str {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if file_type.is_fifo() {
            return "a FIFO";
        }
        if file_type.is_socket() {
            return "a socket";
        }
        if file_type.is_block_device() || file_type.is_char_device() {
            return "a device";
        }
    }
    if file_type.is_symlink() {
        "a symbolic link"
    } else if file_type.is_dir() {
        "a directory"
    } else if file_type.is_file() {
        "a regular file"
    } else {
        "not a regular file"
    }
}

impl MemoryFiles for DiskFiles {
    fn source(&self, dir: &Path) -> io::Result<Result<PathBuf, String>> {
        let meta = std::fs::symlink_metadata(dir)?;
        if !meta.file_type().is_dir() {
            return Ok(Err(format!(
                "the source {} is {}, not a directory; pass the directory's own path",
                dir.display(),
                kind_of(meta.file_type())
            )));
        }
        Ok(Ok(std::fs::canonicalize(dir)?))
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        std::fs::read_dir(dir)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect()
    }

    fn read(&self, root: &Path, path: &Path) -> io::Result<Opened> {
        use std::io::Read;
        let skipped = |kind: &str| {
            Ok(Opened::Skipped(format!(
                "{kind}; only regular files are read"
            )))
        };
        let before = std::fs::symlink_metadata(path)?;
        if !before.file_type().is_file() {
            return skipped(kind_of(before.file_type()));
        }
        // The name may be swapped between the lstat and the open: never
        // follow a final link, never block on a FIFO, and check the handle.
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use nix::fcntl::OFlag;
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits());
        }
        let file = match options.open(path) {
            Ok(file) => file,
            #[cfg(unix)]
            Err(error) if error.raw_os_error() == Some(nix::libc::ELOOP) => {
                return skipped("a symbolic link");
            }
            Err(error) => return Err(error),
        };
        let opened = file.metadata()?;
        if !opened.file_type().is_file() {
            return skipped(kind_of(opened.file_type()));
        }
        let canonical = std::fs::canonicalize(path)?;
        if canonical.parent() != Some(root) {
            return Ok(Opened::Skipped(
                "outside the source directory; only its own files are read".into(),
            ));
        }
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Ok(Opened::Skipped(format!(
                "larger than {MAX_FILE_BYTES} bytes; a memory entry holds at most 8 KiB"
            )));
        }
        String::from_utf8(bytes)
            .map(Opened::Text)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.utf8_error()))
    }
}

/// What reading a Phase 1 directory produced.
#[derive(Debug, Default)]
pub(crate) struct Collected {
    /// The entries to import, in file-name order.
    pub entries: Vec<MemoryImportEntry>,
    /// The file each entry came from.
    pub sources: Vec<String>,
    /// Files that were empty or vanished (not found) before they were read.
    pub empty: Vec<String>,
    /// Names that were not read, with the reason (links, directories,
    /// FIFOs, devices, oversized files).
    pub skipped: Vec<(String, String)>,
    /// Entries whose file recorded no origin (imported as `agent`).
    pub origin_defaulted: usize,
    /// The directory itself was not found.
    pub missing: bool,
}

fn not_found(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
}

fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Read and parse every entry file of `dir`. Any read failure other than
/// "not found", and any file that does not parse, is an error naming the
/// file; all of them are reported at once.
pub(crate) fn collect(dir: &Path, files: &dyn MemoryFiles) -> anyhow::Result<Collected> {
    let mut collected = Collected::default();
    let root = match files.source(dir) {
        Ok(Ok(root)) => root,
        Ok(Err(refusal)) => bail!("{refusal}; nothing was imported"),
        Err(error) if not_found(&error) => {
            collected.missing = true;
            return Ok(collected);
        }
        Err(error) => bail!(
            "cannot read the memory directory {}: {error}; nothing was imported",
            dir.display()
        ),
    };
    let dir = root.as_path();
    let mut paths = match files.list(dir) {
        Ok(paths) => paths,
        Err(error) if not_found(&error) => {
            collected.missing = true;
            return Ok(collected);
        }
        Err(error) => bail!(
            "cannot read the memory directory {}: {error}; nothing was imported",
            dir.display()
        ),
    };
    paths.sort();
    let mut problems = Vec::new();
    let index = match files.read(dir, &dir.join(INDEX_FILE)) {
        Ok(Opened::Text(text)) => parse_index(&text),
        Ok(Opened::Skipped(reason)) => {
            collected.skipped.push((INDEX_FILE.to_string(), reason));
            BTreeMap::new()
        }
        Err(error) if not_found(&error) => BTreeMap::new(),
        Err(error) => {
            problems.push(format!("{INDEX_FILE}: cannot read it: {error}"));
            BTreeMap::new()
        }
    };
    for path in paths {
        let name = name_of(&path);
        if name == INDEX_FILE || name.starts_with('.') || !name.ends_with(".md") {
            continue;
        }
        let text = match files.read(dir, &path) {
            Ok(Opened::Text(text)) => text,
            Ok(Opened::Skipped(reason)) => {
                collected.skipped.push((name, reason));
                continue;
            }
            Err(error) if not_found(&error) => {
                collected.empty.push(name);
                continue;
            }
            Err(error) => {
                problems.push(format!("{name}: cannot read it: {error}"));
                continue;
            }
        };
        if text.trim().is_empty() {
            collected.empty.push(name);
            continue;
        }
        match parse_entry(&name, &text, index.get(&name)) {
            Ok((entry, defaulted)) => {
                collected.origin_defaulted += usize::from(defaulted);
                collected.entries.push(entry);
                collected.sources.push(name);
            }
            Err(problem) => problems.push(format!("{name}: {problem}")),
        }
    }
    // The Hub refuses a credential too; checking here names every file at
    // once, by field and shape, and sends nothing.
    for (entry, name) in collected.entries.iter().zip(&collected.sources) {
        for (field, text) in [
            ("slug", entry.slug.as_str()),
            ("title", entry.title.as_str()),
            ("description", entry.description.as_str()),
            ("body", entry.body.as_str()),
        ] {
            if let Some(shape) = remuda_hub::secret_lint::first_secret(text) {
                problems.push(format!(
                    "{name}: the {field} looks like a credential (shape {shape:?}); remove it"
                ));
            }
        }
    }
    let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
    for (entry, name) in collected.entries.iter().zip(&collected.sources) {
        if let Some(first) = seen.insert(entry.slug.as_str(), name.as_str()) {
            problems.push(format!(
                "{name}: slug {:?} is also the slug of {first}",
                entry.slug
            ));
        }
    }
    if !problems.is_empty() {
        bail!(
            "the Phase 1 memory in {} cannot be imported; nothing was imported:\n  {}",
            dir.display(),
            problems.join("\n  ")
        );
    }
    Ok(collected)
}

/// `MEMORY.md` lines `- [Title](file.md) — hook`, keyed by file name.
fn parse_index(text: &str) -> BTreeMap<String, (String, String)> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line.trim_start().strip_prefix("- [") else {
            continue;
        };
        let Some((title, rest)) = rest.split_once("](") else {
            continue;
        };
        let Some((file, rest)) = rest.split_once(')') else {
            continue;
        };
        let hook = rest
            .trim()
            .trim_start_matches(['—', '–', '-'])
            .trim()
            .to_string();
        let file = file.rsplit('/').next().unwrap_or(file).to_string();
        out.insert(file, (title.trim().to_string(), hook));
    }
    out
}

/// Front matter (`key: value`, plus one nested `metadata:` level) and body.
fn split_front_matter(text: &str) -> (BTreeMap<String, String>, &str) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut fields = BTreeMap::new();
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return (fields, text);
    };
    let mut offset = 0;
    let mut body = None;
    for line in rest.split_inclusive('\n') {
        offset += line.len();
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == "---" {
            body = Some(&rest[offset..]);
            break;
        }
        let indented = trimmed.starts_with([' ', '\t']);
        let Some((key, value)) = trimmed.trim().split_once(':') else {
            continue;
        };
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        let key = key.trim().to_string();
        // A nested key (under `metadata:`) fills in only what the top level
        // did not say.
        if indented {
            fields.entry(key).or_insert_with(|| value.to_string());
        } else if !value.is_empty() {
            fields.insert(key, value.to_string());
        }
    }
    match body {
        Some(body) => (fields, body),
        // An unterminated block is not front matter.
        None => (BTreeMap::new(), text),
    }
}

/// A slug from a Phase 1 name: lowercase, anything outside `a-z 0-9 . _ -`
/// as `-`, at most 64 characters.
fn slug_from(name: &str) -> String {
    let mut slug = String::new();
    for ch in name.trim().chars().flat_map(char::to_lowercase) {
        let ch = if ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '_' | '-')
        {
            ch
        } else {
            '-'
        };
        if !(ch == '-' && slug.ends_with('-')) {
            slug.push(ch);
        }
    }
    let slug = slug.trim_start_matches(['-', '.', '_']);
    slug.chars().take(64).collect::<String>()
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One entry file. Returns the entry and whether its origin was defaulted.
fn parse_entry(
    file: &str,
    text: &str,
    index: Option<&(String, String)>,
) -> Result<(MemoryImportEntry, bool), String> {
    let (fields, body) = split_front_matter(text);
    let body = body.trim().to_string();
    let stem = file.strip_suffix(".md").unwrap_or(file);
    let name = fields.get("name").map(String::as_str).unwrap_or(stem);
    let slug = slug_from(name);
    validate_memory_slug(&slug)
        .map_err(|error| format!("no usable slug from its name: {error}"))?;
    let memory_type = match fields.get("type") {
        Some(raw) => MemoryType::parse(raw.trim())
            .ok_or_else(|| format!("type {raw:?} is not user, feedback, project or reference"))?,
        None => return Err("no type in the front matter".into()),
    };
    let (origin, defaulted) = match fields.get("origin").map(|raw| raw.trim()) {
        None => (MemoryOrigin::Agent, true),
        Some("human") => (MemoryOrigin::Human, false),
        Some("agent") => (MemoryOrigin::Agent, false),
        Some(other) => return Err(format!("origin {other:?} is not human or agent")),
    };
    let heading = body
        .lines()
        .find_map(|line| line.strip_prefix("# "))
        .map(str::to_string);
    let title = fields
        .get("title")
        .cloned()
        .or_else(|| {
            index
                .map(|(title, _)| title.clone())
                .filter(|t| !t.is_empty())
        })
        .or(heading)
        .unwrap_or_else(|| name.replace(['-', '_'], " "));
    let description = fields
        .get("description")
        .cloned()
        .or_else(|| {
            index
                .map(|(_, hook)| hook.clone())
                .filter(|h| !h.is_empty())
        })
        .or_else(|| {
            body.lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(|line| line.chars().take(200).collect())
        })
        .unwrap_or_else(|| title.clone());
    Ok((
        MemoryImportEntry {
            slug,
            memory_type,
            title: one_line(&title),
            description: one_line(&description),
            body,
            origin,
        },
        defaulted,
    ))
}

/// Read `dir`, then send the whole import (or dry run) to the Hub.
pub(crate) async fn run_import(
    client: &HubClient,
    lineage: &str,
    dir: &Path,
    dry_run: bool,
    json_out: bool,
    files: &dyn MemoryFiles,
) -> anyhow::Result<Value> {
    let collected = collect(dir, files)?;
    let local = json!({
        "from": dir.display().to_string(),
        "missing": collected.missing,
        "files": collected.sources,
        "empty": collected.empty,
        "skipped": collected
            .skipped
            .iter()
            .map(|(name, reason)| json!({"file": name, "reason": reason}))
            .collect::<Vec<_>>(),
        "originDefaulted": collected.origin_defaulted,
    });
    if collected.entries.is_empty() {
        let value = json!({"lineageId": lineage, "dryRun": dry_run, "created": [],
                           "skipped": [], "local": local});
        if json_out {
            print_json(&value)?;
        } else if collected.missing {
            println!(
                "no Phase 1 memory at {} (not found): nothing to import",
                dir.display()
            );
        } else {
            println!(
                "no Phase 1 memory entries in {}: nothing to import",
                dir.display()
            );
            for name in &collected.empty {
                println!("empty    {name}");
            }
            for (name, reason) in &collected.skipped {
                println!("not read {name}: {reason}");
            }
        }
        return Ok(value);
    }
    let mut value = client
        .post(
            &format!("/v1/lineages/{lineage}/memory/import"),
            &json!({"entries": collected.entries, "dryRun": dry_run}),
        )
        .await
        .map_err(hub_http_error)?;
    value["local"] = local;
    if json_out {
        print_json(&value)?;
    } else {
        print!("{}", summary(&value, &collected));
    }
    Ok(value)
}

/// The text summary of an import result.
fn summary(value: &Value, collected: &Collected) -> String {
    let list = |key: &str| value[key].as_array().cloned().unwrap_or_default();
    let (created, skipped) = (list("created"), list("skipped"));
    let mut out = if value["dryRun"].as_bool().unwrap_or(false) {
        format!(
            "dry run: {} file(s) read, {} would be created, {} skipped; nothing was written\n",
            collected.entries.len(),
            created.len(),
            skipped.len()
        )
    } else {
        format!(
            "imported: {} file(s) read, {} created, {} skipped\n",
            collected.entries.len(),
            created.len(),
            skipped.len()
        )
    };
    for entry in &created {
        out.push_str(&format!(
            "created  {} (origin {}, revision {})\n",
            entry["slug"].as_str().unwrap_or("?"),
            entry["origin"].as_str().unwrap_or("?"),
            entry["revision"]
        ));
    }
    for entry in &skipped {
        let slug = entry["slug"].as_str().unwrap_or("?");
        let why = match entry["reason"].as_str() {
            Some("same-entry") => "already imported".to_string(),
            Some("slug-exists") => "a different entry has this slug; kept it".to_string(),
            Some("same-content") => format!(
                "same content as {}",
                entry["existing"].as_str().unwrap_or("?")
            ),
            other => other.unwrap_or("?").to_string(),
        };
        out.push_str(&format!("skipped  {slug}: {why}\n"));
    }
    for name in &collected.empty {
        out.push_str(&format!("empty    {name}\n"));
    }
    for (name, reason) in &collected.skipped {
        out.push_str(&format!("not read {name}: {reason}\n"));
    }
    if collected.origin_defaulted > 0 {
        out.push_str(&format!(
            "note: {} file(s) recorded no origin and were imported as agent\n",
            collected.origin_defaulted
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const SEAT_HOST: &str = "hst_memory_import";

    /// A Phase 1 directory as a seat left it: an index, entries with and
    /// without a recorded origin, top-level and nested `type`.
    fn phase1(dir: &Path) {
        let files = [
            (
                INDEX_FILE,
                "# Memory index\n\n\
                 - [Lead with the outcome](status-outcome-first.md) — the owner reads the first line\n\
                 - [Gate order](gate-order.md) — fmt, clippy, tests\n",
            ),
            (
                "status-outcome-first.md",
                "---\nname: status-outcome-first\ndescription: The owner wants the conclusion on the first line.\n\
                 type: feedback\norigin: human\n---\n\n\
                 The owner asked on 2026-10-30 to lead status reports with the outcome.\n\
                 Why: they read the first line to decide whether to read on.\n\
                 How to apply: put the conclusion first.\n",
            ),
            (
                "gate-order.md",
                "---\nname: gate-order\ndescription: \"fmt first: cargo fmt, clippy, then tests\"\n\
                 metadata:\n  type: project\n  origin: agent\n---\n\n\
                 The gate runs cargo fmt --all --check first (seen 2026-10-28).\n",
            ),
            (
                "devbox.md",
                "---\nname: devbox\ndescription: The devbox has 64 cores and no Chrome.\nmetadata:\n  type: reference\n---\n\n\
                 Use PW_CHANNEL=chromium for browser tests there.\n",
            ),
            ("notes.txt", "not an entry"),
        ];
        for (name, text) in files {
            std::fs::write(dir.join(name), text).unwrap();
        }
    }

    /// Disk reads, with chosen paths failing as the test says.
    struct Faulty {
        faults: HashMap<PathBuf, i32>,
    }

    impl Faulty {
        fn new(faults: &[(&Path, i32)]) -> Self {
            Self {
                faults: faults.iter().map(|(p, e)| (p.to_path_buf(), *e)).collect(),
            }
        }

        fn fault(&self, path: &Path) -> io::Result<()> {
            match self.faults.get(path) {
                Some(errno) => Err(io::Error::from_raw_os_error(*errno)),
                None => Ok(()),
            }
        }
    }

    impl MemoryFiles for Faulty {
        fn source(&self, dir: &Path) -> io::Result<Result<PathBuf, String>> {
            DiskFiles.source(dir)
        }

        fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
            self.fault(dir)?;
            DiskFiles.list(dir)
        }

        fn read(&self, root: &Path, path: &Path) -> io::Result<Opened> {
            self.fault(path)?;
            DiskFiles.read(root, path)
        }
    }

    const ENOENT: i32 = 2;
    const EIO: i32 = 5;
    const EACCES: i32 = 13;

    struct Hub {
        _dir: tempfile::TempDir,
        _hub: remuda_hub::RunningHub,
        owner: HubClient,
        lineage: String,
    }

    async fn hub() -> Hub {
        let dir = tempfile::tempdir().unwrap();
        let hub = remuda_hub::spawn(remuda_hub::HubConfig::for_test(dir.path().join("hub")))
            .await
            .unwrap();
        hub.test_insert_host(SEAT_HOST).await.unwrap();
        let lineage = hub
            .store()
            .unwrap()
            .insert_instance_delegated(
                SEAT_HOST.into(),
                None,
                "claude".into(),
                "claude-sdk".into(),
                None,
                json!({"origin": "human"}),
                remuda_hub::store_test_support::continuity_delegation(),
                remuda_hub::agent_scope_test_support::CallerAuthority::internal(),
            )
            .await
            .unwrap()
            .instance_id;
        let token = hub.mint_device_token("import-owner").await.unwrap();
        let owner =
            super::super::hub_client::connect_for_test(format!("http://{}", hub.addr), token)
                .unwrap();
        Hub {
            _dir: dir,
            _hub: hub,
            owner,
            lineage,
        }
    }

    impl Hub {
        /// Every entry of the lineage, archived included, as stored.
        async fn entries(&self) -> Value {
            self.owner
                .get(&format!(
                    "/v1/lineages/{}/memory?includeArchived=true",
                    self.lineage
                ))
                .await
                .unwrap()["items"]
                .clone()
        }

        async fn write(&self, slug: &str, kind: &str, body: &str) {
            self.owner
                .post(
                    &format!("/v1/lineages/{}/memory", self.lineage),
                    &json!({"slug": slug, "type": kind, "title": format!("Title {slug}"),
                            "description": format!("About {slug}"), "body": body}),
                )
                .await
                .unwrap();
        }

        async fn import(
            &self,
            dir: &Path,
            dry_run: bool,
            files: &dyn MemoryFiles,
        ) -> anyhow::Result<Value> {
            run_import(&self.owner, &self.lineage, dir, dry_run, true, files).await
        }
    }

    fn slugs(value: &Value, key: &str) -> Vec<String> {
        value[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["slug"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn the_phase1_layout_parses_into_entries() {
        let dir = tempfile::tempdir().unwrap();
        phase1(dir.path());
        let collected = collect(dir.path(), &DiskFiles).unwrap();
        assert_eq!(
            collected.sources,
            ["devbox.md", "gate-order.md", "status-outcome-first.md"]
        );
        let by: HashMap<&str, &MemoryImportEntry> = collected
            .entries
            .iter()
            .map(|e| (e.slug.as_str(), e))
            .collect();
        let status = by["status-outcome-first"];
        assert_eq!(status.memory_type, MemoryType::Feedback);
        assert_eq!(status.origin, MemoryOrigin::Human);
        assert_eq!(status.title, "Lead with the outcome");
        assert!(status.body.starts_with("The owner asked on 2026-10-30"));
        assert!(status.body.ends_with("put the conclusion first."));
        let gate = by["gate-order"];
        assert_eq!(gate.memory_type, MemoryType::Project);
        assert_eq!(gate.origin, MemoryOrigin::Agent);
        assert_eq!(gate.description, "fmt first: cargo fmt, clippy, then tests");
        // No index line and no recorded origin.
        let devbox = by["devbox"];
        assert_eq!(devbox.memory_type, MemoryType::Reference);
        assert_eq!(devbox.origin, MemoryOrigin::Agent);
        assert_eq!(devbox.title, "devbox");
        assert_eq!(collected.origin_defaulted, 1);
    }

    #[test]
    fn a_file_without_a_type_or_with_a_secret_stops_the_import_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        phase1(dir.path());
        std::fs::write(
            dir.path().join("untyped.md"),
            "---\nname: untyped\n---\nbody\n",
        )
        .unwrap();
        let key = ["sk", "-ant-api03-Zq8vR2nL5tK9wX3mB7cJ4hP"].concat();
        std::fs::write(
            dir.path().join("gateway.md"),
            format!("---\nname: gateway\ndescription: d\ntype: reference\n---\nkey {key}\n"),
        )
        .unwrap();
        let error = collect(dir.path(), &DiskFiles).unwrap_err().to_string();
        assert!(error.contains("untyped.md: no type"), "{error}");
        assert!(
            error.contains("gateway.md: the body looks like a credential"),
            "{error}"
        );
        assert!(error.contains("nothing was imported"), "{error}");
        assert!(!error.contains("Zq8vR2nL"), "{error}");
    }

    /// A source directory with a private file and a directory beside it,
    /// and links inside it that point out of it (and a FIFO, when asked).
    #[cfg(unix)]
    fn planted(fifo: bool) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(
            outside.join("private.md"),
            "---\nname: private\ndescription: not the seat's\ntype: user\n---\nprivate text\n",
        )
        .unwrap();
        std::fs::write(
            outside.join(INDEX_FILE),
            "- [Planted title](devbox.md) — planted hook\n",
        )
        .unwrap();
        let dir = root.path().join("phase1");
        std::fs::create_dir(&dir).unwrap();
        phase1(&dir);
        std::os::unix::fs::symlink(outside.join("private.md"), dir.join("notes.md")).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("folder.md")).unwrap();
        if fifo {
            nix::unistd::mkfifo(&dir.join("pipe.md"), nix::sys::stat::Mode::S_IRWXU).unwrap();
        }
        (root, dir)
    }

    /// `collect` on a thread, so a read that blocks (a FIFO) fails the test
    /// instead of hanging it.
    #[cfg(unix)]
    fn collect_bounded(dir: PathBuf) -> anyhow::Result<Collected> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(collect(&dir, &DiskFiles));
        });
        rx.recv_timeout(std::time::Duration::from_secs(20))
            .expect("collect blocked: it opened something that is not a regular file")
    }

    #[cfg(unix)]
    #[test]
    fn links_out_of_the_directory_are_never_followed() {
        let (_root, dir) = planted(false);
        // The index is a link too, to a planted index outside.
        std::fs::remove_file(dir.join(INDEX_FILE)).unwrap();
        std::os::unix::fs::symlink(
            dir.parent().unwrap().join("outside").join(INDEX_FILE),
            dir.join(INDEX_FILE),
        )
        .unwrap();
        let collected = collect_bounded(dir).unwrap();
        assert_eq!(
            collected.sources,
            ["devbox.md", "gate-order.md", "status-outcome-first.md"]
        );
        assert!(collected.entries.iter().all(|e| e.slug != "private"));
        assert!(
            collected
                .entries
                .iter()
                .all(|e| e.title != "Planted title" && !e.body.contains("private text"))
        );
        let reasons: BTreeMap<&str, &str> = collected
            .skipped
            .iter()
            .map(|(name, reason)| (name.as_str(), reason.as_str()))
            .collect();
        assert_eq!(
            reasons.keys().copied().collect::<Vec<_>>(),
            [INDEX_FILE, "folder.md", "notes.md"]
        );
        for reason in reasons.values() {
            assert!(reason.starts_with("a symbolic link"), "{reasons:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_is_never_opened_for_reading() {
        let (_root, dir) = planted(true);
        let collected = collect_bounded(dir).unwrap();
        assert!(!collected.sources.iter().any(|s| s == "pipe.md"));
        assert!(
            collected
                .skipped
                .iter()
                .any(|(name, reason)| name == "pipe.md" && reason.starts_with("a FIFO")),
            "{:?}",
            collected.skipped
        );
    }

    #[test]
    fn a_file_whose_canonical_path_is_not_in_the_source_is_not_read() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let other = root.path().join("other");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&other).unwrap();
        std::fs::write(other.join("note.md"), "private").unwrap();
        let source = std::fs::canonicalize(&source).unwrap();
        // A name that resolves elsewhere (here: a parent component) is not
        // read even though the file itself is regular.
        let elsewhere = source.join("..").join("other").join("note.md");
        match DiskFiles.read(&source, &elsewhere).unwrap() {
            Opened::Skipped(reason) => assert!(reason.starts_with("outside"), "{reason}"),
            Opened::Text(text) => panic!("read {text:?}"),
        }
    }

    #[test]
    fn an_oversized_file_is_read_only_up_to_the_limit_and_skipped() {
        let dir = tempfile::tempdir().unwrap();
        phase1(dir.path());
        let mut big = String::from("---\nname: big\ndescription: d\ntype: project\n---\n");
        big.push_str(&"x".repeat(MAX_FILE_BYTES as usize));
        std::fs::write(dir.path().join("big.md"), big).unwrap();
        let collected = collect(dir.path(), &DiskFiles).unwrap();
        assert!(collected.entries.iter().all(|e| e.slug != "big"));
        assert_eq!(collected.skipped.len(), 1, "{:?}", collected.skipped);
        assert_eq!(collected.skipped[0].0, "big.md");
        assert!(
            collected.skipped[0]
                .1
                .starts_with("larger than 65536 bytes")
        );
        assert_eq!(collected.entries.len(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn a_source_directory_that_is_a_link_is_refused() {
        let (root, dir) = planted(false);
        let link = root.path().join("linked");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        let error = collect_bounded(link).unwrap_err().to_string();
        assert!(error.contains("symbolic link"), "{error}");
        assert!(error.contains("nothing was imported"), "{error}");
    }

    #[tokio::test]
    async fn a_missing_directory_or_file_is_empty() {
        let hub = hub().await;
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-memory");
        let value = hub.import(&missing, false, &DiskFiles).await.unwrap();
        assert_eq!(value["local"]["missing"], true);
        assert!(slugs(&value, "created").is_empty());
        assert_eq!(hub.entries().await, json!([]));

        // A file that vanishes between listing and reading is empty too.
        phase1(dir.path());
        let gone = dir.path().join("devbox.md");
        let value = hub
            .import(dir.path(), false, &Faulty::new(&[(&gone, ENOENT)]))
            .await
            .unwrap();
        assert_eq!(value["local"]["empty"], json!(["devbox.md"]));
        assert_eq!(
            slugs(&value, "created"),
            ["gate-order", "status-outcome-first"]
        );
    }

    #[tokio::test]
    async fn a_permission_or_io_error_imports_nothing_and_changes_nothing() {
        let hub = hub().await;
        // Existing memory, one of them under a slug the files also use.
        hub.write("gate-order", "project", "the seat's newer note")
            .await;
        hub.write("owner-note", "user", "kept as is").await;
        let before = hub.entries().await;
        let dir = tempfile::tempdir().unwrap();
        phase1(dir.path());
        let path = dir.path();
        for (target, errno) in [
            (path.join("gate-order.md"), EIO),
            (path.join("status-outcome-first.md"), EACCES),
            (path.join(INDEX_FILE), EIO),
            (path.to_path_buf(), EACCES),
            (path.to_path_buf(), EIO),
        ] {
            let files = Faulty::new(&[(&target, errno)]);
            let error = hub
                .import(path, false, &files)
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("nothing was imported"), "{error}");
            let message = io::Error::from_raw_os_error(errno).to_string();
            assert!(error.contains(&message), "{target:?}: {error}");
            // A dry run fails the same way.
            assert!(hub.import(path, true, &files).await.is_err());
            assert_eq!(hub.entries().await, before, "{target:?}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_unreadable_file_on_disk_is_an_error() {
        use std::os::unix::fs::PermissionsExt;
        let hub = hub().await;
        let dir = tempfile::tempdir().unwrap();
        phase1(dir.path());
        let locked = dir.path().join("devbox.md");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&locked).is_ok() {
            return; // running as root: permissions do not bind
        }
        let error = hub
            .import(dir.path(), false, &DiskFiles)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("devbox.md: cannot read it"), "{error}");
        assert_eq!(hub.entries().await, json!([]));
    }

    #[tokio::test]
    async fn dry_run_then_import_then_rerun_is_idempotent() {
        let hub = hub().await;
        let dir = tempfile::tempdir().unwrap();
        phase1(dir.path());
        let dry = hub.import(dir.path(), true, &DiskFiles).await.unwrap();
        assert_eq!(dry["dryRun"], true);
        assert_eq!(
            slugs(&dry, "created"),
            ["devbox", "gate-order", "status-outcome-first"]
        );
        assert_eq!(hub.entries().await, json!([]), "a dry run commits nothing");

        let first = hub.import(dir.path(), false, &DiskFiles).await.unwrap();
        assert_eq!(slugs(&first, "created"), slugs(&dry, "created"));
        let after = hub.entries().await;
        let origins: Vec<(&str, &str)> = after
            .as_array()
            .unwrap()
            .iter()
            .map(|e| (e["slug"].as_str().unwrap(), e["origin"].as_str().unwrap()))
            .collect();
        assert_eq!(
            origins,
            [
                ("status-outcome-first", "human"),
                ("gate-order", "agent"),
                ("devbox", "agent"),
            ]
        );
        for entry in after.as_array().unwrap() {
            assert!(entry.get("writerInstanceId").is_none(), "{entry}");
            assert_eq!(entry["revision"], 1);
        }

        let again = hub.import(dir.path(), false, &DiskFiles).await.unwrap();
        assert!(slugs(&again, "created").is_empty());
        let reasons: Vec<&str> = again["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["reason"].as_str().unwrap())
            .collect();
        assert_eq!(reasons, ["same-entry"; 3]);
        assert_eq!(hub.entries().await, after, "a re-run changes nothing");
    }

    #[tokio::test]
    async fn duplicates_by_slug_and_content_are_skipped_never_overwritten() {
        let hub = hub().await;
        hub.write("gate-order", "project", "the seat's newer note")
            .await;
        hub.write(
            "outcome-first",
            "feedback",
            "The owner asked on 2026-10-30 to lead status reports with the outcome.\n\
             Why: they read the first line to decide whether to read on.\n\
             How to apply: put the conclusion first.",
        )
        .await;
        let before = hub.entries().await;
        let dir = tempfile::tempdir().unwrap();
        phase1(dir.path());
        let value = hub.import(dir.path(), false, &DiskFiles).await.unwrap();
        assert_eq!(slugs(&value, "created"), ["devbox"]);
        assert_eq!(
            value["skipped"],
            json!([
                {"slug": "gate-order", "reason": "slug-exists"},
                {"slug": "status-outcome-first", "reason": "same-content",
                 "existing": "outcome-first"},
            ])
        );
        // The two existing entries are exactly as they were.
        let after = hub.entries().await;
        for old in before.as_array().unwrap() {
            assert!(after.as_array().unwrap().contains(old), "{old}");
        }
    }
}
