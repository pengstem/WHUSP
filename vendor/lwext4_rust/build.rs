use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let c_path = PathBuf::from("c/lwext4")
        .canonicalize()
        .expect("cannot canonicalize path");

    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let lwext4_lib = &format!("lwext4-{arch}");
    {
        let mut cmd = Command::new("make");
        cmd.args([
            "musl-generic",
            "-C",
            c_path.to_str().expect("invalid path of lwext4"),
        ])
        .arg(format!("ARCH={arch}"))
        .arg(format!(
            "ULIBC={}",
            if cfg!(feature = "std") { "OFF" } else { "ON" }
        ));
        configure_toolchain(&arch, &mut cmd);
        let status = cmd
            .status()
            .expect("failed to execute process: make lwext4");
        assert!(status.success());
    }
    generates_bindings_to_rust(binding_include_arg(&arch));

    println!("cargo:rustc-link-lib=static={lwext4_lib}");
    println!(
        "cargo:rustc-link-search=native={}",
        c_path.to_str().unwrap()
    );
    println!("cargo:rerun-if-changed=c/wrapper.h");
    println!("cargo:rerun-if-changed={}/src", c_path.to_str().unwrap());
}

fn configure_toolchain(arch: &str, cmd: &mut Command) {
    match arch {
        "riscv64" => {
            cmd.env("CC", "riscv64-linux-musl-gcc");
            cmd.env("CXX", "riscv64-linux-gnu-g++");
            cmd.env("AR", "riscv64-linux-gnu-ar");
            cmd.env("OBJCOPY", "riscv64-linux-gnu-objcopy");
            cmd.env("OBJDUMP", "riscv64-linux-gnu-objdump");
            cmd.env("SIZE", "riscv64-linux-gnu-size");
        }
        "x86_64" => {
            cmd.env("CC", "cc");
            cmd.env("CXX", "c++");
            cmd.env("AR", "ar");
            cmd.env("OBJCOPY", "objcopy");
            cmd.env("OBJDUMP", "objdump");
            cmd.env("SIZE", "size");
        }
        _ => {}
    }
}

fn binding_include_arg(arch: &str) -> Option<String> {
    let candidates: &[&str] = match arch {
        "riscv64" => &[
            "/usr/riscv64-linux-musl/lib/musl/include",
            "/usr/riscv64-linux-gnu/include",
        ],
        "x86_64" => &["/usr/include"],
        _ => &[],
    };
    candidates
        .iter()
        .find(|path| Path::new(path).exists())
        .map(|path| format!("-I{path}"))
}

fn generates_bindings_to_rust(mpath: Option<String>) {
    let target = env::var("TARGET").unwrap();
    if target.ends_with("-softfloat") {
        // Clang does not recognize the `-softfloat` suffix
        unsafe { env::set_var("TARGET", target.replace("-softfloat", "")) };
    }

    let mut bindings = bindgen::Builder::default()
        .use_core()
        .wrap_unsafe_ops(true)
        // The input header we would like to generate bindings for.
        .header("c/wrapper.h")
        .clang_arg("-I./c/lwext4/include")
        .clang_arg("-I./c/lwext4/build_musl-generic/include/")
        .layout_tests(false)
        // Tell cargo to invalidate the built crate whenever any of the included header files changed.
        .parse_callbacks(Box::new(CustomCargoCallbacks));
    if let Some(mpath) = mpath {
        bindings = bindings.clang_arg(mpath);
    }
    let bindings = bindings.generate().expect("Unable to generate bindings");

    // Restore the original target environment variable
    unsafe { env::set_var("TARGET", target) };

    // Write the bindings to the $OUT_DIR/bindings.rs file.
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}

#[derive(Debug)]
struct CustomCargoCallbacks;
impl bindgen::callbacks::ParseCallbacks for CustomCargoCallbacks {
    fn header_file(&self, filename: &str) {
        add_include(filename);
    }

    fn include_file(&self, filename: &str) {
        add_include(filename);
    }

    fn read_env_var(&self, key: &str) {
        println!("cargo:rerun-if-env-changed={key}");
    }
}

fn add_include(filename: &str) {
    if !Path::new(filename).ends_with("ext4_config.h") {
        println!("cargo:rerun-if-changed={filename}");
    }
}
