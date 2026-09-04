//! Explicit real-provider smoke test. It is ignored by default so the normal
//! suite never depends on a developer-global csharp-ls/.NET installation.

use std::error::Error;
use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use chakra_domain::provenance::Provenance;
use chakra_domain::revision::Revision;
use chakra_domain::state::ProviderState;
use chakra_domain::symbol::Language;
use chakra_engine::{
    CallHierarchyDirections, PreciseProvider, PreciseQueryRequest, ProviderDocument,
    ProviderSymbol, ProviderWorkspace,
};
use chakra_provider_csharp_ls::{CsharpLsCommand, CsharpLsConfig, CsharpLsProvider};

#[test]
#[ignore = "requires csharp-ls 0.26.x and the .NET 10 SDK"]
fn csharp_ls_026_resolves_a_signature_named_method() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    fs::create_dir(repository.path().join("Project"))?;
    fs::write(
        repository.path().join("Project/Project.csproj"),
        concat!(
            "<Project Sdk=\"Microsoft.NET.Sdk\"><PropertyGroup>",
            "<OutputType>Exe</OutputType><TargetFramework>net10.0</TargetFramework>",
            "</PropertyGroup></Project>\n"
        ),
    )?;
    fs::write(
        repository.path().join("Sample.sln"),
        concat!(
            "Microsoft Visual Studio Solution File, Format Version 12.00\n",
            "# Visual Studio Version 17\n",
            "Project(\"{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}\") = ",
            "\"Project\", \"Project/Project.csproj\", ",
            "\"{11111111-1111-1111-1111-111111111111}\"\n",
            "EndProject\nGlobal\nEndGlobal\n"
        ),
    )?;
    let path = RepoRelativePath::new("Project/Class.cs")?;
    let source: Arc<str> = Arc::from(
        "using System;\n\nclass Class\n{\n    public void MethodA(string arg)\n    {\n        string str = \"\";\n        Console.WriteLine(str);\n    }\n\n    public void MethodB(string arg)\n    {\n        MethodA(arg);\n    }\n}\n",
    );
    fs::write(repository.path().join(path.as_str()), source.as_ref())?;
    let workspace = ProviderWorkspace::from_documents(
        fs::canonicalize(repository.path())?,
        Revision(1),
        vec![ProviderDocument {
            path: path.clone(),
            source,
            language: Language::CSharp,
        }],
    );
    let request = PreciseQueryRequest {
        workspace: workspace.clone(),
        symbol: ProviderSymbol {
            name: "MethodA".to_owned(),
            declaration: SourceRange::new(
                path,
                TextPosition::new(5, 5)?,
                TextPosition::new(9, 6)?,
            )?,
            language: Language::CSharp,
        },
        directions: CallHierarchyDirections {
            incoming: true,
            outgoing: false,
        },
        limit: 20,
        priority: chakra_engine::ProviderRequestPriority::Normal,
    };
    let mut command = std::env::var_os("CHAKRA_CSHARP_LS")
        .map_or_else(CsharpLsCommand::discover, |path| {
            Some(CsharpLsCommand::stdio(path))
        })
        .ok_or("csharp-ls not found")?;
    let rpc_log_path = repository.path().join("csharp-ls-rpc.log");
    command.args.push("--rpclog".into());
    command.args.push(rpc_log_path.as_os_str().to_owned());
    let provider = CsharpLsProvider::start(
        workspace,
        CsharpLsConfig {
            command,
            startup_timeout: Duration::from_secs(30),
            request_timeout: Duration::from_secs(60),
            barrier_timeout: Duration::from_secs(20),
            query_wait_timeout: Duration::from_secs(90),
            ..CsharpLsConfig::default()
        },
    )?;

    let started = Instant::now();
    let result = provider.enrich(request);
    let elapsed = started.elapsed();
    assert_eq!(
        result.state,
        ProviderState::Ready,
        "provider error: {:?}",
        provider.last_error()
    );
    let has_expected_caller = result.incoming.iter().any(|relation| {
        relation.name.starts_with("MethodB") && relation.provenance == Provenance::CsharpLs
    });
    if !has_expected_caller {
        let rpc_log = fs::read_to_string(&rpc_log_path)
            .unwrap_or_else(|error| format!("failed to read csharp-ls RPC log: {error}"));
        provider.shutdown()?;
        return Err(format!("incoming: {:?}\nrpc log:\n{rpc_log}", result.incoming).into());
    }
    eprintln!(
        "csharp_ls_enrichment: elapsed={elapsed:?}, incoming={}",
        result.incoming.len()
    );
    provider.shutdown()?;
    Ok(())
}
