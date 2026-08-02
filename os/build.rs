fn main() {
    println!("cargo::rerun-if-env-changed=TARGET");

    match std::env::var("TARGET").as_deref() {
        Ok("riscv64gc-unknown-none-elf") => use_linker_script("src/linker-qemu.ld"),
        Ok("loongarch64-unknown-none") | Ok("loongarch64-unknown-none-softfloat") => {
            use_linker_script("src/linker-loongarch64.ld")
        }
        _ => {}
    }
}

fn use_linker_script(path: &str) {
    println!("cargo::rerun-if-changed={path}");
    println!("cargo::rustc-link-arg-bin=os=-T{path}");
}
