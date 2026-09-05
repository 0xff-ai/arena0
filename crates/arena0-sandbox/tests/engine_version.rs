use std::path::Path;
use std::process::Command;

use arena0_program::ExecutionProfile;
use serde_json::Value;

#[test]
fn profile_matches_wasmtime_version() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let output = Command::new(env!("CARGO"))
        .args([
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--manifest-path",
        ])
        .arg(&manifest)
        .output()
        .expect("run cargo metadata");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: Value = serde_json::from_slice(&output.stdout).expect("Cargo metadata JSON");
    let packages = metadata["packages"].as_array().expect("packages");
    let sandbox = packages
        .iter()
        .find(|package| package["manifest_path"].as_str() == manifest.to_str())
        .expect("sandbox package");
    let node = metadata["resolve"]["nodes"]
        .as_array()
        .expect("resolved graph")
        .iter()
        .find(|node| node["id"] == sandbox["id"])
        .expect("sandbox dependency node");
    let dependency = node["deps"]
        .as_array()
        .expect("sandbox dependencies")
        .iter()
        .find(|dependency| dependency["name"] == "wasmtime")
        .expect("sandbox Wasmtime dependency");
    let wasmtime = packages
        .iter()
        .find(|package| package["id"] == dependency["pkg"])
        .expect("resolved Wasmtime package");
    let version = wasmtime["version"].as_str().expect("Wasmtime version");
    assert_eq!(
        ExecutionProfile::current().engine_id,
        format!("wasmtime-{version}-cranelift"),
        "update the execution profile when changing the sandbox engine"
    );
}
