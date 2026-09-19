//! Explicit real-provider smoke test. It is ignored by default so the normal
//! suite never depends on a developer-global kotlin-lsp installation.

use std::error::Error;
use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use chakra_domain::operation::OperationContext;
use chakra_domain::provenance::Provenance;
use chakra_domain::revision::Revision;
use chakra_domain::state::ProviderState;
use chakra_domain::symbol::Language;
use chakra_engine::{
    CallHierarchyDirections, PreciseProvider, PreciseQueryRequest, PreciseQueryResult,
    ProviderDocument, ProviderInput, ProviderSymbol, ProviderWorkspace,
};
use chakra_provider_kotlin_lsp::{KotlinLspCommand, KotlinLspConfig, KotlinLspProvider};

fn request(
    repository_root: &std::path::Path,
    path: RepoRelativePath,
    source: Arc<str>,
    revision: Revision,
    supporting_documents: &[ProviderDocument],
) -> Result<PreciseQueryRequest, Box<dyn Error>> {
    let target_line = source
        .lines()
        .position(|line| line.starts_with("fun target("))
        .ok_or("fixture has no target declaration")?;
    let mut documents = supporting_documents.to_vec();
    documents.push(ProviderDocument {
        path: path.clone(),
        source,
        language: Language::Kotlin,
    });
    Ok(PreciseQueryRequest {
        workspace: ProviderWorkspace::from_documents_and_inputs(
            fs::canonicalize(repository_root)?,
            revision,
            documents,
            [
                "pom.xml",
                "build.gradle.kts",
                "settings.gradle.kts",
                "gradle.properties",
                "library/build.gradle.kts",
            ]
            .into_iter()
            .filter_map(|path| {
                let metadata = fs::metadata(repository_root.join(path)).ok()?;
                ProviderInput::from_metadata(
                    RepoRelativePath::new(path).ok()?,
                    [Language::Kotlin],
                    &metadata,
                )
            })
            .collect(),
        ),
        symbol: ProviderSymbol {
            name: "target".to_owned(),
            declaration: SourceRange::new(
                path,
                TextPosition::new(target_line as u32 + 1, 1)?,
                TextPosition::new(target_line as u32 + 1, 13)?,
            )?,
            language: Language::Kotlin,
        },
        directions: CallHierarchyDirections {
            incoming: true,
            outgoing: false,
        },
        limit: 20,
        priority: chakra_engine::ProviderRequestPriority::Normal,
    })
}

#[test]
#[ignore = "requires kotlin-lsp on PATH or CHAKRA_KOTLIN_LSP"]
fn current_kotlin_lsp_rejects_unimported_standalone_scripts() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    let path = RepoRelativePath::new("sample.kts")?;
    let source: Arc<str> = Arc::from(
        "fun target() {}\nfun caller() { target() }\njava.io.File(\"script-ran\").writeText(\"unexpected execution\")\n",
    );
    fs::write(repository.path().join(path.as_str()), source.as_ref())?;
    let initial = request(repository.path(), path, source, Revision(1), &[])?;
    let command = std::env::var_os("CHAKRA_KOTLIN_LSP")
        .map_or_else(KotlinLspCommand::discover, |path| {
            Some(KotlinLspCommand::stdio(path))
        })
        .ok_or("kotlin-lsp not found")?;
    let provider = KotlinLspProvider::start(
        initial.workspace.clone(),
        KotlinLspConfig {
            command,
            ..KotlinLspConfig::default()
        },
    )?;
    let result = await_enrichment(&provider, initial);
    assert_eq!(result.state, ProviderState::Degraded);
    assert!(result.incoming.is_empty());
    assert!(result.outgoing.is_empty());
    assert!(
        provider
            .last_error()
            .is_some_and(|error| error.contains("did not successfully import")),
        "expected explicit import failure, got {:?}",
        provider.last_error()
    );
    assert!(!repository.path().join("script-ran").exists());
    provider.shutdown()?;
    Ok(())
}

#[test]
#[ignore = "requires kotlin-lsp on PATH or CHAKRA_KOTLIN_LSP"]
fn current_kotlin_lsp_returns_precise_incoming_calls_across_revisions() -> Result<(), Box<dyn Error>>
{
    check_project(ProjectKind::Maven)
}

