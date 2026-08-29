//! Indexing status assembly from per-language facts and scan evidence.

use chakra_domain::indexing::{
    IndexBudgetKind, IndexBudgets, IndexCapability, IndexCapabilityCoverage, IndexCoverage,
    IndexDegradation, IndexMemoryMetrics, IndexPhase, IndexPhaseMeasurement,
    IndexPublicationMetrics, IndexingStatus,
};
use chakra_domain::symbol::Language;
use chakra_engine::{GraphBuildLimits, GraphBuildReport};

use crate::adapter::AdapterFactCounts;

use super::{WorkerPolicy, WorkspaceSourceScan};

/// One language's facts, graph report, and limits for the indexing status, in
/// registry order.
pub(super) struct LanguageIndexingFacts {
    pub(super) language: Language,
    pub(super) facts: AdapterFactCounts,
    pub(super) graph: GraphBuildReport,
    pub(super) limits: GraphBuildLimits,
}

pub(super) struct IndexingParts {
    pub(super) languages: Vec<LanguageIndexingFacts>,
    pub(super) phases: Vec<IndexPhaseMeasurement>,
    pub(super) memory: IndexMemoryMetrics,
    pub(super) publication: IndexPublicationMetrics,
}

pub(super) fn build_indexing_status(
    budgets: IndexBudgets,
    scan: &WorkspaceSourceScan,
    parts: IndexingParts,
) -> IndexingStatus {
    let IndexingParts {
        languages,
        phases,
        mut memory,
        publication,
    } = parts;
    let mut extracted_symbols = 0_u64;
    let mut extracted_call_sites = 0_u64;
    let mut extracted_relationship_edges = 0_u64;
    let mut parsed_files = 0_u64;
    let mut syntax_error_files = 0_u64;
    let mut retained_symbols = 0_u64;
    let mut retained_edges = 0_u64;
    let mut omitted_edges = 0_u64;
    let mut retained_call_sites = 0_u64;
    let mut omitted_call_sites = 0_u64;
    let mut unknown_relationship_omissions = 0_u64;
    for language in &languages {
        extracted_symbols = extracted_symbols.saturating_add(language.facts.symbols);
        extracted_call_sites = extracted_call_sites.saturating_add(language.facts.call_sites);
        extracted_relationship_edges =
            extracted_relationship_edges.saturating_add(language.facts.relationship_edges);
        parsed_files = parsed_files.saturating_add(language.facts.files);
        syntax_error_files = syntax_error_files.saturating_add(language.facts.syntax_error_files);
        retained_symbols = retained_symbols.saturating_add(language.graph.retained_symbols);
        retained_edges = retained_edges.saturating_add(language.graph.retained_edges);
        omitted_edges = omitted_edges.saturating_add(language.graph.omitted_edges);
        retained_call_sites =
            retained_call_sites.saturating_add(language.graph.retained_call_sites);
        omitted_call_sites = omitted_call_sites.saturating_add(language.graph.omitted_call_sites);
        unknown_relationship_omissions = unknown_relationship_omissions
            .saturating_add(language.graph.call_sites_omitted_by_symbol_budget);
    }
    let skipped_files = scan.discovered_files.saturating_sub(scan.indexed_files);
    let coverage = IndexCoverage {
        discovered_files: scan.discovered_files,
        indexed_files: scan.indexed_files,
        skipped_files,
        unreadable_files: scan.unreadable_files,
        source_bytes: scan.source_bytes,
        parsed_files,
        syntax_error_files,
        extracted_symbols,
        retained_symbols,
        retained_edges,
        omitted_edges,
        extracted_call_sites,
        retained_call_sites,
        omitted_call_sites,
    };
    let mut degradations = scan.degradations.clone();
    for language in &languages {
        append_graph_degradations(
            &mut degradations,
            language.language,
            language.limits,
            language.graph,
        );
    }
    let capabilities = vec![
        capability(
            IndexCapability::FileInventory,
            scan.indexed_files,
            skipped_files,
            true,
        ),
        capability(
            IndexCapability::TextSearch,
            scan.indexed_files,
            skipped_files,
            true,
        ),
        capability(
            IndexCapability::Declarations,
            retained_symbols,
            extracted_symbols.saturating_sub(retained_symbols),
            skipped_files == 0,
        ),
        capability(
            IndexCapability::Relationships,
            retained_edges,
            omitted_edges,
            skipped_files == 0 && unknown_relationship_omissions == 0,
        ),
        capability(
            IndexCapability::CallSites,
            retained_call_sites,
            omitted_call_sites,
            skipped_files == 0,
        ),
    ];
    memory.retained_source_bytes = scan.source_bytes;
    memory.retained_parsed_symbols = extracted_symbols;
    memory.retained_parsed_relationship_edges = extracted_relationship_edges;
    memory.retained_parsed_call_sites = extracted_call_sites;
    memory.retained_graph_symbols = retained_symbols;
    memory.retained_graph_edges = retained_edges;
    memory.retained_graph_call_sites = retained_call_sites;
    let scheduling = WorkerPolicy::from_budgets(budgets).scheduling(&phases);
    IndexingStatus {
        budgets,
        coverage,
        capabilities,
        degradations,
        phases,
        scheduling,
        memory,
        publication,
    }
}

