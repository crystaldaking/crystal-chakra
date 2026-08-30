//! Incremental Tree-sitter parsing evaluation harness (issue #45).
//!
//! Benchmark-only code: nothing here is used by production indexing. The
//! harness races the two parse strategies the issue asks about —
//! full reparse (`Parser::parse(source, None)`, what every production parser
//! does today) against retained-tree incremental parsing (`Tree::edit` +
//! `Parser::parse(new_source, Some(&old_tree))`) — and validates that the
//! incremental tree is structurally identical to a full reparse after every
//! edit. Chakra's extraction passes are deterministic pure functions of tree
//! plus source, so fingerprint equality implies extracted-fact equality.
//!
//! Timing runs are `#[ignore]`d release-only tests in
//! `tests/incremental.rs`; the correctness fuzz runs in the default suite
//! against deterministic generated sources and needs no corpus.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tree_sitter::{InputEdit, Language, Parser, Point, Tree};

#[cfg(unix)]
use nix::sys::resource::{UsageWho, getrusage};
#[cfg(unix)]
use nix::sys::time::TimeValLike;

use crate::corpus::{CorpusManifest, default_cache_root, default_manifest_path};
use crate::{Check, ensure, failure};

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Per-file size cap for benchmark documents, mirroring the production
/// per-file source budget (`MAX_SOURCE_FILE_BYTES` in chakra-language-index).
const MAX_DOCUMENT_BYTES: u64 = 8 * 1024 * 1024;
/// Source-byte cap for the retained-memory file sets so one measurement stays
/// inside a normal workstation budget.
const MEMORY_SET_MAX_SOURCE_BYTES: u64 = 128 * 1024 * 1024;
/// File-count cap for the retained-memory file sets.
const MEMORY_SET_MAX_FILES: usize = 4_000;

/// One language under evaluation.
#[derive(Debug, Clone, Copy)]
pub struct BenchLanguage {
    pub name: &'static str,
    pub extension: &'static str,
    grammar: fn() -> Language,
}

impl BenchLanguage {
    /// Builds a fresh parser for this language.
    pub fn parser(&self) -> Check<Parser> {
        let mut parser = Parser::new();
        parser
            .set_language(&(self.grammar)())
            .map_err(|error| failure(format!("{} grammar rejected: {error}", self.name)))?;
        Ok(parser)
    }
}

/// The representative language set of the evaluation: the two v0.1 languages
/// with bespoke indexers (Rust, PHP) plus three shared-driver languages
/// (Go, Python, TypeScript).
pub fn bench_languages() -> [BenchLanguage; 5] {
    [
        BenchLanguage {
            name: "rust",
            extension: "rs",
            grammar: || tree_sitter_rust::LANGUAGE.into(),
        },
        BenchLanguage {
            name: "php",
            extension: "php",
            grammar: || tree_sitter_php::LANGUAGE_PHP.into(),
        },
        BenchLanguage {
            name: "go",
            extension: "go",
            grammar: || tree_sitter_go::LANGUAGE.into(),
        },
        BenchLanguage {
            name: "python",
            extension: "py",
            grammar: || tree_sitter_python::LANGUAGE.into(),
        },
        BenchLanguage {
            name: "typescript",
            extension: "ts",
            grammar: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        },
    ]
}

/// One text replacement over a source string, in bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub start_byte: usize,
    pub old_end_byte: usize,
    pub new_text: String,
}

impl TextEdit {
    /// Applies the edit, returning the new source text.
    pub fn apply(&self, source: &str) -> String {
        let removed = self.old_end_byte.saturating_sub(self.start_byte);
        let mut next = String::with_capacity(source.len() - removed + self.new_text.len());
        next.push_str(&source[..self.start_byte]);
        next.push_str(&self.new_text);
        next.push_str(&source[self.old_end_byte..]);
        next
    }

    /// The Tree-sitter edit descriptor for applying this edit to a retained
    /// tree (`old` is the tree's text, `new` the post-edit text).
    pub fn input_edit(&self, old: &str, new: &str) -> InputEdit {
        let new_end_byte = self.start_byte + self.new_text.len();
        InputEdit {
            start_byte: self.start_byte,
            old_end_byte: self.old_end_byte,
            new_end_byte,
            start_position: point_for(old, self.start_byte),
            old_end_position: point_for(old, self.old_end_byte),
            new_end_position: point_for(new, new_end_byte),
        }
    }
}

