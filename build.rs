
use std::{
  env,
  fs::File,
  io::Write,
  path::{Path, PathBuf},
};

use embed_resource::CompilationResult;

extern crate embed_resource;

fn main() {
  let target_os = env::var("CARGO_CFG_TARGET_OS")
        .expect("Failed to get CARGO_CFG_TARGET_OS");
  if target_os == "windows" {
    build_windows_rc();
  }
}

fn build_windows_rc() {
  let version = env::var("CARGO_PKG_VERSION").expect("Failed to get CARGO_PKG_VERSION") + ".0";
  let out_dir = env::var("OUT_DIR").expect("Failed to get OUT_DIR");

  let path = generate_resource_rc(&version, &out_dir).expect("Failed to generate rc file.");
  generate_manifest(&version, &out_dir).expect("Failed to generate manifest file.");

  let result = embed_resource::compile(path.as_os_str(), embed_resource::NONE);
  if result != CompilationResult::Ok {
    panic!("Failed to compile resources {}", result);
  } 
}

fn generate_resource_rc(
  version: &String,
  out_dir: &String,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
  let rc_content = format!(
    r#"
#define APSTUDIO_READONLY_SYMBOLS
#include "winres.h"
#undef APSTUDIO_READONLY_SYMBOLS

LANGUAGE LANG_ENGLISH, SUBLANG_ENGLISH_US

#define RT_MANIFEST 24
1 RT_MANIFEST "manifest.xml"

VS_VERSION_INFO VERSIONINFO
 FILEVERSION {0}
 PRODUCTVERSION {0}
 FILEFLAGSMASK 0x3fL
 FILEFLAGS 0x0L
 FILEOS 0x40004L
 FILETYPE 0x1L
 FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "FileVersion", "{1}"
            VALUE "LegalCopyright", "Copyright (C) 2025"
            VALUE "ProductName", "adeploy"
            VALUE "ProductVersion", "{1}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#,
    version.replace(".", ","),
    version
  );

  let dest_path = Path::new(&out_dir).join("adeploy.rc");
  let mut file = File::create(&dest_path)?;
  file.write_all(rc_content.as_bytes())?;

  Ok(dest_path)
}

fn generate_manifest(version: &String, out_dir: &String) -> Result<(), Box<dyn std::error::Error>> {
  let manifest_content = format!(
    r#"
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" xmlns:asmv3="urn:schemas-microsoft-com:asm.v3"
  manifestVersion="1.0">
  <assemblyIdentity name="adeploy" type="win32" version="{}"></assemblyIdentity>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity language="*" name="Microsoft.Windows.Common-Controls"
        processorArchitecture="*" publicKeyToken="6595b64144ccf1df" type="win32" version="6.0.0.0"></assemblyIdentity>
    </dependentAssembly>
  </dependency>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <maxversiontested Id="10.0.18362.1"></maxversiontested>
      <supportedOS Id="{{35138b9a-5d96-4fbd-8e2d-a2440225f93a}}"></supportedOS>
      <supportedOS Id="{{4a2f28e3-53b9-4441-ba9c-d69d4a4a6e38}}"></supportedOS>
      <supportedOS Id="{{1f676c76-80e1-4239-95bb-83d0f6d0da78}}"></supportedOS>
      <supportedOS Id="{{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}}"></supportedOS>
    </application>
  </compatibility>
  <asmv3:application>
    <asmv3:windowsSettings>
      <activeCodePage xmlns="http://schemas.microsoft.com/SMI/2019/WindowsSettings">UTF-8</activeCodePage>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">permonitorv2</dpiAwareness>
      <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
      <printerDriverIsolation xmlns="http://schemas.microsoft.com/SMI/2011/WindowsSettings">true</printerDriverIsolation>
    </asmv3:windowsSettings>
  </asmv3:application>
  <asmv3:trustInfo>
    <asmv3:security>
      <asmv3:requestedPrivileges>
        <asmv3:requestedExecutionLevel level="requireAdministrator" uiAccess="false"></asmv3:requestedExecutionLevel>
      </asmv3:requestedPrivileges>
    </asmv3:security>
  </asmv3:trustInfo>
</assembly>
"#,
    version
  );

  let dest_path = Path::new(&out_dir).join("manifest.xml");
  let mut file = File::create(&dest_path)?;
  file.write_all(manifest_content.as_bytes())?;

  Ok(())
}
