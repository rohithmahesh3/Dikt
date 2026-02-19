fn main() {
    extract_version();

    let library = pkg_config::Config::new()
        .atleast_version("1.5.0")
        .probe("ibus-1.0")
        .expect("Failed to find ibus-1.0 via pkg-config");

    let glib = pkg_config::Config::new()
        .probe("glib-2.0")
        .expect("Failed to find glib-2.0 via pkg-config");

    let gobject = pkg_config::Config::new()
        .probe("gobject-2.0")
        .expect("Failed to find gobject-2.0 via pkg-config");

    let include_paths: Vec<std::path::PathBuf> = library
        .include_paths
        .iter()
        .chain(glib.include_paths.iter())
        .chain(gobject.include_paths.iter())
        .cloned()
        .collect();

    for path in &include_paths {
        println!("cargo:include={}", path.display());
    }

    println!("cargo:rerun-if-changed=wrapper.c");
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-changed=../Cargo.toml");

    let mut build = cc::Build::new();
    build.file("wrapper.c");

    for path in &include_paths {
        build.include(path);
    }

    build.define("PKGDATADIR", Some("\"/usr/share/dikt\""));
    build.compile("ibus_dikt_wrapper");

    println!("cargo:rustc-link-lib=ibus-1.0");
    println!("cargo:rustc-link-lib=glib-2.0");
    println!("cargo:rustc-link-lib=gobject-2.0");
    println!("cargo:rustc-link-lib=gio-2.0");
}

fn extract_version() {
    let manifest_path = std::path::Path::new("../Cargo.toml");
    let manifest = std::fs::read_to_string(manifest_path).expect("Failed to read root Cargo.toml");

    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("version = \"") {
            let start = trimmed.find('"').unwrap_or(0) + 1;
            let end = trimmed.rfind('"').unwrap_or(trimmed.len());
            let version = &trimmed[start..end];
            println!("cargo:rustc-env=DIKT_VERSION={}", version);
            return;
        }
    }

    println!("cargo:rustc-env=DIKT_VERSION=unknown");
}
