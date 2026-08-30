use std::error::Error;
use std::fs;
use std::process::Command;

use tempfile::TempDir;

use super::*;

fn graph_snapshot(graph: &SymbolGraph) -> Vec<String> {
    let mut snapshot = vec![format!("files:{:?}", graph.file_summaries())];
    for symbol in graph.symbols() {
        snapshot.push(format!("symbol:{symbol:?}"));
        snapshot.push(format!("outgoing:{:?}", graph.outgoing_edges(symbol.id)));
        snapshot.push(format!(
            "calls:{:?}",
            graph.call_sites_from(symbol.id).collect::<Vec<_>>()
        ));
    }
    snapshot
}

#[test]
fn bounded_parallel_parsing_is_deterministic() -> Result<(), Box<dyn Error>> {
    let mut sources = BTreeMap::new();
    for index in 0..64 {
        sources.insert(
            RepoRelativePath::new(format!("src/Generated{index:03}.php"))?,
            Arc::<str>::from(format!(
                "<?php namespace Generated\\N{index}; function caller_{index}(): void {{ helper_{index}(); }} function helper_{index}(): void {{}}\n"
            )),
        );
    }
    let cancellation = IndexCancellation::default();
    let (_, sequential, sequential_metrics) = PhpSyntaxIndex::from_sources_scheduled(
        sources.clone(),
        GraphBuildLimits::UNLIMITED,
        1,
        1,
        &cancellation,
    )?;
    let (_, parallel, parallel_metrics) = PhpSyntaxIndex::from_sources_scheduled(
        sources,
        GraphBuildLimits::UNLIMITED,
        4,
        1,
        &cancellation,
    )?;

    assert_eq!(graph_snapshot(&sequential), graph_snapshot(&parallel));
    assert_eq!(sequential_metrics.facts, parallel_metrics.facts);
    assert_eq!(sequential_metrics.graph, parallel_metrics.graph);
    let parallel_parse = parallel_metrics
        .phases
        .iter()
        .find(|phase| phase.phase == IndexPhase::ParseExtraction)
        .ok_or("parallel parse phase missing")?;
    assert_eq!(parallel_parse.effective_workers, 4);
    assert!((1..=parallel_parse.effective_workers).contains(&parallel_parse.peak_active_workers));
    assert_eq!(parallel_parse.peak_queue_depth, 0);
    assert_eq!(parallel_parse.work_items, 64);
    Ok(())
}

fn in_memory_sources(
    sources: &[(&str, &str)],
) -> Result<BTreeMap<RepoRelativePath, Arc<str>>, Box<dyn Error>> {
    sources
        .iter()
        .map(|(path, source)| Ok((RepoRelativePath::new(*path)?, Arc::<str>::from(*source))))
        .collect()
}

fn repository() -> Result<TempDir, Box<dyn Error>> {
    let repository = TempDir::new()?;
    let status = Command::new("git")
        .current_dir(repository.path())
        .args(["init", "--quiet"])
        .status()?;
    if !status.success() {
        return Err("git init failed".into());
    }
    Ok(repository)
}

#[test]
fn resolves_namespaced_php_functions_without_cross_namespace_ambiguity()
-> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    fs::create_dir_all(repository.path().join("src"))?;
    fs::write(
        repository.path().join("src/service.php"),
        r#"<?php
namespace App;
class Service { public function refund(): void { helper(); } }
function helper(): void {}
"#,
    )?;
    fs::write(
        repository.path().join("src/other.php"),
        "<?php namespace Other; function helper(): void {}\n",
    )?;
    let report = index_repository(repository.path())?;
    assert_eq!(report.metrics.parsed_files, 2);
    let refund = report.graph.resolve_name("refund");
    assert_eq!(refund.len(), 1);
    let calls = report.graph.outgoing_edges(refund[0]);
    assert_eq!(
        calls
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Calls)
            .count(),
        1
    );
    let call_site = report
        .graph
        .call_sites_from(refund[0])
        .next()
        .ok_or("helper call site missing")?;
    assert_eq!(
        call_site.resolution,
        chakra_domain::symbol::CallResolution::Resolved {
            target: calls
                .iter()
                .find(|edge| edge.kind == EdgeKind::Calls)
                .ok_or("helper call edge missing")?
                .to,
        }
    );
    let (candidates, truncated) = report.graph.call_candidates(call_site, 10);
    assert!(candidates.is_empty());
    assert!(!truncated);
    assert_eq!(call_site.precision, Precision::Syntax);
    Ok(())
}

