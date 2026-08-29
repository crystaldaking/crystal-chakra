//! Compact binary encoding of per-file syntax facts and the cache manifest
//! (issue #39, budget B4: persisted facts must fit within 2x the retained
//! source bytes; JSON is rejected as the production encoding).
//!
//! The format is deliberately small and fully validated on decode:
//!
//! - integers are LEB128 varints; positions are stored zero-based;
//! - every string of a file lives once in a per-file string table and is
//!   referenced by varint index (source ranges never repeat the file path —
//!   it is implied by the fact file);
//! - enums are single-byte tags; an unknown tag is corruption, never a
//!   guess;
//! - every payload carries [`INDEX_FORMAT_VERSION`], a length, and a
//!   128-bit BLAKE3 checksum, and the decoder must consume it exactly.
//!
//! Any decode failure means the entry is treated as a cache miss (fact
//! file) or as a full deterministic-rebuild fallback (manifest).

use std::collections::HashMap;

use chakra_domain::diagnostic::{
    KnownSyntaxGrammarGap, SyntaxDiagnostic, SyntaxDiagnosticCause, SyntaxDiagnosticKind,
};
use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::symbol::{
    CallForm, CallTargetKind, EdgeKind, Language, ReceiverTypeSource, SymbolKind,
};
use thiserror::Error;

use super::facts::{
    CallFact, FileSyntaxFacts, ImplFact, NamedRelationFact, SymbolFact, TypeRelationFact,
    TypeRelationKindFact,
};
use super::store::{CompatibilityKey, ManifestEntry};

/// Wire format version of the fact/manifest payloads. Bump on any layout
/// change; a mismatch invalidates every cache entry through the
/// compatibility key and the per-file payload header.
pub const INDEX_FORMAT_VERSION: u32 = 1;
/// Version of the graph model the facts reassemble into (entity-id slot
/// layout, materialization semantics). Bump when `chakra-engine` changes
/// graph construction in a way that invalidates reassembled revisions.
pub const GRAPH_MODEL_VERSION: u32 = 1;

const FILE_MAGIC: [u8; 4] = *b"CKF1";
const MANIFEST_MAGIC: [u8; 4] = *b"CKM1";
const CHECKSUM_BYTES: usize = 16;
const LENGTH_BYTES: usize = 4;
/// Envelope overhead: magic + length + checksum.
const ENVELOPE_BYTES: usize = 4 + LENGTH_BYTES + CHECKSUM_BYTES;

/// Why a payload could not be decoded. Every variant is handled as a cache
/// miss or a deterministic-rebuild fallback; none is fatal.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CodecError {
    #[error("payload has a bad magic or length")]
    Envelope,
    #[error("payload checksum mismatch")]
    Checksum,
    #[error("payload is truncated")]
    Truncated,
    #[error("payload has trailing bytes")]
    TrailingBytes,
    #[error("varint overflows u64")]
    Overflow,
    #[error("string is not valid UTF-8")]
    Utf8,
    #[error("string table index {0} is out of bounds")]
    StringIndex(u64),
    #[error("invalid {0} tag {1}")]
    InvalidTag(&'static str, u8),
    #[error("invalid position or range")]
    InvalidRange,
    #[error("invalid repository-relative path")]
    InvalidPath,
    #[error("format version {found} does not match expected {expected}")]
    FormatVersion { expected: u32, found: u32 },
}

// ---------------------------------------------------------------------------
// Primitive writer/reader
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    fn byte(&mut self, value: u8) {
        self.buf.push(value);
    }

    fn varint(&mut self, mut value: u64) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                self.buf.push(byte);
                return;
            }
            self.buf.push(byte | 0x80);
        }
    }

    fn fixed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    fn raw_str(&mut self, value: &str) {
        self.varint(value.len() as u64);
        self.buf.extend_from_slice(value.as_bytes());
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], CodecError> {
        let end = self.pos.checked_add(count).ok_or(CodecError::Overflow)?;
        let slice = self.buf.get(self.pos..end).ok_or(CodecError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    fn byte(&mut self) -> Result<u8, CodecError> {
        Ok(self.take(1)?[0])
    }

    fn varint(&mut self) -> Result<u64, CodecError> {
        let mut value = 0_u64;
        let mut shift = 0_u32;
        loop {
            let byte = self.byte()?;
            if shift == 63 && byte > 1 {
                return Err(CodecError::Overflow);
            }
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
            if shift > 63 {
                return Err(CodecError::Overflow);
            }
        }
    }

    fn fixed<const N: usize>(&mut self) -> Result<[u8; N], CodecError> {
        let slice = self.take(N)?;
        let mut out = [0_u8; N];
        out.copy_from_slice(slice);
        Ok(out)
    }

    fn raw_str(&mut self) -> Result<&'a str, CodecError> {
        let len = usize::try_from(self.varint()?).map_err(|_| CodecError::Overflow)?;
        std::str::from_utf8(self.take(len)?).map_err(|_| CodecError::Utf8)
    }

    fn finish(self) -> Result<(), CodecError> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(CodecError::TrailingBytes)
        }
    }
}