/// Byte-based tree-sitter point for `byte` in `source` (columns are bytes
/// under the UTF-8 input encoding Chakra uses).
fn point_for(source: &str, byte: usize) -> Point {
    let prefix = &source[..byte];
    let row = prefix.bytes().filter(|byte| *byte == b'\n').count();
    let column = match prefix.rfind('\n') {
        Some(newline) => byte - (newline + 1),
        None => byte,
    };
    Point { row, column }
}

/// Structural fingerprint of a whole tree: every node in cursor order,
/// mixing exactly the properties Chakra's extraction passes read (kind, byte
/// range, named/error/missing flags).
pub fn tree_fingerprint(tree: &Tree) -> u64 {
    let mut hash = FNV_OFFSET;
    let mut cursor = tree.walk();
    loop {
        let node = cursor.node();
        for value in [
            u64::from(node.kind_id()),
            node.start_byte() as u64,
            node.end_byte() as u64,
            u64::from(node.is_named()),
            u64::from(node.is_error()),
            u64::from(node.is_missing()),
        ] {
            hash ^= value;
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return hash;
            }
        }
    }
}

/// Byte ranges of identifier-like tokens (ASCII alnum/underscore runs of at
/// least three bytes) — safe, boundary-aligned edit sites.
pub fn identifier_ranges(source: &str) -> Vec<(usize, usize)> {
    let bytes = source.as_bytes();
    let mut ranges = Vec::new();
    let mut start: Option<usize> = None;
    for (index, byte) in bytes.iter().enumerate() {
        let identifier = byte.is_ascii_alphanumeric() || *byte == b'_';
        match (start, identifier) {
            (None, true) => start = Some(index),
            (Some(from), false) => {
                if index - from >= 3 {
                    ranges.push((from, index));
                }
                start = None;
            }
            _ => {}
        }
    }
    if let Some(from) = start
        && bytes.len() - from >= 3
    {
        ranges.push((from, bytes.len()));
    }
    ranges
}

/// One wall+CPU sample.
#[derive(Debug, Clone, Copy, Default)]
pub struct Timing {
    pub wall: Duration,
    pub cpu: Duration,
}

/// Median/min summary of repeated samples.
#[derive(Debug, Clone, Copy, Default)]
pub struct TimingSummary {
    pub iterations: usize,
    pub median_wall: Duration,
    pub min_wall: Duration,
    pub median_cpu: Duration,
    pub min_cpu: Duration,
}

/// Summarizes samples by median and minimum.
pub fn summarize(timings: &[Timing]) -> TimingSummary {
    if timings.is_empty() {
        return TimingSummary::default();
    }
    let mut walls: Vec<Duration> = timings.iter().map(|timing| timing.wall).collect();
    let mut cpus: Vec<Duration> = timings.iter().map(|timing| timing.cpu).collect();
    walls.sort_unstable();
    cpus.sort_unstable();
    TimingSummary {
        iterations: timings.len(),
        median_wall: walls[walls.len() / 2],
        min_wall: walls[0],
        median_cpu: cpus[cpus.len() / 2],
        min_cpu: cpus[0],
    }
}

/// Runs `body`, measuring wall time and process CPU time of just that block.
fn timed<T>(body: impl FnOnce() -> T) -> (T, Timing) {
    let cpu_start = process_cpu_micros();
    let wall_start = Instant::now();
    let value = body();
    let wall = wall_start.elapsed();
    let cpu = process_cpu_micros()
        .zip(cpu_start)
        .map(|(end, start)| Duration::from_micros(end.saturating_sub(start)))
        .unwrap_or(wall);
    (value, Timing { wall, cpu })
}

#[cfg(unix)]
fn process_cpu_micros() -> Option<u64> {
    let usage = getrusage(UsageWho::RUSAGE_SELF).ok()?;
    let total = usage
        .user_time()
        .num_microseconds()
        .checked_add(usage.system_time().num_microseconds())?;
    u64::try_from(total).ok()
}

#[cfg(not(unix))]
fn process_cpu_micros() -> Option<u64> {
    None
}