#[test]
fn unchanged_php_is_not_reparsed() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    fs::write(
        repository.path().join("service.php"),
        "<?php function pay() {}\n",
    )?;
    let report = index_repository(repository.path())?;
    let sources = scan_repository_sources(repository.path())?;
    let reconciled = report.syntax_index.reconcile_classified_sources(sources)?;
    assert!(reconciled.graph.is_none());
    assert_eq!(reconciled.metrics.reparsed_files, 0);
    assert_eq!(reconciled.metrics.unchanged_files, 1);
    Ok(())
}

#[test]
fn receiver_types_prevent_same_name_method_fanout() -> Result<(), Box<dyn Error>> {
    let sources = in_memory_sources(&[
        (
            "src/targets.php",
            r#"<?php
namespace App\Domain;
class TransactionStatusService { public function syncStatus(): void {} }
class ExpirePendingTransactionsJob { public function handle(): void {} }
class UnrelatedJob { public function handle(): void {} }
class UnrelatedStatusService { public function syncStatus(): void {} }
"#,
        ),
        (
            "tests/FeatureTest.php",
            r#"<?php
namespace Tests;
use App\Domain\TransactionStatusService as StatusService;
use App\Domain\ExpirePendingTransactionsJob;
class FeatureTest {
public function parameter(StatusService $service): void { $service->syncStatus(); }
public function locator(): void { app(StatusService::class)->syncStatus(); }
public function local(): void { (new ExpirePendingTransactionsJob)->handle(); }
public function dynamic($service, $job): void {
    $service->syncStatus();
    $job->handle();
}
}
"#,
        ),
    ])?;
    let (_, graph) = PhpSyntaxIndex::from_sources(sources)?;

    let sync_status = graph.resolve_name("App::Domain::TransactionStatusService::syncStatus");
    assert_eq!(sync_status.len(), 1);
    assert_eq!(
        graph
            .incoming_edges(sync_status[0])
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Calls)
            .count(),
        2
    );
    let job_handle = graph.resolve_name("App::Domain::ExpirePendingTransactionsJob::handle");
    assert_eq!(job_handle.len(), 1);
    assert_eq!(
        graph
            .incoming_edges(job_handle[0])
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Calls)
            .count(),
        1
    );
    let unrelated_handle = graph.resolve_name("App::Domain::UnrelatedJob::handle");
    assert_eq!(unrelated_handle.len(), 1);
    assert!(
        graph
            .incoming_edges(unrelated_handle[0])
            .iter()
            .all(|edge| edge.kind != EdgeKind::Calls)
    );
    assert_eq!(
        graph
            .symbols()
            .iter()
            .flat_map(|symbol| graph.call_sites_from(symbol.id))
            .filter(|call| {
                call.resolution == chakra_domain::symbol::CallResolution::Unresolved
                    && matches!(call.name.as_str(), "syncStatus" | "handle")
            })
            .count(),
        2
    );
    Ok(())
}

