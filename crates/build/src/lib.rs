use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use ambient_asset_cache::{AssetCache, SyncAssetKeyExt};
use ambient_native_std::{asset_url::AbsAssetUrl, git_revision_full};
use ambient_project::{Manifest as ProjectManifest, Version};
use ambient_project_semantic::{BuildMetadata, ItemId, Scope, Semantic};
use ambient_std::path::path_to_unix_string;
use anyhow::Context;
use futures::FutureExt;
use itertools::Itertools;
use pipelines::{FileCollection, ProcessCtx, ProcessCtxKey};
use walkdir::WalkDir;

pub mod migrate;
pub mod pipelines;

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct Metadata {
    ambient_version: Version,
    ambient_revision: String,
    client_component_paths: Vec<String>,
    server_component_paths: Vec<String>,
}

impl Metadata {
    pub fn component_paths(&self, target: &str) -> &[String] {
        match target {
            "client" => &self.client_component_paths,
            "server" => &self.server_component_paths,
            _ => panic!("Unknown target `{}`", target),
        }
    }

    pub fn parse(contents: &str) -> anyhow::Result<Self> {
        toml::from_str(contents).context("failed to parse build metadata")
    }
}

pub struct BuildConfiguration<'a> {
    pub build_path: PathBuf,
    pub assets: AssetCache,
    pub semantic: &'a mut Semantic,
    pub optimize: bool,
    pub clean_build: bool,
    pub build_wasm_only: bool,
}

pub async fn build(
    config: BuildConfiguration<'_>,
    root_ember_id: ItemId<Scope>,
) -> anyhow::Result<()> {
    let BuildConfiguration {
        build_path,
        assets,
        semantic,
        optimize,
        clean_build,
        build_wasm_only,
    } = config;

    if clean_build {
        tracing::info!("Removing build directory: {build_path:?}");
        match tokio::fs::remove_dir_all(&build_path).await {
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).context("Failed to remove build directory");
            }
        }
    }

    let mut queue = semantic.items.scope_and_dependencies(root_ember_id);
    while let Some(ember_id) = queue.pop() {
        let id = semantic.items.get(ember_id)?.original_id.clone();
        build_ember(
            BuildConfiguration {
                build_path: build_path.join(id.as_str()),
                assets: assets.clone(),
                semantic,
                optimize,
                clean_build,
                build_wasm_only,
            },
            ember_id,
        )
        .await?;
    }

    Ok(())
}

/// This takes the path to an Ambient ember and builds it. An Ambient ember is expected to
/// have the following structure:
///
/// assets/**  Here assets such as .glb files are stored. Any files found in this directory will be processed
/// src/**  This is where you store Rust source files
/// build  This is the output directory, and is created when building
/// ambient.toml  This is a metadata file to describe the ember
async fn build_ember(
    config: BuildConfiguration<'_>,
    ember_id: ItemId<Scope>,
) -> anyhow::Result<()> {
    let BuildConfiguration {
        build_path,
        assets,
        semantic,
        optimize,
        build_wasm_only,
        ..
    } = config;

    let (manifest, path) = {
        let scope = semantic.items.get(ember_id)?;
        (
            scope
                .manifest
                .clone()
                .context("the ember has no manifest")?,
            scope
                .manifest_path
                .as_ref()
                .context("the ember has no path")?
                .parent()
                .context("the ember path has no parent")?
                .to_owned(),
        )
    };

    let name = manifest
        .ember
        .name
        .as_deref()
        .unwrap_or_else(|| manifest.ember.id.as_str());

    tracing::info!("Building project `{}` ({})", manifest.ember.id, name);

    let assets_path = path.join("assets");
    tokio::fs::create_dir_all(&build_path)
        .await
        .context("Failed to create build directory")?;

    if !build_wasm_only {
        build_assets(&assets, &assets_path, &build_path).await?;
    }

    build_rust_if_available(&path, &manifest, &build_path, optimize)
        .await
        .with_context(|| format!("Failed to build rust {build_path:?}"))?;

    store_manifest(&manifest, &build_path).await?;
    let metadata = store_metadata(&build_path).await?;

    semantic.items.get_mut(ember_id)?.build_metadata = Some(BuildMetadata(Arc::new(metadata)));

    Ok(())
}