// ---------------------------------------------------------------------------
// Envelope
// ---------------------------------------------------------------------------

fn seal(magic: [u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ENVELOPE_BYTES + payload.len());
    out.extend_from_slice(&magic);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    let checksum = blake3::hash(payload);
    out.extend_from_slice(&checksum.as_bytes()[..CHECKSUM_BYTES]);
    out.extend_from_slice(payload);
    out
}

fn open(magic: [u8; 4], raw: &[u8]) -> Result<&[u8], CodecError> {
    if raw.len() < ENVELOPE_BYTES || raw[..4] != magic {
        return Err(CodecError::Envelope);
    }
    let mut length = [0_u8; LENGTH_BYTES];
    length.copy_from_slice(&raw[4..4 + LENGTH_BYTES]);
    let length = u32::from_le_bytes(length) as usize;
    let payload = raw
        .get(ENVELOPE_BYTES..ENVELOPE_BYTES + length)
        .ok_or(CodecError::Envelope)?;
    if ENVELOPE_BYTES + length != raw.len() {
        return Err(CodecError::Envelope);
    }
    let checksum = blake3::hash(payload);
    if raw[4 + LENGTH_BYTES..ENVELOPE_BYTES] != checksum.as_bytes()[..CHECKSUM_BYTES] {
        return Err(CodecError::Checksum);
    }
    Ok(payload)
}