/// Current resident set size, if the platform exposes one. Linux reads
/// `VmRSS`; other Unix falls back to the monotonic `getrusage` high-water
/// mark (an upper bound for deltas).
#[cfg(target_os = "linux")]
fn process_rss_bytes() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    line.split_whitespace()
        .nth(1)?
        .parse::<u64>()
        .ok()?
        .checked_mul(1024)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process_rss_bytes() -> Option<u64> {
    let rss = u64::try_from(getrusage(UsageWho::RUSAGE_SELF).ok()?.max_rss()).ok()?;
    #[cfg(target_vendor = "apple")]
    {
        Some(rss)
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        rss.checked_mul(1024)
    }
}

#[cfg(not(unix))]
fn process_rss_bytes() -> Option<u64> {
    None
}

/// How a scenario derives each iteration's edit from the current source.
#[derive(Debug, Clone)]
pub enum EditPattern {
    /// Replace one identifier with a fresh `zz_bench_<n>` token at a new
    /// deterministic position each iteration.
    SmallEdit,
    /// Alternate inserting and removing a language-specific broken snippet at
    /// one mid-file position.
    SyntaxError,
    /// Alternate the whole file content with another real document.
    AtomicReplace(String),
}

/// Outcome of one scenario run: timing for both arms plus the inline
/// correctness evidence.
#[derive(Debug, Clone)]
pub struct ScenarioOutcome {
    pub full: TimingSummary,
    pub incremental: TimingSummary,
    pub fingerprint_comparisons: u64,
    pub fingerprint_mismatches: u64,
}

fn broken_snippet(language: &BenchLanguage) -> &'static str {
    match language.name {
        "rust" => "fn zz_broken( {",
        "php" => "function zz_broken( {",
        "go" => "func zz_broken( {",
        "python" => "def zz_broken( :\n",
        "typescript" => "function zz_broken( {",
        _ => "(((",
    }
}

/// Derives iteration `index`'s edit from the current source. `pending`
/// carries the inverse removal edit for the syntax-error pattern.
fn iteration_edit(
    pattern: &EditPattern,
    language: &BenchLanguage,
    base: &str,
    current: &str,
    index: usize,
    pending: &mut Option<TextEdit>,
) -> Check<TextEdit> {
    match pattern {
        EditPattern::SmallEdit => {
            let ranges = identifier_ranges(current);
            let (start, end) = ranges
                .get((index.wrapping_mul(97).wrapping_add(31)) % ranges.len())
                .copied()
                .ok_or_else(|| failure("document has no identifier edit sites"))?;
            Ok(TextEdit {
                start_byte: start,
                old_end_byte: end,
                new_text: format!("zz_bench_{index}"),
            })
        }
        EditPattern::SyntaxError => {
            if let Some(removal) = pending.take() {
                return Ok(removal);
            }
            let ranges = identifier_ranges(current);
            let (start, _) = ranges
                .get(ranges.len() / 2)
                .copied()
                .ok_or_else(|| failure("document has no syntax-error insert site"))?;
            let snippet = broken_snippet(language);
            let insertion = TextEdit {
                start_byte: start,
                old_end_byte: start,
                new_text: snippet.to_owned(),
            };
            *pending = Some(TextEdit {
                start_byte: start,
                old_end_byte: start + snippet.len(),
                new_text: String::new(),
            });
            Ok(insertion)
        }
        EditPattern::AtomicReplace(other) => {
            let target = if current == base {
                other.as_str()
            } else {
                base
            };
            Ok(TextEdit {
                start_byte: 0,
                old_end_byte: current.len(),
                new_text: target.to_owned(),
            })
        }
    }
}

/// Cold-parse baseline: `iterations` fresh full parses with no old tree,
/// exactly what production pays per changed file today.
pub fn cold_parse_timing(
    language: &BenchLanguage,
    source: &str,
    iterations: usize,
) -> Check<TimingSummary> {
    let mut parser = language.parser()?;
    let mut timings = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let (tree, timing) = timed(|| parser.parse(source, None));
        drop(tree.ok_or_else(|| failure(format!("{}: cold parse failed", language.name)))?);
        timings.push(timing);
    }
    Ok(summarize(&timings))
}

