//! Sanity checks for the Controller → Service → Provider fixture crate
//! (roadmap §17): the indexer and MCP end-to-end tests build on these files.

mod common;

use std::error::Error;
use std::fs;
use std::path::PathBuf;

use common::fixture_root;

fn read(path: PathBuf) -> Result<String, Box<dyn Error>> {
    Ok(fs::read_to_string(path)?)
}

#[test]
fn fixture_layout_exists() {
    let root = fixture_root();
    for relative in [
        "Cargo.toml",
        "src/lib.rs",
        "src/api/controller.rs",
        "src/service/payment_service.rs",
        "src/provider/mod.rs",
        "tests/refund_flow.rs",
    ] {
        assert!(
            root.join(relative).is_file(),
            "missing fixture file: {relative}"
        );
    }
}

#[test]
fn fixture_contains_the_scenario_declarations() -> Result<(), Box<dyn Error>> {
    let root = fixture_root();

    let controller = read(root.join("src/api/controller.rs"))?;
    assert!(controller.contains("pub struct PaymentController"));
    assert!(controller.contains("self.service.refund("));

    let service = read(root.join("src/service/payment_service.rs"))?;
    assert!(service.contains("pub struct PaymentService"));
    assert!(service.contains("self.provider.refund("));
    assert!(service.contains("#[test]"));

    let provider = read(root.join("src/provider/mod.rs"))?;
    assert!(provider.contains("pub trait PaymentProvider"));
    assert!(provider.contains("pub struct StripeProvider"));
    assert!(provider.contains("impl PaymentProvider for StripeProvider"));

    let flow = read(root.join("tests/refund_flow.rs"))?;
    assert!(flow.contains("controller.refund("));
    Ok(())
}