fn version(reader: &mut Reader<'_>) -> Result<(), CodecError> {
    let found = u32::try_from(reader.varint()?).map_err(|_| CodecError::Overflow)?;
    if found != INDEX_FORMAT_VERSION {
        return Err(CodecError::FormatVersion {
            expected: INDEX_FORMAT_VERSION,
            found,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Enum tags
// ---------------------------------------------------------------------------

fn tag<T>(value: T, table: &[(T, u8)]) -> u8
where
    T: Copy + PartialEq,
{
    table
        .iter()
        .find(|(candidate, _)| *candidate == value)
        .map(|(_, tag)| *tag)
        .unwrap_or(u8::MAX)
}

fn untag<T: Copy>(kind: &'static str, value: u8, table: &[(T, u8)]) -> Result<T, CodecError> {
    table
        .iter()
        .find(|(_, tag)| *tag == value)
        .map(|(value, _)| *value)
        .ok_or(CodecError::InvalidTag(kind, value))
}

const LANGUAGES: &[(Language, u8)] = &[
    (Language::Rust, 0),
    (Language::Php, 1),
    (Language::TypeScript, 2),
    (Language::Python, 3),
    (Language::JavaScript, 4),
    (Language::Java, 5),
    (Language::CSharp, 6),
    (Language::Shell, 7),
    (Language::Cpp, 8),
    (Language::Hcl, 9),
    (Language::Go, 10),
];

pub fn language_tag(language: Language) -> u8 {
    tag(language, LANGUAGES)
}

pub fn language_from_tag(value: u8) -> Result<Language, CodecError> {
    untag("language", value, LANGUAGES)
}

const SYMBOL_KINDS: &[(SymbolKind, u8)] = &[
    (SymbolKind::Module, 0),
    (SymbolKind::Function, 1),
    (SymbolKind::Method, 2),
    (SymbolKind::Struct, 3),
    (SymbolKind::Class, 4),
    (SymbolKind::Enum, 5),
    (SymbolKind::Trait, 6),
    (SymbolKind::Interface, 7),
    (SymbolKind::Constant, 8),
    (SymbolKind::Field, 9),
    (SymbolKind::Property, 10),
    (SymbolKind::TypeAlias, 11),
    (SymbolKind::ImplBlock, 12),
    (SymbolKind::Import, 13),
    (SymbolKind::Test, 14),
    (SymbolKind::Configuration, 15),
];

const EDGE_KINDS: &[(EdgeKind, u8)] = &[
    (EdgeKind::Contains, 0),
    (EdgeKind::Defines, 1),
    (EdgeKind::References, 2),
    (EdgeKind::Calls, 3),
    (EdgeKind::Imports, 4),
    (EdgeKind::Implements, 5),
    (EdgeKind::Extends, 6),
    (EdgeKind::Tests, 7),
    (EdgeKind::DependsOn, 8),
    (EdgeKind::Binds, 9),
    (EdgeKind::Resolves, 10),
    (EdgeKind::RoutesTo, 11),
    (EdgeKind::Dispatches, 12),
    (EdgeKind::ListensTo, 13),
    (EdgeKind::Schedules, 14),
    (EdgeKind::Registers, 15),
    (EdgeKind::AuthorizesWith, 16),
    (EdgeKind::ModifiedBy, 17),
];

const CALL_FORMS: &[(CallForm, u8)] = &[
    (CallForm::Function, 0),
    (CallForm::Member, 1),
    (CallForm::NullsafeMember, 2),
    (CallForm::Scoped, 3),
];

const CALL_TARGET_KINDS: &[(CallTargetKind, u8)] = &[
    (CallTargetKind::Function, 0),
    (CallTargetKind::Method, 1),
    (CallTargetKind::FunctionOrMethod, 2),
    (CallTargetKind::Test, 3),
    (CallTargetKind::Configuration, 4),
];

const RECEIVER_TYPE_SOURCES: &[(ReceiverTypeSource, u8)] = &[
    (ReceiverTypeSource::This, 0),
    (ReceiverTypeSource::Parameter, 1),
    (ReceiverTypeSource::Property, 2),
    (ReceiverTypeSource::PromotedProperty, 3),
    (ReceiverTypeSource::LocalNew, 4),
    (ReceiverTypeSource::ServiceLocator, 5),
    (ReceiverTypeSource::ScopedType, 6),
    (ReceiverTypeSource::SelfType, 7),
    (ReceiverTypeSource::ParentType, 8),
];

const DIAGNOSTIC_KINDS: &[(SyntaxDiagnosticKind, u8)] = &[
    (SyntaxDiagnosticKind::Error, 0),
    (SyntaxDiagnosticKind::Missing, 1),
];

const GRAMMAR_GAPS: &[(KnownSyntaxGrammarGap, u8)] = &[
    (KnownSyntaxGrammarGap::PhpTypedClassConstantNamedDefault, 0),
    (KnownSyntaxGrammarGap::RustLifetimeFirstTraitObject, 1),
    (KnownSyntaxGrammarGap::RustAttributeOnPatternField, 2),
];

const TYPE_RELATION_KINDS: &[(TypeRelationKindFact, u8)] = &[
    (TypeRelationKindFact::Trait, 0),
    (TypeRelationKindFact::Extends, 1),
    (TypeRelationKindFact::Implements, 2),
];

// ---------------------------------------------------------------------------
// Per-file facts
// ---------------------------------------------------------------------------

/// Per-file string table: every string is stored once and referenced by
/// varint index. Table construction order is fixed by the encode walk, so
/// the encoding is deterministic.
struct StringTable<'a> {
    indices: HashMap<&'a str, u64>,
    strings: Vec<&'a str>,
}

impl<'a> StringTable<'a> {
    fn new() -> Self {
        Self {
            indices: HashMap::new(),
            strings: Vec::new(),
        }
    }

    fn intern(&mut self, value: &'a str) {
        if !self.indices.contains_key(value) {
            self.indices.insert(value, self.strings.len() as u64);
            self.strings.push(value);
        }
    }

    fn index(&self, value: &str) -> u64 {
        // Every encoded string is interned during the collect walk.
        self.indices.get(value).copied().unwrap_or(u64::MAX)
    }
}

fn intern_option<'a>(table: &mut StringTable<'a>, value: &'a Option<String>) {
    if let Some(value) = value {
        table.intern(value);
    }
}

fn collect_strings<'a>(table: &mut StringTable<'a>, facts: &'a FileSyntaxFacts) {
    table.intern(facts.path.as_str());
    for value in &facts.module_path {
        table.intern(value);
    }
    for value in &facts.extension_scopes {
        table.intern(value);
    }
    for symbol in &facts.symbols {
        table.intern(&symbol.qualified_name);
        intern_option(table, &symbol.container);
        intern_option(table, &symbol.signature);
    }
    for call in &facts.calls {
        table.intern(&call.name);
        intern_option(table, &call.qualifier);
        intern_option(table, &call.receiver_type);
        intern_option(table, &call.receiver_hint);
    }
    for relation in &facts.named_relations {
        for candidate in &relation.candidates {
            table.intern(candidate);
        }
    }
    for relation in &facts.type_relations {
        table.intern(&relation.target);
    }
    for implementation in &facts.implementations {
        for value in &implementation.module_path {
            table.intern(value);
        }
        intern_option(table, &implementation.target_lookup);
        intern_option(table, &implementation.trait_lookup);
    }
    for diagnostic in &facts.diagnostics {
        table.intern(&diagnostic.node_kind);
    }
}

fn put_index(writer: &mut Writer, table: &StringTable<'_>, value: &str) {
    writer.varint(table.index(value));
}

fn put_option(writer: &mut Writer, table: &StringTable<'_>, value: &Option<String>) {
    match value {
        Some(value) => {
            writer.varint(table.index(value).saturating_add(1));
        }
        None => writer.varint(0),
    }
}

fn read_option<'a>(
    reader: &mut Reader<'a>,
    strings: &[String],
) -> Result<Option<String>, CodecError> {
    let index = reader.varint()?;
    if index == 0 {
        return Ok(None);
    }
    let index = usize::try_from(index - 1).map_err(|_| CodecError::Overflow)?;
    Ok(Some(
        strings
            .get(index)
            .cloned()
            .ok_or(CodecError::StringIndex(index as u64))?,
    ))
}

