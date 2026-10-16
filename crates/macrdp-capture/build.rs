fn main() {
    println!("cargo:rerun-if-changed=src/cursor.m");
    println!("cargo:rerun-if-changed=src/virtual_display.m");
    cc::Build::new()
        .file("src/cursor.m")
        .file("src/virtual_display.m")
        .flag("-fobjc-arc")
        .compile("macrdp_capture_objc");
    println!("cargo:rustc-link-lib=framework=AppKit");
    println!("cargo:rustc-link-lib=framework=CoreGraphics");
    println!("cargo:rustc-link-lib=framework=Foundation");
}