/// Runs one edit scenario with both arms over the same edit sequence and
/// validates every incremental result against a full reparse of the same
/// text. Both arms start from the same base tree state.
pub fn run_edit_scenario(
    language: &BenchLanguage,
    base: &str,
    pattern: &EditPattern,
    iterations: usize,
) -> Check<ScenarioOutcome> {
    ensure(iterations > 0, "scenario needs at least one iteration")?;
    let mut full_parser = language.parser()?;
    let mut incremental_parser = language.parser()?;
    let base_tree = full_parser
        .parse(base, None)
        .ok_or_else(|| failure(format!("{}: base document produced no tree", language.name)))?;

    // Full arm: the production behavior — reparse the new text from scratch.
    let mut full_timings = Vec::with_capacity(iterations);
    let mut full_fingerprints = Vec::with_capacity(iterations);
    let mut current = base.to_owned();
    let mut pending = None;
    for index in 0..iterations {
        let edit = iteration_edit(pattern, language, base, &current, index, &mut pending)?;
        let next = edit.apply(&current);
        let (tree, timing) = timed(|| full_parser.parse(&next, None));
        let tree = tree.ok_or_else(|| failure(format!("{}: full parse aborted", language.name)))?;
        full_timings.push(timing);
        full_fingerprints.push(tree_fingerprint(&tree));
        current = next;
    }

    // Incremental arm: retain one tree, edit it, reparse with reuse.
    let mut incremental_timings = Vec::with_capacity(iterations);
    let mut incremental_fingerprints = Vec::with_capacity(iterations);
    let mut current = base.to_owned();
    let mut retained = base_tree.clone();
    let mut pending = None;
    for index in 0..iterations {
        let edit = iteration_edit(pattern, language, base, &current, index, &mut pending)?;
        let next = edit.apply(&current);
        let input_edit = edit.input_edit(&current, &next);
        let (tree, timing) = timed(|| {
            retained.edit(&input_edit);
            incremental_parser.parse(&next, Some(&retained))
        });
        let tree =
            tree.ok_or_else(|| failure(format!("{}: incremental parse aborted", language.name)))?;
        incremental_timings.push(timing);
        incremental_fingerprints.push(tree_fingerprint(&tree));
        retained = tree;
        current = next;
    }

    let mismatches = full_fingerprints
        .iter()
        .zip(&incremental_fingerprints)
        .filter(|(full, incremental)| full != incremental)
        .count() as u64;
    Ok(ScenarioOutcome {
        full: summarize(&full_timings),
        incremental: summarize(&incremental_timings),
        fingerprint_comparisons: iterations as u64,
        fingerprint_mismatches: mismatches,
    })
}

/// Deterministic xorshift64 for the fuzz sequence.
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        (self.next() % bound as u64) as usize
    }
}

const FUZZ_TOKENS: &[&str] = &[
    "zz_alpha",
    "zz_beta",
    "zz_gamma_9",
    "_delta",
    "zzλ_eta",
    "x",
];

fn fuzz_edit(rng: &mut XorShift, current: &str) -> Check<TextEdit> {
    let ranges = identifier_ranges(current);
    let (start, end) = ranges
        .get(rng.below(ranges.len()))
        .copied()
        .ok_or_else(|| failure("fuzz source ran out of edit sites"))?;
    match rng.below(10) {
        // Replace one identifier with a fuzz token.
        0..=6 => Ok(TextEdit {
            start_byte: start,
            old_end_byte: end,
            new_text: FUZZ_TOKENS[rng.below(FUZZ_TOKENS.len())].to_owned(),
        }),
        // Delete one identifier (leaves a syntax error in most grammars).
        7..=8 => Ok(TextEdit {
            start_byte: start,
            old_end_byte: end,
            new_text: String::new(),
        }),
        // Insert a token before one identifier.
        _ => Ok(TextEdit {
            start_byte: start,
            old_end_byte: start,
            new_text: format!("{} ", FUZZ_TOKENS[rng.below(FUZZ_TOKENS.len())]),
        }),
    }
}

/// Randomized edit-sequence equivalence evidence.
#[derive(Debug, Clone, Copy, Default)]
pub struct FuzzReport {
    pub steps: u64,
    pub mismatches: u64,
    pub error_tree_steps: u64,
}