fn read_index(reader: &mut Reader<'_>, strings: &[String]) -> Result<String, CodecError> {
    let index = reader.varint()?;
    let index = usize::try_from(index).map_err(|_| CodecError::Overflow)?;
    strings
        .get(index)
        .cloned()
        .ok_or(CodecError::StringIndex(index as u64))
}

fn put_position(writer: &mut Writer, position: TextPosition) {
    writer.varint(u64::from(position.line().saturating_sub(1)));
    writer.varint(u64::from(position.column().saturating_sub(1)));
}

fn read_position(reader: &mut Reader<'_>) -> Result<TextPosition, CodecError> {
    let line = reader.varint()?;
    let column = reader.varint()?;
    let line = u32::try_from(line.saturating_add(1)).map_err(|_| CodecError::InvalidRange)?;
    let column = u32::try_from(column.saturating_add(1)).map_err(|_| CodecError::InvalidRange)?;
    TextPosition::new(line, column).map_err(|_| CodecError::InvalidRange)
}

fn put_range(writer: &mut Writer, range: &SourceRange) {
    put_position(writer, range.start());
    put_position(writer, range.end());
}

fn read_range(reader: &mut Reader<'_>, file: &RepoRelativePath) -> Result<SourceRange, CodecError> {
    let start = read_position(reader)?;
    let end = read_position(reader)?;
    SourceRange::new(file.clone(), start, end).map_err(|_| CodecError::InvalidRange)
}

fn put_usize(writer: &mut Writer, value: usize) {
    writer.varint(value as u64);
}

fn read_usize(reader: &mut Reader<'_>) -> Result<usize, CodecError> {
    usize::try_from(reader.varint()?).map_err(|_| CodecError::Overflow)
}

fn put_bool(writer: &mut Writer, value: bool) {
    writer.byte(u8::from(value));
}

fn read_bool(reader: &mut Reader<'_>) -> Result<bool, CodecError> {
    match reader.byte()? {
        0 => Ok(false),
        1 => Ok(true),
        other => Err(CodecError::InvalidTag("bool", other)),
    }
}

