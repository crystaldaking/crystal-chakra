//! Provider degradation scenarios: no provider installed, and a crashing
//! then recovering `PreciseProvider` double. No real language server runs.

use std::sync::Arc;

use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::query::{
    CallersRequest, ContextRequest, QueryService, StatusRequest, SymbolRef,
};
use chakra_domain::state::ProviderState;
use chakra_engine::PreciseRelation;

use super::{candidate, search_symbols, simple_name};
use crate::fixture::with_live;
use crate::manifest::Manifest;
use crate::provider::FlakyProvider;
use crate::runner::fixtures_root;
use crate::{Check, ensure, failure};

pub(super) fn provider_absent_degradation(manifest: &Manifest) -> Check<Vec<String>> {
    with_live(&fixtures_root().join(&manifest.language), |fixture| {
        let expectations = &manifest.expectations;
        let status = fixture.engine.status(StatusRequest)?;
        ensure(
            status.data.providers.is_empty()
                && status.provider_state == ProviderState::NotConfigured,
            format!(
                "expected no reported providers and not_configured state, found {:?} providers ({:?})",
                status.data.providers.len(),
                status.provider_state,
            ),
        )?;

        let callers = fixture.engine.callers(CallersRequest {
            source: Default::default(),
            symbol: Some(SymbolRef::ByName(expectations.callee.clone())),
            ..CallersRequest::default()
        })?;
        ensure(
            callers.data.provider.is_none(),
            "no provider is installed, yet provider info was reported",
        )?;
        ensure(
            callers.data.callers.iter().any(|caller| {
                caller.symbol.qualified_name == expectations.caller
                    && caller.precision == Precision::Heuristic
            }),
            "syntax callers were not served without a provider",
        )?;

        let context = fixture.engine.context(ContextRequest {
            source: Default::default(),
            symbol: Some(SymbolRef::ByName(expectations.caller.clone())),
            ..ContextRequest::default()
        })?;
        ensure(
            context.data.provider.is_none(),
            "context reported provider info without a provider",
        )?;
        ensure(
            context
                .data
                .callees
                .iter()
                .any(|callee| callee.symbol.qualified_name == expectations.callee),
            "syntax callees were not served without a provider",
        )?;
        Ok(vec![
            "provider absent: queries answer with syntax provenance, state not_configured, no failure"
                .to_owned(),
        ])
    })
}

pub(super) fn provider_crash_recovery(manifest: &Manifest) -> Check<Vec<String>> {
    with_live(&fixtures_root().join(&manifest.language), |fixture| {
        let expectations = &manifest.expectations;
        let found = search_symbols(fixture, simple_name(&expectations.caller), None)?;
        let caller_location = candidate(&found.data, &expectations.caller)?
            .location
            .clone();

        let provider = Arc::new(FlakyProvider::crashed());
        fixture.engine.install_precise_provider(provider.clone())?;

        let degraded = fixture.engine.callers(CallersRequest {
            source: Default::default(),
            symbol: Some(SymbolRef::ByName(expectations.callee.clone())),
            ..CallersRequest::default()
        })?;
        let info = degraded
            .data
            .provider
            .as_ref()
            .ok_or_else(|| failure("degraded provider info missing"))?;
        ensure(
            info.state == ProviderState::Degraded,
            format!("expected degraded provider state, found {:?}", info.state),
        )?;
        ensure(
            info.fallback_used && info.fallback_reason.is_some(),
            "degraded provider must surface an explicit fallback",
        )?;
        ensure(
            info.last_error.is_some(),
            "degraded provider must report its error",
        )?;
        ensure(
            degraded.data.callers.iter().any(|caller| {
                caller.symbol.qualified_name == expectations.caller
                    && caller.precision == Precision::Heuristic
            }),
            "syntax callers must be retained (not upgraded) while the provider is degraded",
        )?;

        // The double labels its precise facts with the Chakra-owned precise
        // provenance; see crates/chakra-conformance/src/provider.rs.
        provider.restart(vec![PreciseRelation {
            name: expectations.caller_simple.clone(),
            declaration: caller_location.clone(),
            occurrence_count: 1,
            call_sites: vec![caller_location],
            provenance: Provenance::ChakraResolver,
        }]);
        let recovered = fixture.engine.callers(CallersRequest {
            source: Default::default(),
            symbol: Some(SymbolRef::ByName(expectations.callee.clone())),
            ..CallersRequest::default()
        })?;
        let info = recovered
            .data
            .provider
            .as_ref()
            .ok_or_else(|| failure("recovered provider info missing"))?;
        ensure(
            info.state == ProviderState::Ready && !info.fallback_used,
            format!(
                "expected ready provider without fallback, found {:?} (fallback={})",
                info.state, info.fallback_used
            ),
        )?;
        ensure(
            recovered.data.callers.iter().any(|caller| {
                caller.symbol.qualified_name == expectations.caller
                    && caller.precision == Precision::Precise
                    && caller.provenance == Provenance::ChakraResolver
            }),
            "recovered provider did not contribute precise caller facts",
        )?;
        ensure(
            provider.start_attempts() == 2,
            "provider double did not record failed start plus restart",
        )?;
        Ok(vec![
            "provider crash: explicit fallback, syntax facts retained, precision never upgraded silently"
                .to_owned(),
            "provider recovery: precise facts merge with precise precision and provider provenance"
                .to_owned(),
        ])
    })
}
