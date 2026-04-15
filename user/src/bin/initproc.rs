#![no_std]
#![no_main]

extern crate user_lib;

use user_lib::println;
use user_lib::{exec, fork, wait, yield_};

const SHELL_CANDIDATES: [(&str, &str); 2] = [
    ("/user_shell", "/user_shell\0"),
    ("/x1/user_shell", "/x1/user_shell\0"),
];

#[unsafe(no_mangle)]
fn main() -> i32 {
    if fork() == 0 {
        for (shell_path, shell_path_cstr) in SHELL_CANDIDATES {
            if exec(shell_path_cstr, &[core::ptr::null::<u8>()]) != -1 {
                return 0;
            }
            println!("initproc: failed to exec {}", shell_path);
        }
        -1
    } else {
        loop {
            let mut exit_code: i32 = 0;
            let pid = wait(&mut exit_code);
            if pid == -1 {
                yield_();
                continue;
            }
            /*
            println!(
                "[initproc] Released a zombie process, pid={}, exit_code={}",
                pid,
                exit_code,
            );
            */
        }
    }
}
