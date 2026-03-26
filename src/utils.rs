use anyhow::{Context, Result, anyhow, bail};
use bzip2::read::BzDecoder;
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::Cursor,
    path::{Path, PathBuf},
    process::Command,
};
use tar::Archive;
use wit_parser::{CloneMaps, Package, PackageId, PackageName, Resolve, Stability, World, WorldId};

pub fn parse_wit(
    paths: &[impl AsRef<Path>],
    worlds: &[String],
    ignore_toml_files: bool,
    features: &[String],
    all_features: bool,
) -> Result<(Resolve, WorldId, bool)> {
    let &(ref paths, ref worlds, need_async) =
        &maybe_add_dependencies(paths, worlds, ignore_toml_files)?;

    // If no WIT directory was provided as a parameter and none were referenced
    // by Go packages, use ./wit by default.
    if paths.is_empty() {
        let paths = &[Path::new("wit")];
        return parse_wit(paths, worlds, ignore_toml_files, features, all_features);
    }
    debug_assert!(!paths.is_empty(), "The paths should not be empty");

    let mut resolve = Resolve {
        all_features,
        ..Default::default()
    };
    for features in features {
        for feature in features
            .split(',')
            .flat_map(|s| s.split_whitespace())
            .filter(|f| !f.is_empty())
        {
            resolve.features.insert(feature.to_string());
        }
    }

    let mut main_packages: Vec<PackageId> = vec![];
    for path in paths.iter() {
        let (pkg, _files) = resolve.push_path(path)?;
        main_packages.push(pkg);
    }

    let world = match &worlds[..] {
        [] => resolve.select_world(&main_packages, None)?,
        [world] => resolve.select_world(&main_packages, Some(world))?,
        worlds => {
            let worlds = worlds
                .iter()
                .map(|world| resolve.select_world(&main_packages, Some(world)))
                .collect::<Result<Vec<_>>>()?;

            let union_package = resolve.packages.alloc(Package {
                name: PackageName {
                    namespace: "componentize-go".into(),
                    name: "union".into(),
                    version: None,
                },
                docs: Default::default(),
                interfaces: Default::default(),
                worlds: Default::default(),
            });

            let union_world = resolve.worlds.alloc(World {
                name: "union".into(),
                imports: Default::default(),
                exports: Default::default(),
                package: Some(union_package),
                docs: Default::default(),
                stability: Stability::Unknown,
                includes: Default::default(),
                span: Default::default(),
            });

            resolve.packages[union_package]
                .worlds
                .insert("union".into(), union_world);

            for &world in &worlds {
                resolve.merge_worlds(world, union_world, &mut CloneMaps::default())?;
            }

            union_world
        }
    };

    Ok((resolve, world, need_async))
}