#[test]
fn php_test_relations_require_resolved_receivers_and_are_deduplicated() -> Result<(), Box<dyn Error>>
{
    let sources = in_memory_sources(&[
        (
            "src/targets.php",
            r#"<?php
namespace App\Domain;
class TransactionStatusService { public function syncStatus(): void {} }
class ExpirePendingTransactionsJob { public function handle(): void {} }
class UnrelatedJob { public function handle(): void {} }
class UnrelatedStatusService { public function syncStatus(): void {} }
"#,
        ),
        (
            "tests/RelationshipTest.php",
            r#"<?php
namespace Tests;
use App\Domain\TransactionStatusService;
use App\Domain\ExpirePendingTransactionsJob;
class ExpirationTest {
public function testExpiresPending(ExpirePendingTransactionsJob $job): void {
    $job->handle();
    $job->handle();
}
public function testDynamicHandle($job): void { $job->handle(); }
}
class StatusTest {
public function testSyncStatus(TransactionStatusService $service): void {
    $service->syncStatus();
}
}
"#,
        ),
    ])?;
    let (_, graph) = PhpSyntaxIndex::from_sources(sources)?;

    let job_handle = graph.resolve_name("App::Domain::ExpirePendingTransactionsJob::handle");
    assert_eq!(job_handle.len(), 1);
    let job_calls: Vec<_> = graph
        .incoming_edges(job_handle[0])
        .iter()
        .filter(|edge| edge.kind == EdgeKind::Calls)
        .collect();
    assert_eq!(job_calls.len(), 2);
    let job_tests: Vec<_> = graph
        .incoming_edges(job_handle[0])
        .iter()
        .filter(|edge| edge.kind == EdgeKind::Tests)
        .collect();
    assert_eq!(job_tests.len(), 1);
    let test_symbol = graph
        .symbol(job_tests[0].from)
        .ok_or("expiration test symbol missing")?;
    assert_eq!(
        test_symbol.key.qualified_name,
        "Tests::ExpirationTest::testExpiresPending"
    );
    let evidence = graph
        .call_site_for_edge(job_tests[0])
        .ok_or("representative test call-site evidence missing")?;
    assert_eq!(
        evidence.receiver_type.as_deref(),
        Some("App::Domain::ExpirePendingTransactionsJob")
    );
    assert_eq!(
        evidence.receiver_type_source,
        Some(chakra_domain::symbol::ReceiverTypeSource::Parameter)
    );

    let sync_status = graph.resolve_name("App::Domain::TransactionStatusService::syncStatus");
    assert_eq!(sync_status.len(), 1);
    assert_eq!(
        graph
            .incoming_edges(sync_status[0])
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Tests)
            .count(),
        1
    );

    for unrelated in [
        "App::Domain::UnrelatedJob::handle",
        "App::Domain::UnrelatedStatusService::syncStatus",
    ] {
        let target = graph.resolve_name(unrelated);
        assert_eq!(target.len(), 1);
        assert!(
            graph
                .incoming_edges(target[0])
                .iter()
                .all(|edge| edge.kind != EdgeKind::Tests)
        );
    }

    let dynamic_test = graph
        .resolve_name("Tests::ExpirationTest::testDynamicHandle")
        .into_iter()
        .next()
        .ok_or("dynamic test symbol missing")?;
    assert!(
        graph
            .outgoing_edges(dynamic_test)
            .iter()
            .all(|edge| edge.kind != EdgeKind::Tests)
    );
    let dynamic_call = graph
        .call_sites_from(dynamic_test)
        .find(|call| call.name == "handle")
        .ok_or("dynamic test call site missing")?;
    assert_eq!(
        dynamic_call.resolution,
        chakra_domain::symbol::CallResolution::Unresolved
    );
    Ok(())
}