/// Applies `steps` seeded random edits, maintaining a retained tree with
/// `Tree::edit` bookkeeping, and requires the incremental tree to equal a
/// full reparse at every step. This is the main correctness gate: it
/// exercises exactly the InputEdit bookkeeping an integration would own.
pub fn fuzz_edit_equivalence(
    language: &BenchLanguage,
    base: &str,
    steps: usize,
    seed: u64,
) -> Check<FuzzReport> {
    let mut full_parser = language.parser()?;
    let mut incremental_parser = language.parser()?;
    let mut rng = XorShift(seed.max(1));
    let mut current = base.to_owned();
    let mut retained = incremental_parser
        .parse(&current, None)
        .ok_or_else(|| failure(format!("{}: base document produced no tree", language.name)))?;
    let mut report = FuzzReport::default();
    for _ in 0..steps {
        let edit = fuzz_edit(&mut rng, &current)?;
        let next = edit.apply(&current);
        let input_edit = edit.input_edit(&current, &next);
        retained.edit(&input_edit);
        let incremental = incremental_parser
            .parse(&next, Some(&retained))
            .ok_or_else(|| failure(format!("{}: incremental parse aborted", language.name)))?;
        let full = full_parser
            .parse(&next, None)
            .ok_or_else(|| failure(format!("{}: full parse aborted", language.name)))?;
        report.steps = report.steps.saturating_add(1);
        if full.root_node().has_error() {
            report.error_tree_steps = report.error_tree_steps.saturating_add(1);
        }
        if tree_fingerprint(&incremental) != tree_fingerprint(&full)
            || incremental.root_node().has_error() != full.root_node().has_error()
        {
            report.mismatches = report.mismatches.saturating_add(1);
        }
        retained = incremental;
        current = next;
    }
    Ok(report)
}

/// Deterministic generated source for hermetic runs: `functions` small
/// declarations with cross-calls, valid for the language's grammar.
pub fn hermetic_source(language: &BenchLanguage, functions: usize) -> String {
    let mut source = String::new();
    match language.name {
        "rust" => {
            for index in 0..functions {
                source.push_str(&format!(
                    "pub fn bench_fn_{index}(value: u64) -> u64 {{\n    bench_helper_{index}(value).wrapping_add(1)\n}}\nfn bench_helper_{index}(value: u64) -> u64 {{\n    value ^ {index}\n}}\n"
                ));
            }
        }
        "php" => {
            source.push_str("<?php\n");
            for index in 0..functions {
                source.push_str(&format!(
                    "function bench_fn_{index}(int $value): int {{\n    return bench_helper_{index}($value) + 1;\n}}\nfunction bench_helper_{index}(int $value): int {{\n    return $value ^ {index};\n}}\n"
                ));
            }
        }
        "go" => {
            source.push_str("package bench\n\n");
            for index in 0..functions {
                source.push_str(&format!(
                    "func benchFn{index}(value int) int {{\n\treturn benchHelper{index}(value) + 1\n}}\nfunc benchHelper{index}(value int) int {{\n\treturn value ^ {index}\n}}\n"
                ));
            }
        }
        "python" => {
            for index in 0..functions {
                source.push_str(&format!(
                    "def bench_fn_{index}(value):\n    return bench_helper_{index}(value) + 1\n\ndef bench_helper_{index}(value):\n    return value ^ {index}\n\n"
                ));
            }
        }
        "typescript" => {
            for index in 0..functions {
                source.push_str(&format!(
                    "export function benchFn{index}(value: number): number {{\n    return benchHelper{index}(value) + 1;\n}}\nfunction benchHelper{index}(value: number): number {{\n    return value ^ {index};\n}}\n"
                ));
            }
        }
        _ => {}
    }
    source
}

/// One corpus document selected for benchmarking.
#[derive(Debug, Clone)]
pub struct CorpusDocument {
    pub label: String,
    pub source: String,
}

/// Benchmark documents selected from one language's cached corpus checkouts.
#[derive(Debug, Clone, Default)]
pub struct CorpusSelection {
    /// The biggest cached files of the language.
    pub largest: Vec<CorpusDocument>,
    /// A deterministic stride sample of files between 20 and 200 KiB.
    pub mediums: Vec<CorpusDocument>,
}

impl CorpusSelection {
    pub fn is_empty(&self) -> bool {
        self.largest.is_empty() && self.mediums.is_empty()
    }
}

