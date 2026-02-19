fn main() {
    std::process::Command::new("glib-compile-schemas")
        .arg("data")
        .status()
        .expect("Failed to compile GSettings schemas. Ensure glib2-devel is installed.");
}