/// Encodes one file's facts into a sealed envelope.
pub fn encode_file_facts(facts: &FileSyntaxFacts) -> Vec<u8> {
    let mut table = StringTable::new();
    collect_strings(&mut table, facts);
    let mut writer = Writer::default();
    writer.varint(u64::from(INDEX_FORMAT_VERSION));
    writer.varint(table.strings.len() as u64);
    for value in &table.strings {
        writer.raw_str(value);
    }
    writer.varint(facts.byte_len);
    put_index(&mut writer, &table, facts.path.as_str());
    writer.varint(facts.module_path.len() as u64);
    for value in &facts.module_path {
        put_index(&mut writer, &table, value);
    }
    writer.varint(facts.extension_scopes.len() as u64);
    for value in &facts.extension_scopes {
        put_index(&mut writer, &table, value);
    }
    writer.varint(facts.symbols.len() as u64);
    for symbol in &facts.symbols {
        put_index(&mut writer, &table, &symbol.qualified_name);
        put_option(&mut writer, &table, &symbol.container);
        writer.byte(tag(symbol.kind, SYMBOL_KINDS));
        put_range(&mut writer, &symbol.location);
        put_option(&mut writer, &table, &symbol.signature);
        match symbol.parent {
            Some(parent) => writer.varint(parent as u64 + 1),
            None => writer.varint(0),
        }
        put_bool(&mut writer, symbol.is_extension_method);
    }
    writer.varint(facts.calls.len() as u64);
    for call in &facts.calls {
        put_usize(&mut writer, call.caller);
        writer.byte(tag(call.form, CALL_FORMS));
        writer.byte(tag(call.target_kind, CALL_TARGET_KINDS));
        put_index(&mut writer, &table, &call.name);
        put_option(&mut writer, &table, &call.qualifier);
        put_option(&mut writer, &table, &call.receiver_type);
        match call.receiver_type_source {
            Some(source) => writer.varint(u64::from(tag(source, RECEIVER_TYPE_SOURCES)) + 1),
            None => writer.varint(0),
        }
        put_option(&mut writer, &table, &call.receiver_hint);
        put_bool(&mut writer, call.promoted);
        put_range(&mut writer, &call.location);
    }
    writer.varint(facts.named_relations.len() as u64);
    for relation in &facts.named_relations {
        put_usize(&mut writer, relation.from);
        writer.varint(relation.candidates.len() as u64);
        for candidate in &relation.candidates {
            put_index(&mut writer, &table, candidate);
        }
        writer.varint(relation.target_kinds.len() as u64);
        for kind in &relation.target_kinds {
            writer.byte(tag(*kind, SYMBOL_KINDS));
        }
        writer.byte(tag(relation.kind, EDGE_KINDS));
    }
    writer.varint(facts.type_relations.len() as u64);
    for relation in &facts.type_relations {
        put_usize(&mut writer, relation.from);
        put_index(&mut writer, &table, &relation.target);
        writer.byte(tag(relation.kind, TYPE_RELATION_KINDS));
    }
    writer.varint(facts.implementations.len() as u64);
    for implementation in &facts.implementations {
        put_usize(&mut writer, implementation.symbol);
        writer.varint(implementation.module_path.len() as u64);
        for value in &implementation.module_path {
            put_index(&mut writer, &table, value);
        }
        put_option(&mut writer, &table, &implementation.target_lookup);
        put_option(&mut writer, &table, &implementation.trait_lookup);
    }
    put_bool(&mut writer, facts.has_errors);
    writer.varint(facts.diagnostic_count);
    writer.varint(facts.diagnostics.len() as u64);
    for diagnostic in &facts.diagnostics {
        writer.byte(tag(diagnostic.kind, DIAGNOSTIC_KINDS));
        match diagnostic.cause {
            SyntaxDiagnosticCause::ParseRecovery => writer.byte(0),
            SyntaxDiagnosticCause::KnownGrammarGap(gap) => {
                writer.byte(1);
                writer.byte(tag(gap, GRAMMAR_GAPS));
            }
        }
        put_index(&mut writer, &table, &diagnostic.node_kind);
        put_range(&mut writer, &diagnostic.range);
    }
    seal(FILE_MAGIC, &writer.buf)
}

