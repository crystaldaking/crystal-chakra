use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct CallableIdentity {
    qualified_name: String,
    kind: SymbolKind,
}

#[derive(Debug)]
pub(super) struct SymbolCatalog {
    exact: HashMap<(String, SymbolKind), Vec<SymbolAddress>>,
}

impl SymbolCatalog {
    pub(super) fn new(files: &BTreeMap<RepoRelativePath, Arc<ParsedFile>>) -> Self {
        let mut exact: HashMap<(String, SymbolKind), Vec<SymbolAddress>> = HashMap::new();
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
            }
        }
        for addresses in exact.values_mut() {
            addresses.sort();
        }
        Self { exact }
    }

    pub(super) fn unique_exact(
        &self,
        qualified_name: &str,
        kind: SymbolKind,
    ) -> Option<SymbolAddress> {
        unique(self.exact.get(&(qualified_name.to_owned(), kind)))
    }
}

/// Revision-local lookup for syntax-visible extension-method containers.
///
/// The key keeps ordinary member-call materialization proportional to the
/// imported scopes and same-name extension containers, instead of rescanning
/// every parsed file for every call site.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct ExtensionCatalog {
    pub(super) by_name: HashMap<String, HashMap<String, BTreeSet<String>>>,
}

impl ExtensionCatalog {
    pub(super) fn new(files: &BTreeMap<RepoRelativePath, Arc<ParsedFile>>) -> Self {
        let mut by_name: HashMap<_, HashMap<_, BTreeSet<_>>> = HashMap::new();
        for file in files.values() {
            for symbol in &file.symbols {
                if !symbol.is_extension_method || symbol.key.kind != SymbolKind::Method {
                    continue;
                }
                let namespace = symbol
                    .parent
                    .and_then(|parent| file.symbols.get(parent))
                    .and_then(|container| {
                        container
                            .key
                            .qualified_name
                            .rsplit_once("::")
                            .map(|(namespace, _)| namespace.to_owned())
                    })
                    .unwrap_or_default();
                let Some(container) = symbol.key.container.as_ref() else {
                    continue;
                };
                by_name
                    .entry(symbol_name(symbol).to_owned())
                    .or_default()
                    .entry(namespace)
                    .or_default()
                    .insert(container.clone());
            }
        }
        Self { by_name }
    }

    pub(super) fn qualifier(
        &self,
        calling_file: &ParsedFile,
        call: &CallDraft,
    ) -> (Option<String>, u64) {
        if call.form != chakra_domain::symbol::CallForm::Member
            || call.target_kind != chakra_domain::symbol::CallTargetKind::Method
            || call.qualifier.is_some()
        {
            return (None, 0);
        }
        let mut qualifier: Option<String> = None;
        let mut candidates_examined = 0_u64;
        for scope in &calling_file.extension_scopes {
            let Some(containers) = self
                .by_name
                .get(call.name.as_str())
                .and_then(|namespaces| namespaces.get(scope.as_str()))
            else {
                continue;
            };
            for candidate in containers {
                candidates_examined = candidates_examined.saturating_add(1);
                if qualifier
                    .as_ref()
                    .is_some_and(|qualifier| qualifier != candidate)
                {
                    return (None, candidates_examined);
                }
                qualifier = Some(candidate.clone());
            }
        }
        (qualifier, candidates_examined)
    }
}

fn unique(matches: Option<&Vec<SymbolAddress>>) -> Option<SymbolAddress> {
    let matches = matches?;
    (matches.len() == 1).then(|| matches[0].clone())
}

fn symbol_name(symbol: &SymbolDraft) -> &str {
    symbol
        .key
        .qualified_name
        .rsplit("::")
        .next()
        .unwrap_or(&symbol.key.qualified_name)
}

fn callable_identity(symbol: &SymbolDraft) -> Option<CallableIdentity> {
    callable_target_kind(symbol.key.kind)?;
    Some(CallableIdentity {
        qualified_name: symbol.key.qualified_name.clone(),
        kind: symbol.key.kind,
    })
}

pub(super) fn record_previous_callables(
    path: &RepoRelativePath,
    file: &ParsedFile,
    instances: &mut HashMap<CallableIdentity, Vec<SymbolAddress>>,
) {
    for (index, symbol) in file.symbols.iter().enumerate() {
        let Some(identity) = callable_identity(symbol) else {
            continue;
        };
        instances.entry(identity).or_default().push(SymbolAddress {
            path: path.clone(),
            index,
        });
    }
}

pub(super) fn record_next_callable(
    symbol: &SymbolDraft,
    counts: &mut HashMap<CallableIdentity, usize>,
) {
    let Some(identity) = callable_identity(symbol) else {
        return;
    };
    counts
        .entry(identity)
        .and_modify(|count| *count = count.saturating_add(1))
        .or_insert(1);
}

