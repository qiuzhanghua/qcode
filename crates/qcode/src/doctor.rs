//! `qc doctor`: answer "why doesn't it run?" in one command.

use std::path::PathBuf;
use std::process::Command;

use crate::config::{self, Config};

pub fn run(
    data_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
) -> Result<bool, Box<dyn std::error::Error>> {
    let mut doctor = Doctor { failures: 0 };

    println!("qcode {}", env!("CARGO_PKG_VERSION"));
    println!();

    doctor.check_data_dir(&data_dir);
    let config = doctor.check_config(&config_path);
    doctor.check_provider(&config);
    doctor.check_toolchain();
    doctor.check_sandbox();

    println!();
    if doctor.failures == 0 {
        println!("all checks passed");
    } else {
        println!("{} check(s) failed", doctor.failures);
    }

    Ok(doctor.failures == 0)
}

struct Doctor {
    failures: usize,
}

impl Doctor {
    fn ok(&self, label: &str, detail: impl AsRef<str>) {
        println!("[ok]   {label}: {}", detail.as_ref());
    }

    fn warn(&self, label: &str, detail: impl AsRef<str>) {
        println!("[warn] {label}: {}", detail.as_ref());
    }

    fn fail(&mut self, label: &str, detail: impl AsRef<str>) {
        self.failures += 1;
        println!("[fail] {label}: {}", detail.as_ref());
    }

    fn check_data_dir(&mut self, explicit: &Option<PathBuf>) {
        let dir = crate::resolve_data_dir(explicit.clone());
        let result = std::fs::create_dir_all(&dir).and_then(|()| {
            let probe = dir.join(".qcode-doctor-write");
            std::fs::write(&probe, b"ok")?;
            std::fs::remove_file(&probe)
        });

        match result {
            Ok(()) => self.ok("data dir", dir.display().to_string()),
            Err(err) => self.fail("data dir", format!("{}: {err}", dir.display())),
        }
    }

    fn check_config(&mut self, explicit: &Option<PathBuf>) -> Config {
        let explicit = explicit
            .clone()
            .or_else(|| std::env::var("QCODE_CONFIG").ok().map(PathBuf::from));

        let path = match &explicit {
            Some(path) => path.clone(),
            None => match config::default_path() {
                Ok(path) => path,
                Err(err) => {
                    self.fail("config", err.to_string());
                    return Config::default();
                }
            },
        };

        if !path.exists() {
            if explicit.is_some() {
                self.fail("config", format!("not found: {}", path.display()));
            } else {
                self.warn(
                    "config",
                    format!("not found: {} (using defaults)", path.display()),
                );
            }
            return Config::default();
        }

        match config::load(&path) {
            Ok(config) => {
                self.ok("config", path.display().to_string());
                config
            }
            Err(err) => {
                self.fail("config", err.to_string());
                Config::default()
            }
        }
    }

    fn check_provider(&mut self, config: &Config) {
        match config::resolve(config, &config::Overrides::default(), &|key| {
            std::env::var(key).ok()
        }) {
            Ok(resolved) => {
                self.ok("provider", &resolved.name);
                self.ok("model", &resolved.model);
                if let Some(base_url) = &resolved.base_url {
                    self.ok("base url", base_url);
                }
                if resolved.is_mock {
                    self.warn("credentials", "mock provider; no network calls");
                } else if resolved.api_key.is_some() {
                    self.ok("credentials", "API key present");
                } else {
                    self.ok("credentials", "keyless provider");
                }
            }
            Err(err) => self.fail("provider", err.to_string()),
        }
    }

    fn check_toolchain(&self) {
        match Command::new("rustc").arg("--version").output() {
            Ok(output) if output.status.success() => {
                let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
                self.ok("rustc", version);
            }
            Ok(_) => self.warn("rustc", "`rustc --version` failed"),
            Err(_) => self.warn("rustc", "not found on PATH (only needed to build)"),
        }
    }

    fn check_sandbox(&self) {
        self.warn(
            "sandbox",
            "none in v1; tool calls require approval (see SPEC §3)",
        );
    }
}