/// Decodes one sealed facts envelope. `expected_path` must match the
/// recorded path, binding the payload to its manifest entry; `language`
/// pins the diagnostic language to the owning adapter partition.
pub fn decode_file_facts(
    raw: &[u8],
    expected_path: &RepoRelativePath,
    language: Language,
) -> Result<FileSyntaxFacts, CodecError> {
    let payload = open(FILE_MAGIC, raw)?;
    let mut reader = Reader::new(payload);
    version(&mut reader)?;
    let string_count = read_usize(&mut reader)?;
    let mut strings = Vec::with_capacity(string_count.min(1 << 20));
    for _ in 0..string_count {
        strings.push(reader.raw_str()?.to_owned());
    }
    let byte_len = reader.varint()?;
    let path = RepoRelativePath::new(read_index(&mut reader, &strings)?)
        .map_err(|_| CodecError::InvalidPath)?;
    if path != *expected_path {
        return Err(CodecError::InvalidPath);
    }
    let module_path = read_string_list(&mut reader, &strings)?;
    let extension_scopes = read_string_list(&mut reader, &strings)?;
    let mut symbols = Vec::new();
    for _ in 0..read_usize(&mut reader)? {
        let qualified_name = read_index(&mut reader, &strings)?;
        let container = read_option(&mut reader, &strings)?;
        let kind = untag("symbol kind", reader.byte()?, SYMBOL_KINDS)?;
        let location = read_range(&mut reader, &path)?;
        let signature = read_option(&mut reader, &strings)?;
        let parent = match reader.varint()? {
            0 => None,
            value => Some(read_usize_from(value - 1)?),
        };
        let is_extension_method = read_bool(&mut reader)?;
        symbols.push(SymbolFact {
            qualified_name,
            container,
            kind,
            location,
            signature,
            parent,
            is_extension_method,
        });
    }
    let mut calls = Vec::new();
    for _ in 0..read_usize(&mut reader)? {
        let caller = read_usize(&mut reader)?;
        let form = untag("call form", reader.byte()?, CALL_FORMS)?;
        let target_kind = untag("call target kind", reader.byte()?, CALL_TARGET_KINDS)?;
        let name = read_index(&mut reader, &strings)?;
        let qualifier = read_option(&mut reader, &strings)?;
        let receiver_type = read_option(&mut reader, &strings)?;
        let receiver_type_source = match reader.varint()? {
            0 => None,
            value => Some(untag(
                "receiver type source",
                u8::try_from(value - 1).map_err(|_| CodecError::Overflow)?,
                RECEIVER_TYPE_SOURCES,
            )?),
        };
        let receiver_hint = read_option(&mut reader, &strings)?;
        let promoted = read_bool(&mut reader)?;
        let location = read_range(&mut reader, &path)?;
        calls.push(CallFact {
            caller,
            form,
            target_kind,
            name,
            qualifier,
            receiver_type,
            receiver_type_source,
            receiver_hint,
            promoted,
            location,
        });
    }
    let mut named_relations = Vec::new();
    for _ in 0..read_usize(&mut reader)? {
        let from = read_usize(&mut reader)?;
        let mut candidates = Vec::new();
        for _ in 0..read_usize(&mut reader)? {
            candidates.push(read_index(&mut reader, &strings)?);
        }
        let mut target_kinds = Vec::new();
        for _ in 0..read_usize(&mut reader)? {
            target_kinds.push(untag("symbol kind", reader.byte()?, SYMBOL_KINDS)?);
        }
        let kind = untag("edge kind", reader.byte()?, EDGE_KINDS)?;
        named_relations.push(NamedRelationFact {
            from,
            candidates,
            target_kinds,
            kind,
        });
    }
    let mut type_relations = Vec::new();
    for _ in 0..read_usize(&mut reader)? {
        let from = read_usize(&mut reader)?;
        let target = read_index(&mut reader, &strings)?;
        let kind = untag("type relation kind", reader.byte()?, TYPE_RELATION_KINDS)?;
        type_relations.push(TypeRelationFact { from, target, kind });
    }
    let mut implementations = Vec::new();
    for _ in 0..read_usize(&mut reader)? {
        let symbol = read_usize(&mut reader)?;
        let module_path = read_string_list(&mut reader, &strings)?;
        let target_lookup = read_option(&mut reader, &strings)?;
        let trait_lookup = read_option(&mut reader, &strings)?;
        implementations.push(ImplFact {
            symbol,
            module_path,
            target_lookup,
            trait_lookup,
        });
    }
    let has_errors = read_bool(&mut reader)?;
    let diagnostic_count = reader.varint()?;
    let mut diagnostics = Vec::new();
    for _ in 0..read_usize(&mut reader)? {
        let kind = untag("diagnostic kind", reader.byte()?, DIAGNOSTIC_KINDS)?;
        let cause = match reader.byte()? {
            0 => SyntaxDiagnosticCause::ParseRecovery,
            1 => SyntaxDiagnosticCause::KnownGrammarGap(untag(
                "grammar gap",
                reader.byte()?,
                GRAMMAR_GAPS,
            )?),
            other => return Err(CodecError::InvalidTag("diagnostic cause", other)),
        };
        let node_kind = read_index(&mut reader, &strings)?;
        let range = read_range(&mut reader, &path)?;
        diagnostics.push(SyntaxDiagnostic {
            language,
            range,
            kind,
            provenance: Provenance::TreeSitter,
            precision: Precision::Syntax,
            cause,
            node_kind,
        });
    }
    reader.finish()?;
    Ok(FileSyntaxFacts {
        path,
        byte_len,
        module_path,
        extension_scopes,
        symbols,
        calls,
        named_relations,
        type_relations,
        implementations,
        has_errors,
        diagnostics,
        diagnostic_count,
    })
}