fn get_asset_files(assets_path: &Path) -> impl Iterator<Item = PathBuf> {
    WalkDir::new(assets_path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.metadata().map(|x| x.is_file()).unwrap_or(false))
        .map(|x| x.into_path())
}

async fn build_assets(
    assets: &AssetCache,
    assets_path: &Path,
    build_path: &Path,
) -> anyhow::Result<()> {
    let files = get_asset_files(assets_path).map(Into::into).collect_vec();

    let has_errored = Arc::new(AtomicBool::new(false));

    let ctx = ProcessCtx {
        assets: assets.clone(),
        files: FileCollection(Arc::new(files)),
        in_root: AbsAssetUrl::from_directory_path(assets_path),
        out_root: AbsAssetUrl::from_directory_path(build_path.join("assets")),
        input_file_filter: None,
        package_name: "".to_string(),
        write_file: Arc::new({
            let build_path = build_path.to_owned();
            move |path, contents| {
                let path = build_path.join("assets").join(path);
                async move {
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    tokio::fs::write(&path, contents).await.unwrap();
                    AbsAssetUrl::from_file_path(path)
                }
                .boxed()
            }
        }),
        on_status: Arc::new(|msg| {
            log::info!("{}", msg);
            async {}.boxed()
        }),
        on_error: Arc::new({
            let has_errored = has_errored.clone();
            move |err| {
                log::error!("{:?}", err);
                has_errored.store(true, Ordering::SeqCst);
                async {}.boxed()
            }
        }),
    };

    ProcessCtxKey.insert(&ctx.assets, ctx.clone());

    pipelines::process_pipelines(&ctx)
        .await
        .with_context(|| format!("Failed to process pipelines for {assets_path:?}"))?;

    if has_errored.load(Ordering::SeqCst) {
        anyhow::bail!("Failed to build assets");
    }

    Ok(())
}

pub async fn build_rust_if_available(
    project_path: &Path,
    manifest: &ProjectManifest,
    build_path: &Path,
    optimize: bool,
) -> anyhow::Result<()> {
    let cargo_toml_path = project_path.join("Cargo.toml");
    if !cargo_toml_path.exists() {
        return Ok(());
    }

    let rustc = ambient_rustc::Rust::get_system_installation().await?;

    for feature in &manifest.build.rust.feature_multibuild {
        for (path, bytecode) in rustc.build(project_path, optimize, &[feature])? {
            let component_bytecode = ambient_wasm::shared::build::componentize(&bytecode)?;

            let output_path = build_path.join(feature);
            std::fs::create_dir_all(&output_path)?;

            let filename = path.file_name().context("no filename")?;
            tokio::fs::write(output_path.join(filename), component_bytecode).await?;
        }
    }

    Ok(())
}

fn get_component_paths(target: &str, build_path: &Path) -> Vec<String> {
    std::fs::read_dir(build_path.join(target))
        .ok()
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|p| p.path())
                .filter(|p| p.extension().unwrap_or_default() == "wasm")
                .map(|p| path_to_unix_string(p.strip_prefix(build_path).unwrap()))
                .collect()
        })
        .unwrap_or_default()
}

async fn store_manifest(manifest: &ProjectManifest, build_path: &Path) -> anyhow::Result<()> {
    let manifest_path = build_path.join("ambient.toml");
    tokio::fs::write(&manifest_path, toml::to_string(&manifest)?).await?;
    Ok(())
}

async fn store_metadata(build_path: &Path) -> anyhow::Result<Metadata> {
    let metadata = Metadata {
        ambient_version: Version::new_from_str(env!("CARGO_PKG_VERSION"))
            .expect("Failed to parse CARGO_PKG_VERSION"),
        ambient_revision: git_revision_full().unwrap_or_default(),
        client_component_paths: get_component_paths("client", build_path),
        server_component_paths: get_component_paths("server", build_path),
    };
    let metadata_path = build_path.join("metadata.toml");
    tokio::fs::write(&metadata_path, toml::to_string(&metadata)?).await?;
    Ok(metadata)
}
