use std::{env, path::PathBuf, process::Command};
#[cfg(windows)]
use std::os::windows::process::CommandExt;

fn main() {
    let compiler = env::var_os("MESH_FXC").expect("Use scripts/Build.ps1 to select the installed shader compiler");
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    println!("cargo:rerun-if-changed=shaders/egui.hlsl");
    println!("cargo:rerun-if-env-changed=MESH_FXC");
    for (entry, profile, filename) in [("vs_egui", "vs_4_0", "vs_egui.bin"), ("ps_egui", "ps_4_0", "ps_egui.bin")] {
        let mut command = Command::new(&compiler);
        command.args(["/nologo", "/O3", "/Ges", "/T", profile, "/E", entry, "/Fo"])
            .arg(output.join(filename)).arg("shaders/egui.hlsl");
        #[cfg(windows)]
        command.creation_flags(0x08000000); // Do not create compiler console windows.
        assert!(command.status().expect("Cannot run installed fxc").success(), "Shader compilation failed");
    }
}