#[test]
fn strict_tier_receiver_calls_promote_to_chakra_resolver_precise() -> Result<(), Box<dyn Error>> {
    let sources = in_memory_sources(&[
        (
            "src/services.php",
            r#"<?php
namespace App;
class Mailer {
public function send(): void {}
public static function sendBatch(): void {}
}
class Service {
public function __construct(private Mailer $mailer) {}
public function viaProperty(): void { $this->mailer->send(); }
public function viaParameter(Mailer $mailer): void { $mailer->send(); }
public function viaLocalNew(): void { (new Mailer())->send(); }
public function viaLocator(): void { app(Mailer::class)->send(); }
public function viaScoped(): void { Mailer::sendBatch(); }
public function local(): void {}
public function viaThis(): void { $this->local(); }
public static function helper(): void {}
public function viaSelf(): void { self::helper(); }
public function viaStatic(): void { static::helper(); }
public function viaDynamic($mailer): void { $mailer->send(); }
}
"#,
        ),
        (
            "tests/MailerTest.php",
            r#"<?php
namespace Tests;
use App\Mailer;
class MailerTest {
public function testSends(Mailer $mailer): void { $mailer->send(); }
public function testSendsDynamic($mailer): void { $mailer->send(); }
}
"#,
        ),
    ])?;
    let (_, graph) = PhpSyntaxIndex::from_sources(sources)?;

    let send = graph.resolve_name("App::Mailer::send");
    assert_eq!(send.len(), 1);
    let send_calls: Vec<_> = graph
        .incoming_edges(send[0])
        .iter()
        .filter(|edge| edge.kind == EdgeKind::Calls)
        .collect();
    assert_eq!(send_calls.len(), 5);
    assert!(
        send_calls
            .iter()
            .all(|edge| edge.provenance == Provenance::ChakraResolver
                && edge.precision == Precision::Precise),
        "strict-tier CALLS edges must be chakra_resolver/precise: {send_calls:?}"
    );
    let send_tests: Vec<_> = graph
        .incoming_edges(send[0])
        .iter()
        .filter(|edge| edge.kind == EdgeKind::Tests)
        .collect();
    assert_eq!(send_tests.len(), 1);
    assert_eq!(send_tests[0].provenance, Provenance::ChakraResolver);
    assert_eq!(send_tests[0].precision, Precision::Precise);
    let promoted_site = graph
        .call_site_for_edge(send_calls[0])
        .ok_or("promoted call-site evidence missing")?;
    assert_eq!(promoted_site.provenance, Provenance::ChakraResolver);
    assert_eq!(promoted_site.precision, Precision::Precise);

    let batch = graph.resolve_name("App::Mailer::sendBatch");
    assert_eq!(batch.len(), 1);
    let batch_calls: Vec<_> = graph
        .incoming_edges(batch[0])
        .iter()
        .filter(|edge| edge.kind == EdgeKind::Calls)
        .collect();
    assert_eq!(batch_calls.len(), 1);
    assert_eq!(batch_calls[0].provenance, Provenance::ChakraResolver);
    assert_eq!(batch_calls[0].precision, Precision::Precise);
    Ok(())
}

#[test]
fn non_strict_receiver_calls_stay_heuristic() -> Result<(), Box<dyn Error>> {
    let sources = in_memory_sources(&[(
        "src/services.php",
        r#"<?php
namespace App;
class Service {
public function local(): void {}
public function viaThis(): void { $this->local(); }
public static function helper(): void {}
public function viaSelf(): void { self::helper(); }
public function viaStatic(): void { static::helper(); }
public function viaDynamic($service): void { $service->local(); }
}
"#,
    )])?;
    let (_, graph) = PhpSyntaxIndex::from_sources(sources)?;

    for target in ["App::Service::local", "App::Service::helper"] {
        let ids = graph.resolve_name(target);
        assert_eq!(ids.len(), 1, "missing target {target}");
        let calls: Vec<_> = graph
            .incoming_edges(ids[0])
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Calls)
            .collect();
        assert!(!calls.is_empty(), "expected resolved callers for {target}");
        assert!(
            calls
                .iter()
                .all(|edge| edge.provenance == Provenance::TreeSitter
                    && edge.precision == Precision::Heuristic),
            "non-strict CALLS edges for {target} must stay tree_sitter/heuristic: {calls:?}"
        );
    }

    let via_dynamic = graph.resolve_name("App::Service::viaDynamic");
    assert_eq!(via_dynamic.len(), 1);
    let dynamic_call = graph
        .call_sites_from(via_dynamic[0])
        .find(|call| call.name == "local")
        .ok_or("dynamic call site missing")?;
    assert_eq!(
        dynamic_call.resolution,
        chakra_domain::symbol::CallResolution::Unresolved
    );
    assert_eq!(dynamic_call.provenance, Provenance::TreeSitter);
    assert_eq!(dynamic_call.precision, Precision::Syntax);
    Ok(())
}

