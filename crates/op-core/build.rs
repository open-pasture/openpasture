// Re-embed migrations when any file in migrations/ changes.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