fn callable_dependencies_for_identity(
    identity: &CallableIdentity,
) -> Option<Vec<(u8, String, Option<String>)>> {
    let kind = callable_target_kind(identity.kind)?;
    let name = identity
        .qualified_name
        .rsplit("::")
        .next()
        .unwrap_or(&identity.qualified_name);
    let kind = callable_kind_key(kind);
    let mut qualifiers = vec![None];
    if let Some((container, _)) = identity.qualified_name.rsplit_once("::") {
        qualifiers.push(Some(container.to_owned()));
        let simple = Some(
            container
                .rsplit("::")
                .next()
                .unwrap_or(container)
                .to_owned(),
        );
        if !qualifiers.contains(&simple) {
            qualifiers.push(simple);
        }
    }
    Some(
        qualifiers
            .into_iter()
            .map(|qualifier| (kind, name.to_owned(), qualifier))
            .collect(),
    )
}

fn add_targeted_call_owners(
    graph: &SymbolGraph,
    address: &SymbolAddress,
    include_ambiguous: bool,
    owners: &mut BTreeSet<RepoRelativePath>,
) -> bool {
    let Some(target) = entity_for_address(graph, address) else {
        return false;
    };
    for edge in graph
        .incoming_edges(target)
        .iter()
        .filter(|edge| edge.kind == EdgeKind::Calls)
    {
        if let Some(caller) = graph.symbol(edge.from) {
            owners.insert(caller.key.path.clone());
        }
    }
    if !include_ambiguous {
        return true;
    }
    let (call_sites, truncated) =
        graph.call_sites_for_target(target, MAX_TARGETED_AMBIGUOUS_CALL_SITES);
    for call_site in call_sites {
        if let Some(caller) = graph.symbol(call_site.caller) {
            owners.insert(caller.key.path.clone());
        }
    }
    !truncated
}

pub(super) fn apply_callable_changes(
    graph: &SymbolGraph,
    previous: &HashMap<CallableIdentity, Vec<SymbolAddress>>,
    next: &HashMap<CallableIdentity, usize>,
    targeted_owners: &mut BTreeSet<RepoRelativePath>,
    broad_dependencies: &mut HashSet<(u8, String, Option<String>)>,
) {
    for (identity, previous_instances) in previous {
        let next_count = next.get(identity).copied().unwrap_or(0);
        let paired = previous_instances.len().min(next_count);
        let mut targeted = true;
        for address in previous_instances.iter().take(paired) {
            targeted &= add_targeted_call_owners(graph, address, false, targeted_owners);
        }
        for address in previous_instances.iter().skip(paired) {
            targeted &= add_targeted_call_owners(graph, address, true, targeted_owners);
        }
        if !targeted && let Some(dependencies) = callable_dependencies_for_identity(identity) {
            broad_dependencies.extend(dependencies);
        }
        if next_count > previous_instances.len()
            && let Some(dependencies) = callable_dependencies_for_identity(identity)
        {
            broad_dependencies.extend(dependencies);
        }
    }
    for (identity, next_count) in next {
        if !previous.contains_key(identity)
            && *next_count > 0
            && let Some(dependencies) = callable_dependencies_for_identity(identity)
        {
            broad_dependencies.extend(dependencies);
        }
    }
}

pub(super) fn extension_callables(file: &ParsedFile) -> HashSet<(u8, String)> {
    file.symbols
        .iter()
        .filter(|symbol| symbol.is_extension_method)
        .filter_map(|symbol| {
            callable_target_kind(symbol.key.kind)
                .map(|kind| callable_name_dependency(kind, symbol_name(symbol)))
        })
        .collect()
}

fn callable_target_kind(kind: SymbolKind) -> Option<chakra_domain::symbol::CallTargetKind> {
    use chakra_domain::symbol::CallTargetKind;
    match kind {
        SymbolKind::Function => Some(CallTargetKind::Function),
        SymbolKind::Method => Some(CallTargetKind::Method),
        SymbolKind::Test => Some(CallTargetKind::Test),
        _ => None,
    }
}

fn callable_kind_key(kind: chakra_domain::symbol::CallTargetKind) -> u8 {
    use chakra_domain::symbol::CallTargetKind;
    match kind {
        CallTargetKind::Function => 0,
        CallTargetKind::Method => 1,
        CallTargetKind::FunctionOrMethod => 2,
        CallTargetKind::Test => 3,
        CallTargetKind::Configuration => 4,
    }
}

pub(super) fn callable_name_dependency(
    kind: chakra_domain::symbol::CallTargetKind,
    name: &str,
) -> (u8, String) {
    (callable_kind_key(kind), name.to_owned())
}

pub(super) fn call_dependency(call: &CallDraft) -> Option<(u8, String, Option<String>)> {
    if call.qualifier.is_none()
        && matches!(
            call.form,
            chakra_domain::symbol::CallForm::Member
                | chakra_domain::symbol::CallForm::NullsafeMember
                | chakra_domain::symbol::CallForm::Scoped
        )
    {
        return None;
    }
    Some((
        callable_kind_key(call.target_kind),
        call.name.clone(),
        call.qualifier.clone(),
    ))
}
