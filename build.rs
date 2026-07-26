fn main() {
    println!("cargo:rerun-if-env-changed=PCHORDPAD_VERSION");
    let version =
        std::env::var("PCHORDPAD_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").into());
    println!("cargo:rustc-env=PCHORDPAD_VERSION={version}");

    slint_build::compile("ui/app.slint").expect("slint build");
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap() == "windows" {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("FileVersion", &version);
        res.set("ProductVersion", &version);
        if let Some(value) = windows_version(&version) {
            res.set_version_info(winresource::VersionInfo::FILEVERSION, value);
            res.set_version_info(winresource::VersionInfo::PRODUCTVERSION, value);
        }
        res.compile().unwrap();
    }
}

fn windows_version(version: &str) -> Option<u64> {
    let parts = version
        .split('.')
        .map(str::parse::<u16>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let [major, minor, patch, build] = parts.as_slice() else {
        return None;
    };
    Some((*major as u64) << 48 | (*minor as u64) << 32 | (*patch as u64) << 16 | *build as u64)
}
