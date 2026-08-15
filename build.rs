fn main() {
    // rust-embed needs the folder to exist even when nobody has run
    // `bun run build` yet — otherwise a fresh clone won't compile at all.
    std::fs::create_dir_all("web/build").ok();
    println!("cargo:rerun-if-changed=web/build");
}
