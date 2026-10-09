fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=WT_BUILD_ID");
    let build = std::env::var("WT_BUILD_ID")
        .unwrap_or_else(|_| format!("dev-{}", env!("CARGO_PKG_VERSION")));
    assert!(
        !build.is_empty()
            && build
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)),
        "WT_BUILD_ID must be a safe release identifier"
    );
    let target = std::env::var("TARGET").expect("Cargo supplies TARGET to build scripts");
    println!("cargo:rustc-env=WT_BUILD_ID={build}");
    println!("cargo:rustc-env=WT_TARGET={target}");
}
