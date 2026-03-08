use std::{path::PathBuf, process::Command};

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let web_dir = manifest_dir.join("web");
    let icon_path = manifest_dir.join("assets").join("icon.ico");

    // Re-run this build script whenever web sources change.
    println!("cargo:rerun-if-changed=web/src");
    println!("cargo:rerun-if-changed=web/index.html");
    println!("cargo:rerun-if-changed=web/session.html");
    println!("cargo:rerun-if-changed=web/package.json");
    println!("cargo:rerun-if-changed=web/tsconfig.json");
    println!("cargo:rerun-if-changed=web/vite.config.ts");

    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed={}", icon_path.display());
        let mut res = winres::WindowsResource::new();
        res.set_icon(icon_path.to_str().expect("invalid icon path"));
        res.compile()
            .expect("build.rs: failed to compile Windows resources");
    }

    let profile = std::env::var("PROFILE").unwrap_or_default();
    if profile != "release" {
        // Debug / test builds: nothing to do here.  The Vite dev server is
        // started automatically at runtime inside the daemon process.
        return;
    }

    // ── Release build ────────────────────────────────────────────────────────
    // Run `npm install` then `npm run build` so that web/dist exists and can
    // be embedded into the binary via rust-embed.
    let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };

    eprintln!("build.rs: running `npm install` in {}", web_dir.display());
    let status = Command::new(npm)
        .arg("install")
        .arg("--include=optional")
        .arg("--no-audit")
        .arg("--no-fund")
        .current_dir(&web_dir)
        .status()
        .expect("build.rs: failed to spawn `npm install` – is Node.js installed?");
    assert!(status.success(), "build.rs: `npm install` failed");

    eprintln!("build.rs: running `npm run build` in {}", web_dir.display());
    let status = Command::new(npm)
        .args(["run", "build"])
        .current_dir(&web_dir)
        .status()
        .expect("build.rs: failed to spawn `npm run build`");
    assert!(status.success(), "build.rs: `npm run build` failed");

    eprintln!("build.rs: web/dist is ready for embedding");
}
