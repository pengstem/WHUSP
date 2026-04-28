use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let c_src_path = PathBuf::from("c/lwext4")
        .canonicalize()
        .expect("cannot canonicalize path");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is unset"));
    let c_build_path = out_dir.join("lwext4-src");
    stage_lwext4_source(&c_src_path, &c_build_path);

    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let lwext4_lib = &format!("lwext4-{arch}");
    {
        let mut cmd = Command::new("make");
        cmd.args([
            "musl-generic",
            "-C",
            c_build_path.to_str().expect("invalid path of lwext4"),
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
    generates_bindings_to_rust(&arch, &c_build_path);

    println!("cargo:rustc-link-lib=static={lwext4_lib}");
    println!("cargo:rustc-link-search=native={}", c_build_path.display());
    println!("cargo:rerun-if-changed=c/wrapper.h");
    println!(
        "cargo:rerun-if-changed={}/src",
        c_src_path.to_str().unwrap()
    );
    println!(
        "cargo:rerun-if-changed={}/Makefile",
        c_src_path.to_str().unwrap()
    );
    println!(
        "cargo:rerun-if-changed={}/toolchain/musl-generic.cmake",
        c_src_path.to_str().unwrap()
    );
}

fn stage_lwext4_source(src: &Path, dst: &Path) {
    if dst.exists() {
        fs::remove_dir_all(dst).expect("failed to clean staged lwext4 source");
    }
    fs::create_dir_all(dst.parent().expect("staged source has no parent"))
        .expect("failed to create OUT_DIR parent for staged lwext4 source");
    let status = Command::new("cp")
        .args(["-a", &format!("{}/.", src.display()), dst.to_str().unwrap()])
        .status()
        .expect("failed to stage lwext4 source into OUT_DIR");
    assert!(status.success());
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
        "loongarch64" => {
            cmd.env("CC", "loongarch64-linux-musl-gcc");
            cmd.env("CXX", "loongarch64-linux-gnu-g++");
            cmd.env("AR", "loongarch64-linux-gnu-ar");
            cmd.env("OBJCOPY", "loongarch64-linux-gnu-objcopy");
            cmd.env("OBJDUMP", "loongarch64-linux-gnu-objdump");
            cmd.env("SIZE", "loongarch64-linux-gnu-size");
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

fn compiler_output(compiler: &str, arg: &str) -> Option<String> {
    let output = Command::new(compiler).arg(arg).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn push_include_arg(args: &mut Vec<String>, path: impl AsRef<Path>) {
    let path = path.as_ref();
    if path.exists() {
        args.push(format!("-I{}", path.display()));
    }
}

fn binding_clang_args(arch: &str) -> Vec<String> {
    match arch {
        "riscv64" => {
            let mut args = vec![
                "--target=riscv64-unknown-linux-musl".to_string(),
                "-march=rv64gc".to_string(),
                "-mabi=lp64d".to_string(),
                "-mcmodel=medany".to_string(),
            ];

            if let Some(sysroot) = compiler_output("riscv64-linux-musl-gcc", "-print-sysroot") {
                if sysroot != "/" {
                    args.push(format!("--sysroot={sysroot}"));
                    push_include_arg(&mut args, Path::new(&sysroot).join("include"));
                    push_include_arg(&mut args, Path::new(&sysroot).join("usr/include"));
                }
            }

            if let Some(gcc_include) =
                compiler_output("riscv64-linux-musl-gcc", "-print-file-name=include")
            {
                push_include_arg(&mut args, gcc_include);
            }

            for candidate in [
                "/usr/riscv64-linux-musl/include",
                "/usr/riscv64-linux-musl/lib/musl/include",
            ] {
                push_include_arg(&mut args, candidate);
            }

            args
        }
        "loongarch64" => {
            let mut args = vec!["--target=loongarch64-unknown-linux-musl".to_string()];

            if let Some(sysroot) = compiler_output("loongarch64-linux-musl-gcc", "-print-sysroot") {
                if sysroot != "/" {
                    args.push(format!("--sysroot={sysroot}"));
                    push_include_arg(&mut args, Path::new(&sysroot).join("include"));
                    push_include_arg(&mut args, Path::new(&sysroot).join("usr/include"));
                }
            }

            if let Some(gcc_include) =
                compiler_output("loongarch64-linux-musl-gcc", "-print-file-name=include")
            {
                push_include_arg(&mut args, gcc_include);
            }

            for candidate in [
                "/usr/loongarch64-linux-musl/include",
                "/usr/loongarch64-linux-musl/lib/musl/include",
                "/opt/loongarch64-linux-musl-cross/loongarch64-linux-musl/include",
            ] {
                push_include_arg(&mut args, candidate);
            }

            args
        }
        "x86_64" => vec!["-I/usr/include".to_string()],
        _ => Vec::new(),
    }
}

fn bindgen_target(arch: &str, cargo_target: &str) -> String {
    match arch {
        // Bindgen only needs a clang-compatible target for parsing C headers.
        // The kernel itself still builds for riscv64gc-unknown-none-elf.
        "riscv64" => "riscv64-unknown-linux-musl".to_string(),
        "loongarch64" => "loongarch64-unknown-linux-musl".to_string(),
        _ => cargo_target.replace("-softfloat", ""),
    }
}

fn generates_bindings_to_rust(arch: &str, c_build_path: &Path) {
    let cargo_target = env::var("TARGET").unwrap();
    unsafe { env::set_var("TARGET", bindgen_target(arch, &cargo_target)) };

    let mut bindings = bindgen::Builder::default()
        .use_core()
        .wrap_unsafe_ops(true)
        // The input header we would like to generate bindings for.
        .header("c/wrapper.h")
        .clang_arg(format!("-I{}/include", c_build_path.display()))
        .clang_arg(format!(
            "-I{}/build_musl-generic/include/",
            c_build_path.display()
        ))
        .layout_tests(false)
        // Tell cargo to invalidate the built crate whenever any of the included header files changed.
        .parse_callbacks(Box::new(CustomCargoCallbacks));
    for arg in binding_clang_args(arch) {
        bindings = bindings.clang_arg(arg);
    }
    let bindings = bindings.generate().expect("Unable to generate bindings");

    // Restore the original target environment variable
    unsafe { env::set_var("TARGET", cargo_target) };

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
