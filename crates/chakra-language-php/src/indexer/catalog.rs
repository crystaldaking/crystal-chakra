use super::*;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct SymbolAddress {
    pub(super) path: RepoRelativePath,
    pub(super) index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum DependencyKey {
    Exact(String, SymbolKind),
}

#[derive(Debug, Clone)]
pub(super) struct RelationshipEdge {
    pub(super) kind: EdgeKind,
    pub(super) from: SymbolAddress,
    pub(super) to: SymbolAddress,
    pub(super) provenance: Provenance,
    pub(super) precision: Precision,
    pub(super) location: Option<SourceRange>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct RelationshipContribution {
    pub(super) dependencies: HashSet<DependencyKey>,
    pub(super) edges: Vec<RelationshipEdge>,
    pub(super) omitted_edges: u64,
}

impl RelationshipContribution {
    pub(super) fn push_edge(&mut self, edge: RelationshipEdge, limit: u64) {
        if self.edges.len() as u64 >= limit {
            self.omitted_edges = self.omitted_edges.saturating_add(1);
        } else {
            self.edges.push(edge);
        }
    }
}

#[derive(Debug)]
pub(super) struct SymbolCatalog {
    pub(super) exact: HashMap<(String, SymbolKind), Vec<SymbolAddress>>,
    methods: HashMap<(String, CallTargetKind, String), Vec<SymbolAddress>>,
    type_relations: HashMap<String, Vec<(TypeRelationKind, String)>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MethodLookup {
    Missing,
    Unique(String),
    Ambiguous,
}

impl SymbolCatalog {
    pub(super) fn new(files: &BTreeMap<RepoRelativePath, Arc<ParsedFile>>) -> Self {
        let mut exact: HashMap<(String, SymbolKind), Vec<SymbolAddress>> = HashMap::new();
        let mut methods: HashMap<(String, CallTargetKind, String), Vec<SymbolAddress>> =
            HashMap::new();
        let mut type_relations: HashMap<String, Vec<(TypeRelationKind, String)>> = HashMap::new();
        for (path, file) in files {
            for (index, symbol) in file.symbols.iter().enumerate() {
                let address = SymbolAddress {
                    path: path.clone(),
                    index,
                };
                exact
                    .entry((symbol.key.qualified_name.clone(), symbol.key.kind))
                    .or_default()
                    .push(address.clone());
                let target_kind = match symbol.key.kind {
                    SymbolKind::Method => Some(CallTargetKind::Method),
                    SymbolKind::Test => Some(CallTargetKind::Test),
                    _ => None,
                };
                if let Some(target_kind) = target_kind
                    && let Some((container, name)) = symbol.key.qualified_name.rsplit_once("::")
                {
                    methods
                        .entry((container.to_owned(), target_kind, name.to_owned()))
                        .or_default()
                        .push(address.clone());
                }
            }
            for relation in &file.type_relations {
                let Some(from) = file.symbols.get(relation.from) else {
                    continue;
                };
                type_relations
                    .entry(from.key.qualified_name.clone())
                    .or_default()
                    .push((relation.kind, relation.target.clone()));
            }
        }
        for addresses in exact.values_mut() {
            addresses.sort();
        }
        for addresses in methods.values_mut() {
            addresses.sort();
        }
        for targets in type_relations.values_mut() {
            targets.sort();
            targets.dedup();
        }
        Self {
            exact,
            methods,
            type_relations,
        }
    }

    pub(super) fn unique_exact(
        &self,
        qualified_name: &str,
        kind: SymbolKind,
    ) -> Option<SymbolAddress> {
        let matches = self.exact.get(&(qualified_name.to_owned(), kind))?;
        (matches.len() == 1).then(|| matches[0].clone())
    }

    /// Resolves the declaration container used as the call-site lookup
    /// qualifier, and whether the lookup found exactly one candidate with an
    /// unambiguous inheritance traversal (the ADR-0030 strict tier).
    pub(super) fn method_resolution(
        &self,
        receiver_type: &str,
        target_kind: CallTargetKind,
        method_name: &str,
    ) -> (String, bool) {
        match self.lookup_method(receiver_type, target_kind, method_name, &mut HashSet::new()) {
            MethodLookup::Unique(container) => (container, true),
            MethodLookup::Missing | MethodLookup::Ambiguous => (receiver_type.to_owned(), false),
        }
    }

    fn lookup_method(
        &self,
        receiver_type: &str,
        target_kind: CallTargetKind,
        method_name: &str,
        visiting: &mut HashSet<String>,
    ) -> MethodLookup {
        if !visiting.insert(receiver_type.to_owned()) {
            return MethodLookup::Missing;
        }
        let exact = match self
            .methods
            .get(&(
                receiver_type.to_owned(),
                target_kind,
                method_name.to_owned(),
            ))
            .map(Vec::len)
            .unwrap_or(0)
        {
            0 => MethodLookup::Missing,
            1 => MethodLookup::Unique(receiver_type.to_owned()),
            _ => MethodLookup::Ambiguous,
        };
        if exact != MethodLookup::Missing {
            visiting.remove(receiver_type);
            return exact;
        }

        for kind in [
            TypeRelationKind::Trait,
            TypeRelationKind::Extends,
            TypeRelationKind::Implements,
        ] {
            let mut candidates = Vec::new();
            if let Some(relations) = self.type_relations.get(receiver_type) {
                for (_, target) in relations.iter().filter(|(relation, _)| *relation == kind) {
                    match self.lookup_method(target, target_kind, method_name, visiting) {
                        MethodLookup::Missing => {}
                        MethodLookup::Unique(container) => candidates.push(container),
                        MethodLookup::Ambiguous => {
                            visiting.remove(receiver_type);
                            return MethodLookup::Ambiguous;
                        }
                    }
                }
            }
            candidates.sort();
            candidates.dedup();
            match candidates.as_slice() {
                [] => {}
                [container] => {
                    visiting.remove(receiver_type);
                    return MethodLookup::Unique(container.clone());
                }
                _ => {
                    visiting.remove(receiver_type);
                    return MethodLookup::Ambiguous;
                }
            }
        }
        visiting.remove(receiver_type);
        MethodLookup::Missing
    }
}

/// ADR-0030 strict tier: a receiver-resolved call site is promoted to
/// precise `chakra_resolver` facts only when its receiver type comes from
/// syntactically explicit evidence — a typed parameter, a typed or
/// constructor-promoted property, a local `new`, `app(Foo::class)` /
/// `resolve(Foo::class)`, or an explicit scoped type, including a fluent
/// reassignment that preserves such evidence — AND the type catalog
/// resolved the method to exactly one candidate declaration with an
/// unambiguous inheritance traversal.
///
/// `$this`, `self`/`static`/`parent`, dynamic receivers, missing or
/// ambiguous candidates, and Laravel framework-magic relations (ADR-0017)
/// stay heuristic; precision is never upgraded silently (PROV-01).
pub(super) fn strict_call_site_tier(
    receiver_type_source: Option<ReceiverTypeSource>,
    unique_candidate: bool,
) -> (Provenance, Precision) {
    let explicit_evidence = matches!(
        receiver_type_source,
        Some(
            ReceiverTypeSource::Parameter
                | ReceiverTypeSource::Property
                | ReceiverTypeSource::PromotedProperty
                | ReceiverTypeSource::LocalNew
                | ReceiverTypeSource::ServiceLocator
                | ReceiverTypeSource::ScopedType
        )
    );
    if unique_candidate && explicit_evidence {
        (Provenance::ChakraResolver, Precision::Precise)
    } else {
        (Provenance::TreeSitter, Precision::Syntax)
    }
}
