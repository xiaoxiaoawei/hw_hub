use std::{env, fs, path::Path};

fn main() {
    let root = env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets CARGO_MANIFEST_DIR");
    for (filename, flag) in [
        ("ch343.zip", "has_ch343_driver"),
        ("cp210x.zip", "has_cp210x_driver"),
    ] {
        let relative = format!("assets/drivers/{filename}");
        println!("cargo:rerun-if-changed={relative}");
        println!("cargo:rustc-check-cfg=cfg({flag})");
        let path = Path::new(&root).join(&relative);
        if fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() > 0) {
            println!("cargo:rustc-cfg={flag}");
        }
    }

    // A build script runs on the host, so cfg!(windows) would skip resources
    // when cross-compiling the Windows executable from Linux.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        windows_resources(Path::new(&root));
    }
}

fn windows_resources(root: &Path) {
    println!("cargo:rerun-if-changed=assets/app.ico");
    for variable in [
        "WINDRES",
        "AR",
        "CROSS_COMPILE",
        "RC_PATH",
        "WindowsSdkVerBinPath",
    ] {
        println!("cargo:rerun-if-env-changed={variable}");
    }

    let version = env::var("CARGO_PKG_VERSION").expect("Cargo sets the package version");
    let component = |name: &str| {
        u64::from(
            env::var(name)
                .expect("Cargo sets numeric version components")
                .parse::<u16>()
                .expect("Windows version components must fit in 16 bits"),
        )
    };
    // Windows' numeric version has four words. SemVer's prerelease/build
    // suffix remains intact in the string values; the revision word is zero.
    let numeric_version = (component("CARGO_PKG_VERSION_MAJOR") << 48)
        | (component("CARGO_PKG_VERSION_MINOR") << 32)
        | (component("CARGO_PKG_VERSION_PATCH") << 16);
    let icon = root.join("assets/app.ico");
    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon(icon.to_str().expect("The icon path must be valid Unicode"))
        .set("FileVersion", &version)
        .set("ProductVersion", &version)
        .set("FileDescription", "FlashTool batch flasher")
        .set("ProductName", "FlashTool")
        .set("InternalName", "FlashTool")
        .set("OriginalFilename", "FlashTool.exe")
        .set_version_info(winresource::VersionInfo::FILEVERSION, numeric_version)
        .set_version_info(winresource::VersionInfo::PRODUCTVERSION, numeric_version);
    if env::var("CARGO_PKG_VERSION_PRE").is_ok_and(|value| !value.is_empty()) {
        resource.set_version_info(
            winresource::VersionInfo::FILEFLAGS,
            winresource::VersionInfo::VS_FF_PRERELEASE,
        );
    }
    resource.compile().expect(
        "Compile Windows version/icon resources with the Windows SDK (MSVC) or MinGW windres/ar (GNU)",
    );
}
