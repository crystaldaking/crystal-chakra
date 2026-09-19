use super::*;
use chakra_domain::revision::Revision;
use serde_json::json;

#[test]
#[ignore = "requires Gradle, JDK 21 and Kotlin 2.2.20 dependency resolution"]
fn real_gradle_model_preserves_compiler_plugins() -> Result<(), Box<dyn std::error::Error>> {
    let scratch = tempfile::tempdir()?;
    let root = fs::canonicalize(scratch.path())?;
    fs::write(
        root.join("settings.gradle.kts"),
        "rootProject.name = \"compiler-plugins\"\n",
    )?;
    fs::write(
        root.join("build.gradle.kts"),
        r#"
plugins {
    kotlin("multiplatform") version "2.2.20"
    kotlin("plugin.serialization") version "2.2.20"
    kotlin("plugin.allopen") version "2.2.20"
}
repositories { mavenCentral() }
allOpen { annotation("sample.Open") }
kotlin { jvm(); js(IR) { nodejs() }; linuxX64(); jvmToolchain(21) }
"#,
    )?;
    let workspace = ProviderWorkspace::from_documents(root.clone(), Revision(1), vec![]);
    let importer = ProjectImport::default();
    importer.prepare(
        &workspace,
        Instant::now() + Duration::from_secs(180),
        &|| Ok(()),
    )?;
    let model: serde_json::Value = serde_json::from_slice(&fs::read(
        importer
            .initialization_root()
            .ok_or("missing model")?
            .join("workspace.json"),
    )?)?;
    for name in [":/commonMain", ":/jvmMain", ":/jsMain", ":/linuxX64Main"] {
        let facet = model["kotlinSettings"]
            .as_array()
            .ok_or("missing facets")?
            .iter()
            .find(|facet| facet["module"] == name)
            .ok_or("missing facet")?;
        let encoded = facet["compilerArguments"]
            .as_str()
            .ok_or("missing arguments")?;
        let args: serde_json::Value = serde_json::from_str(&encoded[1..])?;
        let classpath = args["pluginClasspaths"]
            .as_array()
            .ok_or("missing plugin classpath")?;
        for artifact in [
            "kotlin-serialization-compiler-plugin",
            "kotlin-allopen-compiler-plugin",
        ] {
            assert!(
                classpath
                    .iter()
                    .any(
                        |entry| entry.as_str().is_some_and(|path| Path::new(path).is_file()
                            && Path::new(path)
                                .file_name()
                                .is_some_and(|name| name.to_string_lossy().starts_with(artifact)))
                    ),
                "{name}: {artifact}"
            );
        }
        assert!(
            args["pluginOptions"]
                .as_array()
                .ok_or("missing plugin options")?
                .contains(&json!(
                    "plugin:org.jetbrains.kotlin.allopen:annotation=sample.Open"
                )),
            "{name}"
        );
    }
    // A JVM-only plugin must not silently disappear from the shared facet.
    let build = root.join("build.gradle.kts");
    fs::write(root.join("extra-plugin.jar"), [])?;
    fs::write(
        &build,
        format!(
            "{}\n{}",
            fs::read_to_string(&build)?,
            r#"
tasks.named<org.jetbrains.kotlin.gradle.tasks.KotlinJvmCompile>("compileKotlinJvm") {
    pluginClasspath.from(files("extra-plugin.jar"))
}
"#
        ),
    )?;
    let changed = ProviderWorkspace::from_documents(root, Revision(2), vec![]);
    assert!(matches!(
        importer.prepare(&changed, Instant::now() + Duration::from_secs(180), &|| Ok(
            ()
        )),
        Err(WorkerError::ProjectImport(_))
    ));
    assert!(importer.initialization_root().is_none());
    Ok(())
}