#[test]
fn ambiguous_inherited_candidates_stay_heuristic() -> Result<(), Box<dyn Error>> {
    let sources = in_memory_sources(&[(
        "src/ambiguous.php",
        r#"<?php
namespace App;
trait LeftTrait { public function shared(): void {} }
trait RightTrait { public function shared(): void {} }
class Both {
use LeftTrait;
use RightTrait;
}
class Caller {
public function call(Both $both): void { $both->shared(); }
}
"#,
    )])?;
    let (_, graph) = PhpSyntaxIndex::from_sources(sources)?;

    for trait_method in ["App::LeftTrait::shared", "App::RightTrait::shared"] {
        let ids = graph.resolve_name(trait_method);
        assert_eq!(ids.len(), 1, "missing target {trait_method}");
        assert!(
            graph
                .incoming_edges(ids[0])
                .iter()
                .all(|edge| edge.kind != EdgeKind::Calls),
            "ambiguous candidate {trait_method} must not gain a call edge"
        );
    }
    let caller = graph.resolve_name("App::Caller::call");
    assert_eq!(caller.len(), 1);
    let call = graph
        .call_sites_from(caller[0])
        .find(|call| call.name == "shared")
        .ok_or("ambiguous call site missing")?;
    assert!(!matches!(
        call.resolution,
        chakra_domain::symbol::CallResolution::Resolved { .. }
    ));
    assert_eq!(call.provenance, Provenance::TreeSitter);
    assert_eq!(call.precision, Precision::Syntax);
    Ok(())
}

#[test]
fn resolves_php_method_precedence_across_inheritance_and_traits() -> Result<(), Box<dyn Error>> {
    let sources = in_memory_sources(&[
        (
            "src/types.php",
            r#"<?php
namespace App;
interface Runner {
public function run(): void;
public function fromParent(): void;
}
trait SharedBehavior { public function fromTrait(): void {} }
class ParentWorker {
public function fromParent(): void {}
public function fromTrait(): void {}
}
class ChildWorker extends ParentWorker implements Runner {
use SharedBehavior;
public function run(): void {}
}
"#,
        ),
        (
            "src/caller.php",
            r#"<?php
namespace App;
class Caller {
public function call(ChildWorker $worker, Runner $contract): void {
    $worker->run();
    $worker->fromParent();
    $worker->fromTrait();
    $contract->run();
}
}
"#,
        ),
    ])?;
    let (_, graph) = PhpSyntaxIndex::from_sources(sources)?;

    for (target, expected) in [
        ("App::ChildWorker::run", 1),
        ("App::Runner::run", 1),
        ("App::ParentWorker::fromParent", 1),
        ("App::SharedBehavior::fromTrait", 1),
        ("App::Runner::fromParent", 0),
        ("App::ParentWorker::fromTrait", 0),
    ] {
        let ids = graph.resolve_name(target);
        assert_eq!(ids.len(), 1, "missing target {target}");
        assert_eq!(
            graph
                .incoming_edges(ids[0])
                .iter()
                .filter(|edge| edge.kind == EdgeKind::Calls)
                .count(),
            expected,
            "unexpected callers for {target}"
        );
    }
    Ok(())
}

