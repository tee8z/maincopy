use std::{env, error::Error, io, path::PathBuf};

#[path = "build_support/frontend.rs"]
mod frontend_build_support;
#[path = "build_support/frontend_digest.rs"]
mod frontend_digest_contract;
#[path = "build_support/frontend_io.rs"]
mod frontend_io;

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=build_support/frontend.rs");
    println!("cargo::rerun-if-changed=build_support/frontend_digest.rs");
    println!("cargo::rerun-if-changed=build_support/frontend_io.rs");
    println!("cargo::rerun-if-changed=frontend");
    println!("cargo::rerun-if-changed=frontend/css");
    println!("cargo::rerun-if-changed=migrations");

    let manifest_dir = required_path("CARGO_MANIFEST_DIR")?;
    let out_dir = required_path("OUT_DIR")?;
    let input_paths = frontend_build_support::compile_frontend(&manifest_dir, &out_dir)?;
    for input in input_paths {
        println!("cargo::rerun-if-changed={}", input.display());
    }
    Ok(())
}

fn required_path(name: &'static str) -> Result<PathBuf, io::Error> {
    env::var_os(name).map(PathBuf::from).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("Cargo did not provide required environment variable {name}"),
        )
    })
}