#[test]
#[ignore = "requires Gradle, JDK 21 and Kotlin 2.2.20 dependency resolution"]
fn real_gradle_model_resolves_multimodule_project_artifacts()
-> Result<(), Box<dyn std::error::Error>> {
    let scratch = tempfile::tempdir()?;
    let root = fs::canonicalize(scratch.path())?;
    fs::write(
        root.join("settings.gradle.kts"),
        "rootProject.name = \"host\"\ninclude(\":library\")\n",
    )?;
    let common_build = r#"
plugins { kotlin("multiplatform") version "2.2.20" }
repositories { mavenCentral() }
kotlin { jvm(); js(IR) { nodejs() }; jvmToolchain(21) }
"#;
    fs::write(
        root.join("build.gradle.kts"),
        format!(
            "{common_build}\nkotlin {{ sourceSets.commonMain.dependencies {{ implementation(project(\":library\")) }} }}\n"
        ),
    )?;
    fs::create_dir(root.join("library"))?;
    fs::write(root.join("library/build.gradle.kts"), common_build)?;
    for (path, source) in [
        (
            "src/commonMain/kotlin/Host.kt",
            "package sample\nfun host() = libraryTarget()\n",
        ),
        (
            "library/src/commonMain/kotlin/Library.kt",
            "package sample\nfun libraryTarget() = 1\n",
        ),
    ] {
        let file = root.join(path);
        fs::create_dir_all(file.parent().ok_or("fixture parent")?)?;
        fs::write(file, source)?;
    }
    let workspace = ProviderWorkspace::from_documents(root, Revision(1), vec![]);
    let importer = ProjectImport::default();
    importer.prepare(
        &workspace,
        Instant::now() + Duration::from_secs(180),
        &|| Ok(()),
    )?;
    assert!(importer.coverage_complete());
    let model: serde_json::Value = serde_json::from_slice(&fs::read(
        importer
            .initialization_root()
            .ok_or("missing model")?
            .join("workspace.json"),
    )?)?;
    let modules = model["modules"].as_array().ok_or("missing modules")?;
    for (source, target) in [
        (":/commonMain", ":library/commonMain"),
        (":/jvmMain", ":library/jvmMain"),
        (":/jsMain", ":library/jsMain"),
    ] {
        let module = modules
            .iter()
            .find(|m| m["name"] == source)
            .ok_or("missing source module")?;
        assert!(modules.iter().any(|m| m["name"] == target));
        assert!(
            module["dependencies"]
                .as_array()
                .ok_or("dependencies")?
                .iter()
                .any(|d| d["type"] == "module" && d["name"] == target),
            "{source} -> {target}: {module}"
        );
    }
    for module in modules {
        for dependency in module["dependencies"].as_array().ok_or("dependencies")? {
            if dependency["type"] == "module" {
                assert_ne!(dependency["name"], module["name"], "self dependency");
                assert!(
                    modules.iter().any(|m| m["name"] == dependency["name"]),
                    "dangling dependency"
                );
            }
        }
    }
    // Included-build root projects have the same ':' project path as the
    // host. They must never be aliased to the host's source-set modules.
    let root = &workspace.repository_root;
    fs::write(
        root.join("settings.gradle.kts"),
        "rootProject.name = \"host\"\nincludeBuild(\"library\")\n",
    )?;
    fs::write(
        root.join("library/settings.gradle.kts"),
        "rootProject.name = \"library\"\n",
    )?;
    fs::write(
        root.join("library/build.gradle.kts"),
        format!("{common_build}\ngroup = \"org.example\"\nversion = \"1.0\"\n"),
    )?;
    fs::write(
        root.join("build.gradle.kts"),
        format!(
            "{common_build}\nkotlin {{ sourceSets.commonMain.dependencies {{ implementation(\"org.example:library:1.0\") }} }}\n"
        ),
    )?;
    let composite = ProviderWorkspace::from_documents(root.clone(), Revision(2), vec![]);
    assert!(matches!(
        importer.prepare(
            &composite,
            Instant::now() + Duration::from_secs(180),
            &|| Ok(())
        ),
        Err(WorkerError::ProjectImport(_))
    ));
    assert!(
        importer.initialization_root().is_none(),
        "failed import must not retain the old model"
    );
    Ok(())
}

