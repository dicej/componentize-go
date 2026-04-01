#[cfg(test)]
mod tests {
    use anyhow::{Result, anyhow};
    use core::panic;
    use once_cell::sync::Lazy;
    use std::{
        env,
        net::TcpListener,
        path::{Path, PathBuf},
        process::{Child, Command, Stdio},
        time::Duration,
    };

    static COMPONENTIZE_GO_PATH: once_cell::sync::Lazy<PathBuf> = Lazy::new(|| {
        let test_manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let root_manifest = test_manifest.parent().unwrap();
        let build_output = Command::new("cargo")
            .arg("build")
            .args([
                "--manifest-path",
                root_manifest.join("Cargo.toml").to_str().unwrap(),
            ])
            .output()
            .expect("failed to build componentize-go");

        if !build_output.status.success() {
            panic!("{}", String::from_utf8_lossy(&build_output.stderr));
        }

        root_manifest.join("target/debug/componentize-go")
    });

    // TODO: Once the patch is merged in Big Go, this needs to be removed.
    static PATCHED_GO_PATH: tokio::sync::OnceCell<PathBuf> = tokio::sync::OnceCell::const_new();

    async fn patched_go_path() -> &'static PathBuf {
        PATCHED_GO_PATH
            .get_or_init(|| async {
                let test_manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
                let root_manifest = test_manifest.parent().unwrap();

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

                let go_dir = format!("go-{os}-{arch}-bootstrap");
                let go_path = root_manifest.join(&go_dir);
                let go_bin = go_path.join("bin").join("go");

                // Skip if already installed
                if go_bin.exists() {
                    return go_bin;
                }

                // Download the patched Go toolchain
                let archive_name = format!("{go_dir}.tbz");
                let archive_path = root_manifest.join(&archive_name);
                let download_url = format!(
                    "https://github.com/dicej/go/releases/download/go1.25.5-wasi-on-idle/{archive_name}"
                );

                println!("Downloading patched Go from {download_url}");
                let bytes = reqwest::get(&download_url)
                    .await
                    .expect("Failed to download patched Go")
                    .bytes()
                    .await
                    .expect("Failed to read download");

                std::fs::write(&archive_path, &bytes).expect("Failed to write archive");

                // Extract the archive
                println!("Extracting {} to {}", archive_name, root_manifest.display());
                let tar_file =
                    std::fs::File::open(&archive_path).expect("Failed to open archive");
                let tar_decoder = bzip2::read::BzDecoder::new(tar_file);
                let mut archive = tar::Archive::new(tar_decoder);
                archive
                    .unpack(root_manifest)
                    .expect("Failed to extract archive");

                // Clean up archive
                std::fs::remove_file(&archive_path).ok();

                go_bin
            })
            .await
    }

    struct App {
        /// The path to the example application
        path: PathBuf,
        /// The WIT worlds to target
        worlds: Vec<String>,
        /// The output path of the wasm file
        wasm_path: String,
        /// The paths to the directories containing the WIT files
        wit_paths: Vec<PathBuf>,
        /// The child process ID of a running wasm app
        process: Option<Child>,
        /// Any tests that need to be compiled and run as such
        tests: Option<Vec<Test>>,
    }

    #[derive(Clone)]
    struct Test {
        should_fail: bool,
        pkg_path: String,
    }

    impl App {
        /// Create a new app runner.
        fn new(
            path: &Path,
            wit_paths: &[&Path],
            worlds: &[&str],
            tests: Option<Vec<Test>>,
            is_component: bool,
        ) -> Self {
            let path = &componentize_go::utils::make_path_absolute(&PathBuf::from(path))
                .expect("failed to make app path absolute");

            if is_component {
                assert!(!wit_paths.is_empty());
                assert!(!worlds.is_empty());
            }

            App {
                path: path.into(),
                worlds: worlds.iter().map(|v| v.to_string()).collect(),
                wasm_path: path
                    .join("main.wasm")
                    .to_str()
                    .expect("wasm_path is not valid unicode")
                    .to_string(),
                wit_paths: wit_paths.iter().map(|v| v.into()).collect(),
                process: None,
                tests,
            }
        }

        fn build_test_modules(&self, go: Option<&PathBuf>) -> Result<()> {
            let test_pkgs = self.tests.as_ref().expect("missing test_pkg_paths");

            self.generate_bindings(go)?;

            let mut test_cmd = Command::new(COMPONENTIZE_GO_PATH.as_path());
            for world in &self.worlds {
                test_cmd.args(["-w", world]);
            }
            for path in &self.wit_paths {
                test_cmd.arg("-d").arg(path);
            }
            test_cmd.arg("test").arg("--wasip1");

            // Add all the paths to the packages that have unit tests to compile
            for test in test_pkgs.iter() {
                test_cmd.args(["--pkg", &test.pkg_path]);
            }

            // `go test -c` needs to be in the same path as the go.mod file.
            test_cmd.current_dir(&self.path);

            let test_output = test_cmd.output().expect(&format!(
                "failed to execute componentize-go for \"{}\"",
                self.path.display()
            ));

            if !test_output.status.success() {
                return Err(anyhow!(
                    "failed to build application \"{}\": {}",
                    self.path.display(),
                    String::from_utf8_lossy(&test_output.stderr)
                ));
            }

            Ok(())
        }

        fn run_module(&self) -> Result<()> {
            let output = Command::new("wasmtime")
                .arg("run")
                .arg(&self.wasm_path)
                .output()?;

            if !output.status.success() {
                return Err(anyhow!(
                    "Failed to run wasm module for application at '{}':\n{} ",
                    &self.wasm_path,
                    String::from_utf8_lossy(&output.stdout)
                ));
            }

            Ok(())
        }

        fn run_test_modules(&self) -> Result<()> {
            let example_dir = PathBuf::from(&self.path);
            if let Some(tests) = &self.tests {
                let mut test_errors: Vec<String> = vec![];
                for test in tests.iter() {
                    let wasm_file = example_dir.join(componentize_go::cmd_test::get_test_filename(
                        &PathBuf::from(&test.pkg_path),
                    ));
                    match Command::new("wasmtime")
                        .args(["run", wasm_file.to_str().unwrap()])
                        .output()
                    {
                        Ok(output) => {
                            let succeeded = output.status.success();
                            if test.should_fail && succeeded {
                                test_errors.push(format!(
                                    "The '{}' tests should have failed",
                                    test.pkg_path
                                ));
                            } else if !test.should_fail && !succeeded {
                                test_errors.push(format!("The '{}' tests should have passed, but failed with the following output:\n\n{}", test.pkg_path, String::from_utf8_lossy(&output.stdout)));
                            }
                        }
                        Err(e) => {
                            test_errors.push(format!(
                                "Failed to run wasmtime for '{}': {}",
                                test.pkg_path, e
                            ));
                        }
                    }
                }

                if !test_errors.is_empty() {
                    let err_msg = format!(
                        "{}{}{}",
                        "\n====================\n",
                        &test_errors.join("\n\n====================\n"),
                        "\n\n====================\n"
                    );
                    return Err(anyhow!(err_msg));
                }
            } else {
                return Err(anyhow!(
                    "Please include the test_pkg_paths when creating App::new()"
                ));
            }

            Ok(())
        }

        fn build_module(&self, go: Option<&PathBuf>) -> Result<()> {
            // Build component
            let mut build_cmd = Command::new(COMPONENTIZE_GO_PATH.as_path());
            build_cmd
                .arg("build")
                .arg("--wasip1")
                .args(["-o", &self.wasm_path]);

            if let Some(go_path) = go.as_ref() {
                build_cmd.args(["--go", go_path.to_str().unwrap()]);
            }

            // Run `go build` in the same directory as the go.mod file.
            build_cmd.current_dir(&self.path);

            let build_output = build_cmd.output().expect(&format!(
                "failed to execute componentize-go for \"{}\"",
                self.path.display()
            ));

            if !build_output.status.success() {
                return Err(anyhow!(
                    "failed to build application \"{}\": {}",
                    self.path.display(),
                    String::from_utf8_lossy(&build_output.stderr)
                ));
            }

            Ok(())
        }

        fn build_component(&self, go: Option<&PathBuf>) -> Result<()> {
            self.generate_bindings(go)?;

            // Build component
            let mut build_cmd = Command::new(COMPONENTIZE_GO_PATH.as_path());
            for world in &self.worlds {
                build_cmd.args(["-w", world]);
            }
            for path in &self.wit_paths {
                build_cmd.arg("-d").arg(path);
            }
            build_cmd.arg("build").args(["-o", &self.wasm_path]);

            if let Some(go_path) = go.as_ref() {
                build_cmd.args(["--go", go_path.to_str().unwrap()]);
            }

            // Run `go build` in the same directory as the go.mod file.
            build_cmd.current_dir(&self.path);

            let build_output = build_cmd.output().expect(&format!(
                "failed to execute componentize-go for \"{}\"",
                self.path.display()
            ));

            if !build_output.status.success() {
                return Err(anyhow!(
                    "failed to build application \"{}\": {}",
                    self.path.display(),
                    String::from_utf8_lossy(&build_output.stderr)
                ));
            }

            Ok(())
        }

        fn generate_bindings(&self, go: Option<&PathBuf>) -> Result<()> {
            let mut cmd = Command::new(COMPONENTIZE_GO_PATH.as_path());
            for world in &self.worlds {
                cmd.args(["-w", world]);
            }
            for path in &self.wit_paths {
                cmd.arg("-d").arg(path);
            }

            let bindings_output = cmd
                .arg("bindings")
                .arg("-o")
                .arg(&self.path)
                .current_dir(&self.path)
                .output()
                .expect(&format!(
                    "failed to generate bindings for application \"{}\"",
                    self.path.display()
                ));
            if !bindings_output.status.success() {
                return Err(anyhow!(
                    "{}",
                    String::from_utf8_lossy(&bindings_output.stderr)
                ));
            }

            // Tidy Go mod
            let tidy_output = Command::new(if let Some(path) = go.as_ref() {
                String::from(path.to_str().unwrap())
            } else {
                // Default to PATH
                "go".to_string()
            })
            .arg("mod")
            .arg("tidy")
            .current_dir(&self.path)
            .output()
            .expect("failed to tidy Go mod");
            if !tidy_output.status.success() {
                return Err(anyhow!("{}", String::from_utf8_lossy(&tidy_output.stderr)));
            }

            Ok(())
        }

        /// Run the app and check the output.
        async fn run_component(&mut self, route: &str, expected_response: &str) -> Result<()> {
            let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind to a free port");
            let addr = listener.local_addr().expect("Failed to get local address");
            let port = addr.port();
            drop(listener);

            let child = Command::new("wasmtime")
                .arg("serve")
                .args(["--addr", &format!("0.0.0.0:{port}")])
                .arg("-Sp3,cli")
                .arg("-Wcomponent-model-async")
                .arg(&self.wasm_path)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("Failed to start wasmtime serve");

            // Storing for cleanup on drop.
            self.process = Some(child);

            let start = std::time::Instant::now();
            loop {
                match reqwest::get(format!("http://localhost:{port}{route}")).await {
                    Ok(r) => {
                        let actual = r.text().await.expect("Failed to read response");
                        assert_eq!(&actual, expected_response);
                        return Ok(());
                    }
                    Err(e) => {
                        if start.elapsed() > Duration::from_secs(5) {
                            return Err(anyhow!("Unable to reach the app: {e}"));
                        }
                    }
                }
            }
        }
    }

    impl Drop for App {
        fn drop(&mut self) {
            if let Some(child) = &mut self.process {
                _ = child.kill()
            }
        }
    }

    #[test]
    fn example_wasip1() {
        let app = App::new(Path::new("../examples/wasip1"), &[], &[], None, false);
        app.build_module(None).expect("failed to build app module");
        app.run_module().expect("failed to run app module");
    }

    #[tokio::test]
    async fn example_wasip2() {
        let unit_tests = vec![
            Test {
                should_fail: false,
                pkg_path: String::from("./unit_tests_should_pass"),
            },
            Test {
                should_fail: true,
                pkg_path: String::from("./unit_tests_should_fail"),
            },
        ];

        let cwd = env::current_dir().unwrap();
        let app_dir = cwd.parent().unwrap().join("examples").join("wasip2");
        let mut app = App::new(
            &app_dir,
            &[&app_dir.join("wit")],
            &["wasip2-example"],
            Some(unit_tests),
            true,
        );

        app.build_component(None).expect("failed to build app");

        app.run_component("/", "Hello, world!")
            .await
            .expect("app failed to run");

        app.build_test_modules(None)
            .expect("failed to build app unit tests");

        app.run_test_modules()
            .expect("tests succeeded/failed when they should not have");
    }

    #[tokio::test]
    async fn example_wasip3() {
        let cwd = env::current_dir().unwrap();
        let app_dir = cwd.parent().unwrap().join("examples").join("wasip3");
        let mut app = App::new(
            &app_dir,
            &[&app_dir.join("wit")],
            &["wasip3-example"],
            None,
            true,
        );
        app.build_component(Some(patched_go_path().await))
            .expect("failed to build app");
        app.run_component("/hello", "Hello, world!")
            .await
            .expect("app failed to run");
    }

    #[tokio::test]
    async fn example_multiple_worlds() -> Result<()> {
        let cwd = env::current_dir()?;
        let app_dir = cwd
            .parent()
            .unwrap()
            .join("examples")
            .join("multiple-worlds");
        let mut app = App::new(
            &app_dir,
            &[&app_dir.join("wit1"), &app_dir.join("wit2")],
            &["world1", "world2"],
            None,
            true,
        );
        app.build_component(Some(patched_go_path().await))
            .expect("failed to build app");
        app.run_component("/hello", "Hello, world!")
            .await
            .expect("app failed to run");
        Ok(())
    }
}