#[test]
fn receiver_resolution_republishes_without_reparsing_unchanged_callers()
-> Result<(), Box<dyn Error>> {
    let initial = in_memory_sources(&[
        (
            "src/service.php",
            "<?php namespace App; class Service { public function run(): void {} }",
        ),
        (
            "src/caller.php",
            "<?php namespace App; class Caller { public function call(Service $service): void { $service->run(); } }",
        ),
    ])?;
    let (index, graph) = PhpSyntaxIndex::from_sources(initial)?;
    let run = graph.resolve_name("App::Service::run");
    assert_eq!(run.len(), 1);
    assert_eq!(
        graph
            .incoming_edges(run[0])
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Calls)
            .count(),
        1
    );

    let changed = in_memory_sources(&[
        (
            "src/service.php",
            "<?php namespace App; class Service { public function renamed(): void {} }",
        ),
        (
            "src/caller.php",
            "<?php namespace App; class Caller { public function call(Service $service): void { $service->run(); } }",
        ),
    ])?;
    let reconciled = index.reconcile_sources(changed)?;
    assert_eq!(reconciled.metrics.reparsed_files, 1);
    assert_eq!(reconciled.metrics.relationship_files_recomputed, 1);
    let graph = reconciled.graph.ok_or("changed graph missing")?;
    assert!(graph.resolve_name("App::Service::run").is_empty());
    let call = graph
        .symbols()
        .iter()
        .flat_map(|symbol| graph.call_sites_from(symbol.id))
        .find(|call| call.name == "run")
        .ok_or("run call site missing")?;
    assert_eq!(
        call.resolution,
        chakra_domain::symbol::CallResolution::Unresolved
    );
    Ok(())
}

#[test]
fn synthetic_receiver_resolution_records_bounded_measurement() -> Result<(), Box<dyn Error>> {
    const CALLS: usize = 256;
    let mut source = String::from("<?php namespace Bench;\n");
    for index in 0..CALLS {
        source.push_str(&format!(
            "class Service{index} {{ public function call{index}(): void {{}} }}\n"
        ));
    }
    source.push_str("class Caller { public function invoke(");
    for index in 0..CALLS {
        if index > 0 {
            source.push(',');
        }
        source.push_str(&format!("Service{index} $service{index}"));
    }
    source.push_str("): void {\n");
    for index in 0..CALLS {
        source.push_str(&format!("$service{index}->call{index}();\n"));
    }
    source.push_str("}}\n");
    let sources = BTreeMap::from([(
        RepoRelativePath::new("bench.php")?,
        Arc::<str>::from(source),
    )]);

    let started = Instant::now();
    let (_, graph) = PhpSyntaxIndex::from_sources(sources)?;
    let elapsed = started.elapsed();
    assert_eq!(graph.call_site_count(), CALLS as u64);
    assert_eq!(graph.unresolved_call_site_count(), 0);
    assert_eq!(graph.ambiguous_call_site_count(), 0);
    eprintln!(
        "php_receiver_resolution: calls={CALLS}, symbols={}, edges={}, elapsed={elapsed:?}",
        graph.symbol_count(),
        graph.edge_count()
    );
    Ok(())
}

#[test]
fn malformed_cyclic_hierarchy_terminates_without_inventing_a_target() -> Result<(), Box<dyn Error>>
{
    let sources = in_memory_sources(&[(
        "cycle.php",
        r#"<?php
namespace App;
class CycleA extends CycleB {}
class CycleB extends CycleA {}
class Caller { public function call(CycleA $value): void { $value->missing(); } }
"#,
    )])?;
    let (_, graph) = PhpSyntaxIndex::from_sources(sources)?;
    let call = graph
        .symbols()
        .iter()
        .flat_map(|symbol| graph.call_sites_from(symbol.id))
        .find(|call| call.name == "missing")
        .ok_or("missing call site not retained")?;
    assert_eq!(
        call.resolution,
        chakra_domain::symbol::CallResolution::Unresolved
    );
    Ok(())
}

#[test]
fn laravel_fact_extraction_stops_at_the_per_file_budget() -> Result<(), Box<dyn Error>> {
    let mut source = String::from(
        r#"<?php
use Illuminate\Support\Facades\Route;
final class Controller { public function __invoke(): void {} }
"#,
    );
    for index in 0..1_100 {
        source.push_str(&format!(
            "Route::get('/route-{index}', Controller::class);\n"
        ));
    }
    let sources = BTreeMap::from([(
        RepoRelativePath::new("routes/web.php")?,
        Arc::<str>::from(source),
    )]);
    let (index, graph) = PhpSyntaxIndex::from_sources_with_laravel(sources, true)?;
    assert_eq!(index.framework_truncated_files(), 1);
    assert!(index.framework_symbol_count() <= 2_048);
    assert!(index.framework_edge_count() <= 2_048);
    assert!(
        graph
            .symbols()
            .iter()
            .filter(|symbol| symbol.key.kind == SymbolKind::Configuration)
            .count()
            <= 2_048
    );
    Ok(())
}

