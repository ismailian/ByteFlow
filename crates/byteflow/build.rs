//! Windows: embeds the app icon and version info into byteflow.exe so
//! Explorer, the taskbar, shortcuts and "Apps & features" show them.
//! The icon is drawn by `scripts/make-icon.ps1`.

fn main() {
    println!("cargo::rerun-if-changed=assets/icon/byteflow.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let icon = format!("{dir}/assets/icon/byteflow.ico").replace('\\', "/");
    let major: u16 = env_num("CARGO_PKG_VERSION_MAJOR");
    let minor: u16 = env_num("CARGO_PKG_VERSION_MINOR");
    let patch: u16 = env_num("CARGO_PKG_VERSION_PATCH");
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let rc = format!(
        r#"#include <winver.h>
1 ICON "{icon}"
VS_VERSION_INFO VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEOS VOS_NT_WINDOWS32
FILETYPE VFT_APP
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "ByteFlow"
      VALUE "FileDescription", "ByteFlow"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "byteflow"
      VALUE "OriginalFilename", "ByteFlow.exe"
      VALUE "ProductName", "ByteFlow Desktop"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x0409, 1200
  END
END
"#
    );
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("byteflow.rc");
    std::fs::write(&out, rc).unwrap();
    embed_resource::compile(&out, embed_resource::NONE).manifest_optional().unwrap();
}

fn env_num(key: &str) -> u16 {
    std::env::var(key).unwrap().parse().unwrap()
}
