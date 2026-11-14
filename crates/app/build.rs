//! Embeds the application icon into the Windows executable.

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let icon = std::path::absolute(manifest_dir.join("../../packaging/windows/bluesnap.ico")).unwrap();
    println!("cargo:rerun-if-changed={}", icon.display());

    // Generated so the icon path is absolute: rc.exe resolves relative paths
    // against its working directory, not the script's.
    let rc = Path::new(&std::env::var("OUT_DIR").unwrap()).join("bluesnap.rc");
    let escaped = icon.display().to_string().replace('\\', "\\\\");
    std::fs::write(&rc, format!("1 ICON \"{escaped}\"\n")).unwrap();
    embed_resource::compile(&rc, embed_resource::NONE).manifest_optional().unwrap();
}