/// Selects benchmark documents for one language from the fetched corpus
/// cache: the `largest` biggest files plus a deterministic stride sample of
/// `mediums` files between 20 and 200 KiB. Returns an empty selection when
/// the language has no cached checkouts — the caller records a skip.
pub fn corpus_documents(
    language: &BenchLanguage,
    largest: usize,
    mediums: usize,
) -> Check<CorpusSelection> {
    let manifest = CorpusManifest::load(&default_manifest_path())?;
    let Some(entry) = manifest.languages.get(language.name) else {
        return Ok(CorpusSelection::default());
    };
    let cache = default_cache_root();
    let mut files: Vec<(String, u64, std::path::PathBuf)> = Vec::new();
    for repository in &entry.repositories {
        let root = cache.join(repository.slug());
        if !root.is_dir() {
            continue;
        }
        let mut repo_files = Vec::new();
        collect_language_files(&root, &root, language.extension, &mut repo_files)?;
        for (label, size, path) in repo_files {
            files.push((format!("{}/{label}", repository.slug()), size, path));
        }
    }
    files.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    let mut selection = CorpusSelection::default();
    for (label, _, path) in files.iter().take(largest) {
        if let Some(source) = read_source(path)? {
            selection.largest.push(CorpusDocument {
                label: label.clone(),
                source,
            });
        }
    }
    let medium: Vec<&(String, u64, std::path::PathBuf)> = files
        .iter()
        .filter(|(_, size, _)| (20 * 1024..=200 * 1024).contains(size))
        .collect();
    if !medium.is_empty() {
        let stride = (medium.len() / mediums.max(1)).max(1);
        for entry in medium.iter().step_by(stride).take(mediums) {
            if let Some(source) = read_source(&entry.2)? {
                selection.mediums.push(CorpusDocument {
                    label: entry.0.clone(),
                    source,
                });
            }
        }
    }
    Ok(selection)
}

fn collect_language_files(
    root: &Path,
    directory: &Path,
    extension: &str,
    files: &mut Vec<(String, u64, std::path::PathBuf)>,
) -> Check<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_language_files(root, &path, extension, files)?;
        } else if file_type.is_file()
            && path.extension().is_some_and(|ext| ext == extension)
            && let Ok(metadata) = entry.metadata()
        {
            let size = metadata.len();
            if size > 0 && size <= MAX_DOCUMENT_BYTES {
                let label = path
                    .strip_prefix(root)
                    .map_or_else(|_| path.display().to_string(), |p| p.display().to_string());
                files.push((label, size, path));
            }
        }
    }
    Ok(())
}