#[test]
#[ignore = "requires Gradle, JDK 21 and Kotlin 2.2.20 dependency resolution"]
fn real_gradle_mixed_project_reports_incomplete_model_coverage()
-> Result<(), Box<dyn std::error::Error>> {
    let scratch = tempfile::tempdir()?;
    let root = fs::canonicalize(scratch.path())?;
    fs::write(
        root.join("settings.gradle.kts"),
        "rootProject.name = \"mixed-model\"\ninclude(\":app\")\n",
    )?;
    fs::write(
        root.join("build.gradle.kts"),
        r#"
plugins {
    kotlin("multiplatform") version "2.2.20"
    kotlin("jvm") version "2.2.20" apply false
}
repositories { mavenCentral() }
kotlin { jvm(); js(IR) { nodejs() }; jvmToolchain(21) }
"#,
    )?;
    fs::create_dir(root.join("app"))?;
    fs::write(
        root.join("app/build.gradle.kts"),
        "plugins { kotlin(\"jvm\") }\nrepositories { mavenCentral() }\nkotlin { jvmToolchain(21) }\n",
    )?;
    let workspace = ProviderWorkspace::from_documents(root, Revision(1), vec![]);
    let importer = ProjectImport::default();
    importer.prepare(
        &workspace,
        Instant::now() + Duration::from_secs(180),
        &|| Ok(()),
    )?;
    assert!(importer.initialization_root().is_some());
    assert!(
        !importer.coverage_complete(),
        "omitted JVM module must not look complete"
    );
    Ok(())
}

#[test]
#[ignore = "requires Gradle, JDK 21 and Kotlin 2.2.20 dependency resolution"]
fn real_gradle_model_preserves_effective_compiler_options() -> Result<(), Box<dyn std::error::Error>>
{
    let scratch = tempfile::tempdir()?;
    let root = fs::canonicalize(scratch.path())?;
    fs::write(
        root.join("settings.gradle.kts"),
        "rootProject.name = \"compiler-options\"\n",
    )?;
    fs::write(
        root.join("build.gradle.kts"),
        r#"
import org.jetbrains.kotlin.gradle.dsl.KotlinVersion
plugins { kotlin("multiplatform") version "2.2.20" }
repositories { mavenCentral() }
kotlin {
    jvm { compilerOptions { javaParameters.set(true) } }; js(IR) { nodejs() }; jvmToolchain(21)
    compilerOptions {
        languageVersion.set(KotlinVersion.KOTLIN_2_1)
        progressiveMode.set(true)
        allWarningsAsErrors.set(true)
        optIn.add("sample.Experimental")
        freeCompilerArgs.addAll("-Xcontext-parameters", "-Xannotation-target-all")
    }
    sourceSets.all { languageSettings.optIn("sample.LegacyExperimental") }
}
"#,
    )?;
    let workspace = ProviderWorkspace::from_documents(root, Revision(1), vec![]);
    let importer = ProjectImport::default();
    importer.prepare(
        &workspace,
        Instant::now() + Duration::from_secs(180),
        &|| Ok(()),
    )?;
    let model: serde_json::Value = serde_json::from_slice(&fs::read(
        importer
            .initialization_root()
            .ok_or("missing model")?
            .join("workspace.json"),
    )?)?;
    for name in [":/commonMain", ":/jvmMain", ":/jsMain"] {
        let facet = model["kotlinSettings"]
            .as_array()
            .ok_or("missing facets")?
            .iter()
            .find(|facet| facet["module"] == name)
            .ok_or("missing source-set facet")?;
        let encoded = facet["compilerArguments"]
            .as_str()
            .ok_or("missing compiler arguments")?;
        let args: serde_json::Value = serde_json::from_str(&encoded[1..])?;
        assert_eq!(args["languageVersion"], "2.1", "{name}");
        assert_eq!(
            args["apiVersion"], "2.1",
            "unset API follows configured language: {name}"
        );
        assert_eq!(args["progressiveMode"], true, "{name}");
        assert_eq!(args["allWarningsAsErrors"], true, "{name}");
        if name == ":/jvmMain" {
            assert_eq!(args["javaParameters"], true, "{name}");
        }
        assert_eq!(args["contextParameters"], true, "{name}");
        assert_eq!(args["annotationTargetAll"], true, "{name}");
        assert!(
            args["optIn"]
                .as_array()
                .ok_or("missing opt-ins")?
                .contains(&json!("sample.Experimental")),
            "{name}"
        );
        if name == ":/commonMain" {
            assert!(
                args["optIn"]
                    .as_array()
                    .ok_or("missing opt-ins")?
                    .contains(&json!("sample.LegacyExperimental"))
            );
        }
    }
    Ok(())
}

