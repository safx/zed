fn main() {
    // RustEmbed tracks existing files, but adding an icon must also rebuild the embedded assets.
    println!("cargo:rerun-if-changed=../../assets/icons");
}
