fn main() {
    println!("cargo:rerun-if-changed=src/cursor.m");
    cc::Build::new()
        .file("src/cursor.m")
        .flag("-fobjc-arc")
        .compile("macrdp_cursor");
    println!("cargo:rustc-link-lib=framework=AppKit");
    println!("cargo:rustc-link-lib=framework=CoreGraphics");
}