#[test]
#[ignore = "requires Gradle, JDK 21 and Kotlin 2.2.20 dependency resolution"]
fn real_gradle_model_rejects_invalid_and_conflicting_free_arguments()
-> Result<(), Box<dyn std::error::Error>> {
    let scratch = tempfile::tempdir()?;
    let root = fs::canonicalize(scratch.path())?;
    fs::write(
        root.join("settings.gradle.kts"),
        "rootProject.name = \"compiler-negative\"\n",
    )?;
    fs::write(root.join("compiler-options.txt"), "-Xcontext-parameters\n")?;
    for configuration in [
        "compilerOptions { freeCompilerArgs.add(\"@compiler-options.txt\") }",
        "compilerOptions { freeCompilerArgs.add(\"-Xchakra-unknown-option\") }",
        "jvm { compilerOptions { freeCompilerArgs.add(\"-Xcontext-parameters\") } }",
    ] {
        fs::write(
            root.join("build.gradle.kts"),
            format!(
                r#"
plugins {{ kotlin("multiplatform") version "2.2.20" }}
repositories {{ mavenCentral() }}
kotlin {{ jvm(); js(IR) {{ nodejs() }}; jvmToolchain(21); {configuration} }}
"#,
            ),
        )?;
        let workspace = ProviderWorkspace::from_documents(root.clone(), Revision(1), vec![]);
        let importer = ProjectImport::default();
        assert!(matches!(
            importer.prepare(
                &workspace,
                Instant::now() + Duration::from_secs(180),
                &|| Ok(()),
            ),
            Err(WorkerError::ProjectImport(_))
        ));
        assert!(importer.initialization_root().is_none());
    }
    Ok(())
}

#[test]
fn source_root_resolves_existing_ancestor_without_requiring_generated_directories()
-> Result<(), Box<dyn std::error::Error>> {
    let scratch = tempfile::tempdir()?;
    let generated = scratch.path().join("generated/common/kotlin");
    assert_eq!(
        canonical_source_root(&generated)?,
        fs::canonicalize(scratch.path())?.join("generated/common/kotlin")
    );
    let file = scratch.path().join("not-a-directory");
    fs::write(&file, "")?;
    assert!(canonical_source_root(&file).is_err());
    Ok(())
}

#[cfg(unix)]
#[test]
fn source_root_rejects_external_and_dangling_symlink_ancestors()
-> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::symlink;
    let scratch = tempfile::tempdir()?;
    let external = tempfile::tempdir()?;
    let root = fs::canonicalize(scratch.path())?;
    symlink(external.path(), root.join("linked"))?;
    symlink(root.join("missing"), root.join("dangling"))?;
    assert!(canonical_source_root(&root.join("dangling/generated")).is_err());
    let model = root.join("workspace.json");
    fs::write(
        &model,
        serde_json::to_vec(&json!({"modules":[{"contentRoots":[{
            "sourceRoots":[{"path":root.join("linked/generated")}]
        }]}]}))?,
    )?;
    assert!(matches!(
        validate_model(&model, &root),
        Err(WorkerError::ProjectImport(_))
    ));
    Ok(())
}