fn capability(
    capability: IndexCapability,
    retained: u64,
    omitted: u64,
    corpus_complete: bool,
) -> IndexCapabilityCoverage {
    IndexCapabilityCoverage {
        capability,
        retained,
        omitted,
        complete: corpus_complete && omitted == 0,
    }
}

fn append_graph_degradations(
    degradations: &mut Vec<IndexDegradation>,
    language: Language,
    limits: GraphBuildLimits,
    report: GraphBuildReport,
) {
    let mut record = |cause, affected_capabilities, limit, observed, omitted| {
        if omitted != 0 {
            degradations.push(IndexDegradation {
                phase: IndexPhase::GraphMaterialization,
                language: Some(language),
                cause,
                affected_capabilities,
                limit,
                observed,
                omitted,
            });
        }
    };
    let observed_symbols = report
        .retained_symbols
        .saturating_add(report.omitted_symbols);
    let observed_edges = report
        .retained_edges
        .saturating_add(report.edges_omitted_by_edge_budget);
    let observed_call_sites = report
        .retained_call_sites
        .saturating_add(report.call_sites_omitted_by_call_site_budget);
    record(
        IndexBudgetKind::Symbols,
        vec![IndexCapability::Declarations],
        limits.max_symbols,
        observed_symbols,
        report.omitted_symbols,
    );
    record(
        IndexBudgetKind::Symbols,
        vec![IndexCapability::Relationships],
        limits.max_symbols,
        observed_symbols,
        report.edges_omitted_by_symbol_budget,
    );
    record(
        IndexBudgetKind::Symbols,
        vec![IndexCapability::Relationships, IndexCapability::CallSites],
        limits.max_symbols,
        observed_symbols,
        report.call_sites_omitted_by_symbol_budget,
    );
    record(
        IndexBudgetKind::Edges,
        vec![IndexCapability::Relationships],
        limits.max_edges,
        observed_edges,
        report.edges_omitted_by_edge_budget,
    );
    record(
        IndexBudgetKind::Edges,
        vec![IndexCapability::CallSites],
        limits.max_edges,
        observed_edges,
        report.call_sites_omitted_by_edge_budget,
    );
    record(
        IndexBudgetKind::CallSites,
        vec![IndexCapability::Relationships],
        limits.max_call_sites,
        observed_call_sites,
        report.edges_omitted_by_call_site_budget,
    );
    record(
        IndexBudgetKind::CallSites,
        vec![IndexCapability::CallSites],
        limits.max_call_sites,
        observed_call_sites,
        report.call_sites_omitted_by_call_site_budget,
    );
}
