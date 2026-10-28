fn main() {
    #[cfg(windows)]
    use std::os::windows::process::CommandExt;
    use std::{env, path::PathBuf, process::Command};
    let compiler =
        env::var_os("MESH_FXC").expect("Use scripts/Build.ps1 for the installed shader compiler");
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    println!("cargo:rerun-if-changed=shaders/nv12.hlsl");
    println!("cargo:rerun-if-env-changed=MESH_FXC");
    for (entry, profile) in [
        ("vs_video", "vs_4_0"),
        ("ps_video", "ps_4_0"),
        ("ps_probe", "ps_4_0"),
    ] {
        let mut command = Command::new(&compiler);
        command
            .args(["/nologo", "/O3", "/Ges", "/T", profile, "/E", entry, "/Fo"])
            .arg(output.join(format!("{entry}.bin")))
            .arg("shaders/nv12.hlsl");
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        assert!(
            command
                .status()
                .expect("Cannot run installed fxc")
                .success(),
            "Video shader compilation failed"
        );
    }
    // The app icon and version information of meshlive.exe (resources/meshlive.rc),
    // compiled with the SDK's rc.exe next to the configured fxc.exe.
    println!("cargo:rerun-if-changed=resources/meshlive.rc");
    println!("cargo:rerun-if-changed=resources/meshlive.ico");
    let rc = std::path::Path::new(&compiler).with_file_name("rc.exe");
    if rc.is_file() {
        let version = env::var("CARGO_PKG_VERSION").unwrap();
        let parts: Vec<&str> = version.split('.').collect();
        let res = output.join("meshlive.res");
        let mut command = Command::new(&rc);
        command
            .args(["/nologo", "/d", &format!("VER_MAJOR={}", parts[0]), "/d", &format!("VER_MINOR={}", parts[1])])
            .args(["/d", &format!("VER_PATCH={}", parts[2]), "/d", &format!("VER_TEXT=\"{version}\"")])
            .arg("/fo")
            .arg(&res)
            .arg("resources/meshlive.rc");
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        assert!(command.status().expect("Cannot run rc.exe").success(), "Resource compilation failed");
        println!("cargo:rustc-link-arg-bins={}", res.display());
    } else {
        println!("cargo:warning=rc.exe not found next to fxc.exe: meshlive.exe has no icon or version information");
    }
    println!("cargo:rerun-if-env-changed=MESH_OPUS_LIB");
    let directory = std::env::var("MESH_OPUS_LIB")
        .expect("Use scripts/Build.ps1 to verify and build the pinned native codec first");
    let library = std::path::Path::new(&directory).join("opus.lib");
    assert!(library.is_file(), "Pinned static Opus library is missing");
    println!("cargo:rerun-if-changed={}", library.display());
    println!("cargo:rustc-link-search=native={directory}");
    println!("cargo:rustc-link-lib=static=opus");
    // Pinned static libaom (software AV1 encoder) and its C boundary.
    println!("cargo:rerun-if-env-changed=MESH_AOM_LIB");
    println!("cargo:rerun-if-changed=src/video/encode_aom_shim.c");
    let directory = std::env::var("MESH_AOM_LIB")
        .expect("Use scripts/Build.ps1 to verify and build the pinned native video encoder first");
    for name in ["aom.lib", "meshlive_aom.lib"] {
        let library = std::path::Path::new(&directory).join(name);
        assert!(library.is_file(), "Pinned static libaom/shim library is missing");
        println!("cargo:rerun-if-changed={}", library.display());
    }
    println!("cargo:rustc-link-search=native={directory}");
    println!("cargo:rustc-link-lib=static=meshlive_aom");
    println!("cargo:rustc-link-lib=static=aom");
    // Pinned static libvpx (software VP9 encoder) and its C boundary.
    println!("cargo:rerun-if-env-changed=MESH_VPX_LIB");
    println!("cargo:rerun-if-changed=src/video/encode_vpx_shim.c");
    let directory = std::env::var("MESH_VPX_LIB")
        .expect("Use scripts/Build.ps1 to verify and build the pinned native video encoder first");
    for name in ["vpx.lib", "meshlive_vpx.lib"] {
        let library = std::path::Path::new(&directory).join(name);
        assert!(library.is_file(), "Pinned static libvpx/shim library is missing");
        println!("cargo:rerun-if-changed={}", library.display());
    }
    println!("cargo:rustc-link-search=native={directory}");
    // Unbundled: bundling libvpx into the rlib lost its members at the final link.
    println!("cargo:rustc-link-lib=static:-bundle=meshlive_vpx");
    println!("cargo:rustc-link-lib=static:-bundle=vpx");
    // Pinned static dav1d (software AV1 decoder fallback, Design AD) and its C boundary.
    println!("cargo:rerun-if-env-changed=MESH_DAV1D_LIB");
    println!("cargo:rerun-if-changed=src/video/decode_dav1d_shim.c");
    let directory = std::env::var("MESH_DAV1D_LIB")
        .expect("Use scripts/Build.ps1 to verify and build the pinned native video decoder first");
    for name in ["dav1d.lib", "meshlive_dav1d.lib"] {
        let library = std::path::Path::new(&directory).join(name);
        assert!(library.is_file(), "Pinned static dav1d/shim library is missing");
        println!("cargo:rerun-if-changed={}", library.display());
    }
    println!("cargo:rustc-link-search=native={directory}");
    println!("cargo:rustc-link-lib=static:-bundle=meshlive_dav1d");
    println!("cargo:rustc-link-lib=static:-bundle=dav1d");
}