#[cfg(windows)]
#[test]
fn java_style_windows_path_matches_verbatim_worktree_root() -> Result<(), Box<dyn std::error::Error>>
{
    let scratch = tempfile::tempdir()?;
    let root = fs::canonicalize(scratch.path())?;
    let plain = root
        .to_str()
        .ok_or("non-Unicode fixture path")?
        .strip_prefix(r"\\?\")
        .ok_or("expected canonical verbatim path")?;
    let model = root.join("workspace.json");
    fs::write(
        &model,
        serde_json::to_vec(&json!({"modules":[{"contentRoots":[{
            "sourceRoots":[{"path":Path::new(plain).join("generated/kotlin")}]
        }]}]}))?,
    )?;
    assert_eq!(
        validate_model(&model, &root)?,
        vec![root.join("generated/kotlin")]
    );
    Ok(())
}

#[test]
fn project_model_rejects_roots_outside_canonical_worktree() -> Result<(), Box<dyn std::error::Error>>
{
    let scratch = tempfile::tempdir()?;
    let root = fs::canonicalize(scratch.path())?;
    let file = root.join("workspace.json");
    for source in [
        root.join("../outside"),
        std::env::temp_dir(),
        PathBuf::from("src"),
    ] {
        fs::write(
            &file,
            serde_json::to_vec(
                &json!({"modules":[{"contentRoots":[{"sourceRoots":[{"path":source}]}]}]}),
            )?,
        )?;
        assert!(matches!(
            validate_model(&file, &root),
            Err(WorkerError::ProjectImport(_))
        ));
    }
    fs::write(
        &file,
        serde_json::to_vec(
            &json!({"modules":[{"contentRoots":[{"sourceRoots":[{"path":root.join("src/commonMain/kotlin")}]}]}]}),
        )?,
    )?;
    validate_model(&file, &root)?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn wrapper_prepares_an_owned_external_model_and_replaces_it_on_restart()
-> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;
    let scratch = tempfile::tempdir()?;
    let root = fs::canonicalize(scratch.path())?;
    let wrapper = root.join("gradlew");
    fs::write(
        &wrapper,
        "#!/bin/sh\nfor arg in \"$@\"; do\ncase \"$arg\" in -Dchakra.kotlin.modelDir=*) model=${arg#*=} ;; esac\ndone\ncp fixture-model.json \"$model/workspace.json\"\n",
    )?;
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700))?;
    fs::write(root.join("build.gradle.kts"), "// fixture")?;
    fs::write(
        root.join("fixture-model.json"),
        serde_json::to_vec(
            &json!({"modules":[{"contentRoots":[{"sourceRoots":[{"path":root.join("src/commonMain/kotlin")}]}]}]}),
        )?,
    )?;
    let input = chakra_engine::ProviderInput::from_metadata(
        chakra_domain::location::RepoRelativePath::new("build.gradle.kts")?,
        [Language::Kotlin],
        &fs::metadata(root.join("build.gradle.kts"))?,
    )
    .ok_or("missing build input")?;
    let workspace = ProviderWorkspace::from_documents_and_inputs(
        root.clone(),
        Revision(1),
        vec![],
        vec![input],
    );
    let importer = ProjectImport::default();
    importer.prepare(&workspace, Instant::now() + Duration::from_secs(5), &|| {
        Ok(())
    })?;
    let first = importer.initialization_root().ok_or("missing model")?;
    assert!(!first.starts_with(&root));
    assert!(first.join("workspace.json").is_file());
    assert!(importer.covers(
        &workspace,
        &chakra_domain::location::RepoRelativePath::new("src/commonMain/kotlin/Main.kt")?
    ));
    assert!(!importer.covers(
        &workspace,
        &chakra_domain::location::RepoRelativePath::new("unconfigured/Other.kt")?
    ));
    importer.prepare(&workspace, Instant::now() + Duration::from_secs(5), &|| {
        Ok(())
    })?;
    let second = importer
        .initialization_root()
        .ok_or("missing replacement")?;
    assert_ne!(first, second);
    assert!(!first.exists());
    fs::write(
        root.join("build.gradle.kts"),
        "// changed after the revision was captured",
    )?;
    assert!(matches!(
        importer.prepare(&workspace, Instant::now() + Duration::from_secs(5), &|| Ok(
            ()
        )),
        Err(WorkerError::InputsChanged)
    ));
    assert!(importer.initialization_root().is_none());
    let current_input = chakra_engine::ProviderInput::from_metadata(
        chakra_domain::location::RepoRelativePath::new("build.gradle.kts")?,
        [Language::Kotlin],
        &fs::metadata(root.join("build.gradle.kts"))?,
    )
    .ok_or("missing current build input")?;
    let current = ProviderWorkspace::from_documents_and_inputs(
        root.clone(),
        Revision(2),
        vec![],
        vec![current_input],
    );
    let script = fs::read_to_string(&wrapper)?.replace(
        "cp fixture-model.json",
        "printf '// mutated during import' > build.gradle.kts\ncp fixture-model.json",
    );
    fs::write(&wrapper, script)?;
    assert!(matches!(
        importer.prepare(
            &current,
            Instant::now() + Duration::from_secs(5),
            &|| Ok(())
        ),
        Err(WorkerError::InputsChanged)
    ));
    assert!(importer.initialization_root().is_none());
    drop(importer);
    assert!(!second.exists());
    Ok(())
}

