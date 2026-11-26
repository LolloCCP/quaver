fn main() {
    let config = slint_build::CompilerConfiguration::new()
        .with_style("fluent-dark".into())
        .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles);
    slint_build::compile_with_config("ui/app.slint", config).unwrap();

    // Name and version in the file's properties: code signing checks them against each release.
    println!("cargo:rerun-if-env-changed=RUSTBEAT_VERSION");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let version = std::env::var("RUSTBEAT_VERSION")
            .map(|v| v.trim_start_matches('v').to_owned())
            .unwrap_or_else(|_| std::env::var("CARGO_PKG_VERSION").unwrap());
        let parts: Vec<u64> = version.split('.').map(|p| p.parse().expect("version like 0.1.2")).collect();
        assert_eq!(parts.len(), 3, "version like 0.1.2: {version}");
        let packed = parts[0] << 48 | parts[1] << 32 | parts[2] << 16;
        let mut res = winresource::WindowsResource::new();
        res.set("ProductName", "Rustbeat")
            .set("FileDescription", "Rustbeat")
            .set("ProductVersion", &version)
            .set("FileVersion", &version)
            .set_version_info(winresource::VersionInfo::PRODUCTVERSION, packed)
            .set_version_info(winresource::VersionInfo::FILEVERSION, packed);
        res.compile().unwrap();
    }
}
