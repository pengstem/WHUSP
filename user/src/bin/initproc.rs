#![no_std]
#![no_main]

extern crate user_lib;

use user_lib::println;
use user_lib::{exec, fork, wait, yield_};

#[unsafe(no_mangle)]
fn main() -> i32 {
    if fork() == 0 {
        let shell_path = "/x1/user_shell";
        let shell_path_cstr = "/x1/user_shell\0";
        if exec(shell_path_cstr, &[core::ptr::null::<u8>()]) == -1 {
            println!("initproc: failed to exec {}", shell_path);
            return -1;
        }
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
    0
}