#[cfg(unix)]
#[test]
fn import_cancellation_reaps_the_owned_process_group() -> Result<(), Box<dyn std::error::Error>> {
    let scratch = tempfile::tempdir()?;
    let pid_file = scratch.path().join("child.pid");
    let mut command = Command::new("sh");
    command
        .current_dir(scratch.path())
        .args(["-c", "sleep 30 & echo $! > child.pid; wait"]);
    let started = Instant::now();
    let result = run_owned(
        &mut command,
        &scratch.path().join("log"),
        started + Duration::from_secs(5),
        &|| {
            if pid_file.exists() {
                Err(WorkerError::Cancelled)
            } else {
                Ok(())
            }
        },
    );
    assert!(matches!(result, Err(WorkerError::Cancelled)));
    assert!(started.elapsed() < Duration::from_secs(5));
    let pid = fs::read_to_string(pid_file)?.trim().parse::<i32>()?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let output = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "stat="])
            .output()?;
        let state = String::from_utf8_lossy(&output.stdout);
        if state.trim().is_empty() || state.trim().starts_with('Z') {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "owned child {pid} remains running: {state}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn deadline_and_failed_gradle_do_not_expose_raw_build_output()
-> Result<(), Box<dyn std::error::Error>> {
    let scratch = tempfile::tempdir()?;
    let mut command = Command::new("sh");
    command.args(["-c", "sleep 30"]);
    let result = run_owned(
        &mut command,
        &scratch.path().join("deadline.log"),
        Instant::now() + Duration::from_millis(50),
        &|| Ok(()),
    );
    assert!(matches!(result, Err(WorkerError::Timeout)));
    let mut command = Command::new("sh");
    command.args(["-c", "echo private-source-content >&2; exit 7"]);
    let result = run_owned(
        &mut command,
        &scratch.path().join("failure.log"),
        Instant::now() + Duration::from_secs(5),
        &|| Ok(()),
    );
    let Err(error) = result else {
        return Err("failed build was accepted".into());
    };
    assert!(matches!(error, WorkerError::ProjectImport(_)));
    assert!(!error.to_string().contains("private-source-content"));
    Ok(())
}
