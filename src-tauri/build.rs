fn main() {
    let is_win7_target = std::env::var("CARGO_CFG_TARGET_VENDOR").as_deref() == Ok("win7");
    if is_win7_target && std::env::var_os("CARGO_FEATURE_CUSTOM_PROTOCOL").is_none() {
        panic!("Windows 7 release builds must enable the custom-protocol feature");
    }

    // Report which optional host APIs this build carries. `env!("DBX_BUILD_FEATURES")`
    // reaches the About dialog and the plugin Host API handshake, so a patched host can
    // be told apart from a stock one without inferring it from the version number.
    let mut host_features: Vec<&str> = Vec::new();
    if std::env::var_os("CARGO_FEATURE_PLUGIN_TRANSFER").is_some() {
        host_features.push("plugin-transfer");
    }
    println!("cargo:rustc-env=DBX_BUILD_FEATURES={}", host_features.join(","));

    // Force rebuild to re-embed frontend assets
    tauri_build::build()
}
