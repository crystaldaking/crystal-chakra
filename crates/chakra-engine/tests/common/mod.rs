//! Shared test support.
//!
//! `scenario_graph` mirrors `fixtures/rust/controller-service-provider` by
//! hand: once the Tree-sitter indexer lands, the same fixture is what the
//! indexer must reproduce, and these tests become its oracle.

// This module is compiled into each integration-test binary; not every
// binary uses every helper or every `ScenarioIds` field.
#![allow(dead_code)]

use std::error::Error;
use std::path::PathBuf;

use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::symbol::{EdgeKind, EntityId, Language, SymbolKey, SymbolKind};
use chakra_engine::SymbolGraph;

/// Root of the Controller → Service → Provider fixture crate.
pub fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("rust")
        .join("controller-service-provider")
}

/// Entities of the scenario graph, for stable addressing in assertions.
pub struct ScenarioIds {
    pub controller_struct: EntityId,
    pub controller_refund: EntityId,
    pub service_struct: EntityId,
    pub service_refund: EntityId,
    pub provider_trait: EntityId,
    pub provider_refund: EntityId,
    pub stripe_struct: EntityId,
    pub stripe_refund: EntityId,
    pub test_delegates: EntityId,
    pub test_rejects_zero: EntityId,
}

const CONTROLLER_RS: &str = "src/api/controller.rs";
const SERVICE_RS: &str = "src/service/payment_service.rs";
const PROVIDER_RS: &str = "src/provider/mod.rs";

fn range(file: &RepoRelativePath, line: u32) -> SourceRange {
    let start = TextPosition { line, column: 1 };
    let end = TextPosition {
        line: line + 1,
        column: 1,
    };
    SourceRange {
        file: file.clone(),
        start,
        end,
    }
}

#[allow(clippy::too_many_arguments)]
fn add(
    graph: &mut SymbolGraph,
    qualified_name: &str,
    container: Option<&str>,
    kind: SymbolKind,
    path: &str,
    line: u32,
    signature: Option<&str>,
) -> Result<EntityId, Box<dyn Error>> {
    let file = RepoRelativePath::new(path)?;
    let id = graph.add_symbol(
        SymbolKey {
            language: Language::Rust,
            qualified_name: qualified_name.to_owned(),
            container: container.map(str::to_owned),
            kind,
            path: file.clone(),
        },
        range(&file, line),
        signature.map(str::to_owned),
        Provenance::TreeSitter,
        Precision::Syntax,
    )?;
    Ok(id)
}

/// Builds the hand-made equivalent of indexing the fixture crate.
pub fn scenario_graph() -> Result<(SymbolGraph, ScenarioIds), Box<dyn Error>> {
    let mut graph = SymbolGraph::new();

    let controller_struct = add(
        &mut graph,
        "api::controller::PaymentController",
        None,
        SymbolKind::Struct,
        CONTROLLER_RS,
        7,
        Some("pub struct PaymentController<P: PaymentProvider>"),
    )?;
    let controller_refund = add(
        &mut graph,
        "api::controller::PaymentController::refund",
        Some("PaymentController"),
        SymbolKind::Method,
        CONTROLLER_RS,
        16,
        Some(
            "pub fn refund(&self, transaction_id: &str, amount_cents: u64) -> Result<String, String>",
        ),
    )?;
    let service_struct = add(
        &mut graph,
        "service::payment_service::PaymentService",
        None,
        SymbolKind::Struct,
        SERVICE_RS,
        6,
        Some("pub struct PaymentService<P: PaymentProvider>"),
    )?;
    let service_refund = add(
        &mut graph,
        "service::payment_service::PaymentService::refund",
        Some("PaymentService"),
        SymbolKind::Method,
        SERVICE_RS,
        16,
        Some(
            "pub fn refund(&self, transaction_id: &str, amount_cents: u64) -> Result<String, String>",
        ),
    )?;
    let provider_trait = add(
        &mut graph,
        "provider::PaymentProvider",
        None,
        SymbolKind::Trait,
        PROVIDER_RS,
        4,
        Some("pub trait PaymentProvider"),
    )?;
    let provider_refund = add(
        &mut graph,
        "provider::PaymentProvider::refund",
        Some("PaymentProvider"),
        SymbolKind::Method,
        PROVIDER_RS,
        6,
        Some("fn refund(&self, transaction_id: &str, amount_cents: u64) -> Result<String, String>"),
    )?;
    let stripe_struct = add(
        &mut graph,
        "provider::StripeProvider",
        None,
        SymbolKind::Struct,
        PROVIDER_RS,
        10,
        Some("pub struct StripeProvider"),
    )?;
    let stripe_refund = add(
        &mut graph,
        "provider::StripeProvider::refund",
        Some("StripeProvider"),
        SymbolKind::Method,
        PROVIDER_RS,
        15,
        Some("fn refund(&self, transaction_id: &str, amount_cents: u64) -> Result<String, String>"),
    )?;
    let test_delegates = add(
        &mut graph,
        "service::payment_service::tests::refund_delegates_to_provider",
        None,
        SymbolKind::Test,
        SERVICE_RS,
        33,
        None,
    )?;
    let test_rejects_zero = add(
        &mut graph,
        "service::payment_service::tests::refund_rejects_zero_amount",
        None,
        SymbolKind::Test,
        SERVICE_RS,
        39,
        None,
    )?;

    fn call_site(file: &str, line: u32) -> Result<Option<SourceRange>, Box<dyn Error>> {
        let file = RepoRelativePath::new(file)?;
        Ok(Some(SourceRange {
            start: TextPosition { line, column: 13 },
            end: TextPosition { line, column: 30 },
            file,
        }))
    }

    graph.add_edge(
        EdgeKind::Calls,
        controller_refund,
        service_refund,
        Provenance::TreeSitter,
        Precision::Syntax,
        call_site(CONTROLLER_RS, 17)?,
    )?;
    graph.add_edge(
        EdgeKind::Calls,
        service_refund,
        provider_refund,
        Provenance::TreeSitter,
        Precision::Syntax,
        call_site(SERVICE_RS, 20)?,
    )?;
    graph.add_edge(
        EdgeKind::Implements,
        stripe_struct,
        provider_trait,
        Provenance::TreeSitter,
        Precision::Syntax,
        None,
    )?;
    graph.add_edge(
        EdgeKind::Implements,
        stripe_refund,
        provider_refund,
        Provenance::TreeSitter,
        Precision::Syntax,
        None,
    )?;
    graph.add_edge(
        EdgeKind::Tests,
        test_delegates,
        service_refund,
        Provenance::Heuristic,
        Precision::Heuristic,
        None,
    )?;
    graph.add_edge(
        EdgeKind::Tests,
        test_rejects_zero,
        service_refund,
        Provenance::Heuristic,
        Precision::Heuristic,
        None,
    )?;

    let ids = ScenarioIds {
        controller_struct,
        controller_refund,
        service_struct,
        service_refund,
        provider_trait,
        provider_refund,
        stripe_struct,
        stripe_refund,
        test_delegates,
        test_rejects_zero,
    };
    Ok((graph, ids))
}
