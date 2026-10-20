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
        res.set("CompanyName", "Chimbita813");
        res.set("LegalCopyright", "Copyright (c) Chimbita813");
        res.set("OriginalFilename", "chimcast.exe");
        res.set("InternalName", "chimcast");
        // Plain user-level app: never asks for admin, declares Windows 10/11 support.
        res.set_manifest(
            r#"<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="Chimbita813.Chimcast" version="1.0.0.0"/>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security><requestedPrivileges><requestedExecutionLevel level="asInvoker" uiAccess="false"/></requestedPrivileges></security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application><supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/></application>
  </compatibility>
</assembly>"#,
        );
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed the Windows icon: {e}");
        }
    }
}