/// Unless `ignore_toml_files` is `true`, use `go list` to search the current
/// module and its dependencies for any `componentize-go.toml` files.  The WIT
/// path and/or world specified in each such file will be added to the
/// respective list and returned.
fn maybe_add_dependencies(
    paths: &[impl AsRef<Path>],
    worlds: &[String],
    ignore_toml_files: bool,
) -> Result<(Vec<PathBuf>, Vec<String>, bool)> {
    let mut paths = paths
        .iter()
        .map(|v| PathBuf::from(v.as_ref()))
        .collect::<BTreeSet<_>>();
    let mut worlds = worlds.iter().cloned().collect::<BTreeSet<_>>();
    // Only add worlds from `componentize-go.toml` files if none were specified
    // explicitly via the CLI:
    let add_worlds = worlds.is_empty();
    let mut need_async = false;

    if !ignore_toml_files && Path::new("go.mod").exists() {
        let mut command = std::process::Command::new("go");
        let output = command
            .args(["list", "-mod=readonly", "-m", "-f", "{{.Dir}}", "all"])
            .output()?;
        if !output.status.success() {
            bail!(
                "`go list` failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        #[derive(Deserialize)]
        struct ComponentizeGoConfig {
            #[serde(default)]
            worlds: Vec<String>,
            #[serde(default)]
            wit_paths: Vec<String>,
            #[serde(default)]
            need_async: bool,
        }

        for module in String::from_utf8(output.stdout)?.lines() {
            let module = PathBuf::from(module);
            if let Ok(manifest) = fs::read_to_string(module.join("componentize-go.toml")) {
                let config = toml::from_str::<ComponentizeGoConfig>(&manifest)?;
                if add_worlds {
                    worlds.extend(config.worlds);
                }
                paths.extend(config.wit_paths.into_iter().map(|v| module.join(v)));
                need_async |= config.need_async;
            }
        }
    }

    Ok((
        paths.into_iter().collect(),
        worlds.into_iter().collect(),
        need_async,
    ))
}

// Converts a relative path to an absolute path.
pub fn make_path_absolute(p: &Path) -> Result<PathBuf> {
    if p.is_relative() {
        Ok(std::env::current_dir()?.join(p))
    } else {
        Ok(p.to_owned())
    }
}

pub fn embed_wit(wasm_file: &Path, resolve: &Resolve, world: WorldId) -> Result<()> {
    let mut wasm = fs::read(wasm_file)?;
    wit_component::embed_component_metadata(
        &mut wasm,
        resolve,
        world,
        wit_component::StringEncoding::UTF8,
    )?;
    fs::write(wasm_file, wasm).context(format!("failed to write '{}'", wasm_file.display()))?;
    Ok(())
}

/// Update the wasm module to use the current component model ABI.
pub fn module_to_component(wasm_file: &Path) -> Result<()> {
    // In the rare case the snapshot needs to be updated, the latest version
    // can be found here: https://github.com/bytecodealliance/wasmtime/releases
    const WASIP1_SNAPSHOT: &[u8] = include_bytes!("wasi_snapshot_preview1.reactor.wasm");
    let wasm: Vec<u8> = fs::read(wasm_file)?;

    let mut encoder = wit_component::ComponentEncoder::default().validate(true);
    encoder = encoder.module(&wasm)?;
    encoder = encoder.adapter("wasi_snapshot_preview1", WASIP1_SNAPSHOT)?;

    let bytes = encoder
        .encode()
        .context("failed to encode component from module")?;

    fs::write(wasm_file, bytes).context(format!("failed to write `{}`", wasm_file.display()))?;

    Ok(())
}

/// Ensure that the Go version is compatible with the embedded Wasm tooling.
pub fn check_go_version(go_path: &Path) -> Result<()> {
    let output = Command::new(go_path).arg("version").output()?;

    if !output.status.success() {
        return Err(anyhow!(
            "'go version' command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let version_string = String::from_utf8(output.stdout)?;
    let version_regex = regex::Regex::new(r"go(\d+)\.(\d+)\.(\d+)").unwrap();
    let semver = version_regex.captures(&version_string).map(|caps| {
        (
            caps[1].parse::<u32>().unwrap(), // Major
            caps[2].parse::<u32>().unwrap(), // Minor
            caps[3].parse::<u32>().unwrap(), // Patch
        )
    });

    if let Some((major, minor, patch)) = semver {
        // TODO: there might be a patch number correlated with wasip3.
        if major == 1 && minor >= 25 {
            Ok(())
        } else {
            Err(anyhow!(
                "Go version is not valid. Expected '^1.25.0', found '{}.{}.{}'",
                major,
                minor,
                patch
            ))
        }
    } else {
        Err(anyhow!(
            "Failed to parse Go version from: {}",
            version_string
        ))
    }
}

fn check_go_async_support(go: &Path) -> Option<()> {
    fs::read_to_string(go.parent()?.parent()?)
        .ok()?
        .contains("wasiOnIdle")
        .then_some(())
}

pub fn pick_go(go_path: Option<&Path>, need_async: bool) -> Result<PathBuf> {
    let go = match go_path {
        Some(p) => Some(make_path_absolute(p)?),
        None => which::which("go").ok(),
    };

    match go {
        Some(go) if need_async && check_go_async_support(&go).is_none() => {
            eprintln!(
                "Note: {} does not support async operation; will use downloaded version.\n\
                 See https://github.com/golang/go/pull/76775 for details",
                go.display()
            );
        }
        Some(go) if check_go_version(&go).is_err() => {
            eprintln!(
                "Note: {} is not a compatible version of Go; will use downloaded version.",
                go.display()
            );
        }
        Some(go) => return Ok(go),
        None => {
            eprintln!("Note: `go` command not found; will use downloaded version.");
        }
    }

    let Some(cache_dir) = dirs::cache_dir() else {
        bail!("unable to determine cache directory for current user");
    };

    // Determine OS and architecture
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        "windows" => "windows",
        bad_os => panic!("OS not supported: {bad_os}"),
    };

    // Map to Go's naming conventions
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        bad_arch => panic!("ARCH not supported: {bad_arch}"),
    };

    let cache_dir = &cache_dir.join("componentize-go");
    let name = &format!("go-{os}-{arch}-bootstrap");
    let dir = cache_dir.join(name);
    let bin = dir.join("bin").join("go");

    fs::create_dir_all(cache_dir)?;

    // Grab a lock to avoid concurrent downloads
    let lock_file = File::create(cache_dir.join("lock"))?;
    lock_file.lock()?;

    if !bin.exists() {
        let url = format!(
            "https://github.com/dicej/go/releases/download/go1.25.5-wasi-on-idle/{name}.tbz"
        );

        eprintln!("Downloading patched Go from {url}");

        Archive::new(BzDecoder::new(Cursor::new(
            reqwest::blocking::get(&url)?.error_for_status()?.bytes()?,
        )))
        .unpack(cache_dir)?;
    }

    eprintln!("using {}", bin.display());

    Ok(bin)
}
