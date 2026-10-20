fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../assets/chimcast.ico");
    println!("cargo:rerun-if-env-changed=CHIMCAST_BUILD");
    println!("cargo:rerun-if-env-changed=CHIMCAST_UPDATE_REPO");
    // Embed the icon and version info in chimcast.exe (also when cross-compiling from Linux).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/chimcast.ico");
        res.set("ProductName", "Chimcast");
        res.set("FileDescription", "Chimcast");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed the Windows icon: {e}");
        }
    }
}
