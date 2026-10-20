fn main() {
    tauri_build::build();
    // With +crt-static (.cargo/config.toml), tauri-build links the "hybrid CRT": static vcruntime,
    // dynamic UCRT (part of Windows 10+). Rustc's static-CRT flags exclude ucrt as a default lib, so
    // name it as an explicit input, which default-lib exclusions don't touch.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg=ucrt.lib");
    }
}
