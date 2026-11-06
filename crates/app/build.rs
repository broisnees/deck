//! Windows resources for deck.exe: the icon Explorer, the taskbar and the
//! Start menu show (icon id 1), and the version block in Properties > Details.
//! Nothing to do for other targets.

use std::{env, fs, path::Path};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/windows");
    let icon = root.join("deck.ico");
    let manifest = root.join("deck.manifest.xml");
    println!("cargo:rerun-if-changed={}", icon.display());
    println!("cargo:rerun-if-changed={}", manifest.display());

    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let mut parts = version.split(['.', '-', '+']).map(|p| p.parse::<u16>().unwrap_or(0));
    let (a, b, c) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0));

    // The .rc goes through llvm-rc with no preprocessor on Linux, so no
    // #include <winver.h>: 0x40004 is VOS_NT_WINDOWS32, FILETYPE 1 is VFT_APP,
    // 24 is RT_MANIFEST.
    let path = |p: &Path| p.display().to_string().replace('\\', "\\\\");
    let mut rc = format!("1 ICON \"{}\"\n", path(&icon));
    // gpui's manifest settings plus the detached console policy. gpui's
    // own `windows-manifest` feature is off, so this is the only one: two
    // RT_MANIFEST id 1 resources would not link.
    rc += &format!("1 24 \"{}\"\n", path(&manifest));
    rc += &format!(
        r#"1 VERSIONINFO
FILEVERSION {a},{b},{c},0
PRODUCTVERSION {a},{b},{c},0
FILEFLAGSMASK 0x3f
FILEFLAGS 0
FILEOS 0x40004
FILETYPE 1
FILESUBTYPE 0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "CompanyName", "Deck"
      VALUE "FileDescription", "Deck"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "deck"
      VALUE "OriginalFilename", "deck.exe"
      VALUE "ProductName", "Deck"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );
    let out = Path::new(&env::var("OUT_DIR").unwrap()).join("deck.rc");
    fs::write(&out, rc).unwrap();
    embed_resource::compile(&out, embed_resource::NONE).manifest_optional().unwrap();
}