#[test]
#[ignore = "requires kotlin-lsp, JDK 21 and Gradle distribution access"]
fn current_kotlin_lsp_supports_gradle_jvm() -> Result<(), Box<dyn Error>> {
    check_project(ProjectKind::Gradle)
}

#[test]
#[ignore = "requires kotlin-lsp, JDK 21 and Gradle distribution access"]
fn current_kotlin_lsp_supports_java_boundaries_and_java_only_edits() -> Result<(), Box<dyn Error>> {
    check_project(ProjectKind::GradleJava)
}

#[test]
#[ignore = "requires kotlin-lsp, JDK 21, Android SDK 35 and Gradle distribution access"]
fn current_kotlin_lsp_supports_android() -> Result<(), Box<dyn Error>> {
    check_project(ProjectKind::Android)
}

#[test]
#[ignore = "requires kotlin-lsp, JDK 21 and Gradle distribution access"]
fn current_kotlin_lsp_supports_multiplatform() -> Result<(), Box<dyn Error>> {
    check_project(ProjectKind::Multiplatform)
}

#[derive(Clone, Copy, Debug)]
enum ProjectKind {
    Maven,
    Gradle,
    GradleJava,
    Android,
    Multiplatform,
}

fn await_enrichment(
    provider: &KotlinLspProvider,
    request: PreciseQueryRequest,
) -> PreciseQueryResult {
    // Exercise the production short caller wait while cold Gradle downloads
    // and indexing continue in the owner. Only CatchingUp may be retried:
    // a Ready-but-empty or degraded response must reach the assertions.
    let operation = OperationContext::unbounded().bounded_by(Duration::from_secs(600));
    loop {
        assert!(
            operation.check().is_ok(),
            "project never became ready: error={:?}, progress={:?}",
            provider.last_error(),
            provider.progress(),
        );
        let result = provider.enrich_with_context(request.clone(), &operation);
        if result.state != ProviderState::CatchingUp {
            return result;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn check_project(kind: ProjectKind) -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    if matches!(kind, ProjectKind::Maven) {
        fs::write(
            repository.path().join("pom.xml"),
            r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>sample</groupId><artifactId>sample</artifactId><version>1</version>
  <properties><kotlin.version>2.2.20</kotlin.version></properties>
  <dependencies><dependency>
    <groupId>org.jetbrains.kotlin</groupId><artifactId>kotlin-stdlib</artifactId>
    <version>${kotlin.version}</version>
  </dependency></dependencies>
  <build>
    <sourceDirectory>src/main/kotlin</sourceDirectory>
    <plugins><plugin>
      <groupId>org.jetbrains.kotlin</groupId><artifactId>kotlin-maven-plugin</artifactId>
      <version>${kotlin.version}</version>
      <executions><execution><id>compile</id><goals><goal>compile</goal></goals></execution></executions>
    </plugin></plugins>
  </build>
</project>"#,
        )?;
    } else {
        fs::write(
            repository.path().join("settings.gradle.kts"),
            "pluginManagement { repositories { google(); mavenCentral(); gradlePluginPortal() } }\nrootProject.name = \"sample\"\n",
        )?;
        let build = match kind {
            ProjectKind::Android => {
                "plugins { id(\"com.android.library\") version \"8.11.1\"; kotlin(\"android\") version \"2.2.20\" }\nrepositories { google(); mavenCentral() }\nandroid { namespace = \"sample\"; compileSdk = 35; defaultConfig { minSdk = 24 } }\n"
            }
            ProjectKind::Multiplatform => {
                "plugins { kotlin(\"multiplatform\") version \"2.2.20\"; kotlin(\"plugin.serialization\") version \"2.2.20\"; kotlin(\"plugin.allopen\") version \"2.2.20\" }\nrepositories { mavenCentral() }\nkotlin { jvm(); js(IR) { nodejs() }; linuxX64(); jvmToolchain(21); compilerOptions { freeCompilerArgs.addAll(\"-Xcontext-parameters\", \"-Xannotation-target-all\") } }\n"
            }
            _ => {
                "plugins { kotlin(\"jvm\") version \"2.2.20\" }\nrepositories { mavenCentral() }\nkotlin { jvmToolchain(21) }\n"
            }
        };
        fs::write(repository.path().join("build.gradle.kts"), build)?;
        if matches!(kind, ProjectKind::Multiplatform) {
            let settings = repository.path().join("settings.gradle.kts");
            fs::write(
                &settings,
                format!(
                    "{}\ninclude(\":library\")\n",
                    fs::read_to_string(&settings)?
                ),
            )?;
            fs::create_dir(repository.path().join("library"))?;
            fs::write(
                repository.path().join("library/build.gradle.kts"),
                "plugins { kotlin(\"multiplatform\") version \"2.2.20\"; kotlin(\"plugin.serialization\") version \"2.2.20\"; kotlin(\"plugin.allopen\") version \"2.2.20\" }\nrepositories { mavenCentral() }\nkotlin { jvm(); js(IR) { nodejs() }; linuxX64(); jvmToolchain(21) }\n",
            )?;
            fs::write(
                repository.path().join("build.gradle.kts"),
                format!(
                    "{build}\nkotlin {{ sourceSets.commonMain.dependencies {{ implementation(project(\":library\")) }} }}\n"
                ),
            )?;
        }
        fs::create_dir_all(repository.path().join("gradle/wrapper"))?;
        fs::write(
            repository
                .path()
                .join("gradle/wrapper/gradle-wrapper.properties"),
            "distributionUrl=https\\://services.gradle.org/distributions/gradle-8.14.3-bin.zip\ndistributionSha256Sum=bd71102213493060956ec229d946beee57158dbd89d0e62b91bca0fa2c5f3531\n",
        )?;
        fs::write(
            repository.path().join("gradle.properties"),
            "org.gradle.jvmargs=-Xmx512m\norg.gradle.workers.max=2\n",
        )?;
    }
    if matches!(kind, ProjectKind::Android) {
        let sdk = std::env::var_os("ANDROID_HOME")
            .ok_or("ANDROID_HOME is required for the Android fixture")?;
        assert!(
            std::path::Path::new(&sdk)
                .join("platforms/android-35/android.jar")
                .is_file(),
            "Android SDK platform 35 is required"
        );
        fs::create_dir_all(repository.path().join("src/main"))?;
        fs::write(
            repository.path().join("src/main/AndroidManifest.xml"),
            "<manifest xmlns:android=\"http://schemas.android.com/apk/res/android\" />\n",
        )?;
    }
    let source_dir = if matches!(kind, ProjectKind::Multiplatform) {
        "src/commonMain/kotlin"
    } else {
        "src/main/kotlin"
    };
    fs::create_dir_all(repository.path().join(source_dir))?;
    let path = RepoRelativePath::new(format!("{source_dir}/Main.kt"))?;
    // Distinguish project import from a server that can only inspect one file.
    // Android requires an SDK type and overload discrimination. KMP includes
    // expect/actual declarations and consumers from every configured target.
    let supporting_sources: Vec<(&str, &str)> = match kind {
        ProjectKind::GradleJava => vec![(
            "src/main/java/sample/JavaPeer.java",
            "package sample;\npublic class JavaPeer {\n  public static void javaCaller() { MainKt.target(); }\n  public static void numericCaller() { MainKt.target(1); }\n  public static Runnable methodValue = MainKt::target;\n  public static void javaTarget() {}\n}\n",
        )],
        ProjectKind::Multiplatform => vec![
            (
                "library/src/commonMain/kotlin/Library.kt",
                "package sample\nfun libraryLabel(): String = \"library\"\nfun libraryLabel(value: Int): String = value.toString()\n",
            ),
            (
                "src/commonMain/kotlin/Platform.kt",
                "package sample\nexpect fun platformName(): String\n",
            ),
            (
                "src/jvmMain/kotlin/Platform.kt",
                "package sample\nactual fun platformName(): String = System.getProperty(\"os.name\")\nfun jvmCaller() { target() }\nfun jvmNumericCaller() { target(1) }\n",
            ),
            (
                "src/jsMain/kotlin/Platform.kt",
                "package sample\nactual fun platformName(): String = \"js\"\nfun jsCaller() { target() }\nfun jsNumericCaller() { target(1) }\n",
            ),
            (
                "src/linuxX64Main/kotlin/Platform.kt",
                "package sample\nactual fun platformName(): String = \"linux\"\nfun nativeCaller() { target() }\nfun nativeNumericCaller() { target(1) }\n",
            ),
        ],
        _ => vec![],
    };
    let mut supporting_documents = Vec::new();
    if matches!(kind, ProjectKind::Gradle) {
        let build_path = repository.path().join("build.gradle.kts");
        let script = format!(
            "{}\nfun buildTarget() = 42\nfun buildCaller() = buildTarget()\n",
            fs::read_to_string(&build_path)?
        );
        fs::write(&build_path, &script)?;
        supporting_documents.push(ProviderDocument {
            path: RepoRelativePath::new("build.gradle.kts")?,
            source: Arc::from(script),
            language: Language::Kotlin,
        });
    }
    for (relative_path, text) in supporting_sources {
        let file = repository.path().join(relative_path);
        fs::create_dir_all(file.parent().ok_or("fixture file has no parent")?)?;
        fs::write(file, text)?;
        supporting_documents.push(ProviderDocument {
            path: RepoRelativePath::new(relative_path)?,
            source: Arc::from(text),
            language: if relative_path.ends_with(".java") {
                Language::Java
            } else {
                Language::Kotlin
            },
        });
    }
    let source: Arc<str> = Arc::from(match kind {
        ProjectKind::GradleJava => {
            "package sample\n\nfun target() { JavaPeer.javaTarget() }\nfun target(value: Int) {}\nfun caller() { target() }\n"
        }
        ProjectKind::Android => {
            "package sample\n\nfun target(context: android.content.Context) { context.getString(0) }\nfun caller(context: android.content.Context) { target(context) }\nfun target(value: Int) {}\nfun numericCaller() { target(1) }\n"
        }
        ProjectKind::Multiplatform => {
            "package sample\n\nfun target(): String { libraryLabel(); return platformName() }\nfun caller() { target() }\nfun target(value: Int): String = value.toString()\n"
        }
        _ => "package sample\n\nfun target() {}\nfun caller() { target() }\n",
    });
    fs::write(repository.path().join(path.as_str()), source.as_ref())?;
    let initial = request(
        repository.path(),
        path.clone(),
        source.clone(),
        Revision(1),
        &supporting_documents,
    )?;
    let command = std::env::var_os("CHAKRA_KOTLIN_LSP")
        .map_or_else(KotlinLspCommand::discover, |path| {
            Some(KotlinLspCommand::stdio(path))
        })
        .ok_or("kotlin-lsp not found")?;
    let provider = KotlinLspProvider::start(
        initial.workspace.clone(),
        KotlinLspConfig {
            command,
            // Exercise the production startup bound, including cold Gradle
            // model preparation before the language server is spawned.
            request_timeout: Duration::from_secs(60),
            barrier_timeout: Duration::from_secs(20),
            ..KotlinLspConfig::default()
        },
    )?;

    let initial_started = Instant::now();
    let result = await_enrichment(&provider, initial);
    let initial_elapsed = initial_started.elapsed();
    assert_eq!(
        result.state,
        ProviderState::Ready,
        "provider error: {:?}",
        provider.last_error()
    );
    if matches!(kind, ProjectKind::Gradle) {
        let script = &supporting_documents[0];
        let line = script
            .source
            .lines()
            .position(|line| line.starts_with("fun buildTarget"))
            .ok_or("missing build target")? as u32
            + 1;
        assert!(!provider.supports_path(Language::Kotlin, &script.path));
        let mut script_query = request(
            repository.path(),
            path.clone(),
            source.clone(),
            Revision(1),
            &supporting_documents,
        )?;
        script_query.symbol.name = "buildTarget".to_owned();
        script_query.symbol.declaration = SourceRange::new(
            script.path.clone(),
            TextPosition::new(line, 5)?,
            TextPosition::new(line, 16)?,
        )?;
        assert_eq!(provider.enrich(script_query).state, ProviderState::Degraded);
        assert_eq!(provider.state_for(Revision(1)), ProviderState::Ready);
    }
    assert!(
        result
            .incoming
            .iter()
            .any(|relation| relation.name.split('(').next() == Some("caller")),
        "incoming: {:?}",
        result.incoming
    );

    if matches!(kind, ProjectKind::Android) {
        assert!(
            result
                .incoming
                .iter()
                .all(|relation| relation.name.split('(').next() != Some("numericCaller")),
            "Android overloads were conflated: {:?}",
            result.incoming
        );
    }
    if matches!(kind, ProjectKind::GradleJava) {
        assert_eq!(
            result.incoming.len(),
            2,
            "overloads/method values are not calls to this target: {:?}",
            result.incoming
        );
        assert!(!result.incoming_truncated);
        assert!(
            result
                .incoming
                .iter()
                .any(|relation| relation.declaration.file().as_str()
                    == "src/main/java/sample/JavaPeer.java"
                    && relation
                        .name
                        .split('(')
                        .next()
                        .is_some_and(|name| name.ends_with("javaCaller"))),
            "missing Java caller: {:?}",
            result.incoming
        );
    }
    if matches!(kind, ProjectKind::Multiplatform) {
        for name in ["jvmCaller", "jsCaller", "nativeCaller"] {
            assert!(
                result
                    .incoming
                    .iter()
                    .any(|relation| relation.name.split('(').next() == Some(name)),
                "missing {name} from common target's callers: {:?}",
                result.incoming
            );
        }
    }
    if matches!(kind, ProjectKind::Multiplatform) {
        assert_eq!(
            result.incoming.len(),
            4,
            "KMP overloads or callable owners were conflated: {:?}",
            result.incoming
        );
        assert!(!result.incoming_truncated);
    }
    let added_caller = if matches!(kind, ProjectKind::Android) {
        "fun callerTwo(context: android.content.Context) { target(context) }\n"
    } else {
        "fun callerTwo() { target() }\n"
    };
    let caller_line = source.lines().count() as u32 + 1;
    let changed_source: Arc<str> = Arc::from(format!("{source}{added_caller}"));
    assert_eq!(result.revision, Revision(1));
    assert!(
        result
            .incoming
            .iter()
            .all(|relation| relation.provenance == Provenance::KotlinLsp)
    );
    fs::write(
        repository.path().join(path.as_str()),
        changed_source.as_ref(),
    )?;
    let changed_started = Instant::now();
    let changed = await_enrichment(
        &provider,
        request(
            repository.path(),
            path.clone(),
            changed_source.clone(),
            Revision(2),
            &supporting_documents,
        )?,
    );
    let changed_elapsed = changed_started.elapsed();
    assert_eq!(
        changed.state,
        ProviderState::Ready,
        "provider error after edit: {:?}",
        provider.last_error()
    );
    assert_eq!(changed.revision, Revision(2));
    if matches!(kind, ProjectKind::Android) {
        assert!(
            changed
                .incoming
                .iter()
                .all(|relation| relation.name.split('(').next() != Some("numericCaller")),
            "Android overloads were conflated after edit: {:?}",
            changed.incoming
        );
    }
    assert!(
        changed
            .incoming
            .iter()
            .any(|relation| relation.name.split('(').next() == Some("callerTwo")),
        "incoming after edit: {:?}",
        changed.incoming
    );
    assert!(
        changed
            .incoming
            .iter()
            .all(|relation| relation.provenance == Provenance::KotlinLsp)
    );
    let mut outgoing_request = request(
        repository.path(),
        path.clone(),
        changed_source.clone(),
        Revision(2),
        &supporting_documents,
    )?;
    outgoing_request.symbol.name = "callerTwo".to_owned();
    outgoing_request.symbol.declaration = SourceRange::new(
        path,
        TextPosition::new(caller_line, 1)?,
        TextPosition::new(caller_line, added_caller.trim_end().len() as u32 + 1)?,
    )?;
    outgoing_request.directions = CallHierarchyDirections {
        incoming: false,
        outgoing: true,
    };
    if matches!(kind, ProjectKind::Multiplatform) {
        // Exercise the platform functions themselves: the pinned server's
        // hierarchy renderer drops JS/Native items despite valid bindings.
        for (source_path, name) in [
            ("src/jvmMain/kotlin/Platform.kt", "jvmCaller"),
            ("src/jsMain/kotlin/Platform.kt", "jsCaller"),
            ("src/linuxX64Main/kotlin/Platform.kt", "nativeCaller"),
        ] {
            let mut platform_query = outgoing_request.clone();
            platform_query.symbol.name = name.to_owned();
            platform_query.symbol.declaration = SourceRange::new(
                RepoRelativePath::new(source_path)?,
                TextPosition::new(3, 5)?,
                TextPosition::new(3, 5 + name.len() as u32)?,
            )?;
            let platform_result = await_enrichment(&provider, platform_query);
            assert_eq!(
                platform_result.state,
                ProviderState::Ready,
                "{name}: {:?}",
                provider.last_error()
            );
            assert_eq!(platform_result.revision, Revision(2));
            assert_eq!(
                platform_result.outgoing.len(),
                1,
                "{name}: {:?}",
                platform_result.outgoing
            );
            assert_eq!(
                platform_result.outgoing[0].name.split('(').next(),
                Some("target")
            );
            assert_eq!(platform_result.outgoing[0].declaration.start().line(), 3);
            assert_eq!(
                platform_result.outgoing[0].provenance,
                Provenance::KotlinLsp
            );
            assert!(!platform_result.outgoing_truncated);
        }
        let mut expect_query = outgoing_request.clone();
        expect_query.symbol.name = "target".to_owned();
        expect_query.symbol.declaration = SourceRange::new(
            RepoRelativePath::new("src/commonMain/kotlin/Main.kt")?,
            TextPosition::new(3, 5)?,
            TextPosition::new(3, 11)?,
        )?;
        let expect_result = await_enrichment(&provider, expect_query);
        assert_eq!(
            expect_result.state,
            ProviderState::Ready,
            "{:?}",
            provider.last_error()
        );
        assert_eq!(
            expect_result.outgoing.len(),
            2,
            "{:?}",
            expect_result.outgoing
        );
        for (name, source_path) in [
            ("platformName", "src/commonMain/kotlin/Platform.kt"),
            ("libraryLabel", "library/src/commonMain/kotlin/Library.kt"),
        ] {
            let relation = expect_result
                .outgoing
                .iter()
                .find(|relation| relation.name.split('(').next() == Some(name))
                .ok_or_else(|| format!("missing {name}: {:?}", expect_result.outgoing))?;
            assert_eq!(relation.declaration.file().as_str(), source_path);
            assert_eq!(relation.provenance, Provenance::KotlinLsp);
        }
        let mut library_query = outgoing_request.clone();
        library_query.symbol.name = "libraryLabel".to_owned();
        library_query.symbol.declaration = SourceRange::new(
            RepoRelativePath::new("library/src/commonMain/kotlin/Library.kt")?,
            TextPosition::new(2, 5)?,
            TextPosition::new(2, 17)?,
        )?;
        library_query.directions = CallHierarchyDirections {
            incoming: true,
            outgoing: false,
        };
        let library_result = await_enrichment(&provider, library_query);
        assert_eq!(
            library_result.state,
            ProviderState::Ready,
            "{:?}",
            provider.last_error()
        );
        assert_eq!(
            library_result.incoming.len(),
            1,
            "{:?}",
            library_result.incoming
        );
        assert_eq!(
            library_result.incoming[0].name.split('(').next(),
            Some("target")
        );
        assert_eq!(
            library_result.incoming[0].declaration.file().as_str(),
            "src/commonMain/kotlin/Main.kt"
        );
        assert_eq!(library_result.incoming[0].declaration.start().line(), 3);
        assert_eq!(library_result.incoming[0].provenance, Provenance::KotlinLsp);
        assert!(!library_result.incoming_truncated);
        assert!(!expect_result.outgoing_truncated);
        assert_eq!(changed.incoming.len(), 5, "{:?}", changed.incoming);
    }
    let outgoing = await_enrichment(&provider, outgoing_request);
    assert_eq!(
        outgoing.state,
        ProviderState::Ready,
        "outgoing error: {:?}",
        provider.last_error()
    );
    assert_eq!(outgoing.revision, Revision(2));
    assert!(
        outgoing
            .outgoing
            .iter()
            .any(|relation| relation.name.split('(').next() == Some("target")),
        "outgoing: {:?}",
        outgoing.outgoing
    );
    assert!(
        outgoing
            .outgoing
            .iter()
            .all(|relation| relation.provenance == Provenance::KotlinLsp)
    );
    eprintln!(
        "kotlin_lsp_enrichment: project={kind:?}, initial={initial_elapsed:?}, after_edit={changed_elapsed:?}, initial_incoming={}, changed_incoming={}",
        result.incoming.len(),
        changed.incoming.len(),
    );
    if matches!(kind, ProjectKind::GradleJava) {
        let java = supporting_documents
            .iter_mut()
            .find(|document| document.language == Language::Java)
            .ok_or("missing Java fixture")?;
        java.source = Arc::from(java.source.replace("javaCaller()", "javaCallerAfter()"));
        fs::write(
            repository.path().join(java.path.as_str()),
            java.source.as_ref(),
        )?;
        let mut java_edit_request = request(
            repository.path(),
            RepoRelativePath::new("src/main/kotlin/Main.kt")?,
            changed_source.clone(),
            Revision(3),
            &supporting_documents,
        )?;
        let java_edit = await_enrichment(&provider, java_edit_request.clone());
        assert_eq!(
            java_edit.state,
            ProviderState::Ready,
            "{:?}",
            provider.last_error()
        );
        assert_eq!(java_edit.revision, Revision(3));
        assert_eq!(java_edit.incoming.len(), 3, "{:?}", java_edit.incoming);
        assert!(!java_edit.incoming_truncated);
        let sync = provider
            .metrics()
            .ok_or("missing synchronization metrics")?
            .document_sync;
        assert_eq!(sync.revision, Some(Revision(3)));
        assert_eq!(
            sync.changed, 1,
            "Java-only edit must participate in the synchronization barrier"
        );
        assert!(
            java_edit.incoming.iter().any(|relation| relation
                .name
                .split('(')
                .next()
                .is_some_and(|name| name.ends_with("javaCallerAfter"))),
            "missing updated Java caller: {:?}",
            java_edit.incoming
        );
        assert!(
            !java_edit.incoming.iter().any(|relation| relation
                .name
                .split('(')
                .next()
                .is_some_and(|name| name.ends_with("javaCaller"))),
            "stale Java caller: {:?}",
            java_edit.incoming
        );
        java_edit_request.directions = CallHierarchyDirections {
            incoming: false,
            outgoing: true,
        };
        let java_target = await_enrichment(&provider, java_edit_request);
        assert_eq!(
            java_target.state,
            ProviderState::Ready,
            "{:?}",
            provider.last_error()
        );
        assert!(
            java_target
                .outgoing
                .iter()
                .any(|relation| relation.declaration.file().as_str()
                    == "src/main/java/sample/JavaPeer.java"
                    && relation
                        .name
                        .split('(')
                        .next()
                        .is_some_and(|name| name.ends_with("javaTarget"))
                    && relation.provenance == Provenance::KotlinLsp),
            "missing Kotlin-to-Java edge: {:?}",
            java_target.outgoing
        );
    }
    if matches!(kind, ProjectKind::Multiplatform) {
        let build = repository.path().join("build.gradle.kts");
        let mut build_source = fs::read_to_string(&build)?;
        build_source.push_str(
            "\nkotlin.sourceSets.named(\"commonMain\") { kotlin.srcDir(\"custom/common\") }\n",
        );
        fs::write(&build, build_source)?;
        let extra_path = RepoRelativePath::new("custom/common/Extra.kt")?;
        let extra_source: Arc<str> =
            Arc::from("package sample\nfun customRootCaller() { target() }\n");
        fs::create_dir_all(repository.path().join("custom/common"))?;
        fs::write(
            repository.path().join(extra_path.as_str()),
            extra_source.as_ref(),
        )?;
        supporting_documents.push(ProviderDocument {
            path: extra_path,
            source: extra_source,
            language: Language::Kotlin,
        });
        let import_started = Instant::now();
        let reimported = await_enrichment(
            &provider,
            request(
                repository.path(),
                RepoRelativePath::new("src/commonMain/kotlin/Main.kt")?,
                changed_source,
                Revision(3),
                &supporting_documents,
            )?,
        );
        assert_eq!(
            reimported.state,
            ProviderState::Ready,
            "{:?}",
            provider.last_error()
        );
        assert_eq!(reimported.revision, Revision(3));
        assert_eq!(reimported.incoming.len(), 6, "{:?}", reimported.incoming);
        assert!(
            reimported
                .incoming
                .iter()
                .any(|relation| relation.name == "customRootCaller")
        );
        assert!(
            reimported
                .incoming
                .iter()
                .all(|relation| relation.provenance == Provenance::KotlinLsp)
        );
        assert!(!reimported.incoming_truncated);
        eprintln!(
            "kotlin_lsp_build_change: elapsed={:?}, incoming={}",
            import_started.elapsed(),
            reimported.incoming.len()
        );
    }
    provider.shutdown()?;
    Ok(())
}
