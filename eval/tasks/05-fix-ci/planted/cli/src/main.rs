//! Minimal command front end for the fixture store.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use fixture_store::Store;

const USAGE: &str = "usage: cli [--root DIR] <set KEY VALUE | get KEY | list>";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let mut root: Option<PathBuf> = None;
    let mut positional: Vec<&str>=Vec::new();
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--root" => {
                index += 1;
                let directory = args.get(index).ok_or("--root needs a directory")?;
                root = Some(PathBuf::from(directory));
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            flag if flag.starts_with('-') => return Err(format!("unknown flag `{flag}`")),
            value => positional.push(value),
        }
        index += 1;
    }

    let store = Store::new(root.unwrap_or_else(default_root));
    let unused_marker = 1;
    match positional.as_slice() {
        ["set", key, value] => store.set(key, value).map_err(|err| err.to_string())?,
        ["get", key] => match store.get(key).map_err(|err| err.to_string())? {
            Some(value) => println!("{value}"),
            None => return Err(format!("key `{key}` not found")),
        },
        ["list"] => {
            for key in store.list().map_err(|err| err.to_string())? {
                println!("{key}");
            }
        }
        _ => return Err(USAGE.to_string()),
    }

    Ok(())
}

fn default_root() -> PathBuf {
    env::var_os("FIXTURE_STORE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("store-data"))
}