#[test]
fn invalid_composer_metadata_disables_only_optional_laravel_enrichment()
-> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    fs::write(repository.path().join("composer.json"), "{ invalid json")?;
    fs::write(
        repository.path().join("service.php"),
        "<?php function stillIndexed(): void {}",
    )?;
    let report = index_repository(repository.path())?;
    assert!(!report.metrics.laravel_detected);
    assert_eq!(report.metrics.framework_edges, 0);
    assert_eq!(report.graph.resolve_name("stillIndexed").len(), 1);
    Ok(())
}

#[test]
fn composer_metadata_change_republishes_without_reparsing() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    fs::create_dir_all(repository.path().join("app"))?;
    fs::write(
        repository.path().join("composer.json"),
        r#"{"name":"before/package","autoload":{"psr-4":{"App\\":"app/"}}}"#,
    )?;
    fs::write(
        repository.path().join("app/Service.php"),
        "<?php namespace App; class Service {}\n",
    )?;
    let report = index_repository(repository.path())?;
    let path = RepoRelativePath::new("app/Service.php")?;
    assert_eq!(
        report
            .graph
            .file_metadata(&path)
            .and_then(|metadata| metadata.package.as_ref())
            .map(|package| package.name.as_str()),
        Some("before/package")
    );

    fs::write(
        repository.path().join("composer.json"),
        r#"{"name":"after/package","autoload":{"psr-4":{"App\\":"app/"}}}"#,
    )?;
    let sources = scan_repository_sources(repository.path())?;
    let reconciled = report.syntax_index.reconcile_classified_sources(sources)?;
    assert_eq!(reconciled.metrics.reparsed_files, 0);
    let graph = reconciled.graph.ok_or("metadata-only graph missing")?;
    assert_eq!(
        graph
            .file_metadata(&path)
            .and_then(|metadata| metadata.package.as_ref())
            .map(|package| package.name.as_str()),
        Some("after/package")
    );
    Ok(())
}
#[test]
fn metadata_change_revalidates_only_the_affected_files() -> Result<(), Box<dyn Error>> {
    let files = in_memory_sources(&[
        ("app/Service.php", "<?php namespace App; class Service {}\n"),
        ("app/Other.php", "<?php namespace App; class Other {}\n"),
    ])?;
    let metadata_for = |files: &BTreeMap<RepoRelativePath, Arc<str>>, package: &str| {
        files
            .keys()
            .map(|path| {
                let mut metadata = SourceMetadata::path_fallback(path);
                if path.as_str() == "app/Service.php" {
                    metadata.package = Some(chakra_domain::source::SourcePackage {
                        name: package.to_owned(),
                        root: None,
                    });
                }
                (path.clone(), metadata)
            })
            .collect()
    };
    let (index, _) = PhpSyntaxIndex::from_classified_sources(PhpSources {
        files: files.clone(),
        metadata: metadata_for(&files, "before/package"),
    })?;

    let reconciled = index.reconcile_classified_sources(PhpSources {
        files: files.clone(),
        metadata: metadata_for(&files, "after/package"),
    })?;
    // Exactly the metadata-changed file is re-materialized; nothing is
    // reparsed and the graph delta stays structurally incremental.
    assert_eq!(reconciled.metrics.reparsed_files, 0);
    assert_eq!(reconciled.metrics.metadata_files_recomputed, 1);
    assert!(reconciled.metrics.publication.structurally_incremental);
    let graph = reconciled.graph.ok_or("metadata graph missing")?;
    let service = RepoRelativePath::new("app/Service.php")?;
    let other = RepoRelativePath::new("app/Other.php")?;
    assert_eq!(
        graph
            .file_metadata(&service)
            .and_then(|metadata| metadata.package.as_ref())
            .map(|package| package.name.as_str()),
        Some("after/package")
    );
    assert_eq!(
        graph
            .file_metadata(&other)
            .and_then(|metadata| metadata.package.as_ref())
            .map(|package| package.name.as_str()),
        None
    );
    // Symbol identity and count are untouched by the metadata swap.
    assert_eq!(graph.resolve_name("App::Service").len(), 1);
    assert_eq!(graph.resolve_name("App::Other").len(), 1);
    graph.validate_consistency()?;
    Ok(())
}