fn read_source(path: &Path) -> Check<Option<String>> {
    match fs::read_to_string(path) {
        Ok(source) => Ok(Some(source)),
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Retained-memory evidence for one file set.
#[derive(Debug, Clone, Copy, Default)]
pub struct MemoryReport {
    pub files: usize,
    pub source_bytes: u64,
    pub rss_before_bytes: u64,
    pub rss_with_trees_bytes: u64,
}

impl MemoryReport {
    pub fn retained_delta_bytes(&self) -> u64 {
        self.rss_with_trees_bytes
            .saturating_sub(self.rss_before_bytes)
    }

    pub fn tree_bytes_per_source_byte(&self) -> Option<f64> {
        if self.source_bytes == 0 {
            return None;
        }
        Some(self.retained_delta_bytes() as f64 / self.source_bytes as f64)
    }
}

/// The bounded file set used for the retained-memory measurement: every
/// cached file of the language, deterministically stride-sampled into the
/// file/source caps. Returns an empty vector when nothing is cached.
pub fn memory_file_set(language: &BenchLanguage) -> Check<Vec<String>> {
    let manifest = CorpusManifest::load(&default_manifest_path())?;
    let Some(entry) = manifest.languages.get(language.name) else {
        return Ok(Vec::new());
    };
    let cache = default_cache_root();
    let mut files: Vec<(String, u64, std::path::PathBuf)> = Vec::new();
    for repository in &entry.repositories {
        let root = cache.join(repository.slug());
        if root.is_dir() {
            collect_language_files(&root, &root, language.extension, &mut files)?;
        }
    }
    files.sort();
    let stride = (files.len() / MEMORY_SET_MAX_FILES).max(1);
    let mut sources = Vec::new();
    let mut total = 0_u64;
    for (_, size, path) in files.iter().step_by(stride) {
        if total.saturating_add(*size) > MEMORY_SET_MAX_SOURCE_BYTES {
            break;
        }
        if let Some(source) = read_source(path)? {
            total = total.saturating_add(source.len() as u64);
            sources.push(source);
        }
    }
    Ok(sources)
}

/// Parses every source retaining its tree and measures the resident-set
/// delta. A warm-up pass primes the allocator so the delta belongs to the
/// retained trees, not to first-touch growth.
pub fn retained_tree_memory(language: &BenchLanguage, sources: &[String]) -> Check<MemoryReport> {
    ensure(!sources.is_empty(), "memory measurement needs sources")?;
    let mut parser = language.parser()?;
    let warmup = (sources.len() / 10).max(1);
    for source in sources.iter().take(warmup) {
        drop(parser.parse(source, None));
    }
    let rss_before = process_rss_bytes().unwrap_or(0);
    let mut trees = Vec::with_capacity(sources.len());
    let mut source_bytes = 0_u64;
    for source in sources {
        source_bytes = source_bytes.saturating_add(source.len() as u64);
        let tree = parser
            .parse(source, None)
            .ok_or_else(|| failure(format!("{}: memory-set parse failed", language.name)))?;
        trees.push(tree);
    }
    let rss_with_trees = process_rss_bytes().unwrap_or(0);
    let report = MemoryReport {
        files: trees.len(),
        source_bytes,
        rss_before_bytes: rss_before,
        rss_with_trees_bytes: rss_with_trees,
    };
    drop(trees);
    Ok(report)
}

/// Cancellation-latency evidence for both arms.
#[derive(Debug, Clone, Copy, Default)]
pub struct CancellationReport {
    pub full: TimingSummary,
    pub incremental: TimingSummary,
    /// Iterations where the full-arm parse finished before the signal (the
    /// recorded sample is then the whole parse time, an upper bound).
    pub full_completed_before_signal: u64,
    /// Same for the incremental arm.
    pub incremental_completed_before_signal: u64,
}

/// Measures signal-to-return latency when a parse is cancelled through the
/// tree-sitter 0.26 progress callback. Both arms parse the same text — the
/// source with one mid-file broken snippet inserted, modeling a document
/// mid-typing — and both are cancelled by a background thread tripping an
/// atomic flag. The signal delay is self-calibrated per arm to one quarter
/// of that arm's uncancelled parse time (clamped to ≥20 µs), because the
/// two arms differ by orders of magnitude and any fixed delay either misses
/// the fast arm or never fires in the slow one. Production installs no
/// parser-level callback today, so this compares interruptibility of the two
/// strategies.
pub fn cancellation_latency(
    language: &BenchLanguage,
    source: &str,
    iterations: usize,
) -> Check<CancellationReport> {
    let edit = {
        let ranges = identifier_ranges(source);
        let (start, _) = ranges
            .get(ranges.len() / 2)
            .copied()
            .ok_or_else(|| failure("cancellation document has no edit site"))?;
        TextEdit {
            start_byte: start,
            old_end_byte: start,
            new_text: broken_snippet(language).to_owned(),
        }
    };
    let edited = edit.apply(source);

    let mut full_parser = language.parser()?;
    let full_delay = calibrate_delay(&mut || full_parser.parse(&edited, None))?;
    // Calibrate on just the incremental parse: the base parse of the
    // unedited source must not be part of the timed closure.
    let mut calibration_parser = language.parser()?;
    let mut old_tree = calibration_parser
        .parse(source, None)
        .ok_or_else(|| failure(format!("{}: base parse failed", language.name)))?;
    old_tree.edit(&edit.input_edit(source, &edited));
    let mut incremental_parser = language.parser()?;
    let incremental_delay =
        calibrate_delay(&mut || incremental_parser.parse(&edited, Some(&old_tree)))?;

    let mut full_timings = Vec::with_capacity(iterations);
    let mut incremental_timings = Vec::with_capacity(iterations);
    let mut report = CancellationReport::default();
    for _ in 0..iterations {
        let (timing, completed) = cancelled_parse(language, &edited, None, full_delay)?;
        full_timings.push(timing);
        report.full_completed_before_signal += u64::from(completed);
        let mut old_tree = language
            .parser()?
            .parse(source, None)
            .ok_or_else(|| failure(format!("{}: base parse failed", language.name)))?;
        old_tree.edit(&edit.input_edit(source, &edited));
        let (timing, completed) =
            cancelled_parse(language, &edited, Some(&old_tree), incremental_delay)?;
        incremental_timings.push(timing);
        report.incremental_completed_before_signal += u64::from(completed);
    }
    report.full = summarize(&full_timings);
    report.incremental = summarize(&incremental_timings);
    Ok(report)
}

/// Finds the signal delay for one arm: a quarter of the fastest of three
/// uncancelled parses, never below 20 µs.
fn calibrate_delay(parse: &mut impl FnMut() -> Option<Tree>) -> Check<Duration> {
    let mut fastest = Duration::MAX;
    for _ in 0..3 {
        let (tree, timing) = timed(&mut *parse);
        ensure(tree.is_some(), "calibration parse produced no tree")?;
        fastest = fastest.min(timing.wall);
    }
    Ok((fastest / 4).max(Duration::from_micros(20)))
}

/// One cancelled parse. Returns the signal-to-return latency and whether the
/// parse completed before the signal took effect; a completed parse reports
/// its whole parse time, which is the effective interruption latency for a
/// parse that finishes faster than any cancellation signal can arrive.
fn cancelled_parse(
    language: &BenchLanguage,
    text: &str,
    old_tree: Option<&Tree>,
    delay: Duration,
) -> Check<(Timing, bool)> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let signalled_at = Arc::new(Mutex::new(None::<Instant>));
    let thread_cancelled = Arc::clone(&cancelled);
    let thread_signalled_at = Arc::clone(&signalled_at);
    let canceller = thread::spawn(move || {
        thread::sleep(delay);
        thread_cancelled.store(true, Ordering::SeqCst);
        if let Ok(mut slot) = thread_signalled_at.lock() {
            *slot = Some(Instant::now());
        }
    });
    let callback_cancelled = Arc::clone(&cancelled);
    let mut callback = move |_: &tree_sitter::ParseState| {
        if callback_cancelled.load(Ordering::SeqCst) {
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        }
    };
    let options = tree_sitter::ParseOptions::new().progress_callback(&mut callback);
    let bytes = text.as_bytes();
    let mut parser = language.parser()?;
    let parse_started = Instant::now();
    let result = parser.parse_with_options(
        &mut |offset, _| bytes.get(offset..).unwrap_or(&[]),
        old_tree,
        Some(options),
    );
    let returned_at = Instant::now();
    canceller
        .join()
        .map_err(|_| failure("canceller thread panicked"))?;
    let signalled = signalled_at
        .lock()
        .map_err(|_| failure("signalled-at slot poisoned"))?
        .ok_or_else(|| failure("canceller never recorded the signal instant"))?;
    if result.is_some() {
        // The parse finished before the signal fired; the whole parse time is
        // the effective interruption latency for work this fast.
        return Ok((
            Timing {
                wall: parse_started.elapsed(),
                cpu: Duration::ZERO,
            },
            true,
        ));
    }
    Ok((
        Timing {
            wall: returned_at.saturating_duration_since(signalled),
            cpu: Duration::ZERO,
        },
        false,
    ))
}

/// One rendered results-table row.
#[derive(Debug, Clone)]
pub struct ScenarioRow {
    pub language: String,
    pub document: String,
    pub source_bytes: usize,
    pub scenario: String,
    pub outcome: ScenarioOutcome,
}

impl ScenarioRow {
    pub fn speedup(&self) -> f64 {
        let incremental = self.outcome.incremental.median_wall.as_secs_f64();
        if incremental <= f64::EPSILON {
            return f64::INFINITY;
        }
        self.outcome.full.median_wall.as_secs_f64() / incremental
    }
}

/// Renders rows as a markdown table for the evaluation document.
pub fn render_results_markdown(rows: &[ScenarioRow]) -> String {
    let mut out = String::from(
        "| Language | Document | KiB | Scenario | Full median wall | Incr median wall | Speedup | Full median CPU | Incr median CPU | Fingerprint mismatches |\n",
    );
    out.push_str("|---|---|---:|---|---:|---:|---:|---:|---:|---:|\n");
    for row in rows {
        let full = &row.outcome.full;
        let incremental = &row.outcome.incremental;
        out.push_str(&format!(
            "| {} | {} | {:.0} | {} | {:.3} ms | {:.3} ms | {:.1}x | {:.3} ms | {:.3} ms | {}/{} |\n",
            row.language,
            row.document,
            row.source_bytes as f64 / 1024.0,
            row.scenario,
            full.median_wall.as_secs_f64() * 1e3,
            incremental.median_wall.as_secs_f64() * 1e3,
            row.speedup(),
            full.median_cpu.as_secs_f64() * 1e3,
            incremental.median_cpu.as_secs_f64() * 1e3,
            row.outcome.fingerprint_mismatches,
            row.outcome.fingerprint_comparisons,
        ));
    }
    out
}
