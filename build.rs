use std::env;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    // Embed build timestamp as seconds since Unix epoch
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Write timestamp to a file in OUT_DIR so cargo detects the change
    let out_dir = env::var("OUT_DIR").unwrap();
    let dest_path = Path::new(&out_dir).join("build_timestamp.txt");
    fs::write(&dest_path, timestamp.to_string()).unwrap();

    println!("cargo:rustc-env=BUILD_TIMESTAMP={}", timestamp);

    // Always rerun build script (no rerun-if-changed means always run)
}