#[test]
fn framework_config_toggle_rederives_framework_facts_without_reparsing()
-> Result<(), Box<dyn Error>> {
    let files = || {
        in_memory_sources(&[(
            "routes/web.php",
            "<?php\nuse Illuminate\\Support\\Facades\\Route;\nfinal class Controller { public function __invoke(): void {} }\nRoute::get('/users', Controller::class);\n",
        )])
    };
    let classified = |files: BTreeMap<RepoRelativePath, Arc<str>>| PhpSources {
        files,
        metadata: BTreeMap::new(),
    };
    let (index, off_graph) = PhpSyntaxIndex::from_sources_with_laravel(files()?, false)?;
    assert_eq!(index.framework_symbol_count(), 0);
    let (on_index, on_graph) = PhpSyntaxIndex::from_sources_with_laravel(files()?, true)?;
    assert!(on_index.framework_symbol_count() > 0);

    // Toggle on through typed evidence: no source reparse, framework
    // facts match a cold Laravel-enabled build exactly.
    let reconciled = index.reconcile_classified_sources_with_evidence(
        classified(files()?),
        GraphBuildLimits::UNLIMITED,
        Some(true),
        &IndexCancellation::default(),
    )?;
    assert_eq!(reconciled.metrics.reparsed_files, 0);
    assert_eq!(reconciled.metrics.framework_config_changes, 1);
    assert_eq!(reconciled.metrics.framework_files_reparsed, 1);
    let toggled_on = reconciled.graph.ok_or("toggle-on graph missing")?;
    assert_eq!(toggled_on.symbol_count(), on_graph.symbol_count());
    assert_eq!(toggled_on.edge_count(), on_graph.edge_count());
    assert_eq!(
        reconciled
            .next_index
            .as_ref()
            .ok_or("toggle-on index missing")?
            .framework_symbol_count(),
        on_index.framework_symbol_count()
    );
    toggled_on.validate_consistency()?;

    // Toggling back off removes exactly the framework facts again.
    let reconciled = reconciled
        .next_index
        .ok_or("toggle-on index missing")?
        .reconcile_classified_sources_with_evidence(
            classified(files()?),
            GraphBuildLimits::UNLIMITED,
            Some(false),
            &IndexCancellation::default(),
        )?;
    assert_eq!(reconciled.metrics.reparsed_files, 0);
    assert_eq!(reconciled.metrics.framework_config_changes, 1);
    let toggled_off = reconciled.graph.ok_or("toggle-off graph missing")?;
    assert_eq!(toggled_off.symbol_count(), off_graph.symbol_count());
    assert_eq!(toggled_off.edge_count(), off_graph.edge_count());
    toggled_off.validate_consistency()?;
    Ok(())
}

#[test]
fn unchanged_framework_evidence_keeps_the_noop_fast_path() -> Result<(), Box<dyn Error>> {
    let files = in_memory_sources(&[("web.php", "<?php function retained(): void {}\n")])?;
    let (index, _) = PhpSyntaxIndex::from_sources_with_laravel(files.clone(), false)?;
    let reconciled = index.reconcile_classified_sources_with_evidence(
        PhpSources {
            files,
            metadata: BTreeMap::new(),
        },
        GraphBuildLimits::UNLIMITED,
        Some(false),
        &IndexCancellation::default(),
    )?;
    assert!(reconciled.graph.is_none());
    assert_eq!(reconciled.metrics.framework_config_changes, 0);
    assert_eq!(reconciled.metrics.metadata_files_recomputed, 0);
    Ok(())
}