fn read_usize_from(value: u64) -> Result<usize, CodecError> {
    usize::try_from(value).map_err(|_| CodecError::Overflow)
}

fn read_string_list(
    reader: &mut Reader<'_>,
    strings: &[String],
) -> Result<Vec<String>, CodecError> {
    let mut values = Vec::new();
    for _ in 0..read_usize(reader)? {
        values.push(read_index(reader, strings)?);
    }
    Ok(values)
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

/// Encodes the manifest: compatibility key plus one entry per cached file.
pub fn encode_manifest(key: &CompatibilityKey, entries: &[ManifestEntry]) -> Vec<u8> {
    let mut writer = Writer::default();
    writer.varint(u64::from(INDEX_FORMAT_VERSION));
    writer.varint(u64::from(key.graph_model_version));
    writer.raw_str(&key.repository);
    writer.raw_str(&key.head_sha);
    writer.raw_str(&key.chakra_version);
    writer.fixed(&key.config_fingerprint);
    writer.varint(key.extractors.len() as u64);
    for (language, version) in &key.extractors {
        writer.byte(language_tag(*language));
        writer.raw_str(version);
    }
    writer.varint(entries.len() as u64);
    for entry in entries {
        writer.raw_str(entry.path.as_str());
        writer.fixed(&entry.content_hash);
        writer.varint(entry.byte_len);
        writer.raw_str(&entry.fact_file);
    }
    seal(MANIFEST_MAGIC, &writer.buf)
}

/// Decodes and checksum-validates a manifest envelope.
pub fn decode_manifest(raw: &[u8]) -> Result<(CompatibilityKey, Vec<ManifestEntry>), CodecError> {
    let payload = open(MANIFEST_MAGIC, raw)?;
    let mut reader = Reader::new(payload);
    version(&mut reader)?;
    let graph_model_version = u32::try_from(reader.varint()?).map_err(|_| CodecError::Overflow)?;
    let repository = reader.raw_str()?.to_owned();
    let head_sha = reader.raw_str()?.to_owned();
    let chakra_version = reader.raw_str()?.to_owned();
    let config_fingerprint: [u8; 16] = reader.fixed()?;
    let mut extractors = Vec::new();
    for _ in 0..read_usize(&mut reader)? {
        let language = language_from_tag(reader.byte()?)?;
        let version = reader.raw_str()?.to_owned();
        extractors.push((language, version));
    }
    let mut entries = Vec::new();
    for _ in 0..read_usize(&mut reader)? {
        let path = RepoRelativePath::new(reader.raw_str()?).map_err(|_| CodecError::InvalidPath)?;
        let content_hash: [u8; 16] = reader.fixed()?;
        let byte_len = reader.varint()?;
        let fact_file = reader.raw_str()?.to_owned();
        if !is_fact_file_name(&fact_file) {
            return Err(CodecError::InvalidPath);
        }
        entries.push(ManifestEntry {
            path,
            content_hash,
            byte_len,
            fact_file,
        });
    }
    reader.finish()?;
    Ok((
        CompatibilityKey {
            graph_model_version,
            repository,
            head_sha,
            chakra_version,
            config_fingerprint,
            extractors,
        },
        entries,
    ))
}

/// Fact file names are generated by the cache itself: 64 lowercase hex
/// digits plus `.bin`. Anything else in a manifest is corruption (and can
/// never become a path traversal).
pub fn is_fact_file_name(name: &str) -> bool {
    name.len() == 68
        && name.ends_with(".bin")
        && name[..64].bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Deterministic fact file name for one repository-relative path.
pub fn fact_file_name(path: &RepoRelativePath) -> String {
    format!("{}.bin", blake3::hash(path.as_str().as_bytes()).to_hex())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position(line: u32, column: u32) -> Result<TextPosition, CodecError> {
        TextPosition::new(line, column).map_err(|_| CodecError::InvalidRange)
    }

    fn sample_facts() -> Result<FileSyntaxFacts, CodecError> {
        let path = RepoRelativePath::new("src/lib.rs").map_err(|_| CodecError::InvalidPath)?;
        let range = SourceRange::new(path.clone(), position(1, 1)?, position(3, 2)?)
            .map_err(|_| CodecError::InvalidRange)?;
        Ok(FileSyntaxFacts {
            path: path.clone(),
            byte_len: 42,
            module_path: vec!["src".to_owned(), "lib".to_owned()],
            extension_scopes: Vec::new(),
            symbols: vec![SymbolFact {
                qualified_name: "lib::alpha".to_owned(),
                container: Some("lib".to_owned()),
                kind: SymbolKind::Function,
                location: range.clone(),
                signature: Some("pub fn alpha()".to_owned()),
                parent: None,
                is_extension_method: false,
            }],
            calls: vec![CallFact {
                caller: 0,
                form: CallForm::Function,
                target_kind: CallTargetKind::Function,
                name: "alpha".to_owned(),
                qualifier: None,
                receiver_type: Some("App\\Service".to_owned()),
                receiver_type_source: Some(ReceiverTypeSource::Parameter),
                receiver_hint: Some("$service".to_owned()),
                promoted: false,
                location: range.clone(),
            }],
            named_relations: vec![NamedRelationFact {
                from: 0,
                candidates: vec!["Base".to_owned()],
                target_kinds: vec![SymbolKind::Class],
                kind: EdgeKind::Extends,
            }],
            type_relations: vec![TypeRelationFact {
                from: 0,
                target: "Base".to_owned(),
                kind: TypeRelationKindFact::Extends,
            }],
            implementations: vec![ImplFact {
                symbol: 0,
                module_path: vec!["lib".to_owned()],
                target_lookup: Some("Alpha".to_owned()),
                trait_lookup: None,
            }],
            has_errors: true,
            diagnostics: vec![SyntaxDiagnostic {
                language: Language::Rust,
                range,
                kind: SyntaxDiagnosticKind::Error,
                provenance: Provenance::TreeSitter,
                precision: Precision::Syntax,
                cause: SyntaxDiagnosticCause::KnownGrammarGap(
                    KnownSyntaxGrammarGap::RustAttributeOnPatternField,
                ),
                node_kind: "ERROR".to_owned(),
            }],
            diagnostic_count: 1,
        })
    }

    #[test]
    fn file_facts_roundtrip() -> Result<(), CodecError> {
        let facts = sample_facts()?;
        let raw = encode_file_facts(&facts);
        let decoded = decode_file_facts(&raw, &facts.path, Language::Rust)?;
        assert_eq!(decoded, facts);
        Ok(())
    }

    #[test]
    fn file_facts_reject_corruption_and_truncation() -> Result<(), CodecError> {
        let facts = sample_facts()?;
        let raw = encode_file_facts(&facts);
        let mut corrupted = raw.clone();
        let last = corrupted.len() - 1;
        corrupted[last] ^= 0xff;
        assert!(decode_file_facts(&corrupted, &facts.path, Language::Rust).is_err());
        let truncated = &raw[..raw.len() - 3];
        assert!(decode_file_facts(truncated, &facts.path, Language::Rust).is_err());
        let other_path =
            RepoRelativePath::new("src/other.rs").map_err(|_| CodecError::InvalidPath)?;
        assert!(decode_file_facts(&raw, &other_path, Language::Rust).is_err());
        Ok(())
    }

    #[test]
    fn manifest_roundtrip() -> Result<(), CodecError> {
        let key = CompatibilityKey {
            graph_model_version: GRAPH_MODEL_VERSION,
            repository: "repo".to_owned(),
            head_sha: "abc".to_owned(),
            chakra_version: "0.1.3".to_owned(),
            config_fingerprint: [7; 16],
            extractors: vec![(Language::Rust, "rust:f1".to_owned())],
        };
        let path = RepoRelativePath::new("src/lib.rs").map_err(|_| CodecError::InvalidPath)?;
        let entries = vec![ManifestEntry {
            fact_file: fact_file_name(&path),
            path,
            content_hash: [9; 16],
            byte_len: 42,
        }];
        let raw = encode_manifest(&key, &entries);
        let (decoded_key, decoded_entries) = decode_manifest(&raw)?;
        assert_eq!(decoded_key, key);
        assert_eq!(decoded_entries, entries);
        let truncated = &raw[..raw.len() - 5];
        assert!(decode_manifest(truncated).is_err());
        Ok(())
    }
}
